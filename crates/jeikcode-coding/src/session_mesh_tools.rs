//! Cross-process mesh IPC client, stale-PID descriptor reaper, and parent-only peer tools:
//! - `list_live_sessions`: discover PID-checked live JeikCode sessions
//! - `read_session_turns`: inspect recent conversation turns from a peer session
//! - `send_to_session`: send a guarded peer message (cycle/hop-depth, busy-turn, cross-repo trust)

use std::fs;
use std::path::Path;

use async_trait::async_trait;
use jeikcode_capabilities::process_utils::is_pid_alive;
use jeikcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::session_mesh::{
    cross_repo_env_allowed, max_mesh_hops, same_canonical_path, validate_live_session_id,
    IpcRequestEnvelope, LiveSessionDescriptor, LiveSessionView, MeshLocalState, MeshRequest,
    MeshResponse, IPC_CONNECT_TIMEOUT, IPC_IO_TIMEOUT,
};

fn tool_ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: Vec::new(),
    }
}

fn tool_err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: Vec::new(),
    }
}

/// Discover all live, PID-verified peer sessions in `live_dir`, pruning stale descriptors.
pub async fn discover_live_sessions(
    live_dir: &Path,
    self_session_id: Option<&str>,
    self_repo_root: Option<&Path>,
) -> Vec<LiveSessionView> {
    let Ok(entries) = fs::read_dir(live_dir) else {
        return Vec::new();
    };
    let mut descriptors = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(desc) = serde_json::from_str::<LiveSessionDescriptor>(&raw) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if validate_live_session_id(&desc.session_id).is_err() || !is_pid_alive(desc.pid) {
            let _ = fs::remove_file(&path);
            continue;
        }
        descriptors.push((path, desc));
    }

    let mut views = Vec::new();
    for (path, desc) in descriptors {
        match send_ipc_request(&desc, MeshRequest::PingStatus).await {
            Ok(MeshResponse::Status {
                session_id,
                pid,
                phase,
                busy,
                message_count,
            }) if session_id == desc.session_id => {
                let is_self = self_session_id.is_some_and(|s| s == session_id);
                let same_repo =
                    self_repo_root.is_some_and(|r| same_canonical_path(r, &desc.repo_root));
                views.push(LiveSessionView {
                    session_id,
                    pid,
                    working_dir: desc.working_dir,
                    repo_root: desc.repo_root,
                    same_repo,
                    is_self,
                    model: desc.model,
                    phase,
                    busy,
                    message_count,
                    started_at_ms: desc.started_at_ms,
                });
            }
            Ok(_) | Err(_) => {
                if !is_pid_alive(desc.pid) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
    views.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    views
}

/// Load a live session descriptor by ID, checking OS PID liveness.
pub(crate) fn load_live_descriptor(
    live_dir: &Path,
    session_id: &str,
) -> Result<LiveSessionDescriptor, String> {
    let valid_id = validate_live_session_id(session_id)?;
    let path = live_dir.join(format!("{valid_id}.json"));
    let raw = fs::read_to_string(&path)
        .map_err(|_| format!("live session `{valid_id}` not found in mesh registry"))?;
    let desc: LiveSessionDescriptor = serde_json::from_str(&raw)
        .map_err(|e| format!("corrupt descriptor for `{valid_id}`: {e}"))?;
    if !is_pid_alive(desc.pid) {
        let _ = fs::remove_file(&path);
        return Err(format!(
            "live session `{valid_id}` (pid {}) is no longer alive (reaped stale entry)",
            desc.pid
        ));
    }
    Ok(desc)
}

async fn ipc_read_session_turns(
    desc: &LiveSessionDescriptor,
    last_n: usize,
    max_chars: usize,
) -> Result<(String, usize, String), String> {
    match send_ipc_request(desc, MeshRequest::ReadTurns { last_n, max_chars }).await? {
        MeshResponse::Turns {
            phase,
            total_messages,
            rendered,
            ..
        } => Ok((phase, total_messages, rendered)),
        MeshResponse::Error { message } => Err(message),
        _ => Err("unexpected IPC response for read_session_turns".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn ipc_send_to_session(
    desc: &LiveSessionDescriptor,
    from_session_id: &str,
    from_working_dir: &Path,
    from_repo_root: &Path,
    text: String,
    hop_depth: u32,
    visited_sessions: Vec<String>,
    allow_cross_repo: bool,
    steer_if_busy: bool,
) -> Result<(bool, u32), String> {
    match send_ipc_request(
        desc,
        MeshRequest::SendMessage {
            from_session_id: from_session_id.to_string(),
            from_working_dir: from_working_dir.to_path_buf(),
            from_repo_root: from_repo_root.to_path_buf(),
            text,
            hop_depth,
            visited_sessions,
            allow_cross_repo,
            steer_if_busy,
        },
    )
    .await?
    {
        MeshResponse::Delivered {
            steered, hop_depth, ..
        } => Ok((steered, hop_depth)),
        MeshResponse::Error { message } => Err(message),
        _ => Err("unexpected IPC response for send_to_session".to_string()),
    }
}

async fn send_ipc_request(
    desc: &LiveSessionDescriptor,
    payload: MeshRequest,
) -> Result<MeshResponse, String> {
    let stream = tokio::time::timeout(IPC_CONNECT_TIMEOUT, TcpStream::connect(&desc.endpoint))
        .await
        .map_err(|_| format!("timed out connecting to session `{}`", desc.session_id))?
        .map_err(|e| format!("failed to connect to session `{}`: {e}", desc.session_id))?;

    let (read_half, mut write_half) = stream.into_split();
    let env = IpcRequestEnvelope {
        auth_token: desc.auth_token.clone(),
        payload,
    };
    let mut bytes = serde_json::to_vec(&env).map_err(|e| e.to_string())?;
    bytes.push(b'\n');

    tokio::time::timeout(IPC_IO_TIMEOUT, write_half.write_all(&bytes))
        .await
        .map_err(|_| "IPC write timeout".to_string())?
        .map_err(|e| format!("IPC write failed: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    let n = tokio::time::timeout(IPC_IO_TIMEOUT, reader.read_line(&mut resp_line))
        .await
        .map_err(|_| "IPC read timeout".to_string())?
        .map_err(|e| format!("IPC read failed: {e}"))?;
    if n == 0 {
        return Err("empty IPC response from peer session".to_string());
    }
    serde_json::from_str::<MeshResponse>(&resp_line)
        .map_err(|e| format!("invalid IPC response: {e}"))
}

/// `list_live_sessions` — list live JeikCode sessions across local OS processes.
pub(crate) struct ListLiveSessionsTool {
    pub local: MeshLocalState,
}

#[async_trait]
impl Tool for ListLiveSessionsTool {
    fn name(&self) -> &str {
        "list_live_sessions"
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Discover live JeikCode sessions running across local OS processes (standalone TUIs, CLI, daemon) with PID verification, repo match, working directory, and busy status."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        let self_id = self.local.current_session_id();
        let self_repo = self.local.current_repo_root();
        let sessions = discover_live_sessions(
            &self.local.live_dir,
            (!self_id.is_empty()).then_some(self_id.as_str()),
            Some(self_repo.as_path()),
        )
        .await;

        if sessions.is_empty() {
            return tool_ok("No live peer sessions discovered.");
        }
        let rendered = serde_json::to_string_pretty(&sessions)
            .unwrap_or_else(|e| format!("failed to serialize sessions: {e}"));
        tool_ok(rendered)
    }
}

#[derive(Deserialize)]
struct ReadSessionTurnsArgs {
    session_id: String,
    #[serde(default)]
    last_n: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    allow_cross_repo: Option<bool>,
}

/// `read_session_turns` — inspect recent turns of a live peer session without blocking mid-turn.
pub(crate) struct ReadSessionTurnsTool {
    pub local: MeshLocalState,
}

#[async_trait]
impl Tool for ReadSessionTurnsTool {
    fn name(&self) -> &str {
        "read_session_turns"
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Read recent user/assistant turns from another live JeikCode session. Same-repo sessions are allowed automatically; cross-repo reads require `allow_cross_repo: true`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Target live session ID from `list_live_sessions`."
                },
                "last_n": {
                    "type": "integer",
                    "description": "Number of recent real messages to return (default 8, max 50)."
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Character cap on returned transcript snippet (default 6000, max 32000)."
                },
                "allow_cross_repo": {
                    "type": "boolean",
                    "description": "Set true to read a session in a different repository."
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let parsed: ReadSessionTurnsArgs = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return tool_err(format!("invalid read_session_turns arguments: {e}")),
        };
        let desc = match load_live_descriptor(&self.local.live_dir, &parsed.session_id) {
            Ok(d) => d,
            Err(e) => return tool_err(e),
        };
        let self_repo = self.local.current_repo_root();
        let same_repo = same_canonical_path(&self_repo, &desc.repo_root);
        let allow_cross = parsed.allow_cross_repo.unwrap_or(false) || cross_repo_env_allowed();
        if !same_repo && !allow_cross {
            return tool_err(format!(
                "cross-repo read_session_turns denied (`{}` vs `{}`); pass `allow_cross_repo: true`",
                self_repo.display(),
                desc.repo_root.display()
            ));
        }

        let last_n = parsed.last_n.unwrap_or(8).clamp(1, 50);
        let max_chars = parsed.max_chars.unwrap_or(6000).clamp(256, 32_000);
        match ipc_read_session_turns(&desc, last_n, max_chars).await {
            Ok((phase, total_messages, rendered)) => tool_ok(format!(
                "Session `{}` (phase={phase}, total_messages={total_messages}):\n\n{rendered}",
                desc.session_id
            )),
            Err(e) => tool_err(e),
        }
    }
}

#[derive(Deserialize)]
struct SendToSessionArgs {
    session_id: String,
    message: String,
    #[serde(default)]
    steer_if_busy: Option<bool>,
    #[serde(default)]
    allow_cross_repo: Option<bool>,
}

/// `send_to_session` — deliver a peer message to another live JeikCode session with guardrails.
pub(crate) struct SendToSessionTool {
    pub local: MeshLocalState,
}

#[async_trait]
impl Tool for SendToSessionTool {
    fn name(&self) -> &str {
        "send_to_session"
    }

    fn description(&self) -> &str {
        "Send a peer message to another live JeikCode session. Enforces self/cycle prevention, max hop-depth, busy-turn admission, and cross-repo trust checks."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Target live session ID from `list_live_sessions`."
                },
                "message": {
                    "type": "string",
                    "description": "Instruction or question to send to the peer session."
                },
                "steer_if_busy": {
                    "type": "boolean",
                    "description": "When true, steer the message into an active InTurn session instead of rejecting busy targets (default false)."
                },
                "allow_cross_repo": {
                    "type": "boolean",
                    "description": "Required (and approval-gated) when targeting a session in a different repository."
                }
            },
            "required": ["session_id", "message"]
        })
    }

    fn risk(&self, args: &str) -> RiskLevel {
        if cross_repo_env_allowed() {
            return RiskLevel::Safe;
        }
        let Ok(parsed) = serde_json::from_str::<SendToSessionArgs>(args) else {
            return RiskLevel::Safe;
        };
        let Ok(desc) = load_live_descriptor(&self.local.live_dir, &parsed.session_id) else {
            return RiskLevel::Safe;
        };
        let self_repo = self.local.current_repo_root();
        if same_canonical_path(&self_repo, &desc.repo_root) {
            RiskLevel::Safe
        } else if parsed.allow_cross_repo.unwrap_or(false) {
            // Explicit cross-repo peer writes require user approval via ApprovalMiddleware.
            RiskLevel::Risky
        } else {
            // Missing `allow_cross_repo: true`: `execute()` immediately fails closed with an
            // actionable error instead of prompting the user for an unflagged call.
            RiskLevel::Safe
        }
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let parsed: SendToSessionArgs = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return tool_err(format!("invalid send_to_session arguments: {e}")),
        };
        if parsed.message.trim().is_empty() {
            return tool_err("`message` cannot be empty");
        }

        let self_id = self.local.current_session_id();
        if self_id.is_empty() {
            return tool_err("current runtime has no active session_id for mesh messaging");
        }
        let target_id = parsed.session_id.trim();
        if target_id == self_id {
            return tool_err("cannot send a peer message to self");
        }

        let hop = self.local.current_hop_context();
        if hop.visited_sessions.iter().any(|s| s == target_id) {
            return tool_err(format!(
                "mesh cycle detected: `{target_id}` is already in visited chain {:?}",
                hop.visited_sessions
            ));
        }
        let max_hops = max_mesh_hops();
        if hop.hop_depth >= max_hops {
            return tool_err(format!(
                "mesh hop-depth limit reached (hop_depth={} >= max_hops={max_hops})",
                hop.hop_depth
            ));
        }

        let desc = match load_live_descriptor(&self.local.live_dir, target_id) {
            Ok(d) => d,
            Err(e) => return tool_err(e),
        };

        let self_wd = self.local.current_working_dir();
        let self_repo = self.local.current_repo_root();
        let same_repo = same_canonical_path(&self_repo, &desc.repo_root);
        let allow_cross = parsed.allow_cross_repo.unwrap_or(false) || cross_repo_env_allowed();
        if !same_repo && !allow_cross {
            return tool_err(format!(
                "cross-repo send_to_session denied (`{}` -> `{}`); pass `allow_cross_repo: true`",
                self_repo.display(),
                desc.repo_root.display()
            ));
        }

        match ipc_send_to_session(
            &desc,
            &self_id,
            &self_wd,
            &self_repo,
            parsed.message,
            hop.hop_depth,
            hop.visited_sessions,
            allow_cross,
            parsed.steer_if_busy.unwrap_or(false),
        )
        .await
        {
            Ok((steered, delivered_hop)) => tool_ok(format!(
                "Delivered peer message to session `{}` (steered={steered}, hop_depth={delivered_hop}).",
                desc.session_id
            )),
            Err(e) => tool_err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_mesh::write_descriptor;

    #[tokio::test]
    async fn discover_live_sessions_reaps_dead_pid_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let dead_desc = LiveSessionDescriptor {
            session_id: "dead-session-1".to_string(),
            pid: 0,
            endpoint: "127.0.0.1:9".to_string(),
            auth_token: "tok".to_string(),
            working_dir: dir.path().to_path_buf(),
            repo_root: dir.path().to_path_buf(),
            model: "test".to_string(),
            started_at_ms: 1,
        };
        let path = write_descriptor(dir.path(), &dead_desc).unwrap();
        assert!(path.exists());

        let found = discover_live_sessions(dir.path(), None, None).await;
        assert!(found.is_empty());
        assert!(!path.exists(), "dead-PID descriptor should be reaped");
    }
}
