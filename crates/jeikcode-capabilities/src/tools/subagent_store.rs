//! Session-scoped subagent state store (`resume_agent_id`) and async background
//! dispatch registry (`run_in_background` + `TaskOutputTool`).

use async_trait::async_trait;
use jeikcode_kernel::message::SessionSnapshot;
use jeikcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::subagent_verifier::SubagentRole;

const MAX_SAVED_SUBAGENTS: usize = 64;
const MAX_BACKGROUND_TASKS: usize = 64;
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 30;
const MAX_WAIT_TIMEOUT_SECS: u64 = 600;

static NEXT_SCOPE_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct SavedSubagentState {
    pub agent_id: String,
    pub session_scope: String,
    pub role: SubagentRole,
    pub scope: Vec<String>,
    #[serde(default)]
    pub difficulty: Option<String>,
    #[serde(default)]
    pub inherit_context: bool,
    #[allow(dead_code)]
    pub working_dir: PathBuf,
    pub model: String,
    pub snapshot: SessionSnapshot,
    pub updated_seq: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct BackgroundCompletion {
    pub rendered_block: String,
    pub is_error: bool,
    pub status_label: &'static str,
}

pub(crate) struct BackgroundTaskHandle {
    pub task_id: String,
    pub agent_id: String,
    pub session_scope: String,
    #[allow(dead_code)]
    pub label: String,
    pub description: String,
    pub role: SubagentRole,
    #[allow(dead_code)]
    pub model: String,
    pub started_at: Instant,
    pub cancel: CancellationToken,
    pub live_activity: Mutex<Option<String>>,
    tx: watch::Sender<Option<BackgroundCompletion>>,
    rx: watch::Receiver<Option<BackgroundCompletion>>,
}

impl BackgroundTaskHandle {
    pub(crate) fn set_activity(&self, activity: impl Into<String>) {
        if let Ok(mut g) = self.live_activity.lock() {
            *g = Some(activity.into());
        }
    }

    pub(crate) fn finish(&self, completion: BackgroundCompletion) {
        let _ = self.tx.send(Some(completion));
    }

    pub(crate) fn snapshot_completion(&self) -> Option<BackgroundCompletion> {
        self.rx.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<BackgroundCompletion>> {
        self.rx.clone()
    }
}

/// Shared per-session store for resumable child `Agent` snapshots and async
/// background subagent handles.
pub(crate) struct SubagentStore {
    session_scope: RwLock<String>,
    next_seq: AtomicU64,
    root_cancel: CancellationToken,
    saved: Mutex<HashMap<String, SavedSubagentState>>,
    in_flight: Mutex<HashSet<String>>,
    background: Mutex<HashMap<String, Arc<BackgroundTaskHandle>>>,
}

pub(crate) struct ResumeInFlightGuard {
    store: std::sync::Weak<SubagentStore>,
    agent_id: String,
}

impl Drop for ResumeInFlightGuard {
    fn drop(&mut self) {
        if let Some(store) = self.store.upgrade() {
            store.release_in_flight(&self.agent_id);
        }
    }
}

impl SubagentStore {
    pub(crate) fn new() -> Self {
        let scope = format!(
            "subagent-scope-{}",
            NEXT_SCOPE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            session_scope: RwLock::new(scope),
            next_seq: AtomicU64::new(1),
            root_cancel: CancellationToken::new(),
            saved: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            background: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn release_in_flight(&self, agent_id: &str) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(agent_id.trim());
        }
    }

    pub(crate) fn inherit_saved_from(&self, other: &SubagentStore) {
        let Ok(other_map) = other.saved.lock() else {
            return;
        };
        let Ok(mut my_map) = self.saved.lock() else {
            return;
        };
        for (k, v) in other_map.iter() {
            my_map.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    pub(crate) fn bind_session(&self, session_id: &str) {
        let trimmed = session_id.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(mut g) = self.session_scope.write() {
            *g = trimmed.to_string();
        }
    }

    pub(crate) fn current_session_scope(&self) -> String {
        self.session_scope
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    pub(crate) fn child_cancel_token(&self) -> CancellationToken {
        self.root_cancel.child_token()
    }

    pub(crate) fn mint_agent_id(&self, role: SubagentRole) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        format!("agent-{}-{seq}", role.as_str())
    }

    pub(crate) fn mint_task_id(&self) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        format!("bg-{seq}")
    }

    pub(crate) fn save_state(&self, mut state: SavedSubagentState) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        state.updated_seq = seq;
        let Ok(mut map) = self.saved.lock() else {
            return;
        };
        map.insert(state.agent_id.clone(), state);
        if map.len() > MAX_SAVED_SUBAGENTS {
            if let Some(oldest_key) = map
                .iter()
                .min_by_key(|(_, v)| v.updated_seq)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest_key);
            }
        }
    }

    pub(crate) fn peek_role(&self, agent_id: &str) -> Option<SubagentRole> {
        let scope = self.current_session_scope();
        let map = self.saved.lock().ok()?;
        let entry = map.get(agent_id.trim())?;
        (entry.session_scope == scope).then_some(entry.role)
    }

    pub(crate) fn get_for_resume(
        self: &Arc<Self>,
        agent_id: &str,
        requested_role: Option<SubagentRole>,
        requested_scope: &[String],
    ) -> Result<(SavedSubagentState, ResumeInFlightGuard), String> {
        let id = agent_id.trim();
        if id.is_empty() {
            return Err("`resume_agent_id` must be non-empty".to_string());
        }
        let session_scope = self.current_session_scope();
        let map = self
            .saved
            .lock()
            .map_err(|_| "subagent state store lock poisoned".to_string())?;
        let Some(entry) = map.get(id) else {
            return Err(format!(
                "unknown or expired `resume_agent_id` `{id}`; omit `resume_agent_id` to start a fresh subagent"
            ));
        };
        if entry.session_scope != session_scope {
            return Err(format!(
                "`resume_agent_id` `{id}` belongs to a different session and cannot be resumed here"
            ));
        }
        if let Some(req_role) = requested_role {
            if req_role != entry.role {
                return Err(format!(
                    "cannot resume `{id}` as `{}`: saved subagent role is `{}`",
                    req_role.as_str(),
                    entry.role.as_str()
                ));
            }
        }
        let req_scope: Vec<String> = requested_scope
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !req_scope.is_empty() && req_scope != entry.scope {
            return Err(format!(
                "cannot change `scope` when resuming `{id}` (saved scope: [{}])",
                entry.scope.join(", ")
            ));
        }
        let mut inflight = self
            .in_flight
            .lock()
            .map_err(|_| "subagent in-flight lock poisoned".to_string())?;
        if !inflight.insert(entry.agent_id.clone()) {
            return Err(format!(
                "subagent `{}` is already running; wait for it to finish before resuming it again",
                entry.agent_id
            ));
        }
        let guard = ResumeInFlightGuard {
            store: Arc::downgrade(self),
            agent_id: entry.agent_id.clone(),
        };
        Ok((entry.clone(), guard))
    }

    pub(crate) fn register_background(
        &self,
        task_id: String,
        agent_id: String,
        label: String,
        description: String,
        role: SubagentRole,
        model: String,
        cancel: CancellationToken,
    ) -> Arc<BackgroundTaskHandle> {
        let (tx, rx) = watch::channel(None);
        let session_scope = self.current_session_scope();
        let handle = Arc::new(BackgroundTaskHandle {
            task_id: task_id.clone(),
            agent_id,
            session_scope,
            label,
            description,
            role,
            model,
            started_at: Instant::now(),
            cancel,
            live_activity: Mutex::new(None),
            tx,
            rx,
        });
        if let Ok(mut map) = self.background.lock() {
            map.insert(task_id, Arc::clone(&handle));
            if map.len() > MAX_BACKGROUND_TASKS {
                if let Some(evict) = map
                    .iter()
                    .filter(|(_, h)| h.snapshot_completion().is_some())
                    .min_by_key(|(_, h)| h.started_at)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&evict);
                }
            }
        }
        handle
    }

    pub(crate) fn lookup_background(&self, id: &str) -> Option<Arc<BackgroundTaskHandle>> {
        let needle = id.trim();
        if needle.is_empty() {
            return None;
        }
        let scope = self.current_session_scope();
        let map = self.background.lock().ok()?;
        if let Some(direct) = map.get(needle) {
            if direct.session_scope == scope {
                return Some(Arc::clone(direct));
            }
            return None;
        }
        map.values()
            .find(|h| h.session_scope == scope && h.agent_id == needle)
            .map(Arc::clone)
    }

    pub(crate) fn list_summary(&self) -> (Vec<String>, Vec<String>) {
        let scope = self.current_session_scope();
        let mut bg_lines = Vec::new();
        if let Ok(map) = self.background.lock() {
            let mut entries: Vec<_> = map.values().filter(|h| h.session_scope == scope).collect();
            entries.sort_by_key(|h| h.started_at);
            for h in entries {
                let status = match h.snapshot_completion() {
                    Some(c) => c.status_label,
                    None if h.cancel.is_cancelled() => "cancelling",
                    None => "running",
                };
                let act = h
                    .live_activity
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_default();
                let act_suffix = if act.is_empty() || status != "running" {
                    String::new()
                } else {
                    format!(" ({act})")
                };
                bg_lines.push(format!(
                    "- task_id={} agent_id={} role={} status={} elapsed={}s desc={:?}{}",
                    h.task_id,
                    h.agent_id,
                    h.role.as_str(),
                    status,
                    h.started_at.elapsed().as_secs(),
                    h.description,
                    act_suffix
                ));
            }
        }

        let scope = self.current_session_scope();
        let mut saved_lines = Vec::new();
        if let Ok(map) = self.saved.lock() {
            let mut items: Vec<_> = map.values().filter(|s| s.session_scope == scope).collect();
            items.sort_by_key(|s| s.updated_seq);
            for s in items {
                saved_lines.push(format!(
                    "- agent_id={} role={} turns_msgs={} model={}",
                    s.agent_id,
                    s.role.as_str(),
                    s.snapshot.messages.len(),
                    s.model
                ));
            }
        }
        (bg_lines, saved_lines)
    }
}

