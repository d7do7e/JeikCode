//! Cross-process live session mesh & peer talk engine (`~/.jeikcode/live/*.json` + loopback IPC).
//!
//! Enables standalone TUI, CLIx, and daemon `CodingRuntime` processes to discover
//! live peer sessions across OS PIDs, inspect recent conversation turns non-blockingly,
//! and exchange peer messages under strict cycle/hop-depth, busy-turn, and cross-repo
//! trust guardrails.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jeikcode_capabilities::memory::resolve_canonical_repo_root;
use jeikcode_capabilities::session::SessionManager;
use jeikcode_kernel::event::StopReason;
use jeikcode_kernel::message::SessionSnapshot;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    CodingRuntimeHandle, PeerSubmitOrigin, RuntimePhase, SubmitReceipt, UserInput,
};
use crate::session_mesh_tools::{format_snapshot_turns, last_assistant_reply_text};

/// Default maximum cross-session peer message hop depth before rejecting forwarding.
pub const DEFAULT_MESH_MAX_HOPS: u32 = 3;
/// Default wait window when `send_to_session` waits for the peer's reply.
pub const DEFAULT_PEER_REPLY_TIMEOUT_SECS: u64 = 120;
/// Default timeout constants shared by IPC server and client.
pub(crate) const IPC_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const IPC_IO_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_IPC_LINE_BYTES: usize = 512 * 1024;
const TURN_OUTCOME_CHANNEL_CAP: usize = 32;

/// Whether the cross-process session mesh is enabled (`JEIKCODE_SESSION_MESH`, default ON).
pub(crate) fn session_mesh_enabled() -> bool {
    crate::parts::subagent_enabled_from_env(std::env::var("JEIKCODE_SESSION_MESH").ok().as_deref())
}

/// Resolve the maximum mesh hop depth (`JEIKCODE_MESH_MAX_HOPS`, default 3).
pub(crate) fn max_mesh_hops() -> u32 {
    std::env::var("JEIKCODE_MESH_MAX_HOPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MESH_MAX_HOPS)
}

/// Directory storing live session descriptor files (`$JEIKCODE_HOME/live`).
pub fn live_registry_dir() -> PathBuf {
    jeikcode_config::Config::config_dir().join("live")
}

/// Validate a session ID before turning it into a filename under `live/`.
pub(crate) fn validate_live_session_id(id: &str) -> Result<&str, String> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err("session_id must be 1..=128 characters".to_string());
    }
    if trimmed.starts_with('.')
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("invalid session_id `{trimmed}`"));
    }
    Ok(trimmed)
}

/// Descriptor persisted at `~/.jeikcode/live/<session_id>.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSessionDescriptor {
    pub session_id: String,
    pub pid: u32,
    pub endpoint: String,
    pub auth_token: String,
    pub working_dir: PathBuf,
    pub repo_root: PathBuf,
    pub model: String,
    pub started_at_ms: i64,
}

/// Live session summary returned by [`discover_live_sessions`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSessionView {
    pub session_id: String,
    pub pid: u32,
    pub working_dir: PathBuf,
    pub repo_root: PathBuf,
    pub same_repo: bool,
    pub repo_scope: String,
    pub is_self: bool,
    pub model: String,
    pub phase: String,
    pub busy: bool,
    pub message_count: usize,
    pub started_at_ms: i64,
}

/// Cycle / hop-depth provenance carried while a runtime processes a peer-originated turn.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshHopContext {
    pub hop_depth: u32,
    pub visited_sessions: Vec<String>,
}

/// Completion outcome broadcast when a local runtime turn ends.
#[derive(Clone, Debug)]
pub(crate) struct MeshTurnOutcome {
    pub turn_id: u64,
    pub stop_reason: String,
    pub assistant_reply: String,
    pub error: Option<String>,
}

/// Shared runtime identity + snapshot state exposed to the local mesh IPC server and tools.
#[derive(Clone)]
pub(crate) struct MeshLocalState {
    pub live_dir: PathBuf,
    pub session_id: Arc<RwLock<String>>,
    pub working_dir: Arc<RwLock<PathBuf>>,
    pub repo_root: Arc<RwLock<PathBuf>>,
    pub model: Arc<RwLock<String>>,
    pub latest_snapshot: Arc<RwLock<Option<Arc<SessionSnapshot>>>>,
    pub active_hop: Arc<RwLock<Option<MeshHopContext>>>,
    pub turn_outcomes: broadcast::Sender<MeshTurnOutcome>,
    pub handle: CodingRuntimeHandle,
}

