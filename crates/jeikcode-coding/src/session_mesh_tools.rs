//! Cross-process mesh IPC client and parent-only peer tools:
//! - `list_live_sessions`: discover PID-checked live JeikCode sessions
//! - `read_session_turns`: inspect recent conversation turns from a peer session (by name or ID)
//! - `send_to_session`: send a guarded peer message (by name or ID) and return the peer's reply/outcome
//! - `set_session_name`: set or update this session's own live mesh nickname/role

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use jeikcode_capabilities::process_utils::is_pid_alive;
use jeikcode_kernel::message::{Role, SessionSnapshot};
use jeikcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::session_mesh::{
    IpcRequestEnvelope, LiveSessionDescriptor, LiveSessionView, MeshLocalState, MeshRequest,
    MeshResponse, DEFAULT_PEER_REPLY_TIMEOUT_SECS, IPC_CONNECT_TIMEOUT, IPC_IO_TIMEOUT,
    MAX_IPC_FRAME_BYTES,
};

pub(crate) const DEFAULT_MESH_REPLY_CHARS: usize = 32_000;
pub(crate) const MAX_MESH_REPLY_CHARS: usize = 64_000;
pub(crate) const DEFAULT_READ_TURNS_MAX_CHARS: usize = 16_000;
pub(crate) const MAX_READ_TURNS_MAX_CHARS: usize = 64_000;
const PER_TURN_MAX_CHARS: usize = 8_000;
use crate::session_mesh_naming::{
    default_display_name, display_clean_path, max_mesh_hops_from_mesh,
    read_live_descriptors_reaping_stale, resolve_live_target, same_canonical_path,
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

pub(crate) fn last_assistant_reply_text(snapshot: &SessionSnapshot) -> String {
    let text = snapshot
        .messages
        .iter()
        .rev()
        .find(|m| !m.synthetic && m.role == Role::Assistant && !m.text.trim().is_empty())
        .map(|m| m.text.trim().to_string())
        .unwrap_or_default();
    truncate_chars_with_footer(&text, MAX_MESH_REPLY_CHARS)
}

pub(crate) fn format_snapshot_turns(
    snap: &SessionSnapshot,
    last_n: usize,
    max_chars: usize,
) -> String {
    let real_msgs: Vec<_> = snap
        .messages
        .iter()
        .filter(|m| !m.synthetic && matches!(m.role, Role::User | Role::Assistant))
        .collect();
    if real_msgs.is_empty() {
        return "(no user/assistant turns recorded yet)".to_string();
    }
    let start = real_msgs.len().saturating_sub(last_n);
    let mut lines = Vec::new();
    for (idx, msg) in real_msgs[start..].iter().enumerate() {
        let role_label = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            _ => "other",
        };
        let mut body = msg.text.trim().to_string();
        if body.is_empty() && !msg.tool_calls.is_empty() {
            let tool_names: Vec<&str> = msg.tool_calls.iter().map(|t| t.name.as_str()).collect();
            body = format!("[tool calls: {}]", tool_names.join(", "));
        }
        let clipped = truncate_chars_with_footer(&body, PER_TURN_MAX_CHARS);
        lines.push(format!("[{}] {}: {}", start + idx + 1, role_label, clipped));
    }
    truncate_chars_with_footer(&lines.join("\n\n"), max_chars)
}

pub(crate) fn truncate_chars_with_footer(s: &str, max_chars: usize) -> String {
    let mut count = 0usize;
    let mut end_byte = s.len();
    for (idx, _) in s.char_indices() {
        if count == max_chars {
            end_byte = idx;
        }
        count += 1;
    }
    if count <= max_chars {
        return s.to_string();
    }
    let dropped = count - max_chars;
    format!("{}\n[... truncated {dropped} chars ...]", &s[..end_byte])
}