impl Drop for SubagentStore {
    fn drop(&mut self) {
        self.root_cancel.cancel();
    }
}

/// Companion polling/collection tool for `task` with `run_in_background: true`.
pub struct TaskOutputTool {
    store: Arc<SubagentStore>,
}

impl TaskOutputTool {
    pub(crate) fn new(store: Arc<SubagentStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct TaskOutputArgs {
    #[serde(default, alias = "id", alias = "agent_id")]
    task_id: Option<String>,
    #[serde(default = "default_true")]
    block: bool,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    cancel: bool,
    #[serde(default)]
    list: bool,
}

fn default_true() -> bool {
    true
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Inspect, await, or cancel background subagent tasks spawned by `task` with `run_in_background: true`. \
         Pass `task_id` (or `agent_id`) to check status (`block: false`) or await completion (`block: true`, default). \
         Pass `list: true` to view all background tasks and resumable `agent_id`s."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Background task_id (or agent_id) returned by `task` when `run_in_background: true`."
                },
                "block": {
                    "type": "boolean",
                    "description": "Wait for completion (default: true). Set false for a non-blocking status poll."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "description": "Max seconds to wait when block=true (default: 30, max: 600)."
                },
                "cancel": {
                    "type": "boolean",
                    "description": "Set true to cancel the running background task."
                },
                "list": {
                    "type": "boolean",
                    "description": "Set true to list all background tasks and resumable agent_ids."
                }
            }
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    fn never_truncate_result(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let parsed: TaskOutputArgs = match serde_json::from_str(args)
            .or_else(|_| serde_json::from_str(&super::repair::repair_json(args)))
        {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid task_output args: {e}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };

        if parsed.list
            || (parsed.task_id.as_deref().unwrap_or("").trim().is_empty() && !parsed.cancel)
        {
            let (bg, saved) = self.store.list_summary();
            let mut out = String::new();
            out.push_str("Background tasks:\n");
            if bg.is_empty() {
                out.push_str("(none)\n");
            } else {
                out.push_str(&bg.join("\n"));
                out.push('\n');
            }
            out.push_str("\nResumable subagents:\n");
            if saved.is_empty() {
                out.push_str("(none)");
            } else {
                out.push_str(&saved.join("\n"));
            }
            return ToolResult {
                call_id: String::new(),
                content: out,
                is_error: false,
                images: vec![],
            };
        }

        let target_id = parsed.task_id.as_deref().unwrap_or("").trim();
        let Some(handle) = self.store.lookup_background(target_id) else {
            return ToolResult {
                call_id: String::new(),
                content: format!("unknown background `task_id` or `agent_id`: `{target_id}`"),
                is_error: true,
                images: vec![],
            };
        };

        if parsed.cancel {
            handle.cancel.cancel();
            let mut rx = handle.subscribe();
            if rx.borrow().is_none() {
                let _ = tokio::time::timeout(Duration::from_secs(2), rx.changed()).await;
            }
            if let Some(done) = handle.snapshot_completion() {
                return ToolResult {
                    call_id: String::new(),
                    content: done.rendered_block,
                    is_error: done.is_error,
                    images: vec![],
                };
            }
            return ToolResult {
                call_id: String::new(),
                content: format!(
                    "<task_output task_id=\"{}\" agent_id=\"{}\" status=\"cancelled\">Cancellation requested for `{}`.</task_output>",
                    handle.task_id, handle.agent_id, handle.description
                ),
                is_error: false,
                images: vec![],
            };
        }

        if let Some(done) = handle.snapshot_completion() {
            return ToolResult {
                call_id: String::new(),
                content: done.rendered_block,
                is_error: done.is_error,
                images: vec![],
            };
        }

        if !parsed.block {
            let activity = handle
                .live_activity
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| "running".to_string());
            return ToolResult {
                call_id: String::new(),
                content: format!(
                    "<task_output task_id=\"{}\" agent_id=\"{}\" status=\"running\" elapsed_secs=\"{}\">Latest activity: {activity}</task_output>",
                    handle.task_id,
                    handle.agent_id,
                    handle.started_at.elapsed().as_secs(),
                ),
                is_error: false,
                images: vec![],
            };
        }