impl MeshLocalState {
    pub(crate) fn current_session_id(&self) -> String {
        self.session_id
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub(crate) fn current_working_dir(&self) -> PathBuf {
        self.working_dir
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    pub(crate) fn current_repo_root(&self) -> PathBuf {
        self.repo_root
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    pub(crate) fn current_hop_context(&self) -> MeshHopContext {
        self.active_hop
            .read()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default()
    }

    pub(crate) fn clear_hop_context(&self) {
        if let Ok(mut guard) = self.active_hop.write() {
            *guard = None;
        }
    }

    pub(crate) fn update_snapshot(&self, snap: Option<Arc<SessionSnapshot>>) {
        if let Ok(mut guard) = self.latest_snapshot.write() {
            *guard = snap;
        }
    }

    pub(crate) fn notify_turn_completed(
        &self,
        turn_id: u64,
        reason: StopReason,
        snapshot: &SessionSnapshot,
    ) {
        let assistant_reply = last_assistant_reply_text(snapshot);
        let _ = self.turn_outcomes.send(MeshTurnOutcome {
            turn_id,
            stop_reason: format!("{reason:?}"),
            assistant_reply,
            error: None,
        });
    }

    pub(crate) fn notify_turn_failed(&self, turn_id: u64, message: impl Into<String>) {
        let _ = self.turn_outcomes.send(MeshTurnOutcome {
            turn_id,
            stop_reason: "Failed".to_string(),
            assistant_reply: String::new(),
            error: Some(message.into()),
        });
    }
}

/// RAII registration owning the loopback IPC listener task and descriptor file cleanup.
pub(crate) struct SessionMeshHost {
    pub local: MeshLocalState,
    endpoint: String,
    auth_token: String,
    started_at_ms: i64,
    descriptor_path: Mutex<Option<PathBuf>>,
    cancel: CancellationToken,
    server_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SessionMeshHost {
    /// Start the loopback IPC listener and publish `~/.jeikcode/live/<session_id>.json`.
    pub(crate) fn start(
        live_dir: PathBuf,
        session_id: String,
        working_dir: PathBuf,
        model: String,
        handle: CodingRuntimeHandle,
    ) -> io::Result<Arc<Self>> {
        let valid_id = validate_live_session_id(&session_id)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
            .to_string();
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;
        let endpoint = addr.to_string();
        let listener = TcpListener::from_std(std_listener)?;

        let repo_root = resolve_canonical_repo_root(&working_dir);
        let auth_token = uuid::Uuid::new_v4().simple().to_string();
        let started_at_ms = now_ms();

        let (turn_outcomes, _) = broadcast::channel(TURN_OUTCOME_CHANNEL_CAP);
        let local = MeshLocalState {
            live_dir: live_dir.clone(),
            session_id: Arc::new(RwLock::new(valid_id.clone())),
            working_dir: Arc::new(RwLock::new(working_dir.clone())),
            repo_root: Arc::new(RwLock::new(repo_root.clone())),
            model: Arc::new(RwLock::new(model.clone())),
            latest_snapshot: Arc::new(RwLock::new(None)),
            active_hop: Arc::new(RwLock::new(None)),
            turn_outcomes,
            handle,
        };

        let descriptor = LiveSessionDescriptor {
            session_id: valid_id.clone(),
            pid: std::process::id(),
            endpoint: endpoint.clone(),
            auth_token: auth_token.clone(),
            working_dir,
            repo_root,
            model,
            started_at_ms,
        };
        let path = write_descriptor(&live_dir, &descriptor)?;

        let cancel = CancellationToken::new();
        let host = Arc::new(Self {
            local,
            endpoint,
            auth_token,
            started_at_ms,
            descriptor_path: Mutex::new(Some(path)),
            cancel: cancel.clone(),
            server_task: Mutex::new(None),
        });

        let server_host = Arc::clone(&host);
        let task = tokio::spawn(async move {
            run_ipc_server(listener, server_host, cancel).await;
        });
        if let Ok(mut guard) = host.server_task.lock() {
            *guard = Some(task);
        }
        Ok(host)
    }

    /// Re-publish the descriptor after session transition (`FreshSession`, `ResumeSession`, `ChangeDirectory`, `Reprepare`).
    pub(crate) fn reconfigure_session(
        &self,
        new_session_id: Option<String>,
        new_working_dir: PathBuf,
        new_model: String,
    ) {
        self.remove_descriptor_file();
        let Some(sid) = new_session_id else {
            return;
        };
        let Ok(valid_id) = validate_live_session_id(&sid) else {
            return;
        };
        let repo_root = resolve_canonical_repo_root(&new_working_dir);
        if let Ok(mut g) = self.local.session_id.write() {
            *g = valid_id.to_string();
        }
        if let Ok(mut g) = self.local.working_dir.write() {
            *g = new_working_dir.clone();
        }
        if let Ok(mut g) = self.local.repo_root.write() {
            *g = repo_root.clone();
        }
        if let Ok(mut g) = self.local.model.write() {
            *g = new_model.clone();
        }
        self.local.update_snapshot(None);
        self.local.clear_hop_context();

        let desc = LiveSessionDescriptor {
            session_id: valid_id.to_string(),
            pid: std::process::id(),
            endpoint: self.endpoint.clone(),
            auth_token: self.auth_token.clone(),
            working_dir: new_working_dir,
            repo_root,
            model: new_model,
            started_at_ms: self.started_at_ms,
        };
        if let Ok(path) = write_descriptor(&self.local.live_dir, &desc) {
            if let Ok(mut g) = self.descriptor_path.lock() {
                *g = Some(path);
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.cancel.cancel();
        if let Ok(mut g) = self.server_task.lock() {
            if let Some(task) = g.take() {
                task.abort();
            }
        }
        self.remove_descriptor_file();
    }

    fn remove_descriptor_file(&self) {
        if let Ok(mut g) = self.descriptor_path.lock() {
            if let Some(path) = g.take() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl Drop for SessionMeshHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct IpcRequestEnvelope {
    pub auth_token: String,
    pub payload: MeshRequest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum MeshRequest {
    PingStatus,
    ReadTurns {
        last_n: usize,
        max_chars: usize,
    },
    SendMessage {
        from_session_id: String,
        from_working_dir: PathBuf,
        from_repo_root: PathBuf,
        text: String,
        hop_depth: u32,
        visited_sessions: Vec<String>,
        steer_if_busy: bool,
        wait_for_reply: Option<bool>,
        timeout_secs: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum MeshResponse {
    Status {
        session_id: String,
        pid: u32,
        phase: String,
        busy: bool,
        message_count: usize,
    },
    Turns {
        session_id: String,
        phase: String,
        total_messages: usize,
        rendered: String,
    },
    Delivered {
        session_id: String,
        turn_id: u64,
        steered: bool,
        hop_depth: u32,
        same_repo: bool,
        target_repo_root: PathBuf,
        completed: bool,
        stop_reason: Option<String>,
        reply: Option<String>,
    },
    Error {
        message: String,
    },
}

async fn run_ipc_server(
    listener: TcpListener,
    host: Arc<SessionMeshHost>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else {
                    continue;
                };
                let conn_host = Arc::clone(&host);
                tokio::spawn(async move {
                    handle_ipc_connection(stream, conn_host).await;
                });
            }
        }
    }
}

async fn handle_ipc_connection(stream: TcpStream, host: Arc<SessionMeshHost>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read_res = tokio::time::timeout(IPC_IO_TIMEOUT, reader.read_line(&mut line)).await;
    let Ok(Ok(n)) = read_res else {
        return;
    };
    if n == 0 || line.len() > MAX_IPC_LINE_BYTES {
        return;
    }

    let req: IpcRequestEnvelope = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_ipc_response(
                &mut write_half,
                &MeshResponse::Error {
                    message: format!("invalid mesh request: {e}"),
                },
            )
            .await;
            return;
        }
    };

    if !constant_time_eq(req.auth_token.as_bytes(), host.auth_token.as_bytes()) {
        let _ = write_ipc_response(
            &mut write_half,
            &MeshResponse::Error {
                message: "unauthorized mesh IPC token".to_string(),
            },
        )
        .await;
        return;
    }

    let response = dispatch_ipc_request(&host.local, req.payload).await;
    let _ = write_ipc_response(&mut write_half, &response).await;
}

async fn write_ipc_response(
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    resp: &MeshResponse,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(resp).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    tokio::time::timeout(IPC_IO_TIMEOUT, write_half.write_all(&bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC write timeout"))?
}

async fn dispatch_ipc_request(local: &MeshLocalState, payload: MeshRequest) -> MeshResponse {
    let session_id = local.current_session_id();
    let phase = local.handle.status().phase;
    let phase_label = phase_name(phase).to_string();
    let busy = !matches!(phase, RuntimePhase::Ready);

    match payload {
        MeshRequest::PingStatus => {
            let msg_count = resolve_snapshot_nonblocking(local)
                .map(|s| s.messages.len())
                .unwrap_or(0);
            MeshResponse::Status {
                session_id,
                pid: std::process::id(),
                phase: phase_label,
                busy,
                message_count: msg_count,
            }
        }
        MeshRequest::ReadTurns { last_n, max_chars } => {
            let snapshot = resolve_snapshot_nonblocking(local);
            let Some(snap) = snapshot else {
                return MeshResponse::Turns {
                    session_id,
                    phase: phase_label,
                    total_messages: 0,
                    rendered: "(session has no recorded turns yet)".to_string(),
                };
            };
            let rendered =
                format_snapshot_turns(&snap, last_n.clamp(1, 50), max_chars.clamp(256, 32_000));
            MeshResponse::Turns {
                session_id,
                phase: phase_label,
                total_messages: snap.messages.len(),
                rendered,
            }
        }
        MeshRequest::SendMessage {
            from_session_id,
            from_working_dir,
            from_repo_root,
            text,
            hop_depth,
            visited_sessions,
            steer_if_busy,
            wait_for_reply,
            timeout_secs,
        } => {
            if session_id == from_session_id {
                return MeshResponse::Error {
                    message: "cannot send a peer message to self".to_string(),
                };
            }
            if visited_sessions.iter().any(|s| s == &session_id) {
                return MeshResponse::Error {
                    message: format!(
                        "mesh cycle detected: session `{session_id}` already visited in {:?}",
                        visited_sessions
                    ),
                };
            }
            let max_hops = max_mesh_hops();
            if hop_depth >= max_hops {
                return MeshResponse::Error {
                    message: format!(
                        "mesh hop limit reached (hop_depth={hop_depth} >= max_hops={max_hops})"
                    ),
                };
            }
            let target_repo = local.current_repo_root();
            let same_repo = same_canonical_path(&from_repo_root, &target_repo);

            let steered = match phase {
                RuntimePhase::Ready => false,
                RuntimePhase::InTurn if steer_if_busy => true,
                RuntimePhase::InTurn => {
                    return MeshResponse::Error {
                        message: format!(
                            "target session `{session_id}` is busy mid-turn (phase=InTurn); pass `steer_if_busy: true` to steer or wait"
                        ),
                    };
                }
                other => {
                    return MeshResponse::Error {
                        message: format!(
                            "target session `{session_id}` is not accepting peer messages (phase={})",
                            phase_name(other)
                        ),
                    };
                }
            };

            let next_hop = hop_depth.saturating_add(1);
            let mut next_visited = visited_sessions;
            if !next_visited.iter().any(|s| s == &from_session_id) {
                next_visited.push(from_session_id.clone());
            }
            if let Ok(mut guard) = local.active_hop.write() {
                *guard = Some(MeshHopContext {
                    hop_depth: next_hop,
                    visited_sessions: next_visited,
                });
            }

            let repo_scope_attr = if same_repo {
                "same_repo"
            } else {
                "external_repo"
            };
            let external_repo_note = if same_repo {
                String::new()
            } else {
                format!("[EXTERNAL REPO: {}]\n", from_repo_root.display())
            };
            let trimmed_text = text.trim().to_string();
            let framed = format!(
                "<peer_message from_session=\"{}\" from_cwd=\"{}\" from_repo=\"{}\" repo_scope=\"{}\" hop=\"{}\">\n{}{}\n</peer_message>",
                from_session_id,
                from_working_dir.display(),
                from_repo_root.display(),
                repo_scope_attr,
                next_hop,
                external_repo_note,
                trimmed_text
            );

            let should_wait = wait_for_reply.unwrap_or(!steered);
            let mut outcome_rx = should_wait.then(|| local.turn_outcomes.subscribe());

            let origin = PeerSubmitOrigin {
                from_session_id,
                from_working_dir,
                from_repo_root,
                same_repo,
                hop_depth: next_hop,
                display_text: trimmed_text,
            };
            let turn_id = match local
                .handle
                .submit_peer_message(UserInput::from(framed), origin)
                .await
            {
                Ok(SubmitReceipt::Started { turn_id, .. })
                | Ok(SubmitReceipt::Steered { turn_id, .. }) => turn_id,
                Err(err) => {
                    local.clear_hop_context();
                    return MeshResponse::Error {
                        message: format!("target session `{session_id}` rejected submit: {err}"),
                    };
                }
            };

            if let Some(ref mut rx) = outcome_rx {
                let wait_secs = timeout_secs
                    .unwrap_or(DEFAULT_PEER_REPLY_TIMEOUT_SECS)
                    .clamp(1, 300);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
                loop {
                    let recv = tokio::time::timeout_at(deadline, rx.recv()).await;
                    match recv {
                        Ok(Ok(outcome)) if outcome.turn_id == turn_id => {
                            if let Some(err) = outcome.error {
                                return MeshResponse::Error {
                                    message: format!(
                                        "target session `{session_id}` turn {turn_id} failed: {err}"
                                    ),
                                };
                            }
                            return MeshResponse::Delivered {
                                session_id,
                                turn_id,
                                steered,
                                hop_depth: next_hop,
                                same_repo,
                                target_repo_root: target_repo,
                                completed: true,
                                stop_reason: Some(outcome.stop_reason),
                                reply: Some(outcome.assistant_reply),
                            };
                        }
                        Ok(Ok(_)) => continue,
                        Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                        Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
                    }
                }
            }

            MeshResponse::Delivered {
                session_id,
                turn_id,
                steered,
                hop_depth: next_hop,
                same_repo,
                target_repo_root: target_repo,
                completed: false,
                stop_reason: None,
                reply: None,
            }
        }
    }
}

fn resolve_snapshot_nonblocking(local: &MeshLocalState) -> Option<Arc<SessionSnapshot>> {
    if let Ok(guard) = local.latest_snapshot.read() {
        if let Some(snap) = guard.as_ref() {
            return Some(Arc::clone(snap));
        }
    }
    let sid = local.current_session_id();
    let wd = local.current_working_dir();
    if sid.is_empty() {
        return None;
    }
    let mgr = SessionManager::for_project(&wd);
    mgr.load_native_session(&sid)
        .ok()
        .map(|loaded| Arc::new(loaded.snapshot))
}

pub(crate) fn same_canonical_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> String {
        let s = fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/");
        let stripped = s.strip_prefix("//?/").unwrap_or(&s);
        if cfg!(windows) {
            stripped.to_ascii_lowercase()
        } else {
            stripped.to_string()
        }
    };
    norm(a) == norm(b)
}

fn phase_name(phase: RuntimePhase) -> &'static str {
    match phase {
        RuntimePhase::Ready => "Ready",
        RuntimePhase::InTurn => "InTurn",
        RuntimePhase::WaitingApproval => "WaitingApproval",
        RuntimePhase::Reconfiguring => "Reconfiguring",
        RuntimePhase::AwaitingProvider => "AwaitingProvider",
        RuntimePhase::ShuttingDown => "ShuttingDown",
        RuntimePhase::Stopped => "Stopped",
        RuntimePhase::Failed => "Failed",
    }
}

pub(crate) fn write_descriptor(
    live_dir: &Path,
    desc: &LiveSessionDescriptor,
) -> io::Result<PathBuf> {
    fs::create_dir_all(live_dir)?;
    let target = live_dir.join(format!("{}.json", desc.session_id));
    let tmp = live_dir.join(format!(".{}.{}.tmp", desc.session_id, uuid::Uuid::new_v4()));
    let json = serde_json::to_vec_pretty(desc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &target)?;
    Ok(target)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_live_session_id_rejects_traversal() {
        assert!(validate_live_session_id("abc-123_XYZ").is_ok());
        assert!(validate_live_session_id("../evil").is_err());
        assert!(validate_live_session_id(".hidden").is_err());
        assert!(validate_live_session_id("a/b").is_err());
        assert!(validate_live_session_id("").is_err());
    }
}