/// Discover all live, PID-verified peer sessions in `live_dir`, pruning stale descriptors.
pub async fn discover_live_sessions(
    live_dir: &Path,
    self_session_id: Option<&str>,
    self_repo_root: Option<&Path>,
) -> Vec<LiveSessionView> {
    let descriptors = read_live_descriptors_reaping_stale(live_dir);
    let mut views = Vec::new();
    for (path, desc) in descriptors {
        match send_ipc_request(&desc, MeshRequest::PingStatus, IPC_IO_TIMEOUT).await {
            Ok(MeshResponse::Status {
                session_id,
                name,
                pid,
                phase,
                busy,
                message_count,
            }) if session_id == desc.session_id => {
                let effective_name = name.or(desc.name);
                let display_name = effective_name.clone().unwrap_or_else(|| {
                    default_display_name(&desc.working_dir, &desc.repo_root, &session_id)
                });
                let is_self = self_session_id.is_some_and(|s| s == session_id);
                let same_repo =
                    self_repo_root.is_some_and(|r| same_canonical_path(r, &desc.repo_root));
                let clean_wd = display_clean_path(&desc.working_dir);
                let clean_repo = display_clean_path(&desc.repo_root);
                let repo_scope = if same_repo {
                    "same_repo".to_string()
                } else {
                    format!("EXTERNAL REPO: {clean_repo}")
                };
                views.push(LiveSessionView {
                    session_id,
                    name: effective_name,
                    display_name,
                    pid,
                    working_dir: clean_wd,
                    repo_root: clean_repo,
                    same_repo,
                    repo_scope,
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
    views.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    views
}

async fn ipc_read_session_turns(
    desc: &LiveSessionDescriptor,
    last_n: usize,
    max_chars: usize,
) -> Result<(Option<String>, String, usize, String), String> {
    match send_ipc_request(
        desc,
        MeshRequest::ReadTurns { last_n, max_chars },
        IPC_IO_TIMEOUT,
    )
    .await?
    {
        MeshResponse::Turns {
            name,
            phase,
            total_messages,
            rendered,
            ..
        } => Ok((name, phase, total_messages, rendered)),
        MeshResponse::Error { message } => Err(message),
        _ => Err("unexpected IPC response for read_session_turns".to_string()),
    }
}

struct PeerDeliveryOutcome {
    name: Option<String>,
    turn_id: u64,
    steered: bool,
    hop_depth: u32,
    same_repo: bool,
    target_repo_root: PathBuf,
    completed: bool,
    stop_reason: Option<String>,
    reply: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn ipc_send_to_session(
    desc: &LiveSessionDescriptor,
    from_session_id: &str,
    from_session_name: Option<String>,
    from_working_dir: &Path,
    from_repo_root: &Path,
    text: String,
    hop_depth: u32,
    visited_sessions: Vec<String>,
    steer_if_busy: bool,
    wait_for_reply: Option<bool>,
    timeout_secs: Option<u64>,
    max_reply_chars: Option<usize>,
) -> Result<PeerDeliveryOutcome, String> {
    let wait_budget = if wait_for_reply == Some(false) {
        IPC_IO_TIMEOUT
    } else {
        let secs = timeout_secs
            .unwrap_or(DEFAULT_PEER_REPLY_TIMEOUT_SECS)
            .clamp(1, 300);
        Duration::from_secs(secs.saturating_add(5))
    };
    match send_ipc_request(
        desc,
        MeshRequest::SendMessage {
            from_session_id: from_session_id.to_string(),
            from_session_name,
            from_working_dir: from_working_dir.to_path_buf(),
            from_repo_root: from_repo_root.to_path_buf(),
            text,
            hop_depth,
            visited_sessions,
            steer_if_busy,
            wait_for_reply,
            timeout_secs,
            max_reply_chars,
        },
        wait_budget,
    )
    .await?
    {
        MeshResponse::Delivered {
            name,
            turn_id,
            steered,
            hop_depth,
            same_repo,
            target_repo_root,
            completed,
            stop_reason,
            reply,
            ..
        } => Ok(PeerDeliveryOutcome {
            name,
            turn_id,
            steered,
            hop_depth,
            same_repo,
            target_repo_root,
            completed,
            stop_reason,
            reply,
        }),
        MeshResponse::Error { message } => Err(message),
        _ => Err("unexpected IPC response for send_to_session".to_string()),
    }
}

async fn send_ipc_request(
    desc: &LiveSessionDescriptor,
    payload: MeshRequest,
    read_timeout: Duration,
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
    if bytes.len() > MAX_IPC_FRAME_BYTES {
        return Err(format!(
            "mesh IPC request exceeds {} bytes ({} bytes)",
            MAX_IPC_FRAME_BYTES,
            bytes.len()
        ));
    }

    tokio::time::timeout(IPC_IO_TIMEOUT, write_half.write_all(&bytes))
        .await
        .map_err(|_| "IPC write timeout".to_string())?
        .map_err(|e| format!("IPC write failed: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    let n = tokio::time::timeout(read_timeout, reader.read_line(&mut resp_line))
        .await
        .map_err(|_| "IPC read timeout".to_string())?
        .map_err(|e| format!("IPC read failed: {e}"))?;
    if n == 0 {
        return Err("empty IPC response from peer session".to_string());
    }
    if resp_line.len() > MAX_IPC_FRAME_BYTES {
        return Err(format!(
            "mesh IPC response exceeds {} bytes ({} bytes)",
            MAX_IPC_FRAME_BYTES,
            resp_line.len()
        ));
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
        "Discover PID-verified live JeikCode sessions with nicknames, repo scope (`same_repo` or `EXTERNAL REPO`), clean paths, and busy status."
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
    #[serde(alias = "session_id")]
    target: String,
    #[serde(default)]
    last_n: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
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

    fn never_truncate_result(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Read recent user/assistant turns from a live peer session."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Peer `name` or `session_id`."
                },
                "last_n": {
                    "type": "integer",
                    "description": "Recent real messages to return (default 8, max 50)."
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Max transcript chars (default 16000, max 64000)."
                }
            },
            "required": ["target"]
        })
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let parsed: ReadSessionTurnsArgs = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return tool_err(format!("invalid read_session_turns arguments: {e}")),
        };
        let desc = match resolve_live_target(&self.local.live_dir, &parsed.target) {
            Ok(d) => d,
            Err(e) => return tool_err(e),
        };
        let self_repo = self.local.current_repo_root();
        let same_repo = same_canonical_path(&self_repo, &desc.repo_root);
        let clean_wd = display_clean_path(&desc.working_dir);
        let clean_repo = display_clean_path(&desc.repo_root);
        let scope_tag = if same_repo {
            "SAME REPO".to_string()
        } else {
            format!("EXTERNAL REPO: {clean_repo}")
        };

        let last_n = parsed.last_n.unwrap_or(8).clamp(1, 50);
        let max_chars = parsed
            .max_chars
            .unwrap_or(DEFAULT_READ_TURNS_MAX_CHARS)
            .clamp(256, MAX_READ_TURNS_MAX_CHARS);
        match ipc_read_session_turns(&desc, last_n, max_chars).await {
            Ok((live_name, phase, total_messages, rendered)) => {
                let display = live_name.or(desc.name).unwrap_or_else(|| {
                    default_display_name(&desc.working_dir, &desc.repo_root, &desc.session_id)
                });
                tool_ok(format!(
                    "Session `{display}` (id={}, [{scope_tag}], cwd={clean_wd}, phase={phase}, total_messages={total_messages}):\n\n{rendered}",
                    desc.session_id,
                ))
            }
            Err(e) => tool_err(e),
        }
    }
}

#[derive(Deserialize)]
struct SendToSessionArgs {
    #[serde(alias = "session_id")]
    target: String,
    message: String,
    #[serde(default)]
    steer_if_busy: Option<bool>,
    #[serde(default)]
    wait_for_reply: Option<bool>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_reply_chars: Option<usize>,
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

    fn never_truncate_result(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Send a peer message to another live JeikCode session and wait for its reply by default."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Peer `name` or `session_id`."
                },
                "message": {
                    "type": "string",
                    "description": "Instruction or question for the peer session."
                },
                "steer_if_busy": {
                    "type": "boolean",
                    "description": "Steer into an active InTurn peer instead of rejecting (default false)."
                },
                "wait_for_reply": {
                    "type": "boolean",
                    "description": "Await reply (default true when Ready, false if steered)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Wait timeout in seconds (default 120, max 300)."
                },
                "max_reply_chars": {
                    "type": "integer",
                    "description": "Max reply chars to return (default 32000, max 64000)."
                }
            },
            "required": ["target", "message"]
        })
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
        let desc = match resolve_live_target(&self.local.live_dir, &parsed.target) {
            Ok(d) => d,
            Err(e) => return tool_err(e),
        };
        if desc.session_id == self_id {
            return tool_err("cannot send a peer message to self");
        }

        let hop = self.local.current_hop_context();
        if hop.visited_sessions.iter().any(|s| s == &desc.session_id) {
            return tool_err(format!(
                "mesh cycle detected: `{}` is already in visited chain {:?}",
                desc.session_id, hop.visited_sessions
            ));
        }
        let max_hops = max_mesh_hops_from_mesh();
        if hop.hop_depth >= max_hops {
            return tool_err(format!(
                "mesh hop-depth limit reached (hop_depth={} >= max_hops={max_hops})",
                hop.hop_depth
            ));
        }

        let self_wd = self.local.current_working_dir();
        let self_repo = self.local.current_repo_root();
        let self_name = self.local.current_name();

        let max_reply_chars = parsed
            .max_reply_chars
            .map(|n| n.clamp(1_024, MAX_MESH_REPLY_CHARS));

        match ipc_send_to_session(
            &desc,
            &self_id,
            self_name,
            &self_wd,
            &self_repo,
            parsed.message,
            hop.hop_depth,
            hop.visited_sessions,
            parsed.steer_if_busy.unwrap_or(false),
            parsed.wait_for_reply,
            parsed.timeout_secs,
            max_reply_chars,
        )
        .await
        {
            Ok(outcome) => {
                let display = outcome.name.or(desc.name).unwrap_or_else(|| {
                    default_display_name(&desc.working_dir, &desc.repo_root, &desc.session_id)
                });
                let scope_tag = if outcome.same_repo {
                    "SAME REPO".to_string()
                } else {
                    format!(
                        "EXTERNAL REPO: {}",
                        display_clean_path(&outcome.target_repo_root)
                    )
                };
                if outcome.completed {
                    let reason = outcome.stop_reason.as_deref().unwrap_or("Stopped");
                    let reply = outcome
                        .reply
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("(peer completed turn with no text output)");
                    tool_ok(format!(
                        "Peer session `{display}` (id={}, [{scope_tag}], turn_id={}, steered={}, hop_depth={}, stop_reason={reason}) replied:\n\n{reply}",
                        desc.session_id, outcome.turn_id, outcome.steered, outcome.hop_depth
                    ))
                } else {
                    tool_ok(format!(
                        "Delivered peer message to session `{display}` (id={}, [{scope_tag}], turn_id={}, steered={}, hop_depth={}); turn still running (use `read_session_turns` to inspect its output).",
                        desc.session_id, outcome.turn_id, outcome.steered, outcome.hop_depth
                    ))
                }
            }
            Err(e) => tool_err(e),
        }
    }
}