        let wait_secs = parsed
            .timeout_secs
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
            .clamp(1, MAX_WAIT_TIMEOUT_SECS);
        let mut rx = handle.subscribe();
        let sleep = tokio::time::sleep(Duration::from_secs(wait_secs));
        tokio::pin!(sleep);

        loop {
            if let Some(done) = rx.borrow().clone() {
                return ToolResult {
                    call_id: String::new(),
                    content: done.rendered_block,
                    is_error: done.is_error,
                    images: vec![],
                };
            }
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = ctx.cancel.cancelled() => {
                    return ToolResult {
                        call_id: String::new(),
                        content: format!(
                            "<task_output task_id=\"{}\" agent_id=\"{}\" status=\"cancelled\">Parent wait cancelled.</task_output>",
                            handle.task_id, handle.agent_id
                        ),
                        is_error: true,
                        images: vec![],
                    };
                }
                _ = &mut sleep => {
                    let activity = handle
                        .live_activity
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .unwrap_or_else(|| "running".to_string());
                    return ToolResult {
                        call_id: String::new(),
                        content: format!(
                            "<task_output task_id=\"{}\" agent_id=\"{}\" status=\"running\" timed_out=\"true\" elapsed_secs=\"{}\">Still running ({activity}). Call `task_output` again to wait or pass `cancel: true`.</task_output>",
                            handle.task_id,
                            handle.agent_id,
                            handle.started_at.elapsed().as_secs(),
                        ),
                        is_error: false,
                        images: vec![],
                    };
                }
            }
        }

        ToolResult {
            call_id: String::new(),
            content: format!(
                "<task_output task_id=\"{}\" agent_id=\"{}\" status=\"failed\">Background task channel closed unexpectedly.</task_output>",
                handle.task_id, handle.agent_id
            ),
            is_error: true,
            images: vec![],
        }
    }
}