#[derive(Deserialize)]
struct SetSessionNameArgs {
    name: String,
}

/// `set_session_name` — set this session's own live mesh nickname/role (self-owned only).
pub(crate) struct SetSessionNameTool {
    pub local: MeshLocalState,
}

#[async_trait]
impl Tool for SetSessionNameTool {
    fn name(&self) -> &str {
        "set_session_name"
    }

    fn description(&self) -> &str {
        "Set this session's unified name across `/sessions`, UI, and live mesh (self-owned, live-unique)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Nickname (1..=48 chars, no control chars)."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let parsed: SetSessionNameArgs = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return tool_err(format!("invalid set_session_name arguments: {e}")),
        };
        match self.local.handle.set_mesh_name(Some(parsed.name)).await {
            Ok(Some(assigned)) => tool_ok(format!(
                "Session name set to `{assigned}` (persisted in /sessions and live mesh; session_id={}).",
                self.local.current_session_id()
            )),
            Ok(None) => tool_ok(format!(
                "Session mesh name cleared (session_id={}).",
                self.local.current_session_id()
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
            name: Some("stale".to_string()),
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

    #[test]
    fn truncate_chars_with_footer_preserves_under_cap_and_marks_exact_cutoff() {
        assert_eq!(truncate_chars_with_footer("hello", 10), "hello");
        assert_eq!(truncate_chars_with_footer("hello", 5), "hello");
        assert_eq!(
            truncate_chars_with_footer("abcdefghij", 4),
            "abcd\n[... truncated 6 chars ...]"
        );
    }
}
