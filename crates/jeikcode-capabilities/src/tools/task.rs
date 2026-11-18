//! `task` — 把子任务派发给隔离上下文的子 agent(subagent-by-composition)。
//! 主 agent 按难度选档位(fast/capable)、按类型(explore 只读 / worker 可编辑)
//! 选子工具集。子 agent 跑在独立内核会话里,结果用 <task_result> 包回。

use async_trait::async_trait;
use jeikcode_kernel::agent::{Agent, AutoRespond, Outcome, ToolLoopPolicy};
use jeikcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use jeikcode_kernel::hook::{LifecycleHooks, TurnCtx};
use jeikcode_kernel::message::{Message, SessionSnapshot};
use jeikcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use jeikcode_kernel::provider::LlmProvider;
use jeikcode_kernel::request::RequestCtx;
use jeikcode_kernel::tool::{
    MountedTools, ProgressSink, RiskLevel, Tool, ToolCall, ToolContext, ToolResult,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::subagent_store::{
    BackgroundCompletion, BackgroundTaskHandle, ResumeInFlightGuard, SavedSubagentState,
    SubagentStore, TaskOutputTool,
};
use super::subagent_verifier::{SubagentRole, VerifierReadOnlyShellGuard, VERIFIER_PERSONA};
use crate::tools::output_artifact::ArtifactStore;

/// Sentinel prefix on a `ctx.progress` line that marks it as EPHEMERAL live activity
/// (current action of a running subtask) rather than a committed ↻/✓/✗ scrollback line.
/// The TUI routes marker-prefixed chunks to the in-place spinner instead of scrollback.
/// jeikcode-tuix references THIS const (can't drift). The jeikcode-daemon leg has no
/// dependency on this crate and hard-codes the literal `'\u{1e}'` in `to_wire` (to drop
/// these lines from the webui) — if you ever change this sentinel, update THAT literal too.
pub const SUBAGENT_ACTIVITY_MARKER: char = '\u{1e}';
/// Hard-denies any child tool call that references a sensitive path (credentials, `~/.ssh`,
/// `.env`, cloud creds). Mounted on every subagent child. Unlike the parent's
/// `SensitivePathGate` — which PROMPTS — this DENIES outright, because a subagent runs
/// `AutoRespond::AllowAll`, so a prompt would just auto-approve itself. The generic credential
/// bash gate runs immediately before this one; this guard terminates any remaining sensitive
/// path access rather than letting a child repeatedly rephrase it.
#[derive(Default)]
pub(crate) struct DenySensitivePaths {
    working_dir: Option<PathBuf>,
}

impl DenySensitivePaths {
    pub(crate) fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: Some(working_dir.into()),
        }
    }

    fn references_resolved_sensitive_path(&self, args: &str) -> bool {
        let Some(wd) = self.working_dir.as_deref() else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
            return false;
        };
        let mut raw_paths: Vec<&str> = Vec::new();
        for key in ["path", "file_path", "target_file", "filePath", "filename"] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                raw_paths.push(s);
            }
        }
        if let Some(arr) = v.get("paths").and_then(|x| x.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    raw_paths.push(s);
                }
            }
        }
        raw_paths.into_iter().any(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with("artifact://") {
                return false;
            }
            let resolved = crate::tools::resolve_path(trimmed, wd);
            crate::tools::path_is_sensitive(&resolved)
                || crate::ignore_policy::IgnorePolicy::is_sensitive_at(wd, &resolved)
        })
    }

    fn requests_include_sensitive(tool_name: &str, args: &str) -> bool {
        if !matches!(tool_name, "grep" | "glob") {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| {
                v.get("include_sensitive")
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false)
    }
}

#[async_trait]
impl ToolMiddleware for DenySensitivePaths {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if Self::requests_include_sensitive(&call.name, &call.arguments)
            || crate::tools::references_sensitive_path(&call.arguments)
            || self.references_resolved_sensitive_path(&call.arguments)
        {
            return BeforeOutcome::deny_turn(format!(
                "subagent may not touch sensitive paths (credentials / ~/.ssh / .env / .codegraphignore.graph): {}",
                call.name
            ));
        }
        BeforeOutcome::Proceed
    }
}

/// Hard-denies any child tool call attempting external network egress when offline/air-gap
/// mode is active. Mounted on every subagent child (both explore and worker).
pub struct DenyOfflineNetwork;

pub(crate) fn is_network_egress_tool(name: &str) -> bool {
    let clean = name.trim().to_ascii_lowercase();
    let is_network = |s: &str| {
        matches!(
            s,
            "web_search"
                | "web_fetch"
                | "web_scrape"
                | "search_web"
                | "fetch"
                | "read_url"
                | "read_url_content"
                | "execute_url"
                | "browser"
                | "browse"
        ) || s.starts_with("web_")
            || s.contains("web_search")
            || s.contains("web_fetch")
            || s.contains("web_scrape")
            || s.contains("search_web")
            || s.ends_with("_web_search")
    };

    if is_network(&clean) {
        return true;
    }
    // Check leaf name if prefixed by MCP server namespace (e.g., server/tool or server__tool)
    if let Some((_, leaf)) = clean.rsplit_once('/').or_else(|| clean.rsplit_once("__")) {
        if is_network(leaf) {
            return true;
        }
    }
    false
}

#[async_trait]
impl ToolMiddleware for DenyOfflineNetwork {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if jeikcode_config::config::offline::is_offline_active()
            && is_network_egress_tool(&call.name)
        {
            // Offline network is a capability boundary, not a hard policy
            // violation. `Deny` blocks this call without terminating the
            // subagent turn or cancelling safe siblings already planned in the
            // same batch; the worker can continue with local tools.
            return BeforeOutcome::deny(format!(
                "network egress tool `{}` is disabled for subagents: offline / air-gap mode is active",
                call.name
            ));
        }
        BeforeOutcome::Proceed
    }
}

/// The literal directory prefix of a glob: the leading path segments before the first
/// segment that contains a glob metacharacter. `src/auth/**` → `src/auth`; `**` → ``;
/// `Cargo.toml` → `Cargo.toml`. Used to test a `search_replace` DIR root against a scope
/// (globset's `src/auth/**` does NOT match the bare dir `src/auth`).
fn recursive_dir_prefix(glob: &str) -> Option<String> {
    // `**` covers the whole tree.
    if glob == "**" {
        return Some(String::new());
    }
    // Only a recursive dir glob (`<literal-dir>/**`) confines a search_replace root: the tool
    // rewrites EVERY file under its root, so the root is "entirely in scope" only when the
    // scope covers the whole subtree. A non-recursive scope (`*.rs`, `src/*.rs`, `Cargo.toml`,
    // `src/**/x.rs`, or a bare dir like `src/auth`) matches only specific files, never a whole
    // directory, so it grants NO search_replace root.
    let prefix = glob.strip_suffix("/**")?;
    if prefix.is_empty() || prefix.contains(['*', '?', '[', ']', '{', '}']) {
        return None;
    }
    Some(prefix.to_string())
}

/// Lexically collapse `.` / `..` WITHOUT touching the filesystem (targets may be new files
/// that don't exist yet). A `..` at the root is absorbed, so an escape normalizes to a path
/// that will fail the working-dir `strip_prefix` below → denied.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 1-based indices of explicit fresh `worker` subtasks that declared no non-empty `scope`.
/// Resumed workers (`resume_agent_id` set) inherit their saved scope when `scope` is omitted.
fn workers_missing_scope(tasks: &[SubTask]) -> Vec<usize> {
    tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.is_worker()
                && t.resume_agent_id
                    .as_deref()
                    .is_none_or(|id| id.trim().is_empty())
                && t.scope.iter().all(|s| s.trim().is_empty())
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// Confines a `worker` subagent's WRITE tools to its declared `scope`. Mirrors
/// [`DenySensitivePaths`]: a hard deny (the child runs `AutoRespond::AllowAll`, so a prompt
/// would self-approve). ONLY the write tools are gated — reads are unrestricted (a worker
/// often reads elsewhere for context) and `bash` retains dispatch-level trust (design §6).
struct WorkerScopeGate {
    working_dir: PathBuf,
    /// Compiled globs for single-file targets (`edit` / `edit_file` / `write` / `write_file`).
    globs: globset::GlobSet,
    /// Literal directory prefix of each scope, for `search_replace` DIR roots.
    dir_prefixes: Vec<PathBuf>,
    /// Human-readable scope list for deny messages.
    display: String,
}

impl WorkerScopeGate {
    fn new(scopes: &[String], working_dir: &Path) -> Self {
        let mut builder = globset::GlobSetBuilder::new();
        let mut dir_prefixes = Vec::new();
        for s in scopes {
            // Only scopes whose glob compiles participate — in BOTH the file-path globset and
            // the search_replace dir-prefix list — so a malformed scope can't confine writes
            // one way and allow them the other.
            if let Ok(g) = globset::GlobBuilder::new(s).literal_separator(true).build() {
                builder.add(g);
                if let Some(dir) = recursive_dir_prefix(s) {
                    dir_prefixes.push(PathBuf::from(dir));
                }
            }
        }
        let globs = builder
            .build()
            .unwrap_or_else(|_| globset::GlobSet::empty());
        Self {
            working_dir: working_dir.to_path_buf(),
            globs,
            dir_prefixes,
            display: scopes.join(", "),
        }
    }

    /// `None` = allow; `Some(reason)` = deny. Non-write tools (reads, `bash`, anything else)
    /// always return `None`.
    fn violation(&self, tool: &str, args_json: &str) -> Option<String> {
        // Name sets come from capabilities (`is_file_mutation_tool` / `is_search_replace_tool`)
        // so a newly registered alias can never fall through to the `None` (allow) arm again —
        // `create_file` used to escape this gate exactly that way.
        if super::is_search_replace_tool(tool) {
            let value = serde_json::from_str::<serde_json::Value>(args_json)
                .unwrap_or(serde_json::Value::Null);
            match value.get("path").and_then(|x| x.as_str()) {
                None => Some(format!(
                    "worker {tool} has no `path`, which would rewrite the whole tree; \
                     restrict `path` to within the declared scope [{}].",
                    self.display
                )),
                Some(dir) => match self.workspace_relative(dir) {
                    None => Some(format!(
                        "worker edit out of scope: {dir} is outside the working directory."
                    )),
                    Some(rel_dir) if self.dir_in_scope(&rel_dir) => None,
                    Some(rel_dir) => Some(self.deny_out_of_scope(&rel_dir)),
                },
            }
        } else if super::is_file_mutation_tool(tool) {
            let raw = match serde_json::from_str::<serde_json::Value>(args_json)
                .ok()
                .as_ref()
                .and_then(json_path_arg)
            {
                Some(p) => p.to_string(),
                // Fail closed: a write tool with no usable `path` or `file_path` must not slip past
                // the gate (defense-in-depth; the tool itself also rejects it).
                None => {
                    return Some(format!(
                        "worker {tool} call has no usable `path` or `file_path`; cannot verify it is within scope."
                    ))
                }
            };
            match self.workspace_relative(&raw) {
                None => Some(format!(
                    "worker edit out of scope: {raw} is outside the working directory."
                )),
                Some(rel) if self.globs.is_match(&rel) => None,
                Some(rel) => Some(self.deny_out_of_scope(&rel)),
            }
        } else {
            None
        }
    }

    fn deny_out_of_scope(&self, rel: &str) -> String {
        format!(
            "worker edit out of scope: {rel} is not within the declared scope [{}]. To change \
             it, re-dispatch this worker with a wider scope that includes it.",
            self.display
        )
    }

    /// Resolve `raw` (absolute, or relative to the working dir) to a working-dir-relative,
    /// `.`/`..`-collapsed path with `/` separators. `None` if it escapes the working dir
    /// (absolute-outside, or `..` above the root) — such writes are denied.
    fn workspace_relative(&self, raw: &str) -> Option<String> {
        let joined = crate::pathutil::resolve_path(raw, &self.working_dir);
        let base = lexical_normalize(&self.working_dir);
        let full = lexical_normalize(&joined);
        full.strip_prefix(&base)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    }

    /// Whether a working-dir-relative DIRECTORY (a `search_replace` root) is within scope: it
    /// equals or lives under any RECURSIVE scope's dir (see [`recursive_dir_prefix`]). An empty
    /// prefix (scope `**`) covers the whole tree. Only recursive `<dir>/**` scopes grant a root
    /// here — a non-recursive scope (`*.rs`, `src/*.rs`, `Cargo.toml`, or a bare dir `src/auth`)
    /// covers only specific files, so it grants NO search_replace root even though it may still
    /// match a single-file `edit_file`/`write_file` target. A worker wanting to search_replace a
    /// whole directory must declare it recursively: `src/auth/**`.
    fn dir_in_scope(&self, rel_dir: &str) -> bool {
        let rd = Path::new(rel_dir);
        self.dir_prefixes
            .iter()
            .any(|p| p.as_os_str().is_empty() || rd == p.as_path() || rd.starts_with(p))
    }
}

#[async_trait]
impl ToolMiddleware for WorkerScopeGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        match self.violation(&call.name, &call.arguments) {
            Some(reason) => BeforeOutcome::deny(reason),
            None => BeforeOutcome::Proceed,
        }
    }
}

/// The middleware stack for a subagent child: sensitive-path guard for everyone,
/// air-gap offline network shielding, plus `WorkerScopeGate` for `worker` and
/// `VerifierReadOnlyShellGuard` for `verifier`.
fn child_middlewares(
    role: SubagentRole,
    scope: &[String],
    working_dir: &Path,
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
) -> Vec<Arc<dyn ToolMiddleware>> {
    let mut mw: Vec<Arc<dyn ToolMiddleware>> = vec![
        Arc::new(DenySensitivePaths::new(working_dir)),
        Arc::new(DenyOfflineNetwork),
    ];
    match role {
        SubagentRole::Worker => {
            mw.extend(inherited_worker_middlewares.iter().cloned());
            mw.push(Arc::new(WorkerScopeGate::new(scope, working_dir)));
        }
        SubagentRole::Verifier => {
            mw.push(Arc::new(VerifierReadOnlyShellGuard));
        }
        SubagentRole::Explore => {}
    }
    mw
}

const EXPLORE_PERSONA: &str = "You are a READ-ONLY investigation subagent. Use read/search \
tools to answer the assigned task about the codebase. You CANNOT edit files. When done, \
stop with a concise findings report the parent agent can act on.";

const WORKER_PERSONA: &str = "You are a focused EXECUTION subagent. Do exactly the task \
described — no more, no less — honoring the working directory. Make the change, verify it \
if cheap, then stop with a one-line summary of what you changed. Do not wander outside the \
task's stated scope.";

#[derive(Deserialize)]
struct SubTask {
    #[serde(default, alias = "desc", alias = "name", alias = "title")]
    description: String,
    prompt: String,
    #[serde(default, alias = "type", alias = "agent_type", alias = "role")]
    subagent_type: String,
    #[serde(default, alias = "tier", alias = "level", alias = "model_tier")]
    difficulty: Option<String>,
    /// Worker-only: working-dir-relative globs the worker may WRITE within. Required for
    /// `worker`; ignored for `explore` / `verifier` (read-only). Enforced by `WorkerScopeGate`.
    #[serde(
        default,
        deserialize_with = "crate::tools::repair::deserialize_lenient_string_list"
    )]
    scope: Vec<String>,
    /// Optional `agent_id` from a prior `<task>` output to resume its conversation.
    #[serde(default, alias = "agent_id")]
    resume_agent_id: Option<String>,
    /// When true, spawn in the background and return an immediate `<task_spawned>` handle.
    #[serde(default)]
    run_in_background: bool,
    /// When true, inject full uncapped `<project_instructions>`, `<user_preferences>`, and
    /// read-only `<memory>` in exact parent block order before the subtask prompt.
    #[serde(default)]
    inherit_context: bool,
    /// Optional per-subtask round limit override (clamped to `1..=64`; `0` or omitted uses default).
    #[serde(default)]
    max_rounds: Option<u32>,
}

impl SubTask {
    fn requested_role(&self) -> Option<SubagentRole> {
        SubagentRole::parse_opt(&self.subagent_type)
    }

    fn is_worker(&self) -> bool {
        self.requested_role().is_some_and(SubagentRole::is_worker)
    }

    fn requested_difficulty(&self) -> Option<&str> {
        self.difficulty
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    #[cfg(test)]
    fn is_hard(&self) -> bool {
        self.requested_difficulty().is_some_and(is_hard_difficulty)
    }

    fn resolved_description(&self, index: usize) -> String {
        let d = self.description.trim();
        if !d.is_empty() {
            return d.to_string();
        }
        let first = self.prompt.lines().next().unwrap_or("").trim();
        if !first.is_empty() {
            first_line_capped(first, 30)
        } else {
            format!("subtask #{}", index + 1)
        }
    }
}

fn is_hard_difficulty(raw: &str) -> bool {
    let diff = raw.trim();
    diff.eq_ignore_ascii_case("hard") || diff.eq_ignore_ascii_case("complex")
}

#[derive(Deserialize)]
struct Args {
    tasks: Vec<SubTask>,
    #[serde(default)]
    run_in_background: bool,
}

pub struct TaskTool {
    make_fast_provider: Arc<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_capable_provider: Arc<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_explore_tools: Arc<dyn Fn() -> MountedTools + Send + Sync>,
    make_worker_tools: Arc<dyn Fn() -> MountedTools + Send + Sync>,
    make_verifier_tools: Option<Arc<dyn Fn() -> MountedTools + Send + Sync>>,
    /// `Some(n)` bounds how many children run at once; `None` = unbounded
    /// (config `[subagent] max_concurrent = 0`).
    max_concurrent: Option<usize>,
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
    max_rounds: Option<u32>,
    tool_loop_policy: Option<ToolLoopPolicy>,
    inherited_worker_middlewares: Vec<Arc<dyn ToolMiddleware>>,
    project_context_loader: Option<Arc<super::subagent_context::ProjectContextLoader>>,
    inherited_context_loader: Option<Arc<super::subagent_context::InheritedContextLoader>>,
    artifact_store: Arc<std::sync::RwLock<Option<Arc<ArtifactStore>>>>,
    store: Arc<SubagentStore>,
}

impl TaskTool {
    pub fn new(
        make_fast_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_capable_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_explore_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
        make_worker_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
    ) -> Self {
        Self {
            make_fast_provider: Arc::new(make_fast_provider),
            make_capable_provider: Arc::new(make_capable_provider),
            make_explore_tools: Arc::new(make_explore_tools),
            make_worker_tools: Arc::new(make_worker_tools),
            make_verifier_tools: None,
            // Unbounded by default: the shipped config is `max_concurrent = 0`.
            max_concurrent: None,
            semaphore: None,
            max_rounds: Some(super::DEFAULT_CHILD_MAX_ROUNDS),
            tool_loop_policy: Some(ToolLoopPolicy::default()),
            inherited_worker_middlewares: Vec::new(),
            project_context_loader: None,
            inherited_context_loader: None,
            artifact_store: Arc::new(std::sync::RwLock::new(None)),
            store: Arc::new(SubagentStore::new()),
        }
    }

    /// Attach an optional session [`ArtifactStore`] so oversized (`>16 KB`) subagent outputs
    /// spill their full text to `artifact://<id>` before head/tail compaction.
    pub fn with_optional_artifact_store(mut self, store: Option<Arc<ArtifactStore>>) -> Self {
        self.artifact_store = Arc::new(std::sync::RwLock::new(store));
        self
    }

    /// Update the session [`ArtifactStore`] dynamically (e.g. during `parts::assemble`).
    pub fn set_artifact_store(&self, store: Option<Arc<ArtifactStore>>) {
        if let Ok(mut guard) = self.artifact_store.write() {
            *guard = store;
        }
    }

    /// Carry over saved `resume_agent_id` states from a previous `TaskTool` instance
    /// across capability reloads (`/reload all`).
    pub fn inherit_store_from(&self, previous: &TaskTool) {
        self.store.inherit_saved_from(&previous.store);
    }

    /// Supply the toolset factory for read-only `verifier` subagents (read/search +
    /// guarded verification `run_command`, zero write/edit tools). Falls back to
    /// `make_explore_tools` when unset.
    pub fn with_verifier_tools(
        mut self,
        make_verifier_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
    ) -> Self {
        self.make_verifier_tools = Some(Arc::new(make_verifier_tools));
        self
    }

    /// Construct the companion `task_output` tool sharing this `TaskTool`'s state store.
    pub fn output_tool(&self) -> TaskOutputTool {
        TaskOutputTool::new(Arc::clone(&self.store))
    }

    /// Bind the parent session ID so resumable `agent_id` snapshots stay strictly isolated
    /// per parent session.
    pub fn bind_session(&self, session_id: &str) {
        self.store.bind_session(session_id);
    }

    /// Bound how many children run at once. `0` removes the bound entirely —
    /// every dispatched subtask starts immediately, and liveness is left to the
    /// provider idle clocks, the per-child round cap, and explicit cancel.
    /// Any other value is used as-is (no floor, so `1` means strictly serial).
    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = (n != 0).then_some(n);
        self.semaphore = self
            .max_concurrent
            .map(|limit| Arc::new(tokio::sync::Semaphore::new(limit)));
        self
    }

    /// Override the per-child model-round high-water mark. `0` disables this cap;
    /// the exact no-progress policy is configured independently.
    pub fn with_max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = (n != 0).then_some(n);
        self
    }

    /// Use the embedding product's exact no-progress policy. `None` disables it
    /// for intentional repeated operations; the independent round cap remains.
    pub fn with_tool_loop_policy(mut self, policy: Option<ToolLoopPolicy>) -> Self {
        self.tool_loop_policy = policy;
        self
    }

    /// Install a parent-owned hard policy in every worker child. Explore children have
    /// no shell/write tools and deliberately remain unaffected.
    pub fn with_worker_middleware(mut self, middleware: Arc<dyn ToolMiddleware>) -> Self {
        self.inherited_worker_middlewares.push(middleware);
        self
    }

    /// Supply a custom `<project_instructions>` loader for child subagents (e.g. including
    /// cached project knowledge packs). When unset, `TaskTool` falls back to loading
    /// `PROJECT_INSTRUCTION_FILES` directly from `ctx.working_dir`.
    pub fn with_project_context_loader(
        mut self,
        loader: impl Fn(&Path) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.project_context_loader = Some(Arc::new(loader));
        self
    }

    /// Supply a custom full-context loader used when `inherit_context: true` is requested
    /// on a subtask (`<memory>`, `<user_preferences>`, uncapped `<project_instructions>`).
    pub fn with_inherited_context_loader(
        mut self,
        loader: impl Fn(&Path) -> super::subagent_context::InheritedSubagentContext
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.inherited_context_loader = Some(Arc::new(loader));
        self
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Dispatch subagents to execute isolated subtasks. \
         `explore` is read-only for research and findings; \
         `worker` can edit files and must declare a `scope` to restrict write access; \
         `verifier` is a blind, fresh-context read-only evaluator that inspects files and runs \
         verification checks (emitting PASS / NEEDS_WORK). \
         Pass `resume_agent_id` to continue a prior subagent's conversation, or set \
         `run_in_background: true` and poll/await with `task_output`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "run_in_background": {
                    "type": "boolean",
                    "description": "Optional: dispatch all subtasks asynchronously in the background and return task_id handles for `task_output`."
                },
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {"type": "string", "description": "3-5 word label"},
                            "prompt": {"type": "string", "description": "The full subtask for the subagent"},
                            "subagent_type": {"type": "string", "enum": ["explore", "worker", "verifier"]},
                            "difficulty": {"type": "string", "enum": ["simple", "hard"]},
                            "scope": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Worker-only, REQUIRED for worker: working-directory-relative globs the worker may write within (e.g. [\"src/auth/**\", \"Cargo.toml\"]). The worker can only write files inside this scope; reads are unrestricted. Ignored for explore/verifier."
                            },
                            "resume_agent_id": {
                                "type": "string",
                                "description": "Optional agent_id from a prior `<task>` output to resume its conversation state."
                            },
                            "run_in_background": {
                                "type": "boolean",
                                "description": "Optional: run this subtask asynchronously in the background and return a task_id handle immediately."
                            },
                            "inherit_context": {
                                "type": "boolean",
                                "description": "Optional: inject full uncapped <project_instructions>, <user_preferences>, and read-only <memory> in parent block order (default false; useful for high-context plan review, critique, or judging)."
                            },
                            "max_rounds": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 64,
                                "description": "Optional: cap the maximum model rounds for this subtask (1..=64; useful for 1-round synthesis judges or bounded verification checks)."
                            }
                        },
                        "required": ["description", "prompt"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    fn never_truncate_result(&self) -> bool {
        true
    }

    fn risk(&self, args: &str) -> RiskLevel {
        // Use the SAME repair-aware parse as `execute` so a `worker` dispatch (or
        // resumed worker via `resume_agent_id`) is still detected as Risky.
        match parse_task_args(args) {
            Ok(a)
                if a.tasks.iter().any(|t| {
                    t.is_worker()
                        || t.resume_agent_id
                            .as_deref()
                            .and_then(|id| self.store.peek_role(id))
                            .is_some_and(SubagentRole::is_worker)
                }) =>
            {
                RiskLevel::Risky
            }
            _ => RiskLevel::Safe,
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let parsed: Args = match parse_task_args(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!(
                        "invalid task args: {e}\n\nThe arguments were not valid JSON — the output \
                         was likely truncated (a large batch can exceed the model's output limit) \
                         or a string contained an unescaped quote. Retry with FEWER subtasks \
                         and/or SHORTER prompts, and ensure every string value is JSON-escaped."
                    ),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        if parsed.tasks.is_empty() {
            return ToolResult {
                call_id: String::new(),
                content: "no tasks provided".into(),
                is_error: true,
                images: vec![],
            };
        }

        let missing = workers_missing_scope(&parsed.tasks);
        if !missing.is_empty() {
            let idxs = missing
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            return ToolResult {
                call_id: String::new(),
                content: format!(
                    "worker subtask {idxs} declared no `scope`. Each worker must declare `scope` \
                     (working-dir-relative globs, e.g. [\"src/auth/**\"]) — its writable file lane, \
                     shown at approval time and enforced during the run. Add a scope and retry."
                ),
                is_error: true,
                images: vec![],
            };
        }

        let top_bg = parsed.run_in_background;
        let offline_active = jeikcode_config::config::offline::is_offline_active();
        let sem = self.semaphore.clone();
        let max_rounds = self.max_rounds;
        let tool_loop_policy = self.tool_loop_policy;
        let inherited_worker_middlewares = self.inherited_worker_middlewares.clone();
        let project_instructions = super::subagent_context::resolve_project_instructions(
            &ctx.working_dir,
            self.project_context_loader.as_ref(),
        );
        let mut cached_inherited_context: Option<
            super::subagent_context::InheritedSubagentContext,
        > = None;
        let session_scope = self.store.current_session_scope();
        let mut set = tokio::task::JoinSet::new();
        let mut immediate_blocks: Vec<(String, String, bool)> = Vec::new();

        if offline_active {
            ctx.progress.emit(format!(
                "dispatching {} subtask(s) [air-gap offline mode]…",
                parsed.tasks.len()
            ));
        } else {
            ctx.progress
                .emit(format!("dispatching {} subtask(s)…", parsed.tasks.len()));
        }

        for (idx, t) in parsed.tasks.into_iter().enumerate() {
            let requested_role = t.requested_role();
            let desc = t.resolved_description(idx);
            let requested_difficulty = t.requested_difficulty().map(str::to_string);
            let run_bg = top_bg || t.run_in_background;
            let resume_id = t
                .resume_agent_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let (
                role,
                scope,
                agent_id,
                resume_snapshot,
                difficulty,
                inherit_context,
                inflight_guard,
            ) = if let Some(rid) = resume_id {
                match self.store.get_for_resume(&rid, requested_role, &t.scope) {
                    Ok((saved, guard)) => (
                        saved.role,
                        saved.scope,
                        saved.agent_id,
                        Some(saved.snapshot),
                        requested_difficulty.or(saved.difficulty),
                        t.inherit_context || saved.inherit_context,
                        Some(guard),
                    ),
                    Err(err_msg) => {
                        let role_guess = requested_role.unwrap_or(SubagentRole::Explore);
                        let label = format!("{}#{}", role_guess.as_str(), idx + 1);
                        let block = render_task_block(
                            &label,
                            &rid,
                            &desc,
                            "none",
                            "error",
                            "task_error",
                            &err_msg,
                        );
                        immediate_blocks.push((label, block, true));
                        continue;
                    }
                }
            } else {
                let role = requested_role.unwrap_or(SubagentRole::Explore);
                let agent_id = self.store.mint_agent_id(role);
                (
                    role,
                    t.scope.clone(),
                    agent_id,
                    None,
                    requested_difficulty,
                    t.inherit_context,
                    None::<ResumeInFlightGuard>,
                )
            };

            let is_hard = difficulty.as_deref().is_some_and(is_hard_difficulty);
            let child_max_rounds = t
                .max_rounds
                .filter(|&n| n > 0)
                .map(|n| n.clamp(1, 64))
                .or(max_rounds);
            let artifact_store = self.artifact_store.read().ok().and_then(|g| g.clone());

            let inherited_ctx = if inherit_context {
                if cached_inherited_context.is_none() {
                    cached_inherited_context =
                        Some(super::subagent_context::resolve_inherited_context(
                            &ctx.working_dir,
                            self.inherited_context_loader.as_ref(),
                        ));
                }
                cached_inherited_context.clone()
            } else {
                None
            };

            let provider = if is_hard {
                (self.make_capable_provider)()
            } else {
                (self.make_fast_provider)()
            };
            let model = provider.model_name().to_string();
            let tools = match role {
                SubagentRole::Worker => (self.make_worker_tools)(),
                SubagentRole::Verifier => match &self.make_verifier_tools {
                    Some(mk) => mk(),
                    None => (self.make_explore_tools)(),
                },
                SubagentRole::Explore => (self.make_explore_tools)(),
            };
            let base_persona = match role {
                SubagentRole::Worker => WORKER_PERSONA,
                SubagentRole::Verifier => VERIFIER_PERSONA,
                SubagentRole::Explore => EXPLORE_PERSONA,
            };
            let persona = super::subagent_context::build_child_persona(
                base_persona,
                role,
                &scope,
                if inherit_context {
                    None
                } else {
                    project_instructions.as_deref()
                },
            );
            let wd = ctx.working_dir.clone();
            let label = format!("{}#{}", role.as_str(), idx + 1);
            let prompt = t.prompt;
            let sem = sem.clone();
            let inherited_worker_middlewares = inherited_worker_middlewares.clone();
            let store_weak = Arc::downgrade(&self.store);
            let session_scope = session_scope.clone();

            if run_bg {
                let task_id = self.store.mint_task_id();
                let child_cancel = self.store.child_cancel_token();
                let hook_cancel = child_cancel.clone();
                let bg_handle = self.store.register_background(
                    task_id.clone(),
                    agent_id.clone(),
                    label.clone(),
                    desc.clone(),
                    role,
                    model.clone(),
                    child_cancel.clone(),
                );
                let parent_cancel = ctx.cancel.clone();
                let watcher_cancel = child_cancel.clone();
                let mut watcher_rx = bg_handle.subscribe();
                tokio::spawn(async move {
                    if watcher_rx.borrow().is_some() {
                        return;
                    }
                    tokio::select! {
                        _ = parent_cancel.cancelled() => watcher_cancel.cancel(),
                        _ = watcher_rx.changed() => {}
                    }
                });
                ctx.progress.emit(subtask_progress_line(
                    &format!("\u{21bb} bg {label} [{task_id}]"),
                    &model,
                    &desc,
                ));
                let spawned_block = format!(
                    "<task_spawned id=\"{label}\" task_id=\"{task_id}\" agent_id=\"{agent_id}\" role=\"{}\" model=\"{model}\" status=\"running\">\n<summary>{desc}</summary>\nDispatched in background. Call `task_output` with `{{\"task_id\": \"{task_id}\"}}` to poll or await completion.\n</task_spawned>",
                    role.as_str()
                );
                immediate_blocks.push((label.clone(), spawned_block, false));
                let bg_handle_for_task = Arc::clone(&bg_handle);
                tokio::spawn(async move {
                    let _inflight_guard = inflight_guard;
                    let _permit = match &sem {
                        Some(sem) => Some(
                            sem.clone()
                                .acquire_owned()
                                .await
                                .expect("semaphore not closed"),
                        ),
                        None => None,
                    };
                    let ledger = Arc::new(Mutex::new(ActionReceiptLedger::default()));
                    let progress_hook = Arc::new(
                        SubtaskProgressHook::with_ledger(
                            ProgressSink::noop(),
                            label.clone(),
                            desc.contains(|ch: char| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
                            hook_cancel,
                            ledger.clone(),
                        )
                        .with_background_handle(Arc::clone(&bg_handle_for_task)),
                    );
                    let telemetry_hook = Arc::clone(&progress_hook);
                    let (outcome, snapshot) = spawn_and_run_child(
                        provider,
                        tools,
                        persona,
                        resume_snapshot,
                        inherited_ctx,
                        wd.clone(),
                        child_cancel,
                        progress_hook,
                        tool_loop_policy,
                        child_max_rounds,
                        role,
                        &scope,
                        &inherited_worker_middlewares,
                        prompt,
                    )
                    .await;
                    if let Some(snap) = snapshot {
                        if let Some(store) = store_weak.upgrade() {
                            store.save_state(SavedSubagentState {
                                agent_id: agent_id.clone(),
                                session_scope,
                                role,
                                scope,
                                difficulty,
                                inherit_context,
                                working_dir: wd,
                                model: model.clone(),
                                snapshot: snap,
                                updated_seq: 0,
                            });
                        }
                    }
                    let receipt_ledger = ledger.lock().map(|l| l.clone()).unwrap_or_default();
                    let telemetry = telemetry_hook.snapshot_telemetry();
                    let (rendered_block, is_err) = render_subtask_outcome(
                        &label,
                        &agent_id,
                        &desc,
                        &model,
                        role,
                        outcome,
                        &receipt_ledger,
                        artifact_store.as_deref(),
                        Some(telemetry),
                    );
                    let status_label = if is_err {
                        if bg_handle_for_task.cancel.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        }
                    } else {
                        "completed"
                    };
                    bg_handle_for_task.finish(BackgroundCompletion {
                        rendered_block,
                        is_error: is_err,
                        status_label,
                    });
                });
                continue;
            }

            let child_cancel = ctx.cancel.child_token();
            let hook_cancel = child_cancel.clone();
            let progress = ctx.progress.clone();
            progress.emit(format!(
                "{SUBAGENT_ACTIVITY_MARKER}{}",
                subtask_progress_line(&format!("\u{25cb} queued \u{b7} {label}"), &model, &desc,)
            ));

            set.spawn(async move {
                let _inflight_guard = inflight_guard;
                let _permit = match &sem {
                    Some(sem) => Some(
                        sem.clone()
                            .acquire_owned()
                            .await
                            .expect("semaphore not closed"),
                    ),
                    None => None,
                };
                progress.emit(subtask_progress_line(
                    &format!("\u{21bb} {label}"),
                    &model,
                    &desc,
                ));
                let ledger = Arc::new(Mutex::new(ActionReceiptLedger::default()));
                let progress_hook = Arc::new(SubtaskProgressHook::with_ledger(
                    progress.clone(),
                    label.clone(),
                    desc.contains(|ch: char| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
                    hook_cancel,
                    ledger.clone(),
                ));
                let telemetry_hook = Arc::clone(&progress_hook);
                let (outcome, snapshot) = spawn_and_run_child(
                    provider,
                    tools,
                    persona,
                    resume_snapshot,
                    inherited_ctx,
                    wd.clone(),
                    child_cancel,
                    progress_hook,
                    tool_loop_policy,
                    child_max_rounds,
                    role,
                    &scope,
                    &inherited_worker_middlewares,
                    prompt,
                )
                .await;
                if let Some(snap) = snapshot {
                    if let Some(store) = store_weak.upgrade() {
                        store.save_state(SavedSubagentState {
                            agent_id: agent_id.clone(),
                            session_scope,
                            role,
                            scope,
                            difficulty,
                            inherit_context,
                            working_dir: wd,
                            model: model.clone(),
                            snapshot: snap,
                            updated_seq: 0,
                        });
                    }
                }
                let receipt_ledger = ledger.lock().map(|l| l.clone()).unwrap_or_default();
                let telemetry = telemetry_hook.snapshot_telemetry();
                let head = if outcome.stop == StopReason::Stopped {
                    format!("\u{2713} done \u{b7} {label}")
                } else {
                    format!("\u{2717} failed ({:?}) \u{b7} {label}", outcome.stop)
                };
                progress.emit(subtask_progress_line(&head, &model, &desc));
                let (block, is_err) = render_subtask_outcome(
                    &label,
                    &agent_id,
                    &desc,
                    &model,
                    role,
                    outcome,
                    &receipt_ledger,
                    artifact_store.as_deref(),
                    Some(telemetry),
                );
                (label, block, is_err)
            });
        }

        let mut all_blocks = immediate_blocks;
        while let Some(res) = set.join_next().await {
            if let Ok(tuple) = res {
                all_blocks.push(tuple);
            }
        }
        all_blocks.sort_by(|a, b| a.0.cmp(&b.0));

        let n_total = all_blocks.len();
        let n_error = all_blocks.iter().filter(|(_, _, is_err)| *is_err).count();
        let blocks: Vec<String> = all_blocks.into_iter().map(|(_, b, _)| b).collect();

        ToolResult {
            call_id: String::new(),
            content: blocks.join("\n"),
            is_error: n_total > 0 && n_error == n_total,
            images: vec![],
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn spawn_and_run_child(
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    persona: String,
    resume_snapshot: Option<SessionSnapshot>,
    inherited_context: Option<super::subagent_context::InheritedSubagentContext>,
    wd: PathBuf,
    child_cancel: tokio_util::sync::CancellationToken,
    progress_hook: Arc<SubtaskProgressHook>,
    tool_loop_policy: Option<ToolLoopPolicy>,
    max_rounds: Option<u32>,
    role: SubagentRole,
    scope: &[String],
    inherited_worker_middlewares: &[Arc<dyn ToolMiddleware>],
    prompt: String,
) -> (Outcome, Option<SessionSnapshot>) {
    let mut builder = Agent::builder()
        .provider(provider)
        .tools(tools)
        .working_dir(wd.clone())
        .cancel_token(child_cancel)
        .hook(progress_hook.clone());
    if let Some(ctx) = inherited_context {
        builder = builder.hook(Arc::new(
            super::subagent_context::SubagentInheritedContextHook::new(ctx),
        ));
    }
    if let Some(snap) = resume_snapshot {
        builder = builder.resume(snap);
    } else {
        builder = builder.persona(persona);
    }
    if let Some(policy) = tool_loop_policy {
        builder = builder.tool_loop_policy(policy);
    }
    if let Some(max_rounds) = max_rounds {
        builder = builder.max_rounds(max_rounds);
    }
    for mw in child_middlewares(role, scope, &wd, inherited_worker_middlewares) {
        builder = builder.middleware(mw);
    }
    let child = builder.build();
    let handle = tokio::spawn(run_child_to_completion(
        child,
        prompt,
        AutoRespond::AllowAll,
        progress_hook,
    ));
    match handle.await {
        Ok(res) => res,
        Err(join_err) => (
            Outcome {
                stop: StopReason::ProviderError,
                error: Some(format!("subagent task crashed: {join_err}")),
                ..Default::default()
            },
            None,
        ),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SubtaskTelemetry {
    rounds: u32,
    duration_ms: u64,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
}

#[allow(clippy::too_many_arguments)]
fn render_subtask_outcome(
    label: &str,
    agent_id: &str,
    desc: &str,
    model: &str,
    role: SubagentRole,
    outcome: Outcome,
    ledger: &ActionReceiptLedger,
    artifact_store: Option<&ArtifactStore>,
    telemetry: Option<SubtaskTelemetry>,
) -> (String, bool) {
    let is_err = outcome.stop != StopReason::Stopped;
    let produced = if !outcome.text.is_empty() {
        outcome.text
    } else {
        outcome
            .tool_results
            .iter()
            .map(|r| r.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let produced = compact_subtask_output_with_store(&produced, artifact_store);
    let receipt = if role.is_worker() {
        ledger.render_worker_receipt()
    } else {
        ledger.render_explore_receipt()
    };
    let (state, tag, body) = if is_err {
        let mut b = format!("{receipt}\nsubagent stopped early ({:?})", outcome.stop);
        if let Some(e) = &outcome.error {
            b.push_str(&format!(": {e}"));
        }
        match outcome.stop {
            StopReason::MaxRounds => {
                b.push_str("\n[Hint: subagent reached max turn rounds before completing. Consider decomposing into smaller, focused subtasks with narrower scopes.]");
            }
            StopReason::RepeatLoop | StopReason::ToolLoopDetected => {
                b.push_str("\n[Hint: subagent repeated identical tool calls in a loop. Clarify task instructions or provide direct guidance.]");
            }
            StopReason::PolicyDenied => {
                b.push_str("\n[Hint: subagent action violated safety or scope policy (e.g. attempted out-of-scope write or sensitive path). Check declared `scope`.]");
            }
            _ => {}
        }
        if !produced.is_empty() {
            b.push_str(&format!("\n--- partial output ---\n{produced}"));
        }
        ("error", "task_error", b)
    } else {
        let b = if produced.trim().is_empty() {
            receipt
        } else {
            format!("{receipt}\n{produced}")
        };
        ("completed", "task_result", b)
    };
    (
        render_task_block_with_telemetry(
            label, agent_id, desc, model, state, tag, &body, telemetry,
        ),
        is_err,
    )
}

/// Parse the tool args, repairing unescaped control characters on failure (weak
/// models / gateways sometimes emit a raw newline inside a JSON string value, which
/// serde rejects). Repairs ONLY on failure, so valid JSON is never altered. This is
/// the primary repair for a fresh dispatch — the model's tool-call args arrive
/// verbatim (no upstream repair on the inbound path). It CANNOT recover a truncated
/// payload (a large batch hitting the model's output limit) or an unescaped quote;
/// the tool description advises smaller batches to avoid producing one. Shared by
/// `risk` and `execute` so both agree on whether a dispatch contains a `worker` — a
/// mismatch would let a file-editing worker with control-char args skip the approval
/// gate.
fn parse_task_args(args: &str) -> Result<Args, serde_json::Error> {
    let mut value: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => serde_json::from_str(&super::repair::repair_json(args))?,
    };
    super::repair::decode_lenient_array_field(&mut value, "tasks", false);
    if let Some(arr) = value.get_mut("tasks").and_then(|x| x.as_array_mut()) {
        for task in arr {
            super::repair::decode_lenient_array_field(task, "scope", true);
        }
    }
    serde_json::from_value(value)
}

/// A one-line preview of what a child is about to do this round — the tool name plus a
/// concise argument (path / pattern / command / …) when one is present. Best-effort: if the
/// args aren't parseable JSON or carry no recognisable key, just the tool name.
fn summarize_tool_call(call: &ToolCall) -> String {
    const KEYS: &[&str] = &[
        "path",
        "file_path",
        "target_file",
        "filePath",
        "filename",
        "pattern",
        "query",
        "command",
        "cmd",
        "url",
        "description",
        "name",
    ];
    let arg = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|v| {
            KEYS.iter()
                .find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(str::to_string))
        });
    let short = arg
        .as_deref()
        .map(|a| first_line_capped(a, 30))
        .unwrap_or_default();
    if short.is_empty() {
        call.name.clone()
    } else {
        format!("{} {}", call.name, short)
    }
}

/// First line of `s`, trimmed, capped to `max` chars with a trailing ellipsis when it's
/// longer. Char-based (never slices a code point mid-way). Empty first line → empty string.
/// Shared by the tool-call preview and the subtask progress line so the two can't drift.
fn first_line_capped(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        format!(
            "{}\u{2026}",
            first.chars().take(max - 1).collect::<String>()
        )
    } else {
        first.to_string()
    }
}

/// Compact record of in-flight and completed tool actions executed by a subagent.
#[derive(Debug, Default, Clone)]
pub struct ActionReceiptLedger {
    /// In-flight or planned tool calls keyed by call ID.
    calls: BTreeMap<String, ToolCallInfo>,
    /// Chronological list of completed actions.
    completed: Vec<CompletedAction>,
}

#[derive(Debug, Clone)]
struct ToolCallInfo {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone)]
struct CompletedAction {
    name: String,
    arguments: String,
    is_error: bool,
}

impl ActionReceiptLedger {
    pub fn record_call(&mut self, id: &str, name: &str, arguments: &str) {
        self.calls.insert(
            id.to_string(),
            ToolCallInfo {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        );
    }

    pub fn record_tool_started(&mut self, call: &ToolCall) {
        self.record_call(&call.id, &call.name, &call.arguments);
    }

    pub fn record_tool_result(&mut self, result: &ToolResult) {
        let (name, arguments) = if let Some(info) = self.calls.remove(&result.call_id) {
            (info.name, info.arguments)
        } else {
            (String::new(), String::new())
        };
        self.completed.push(CompletedAction {
            name,
            arguments,
            is_error: result.is_error,
        });
    }

    /// Authoritative list of unique files successfully touched/mutated during the run,
    /// in chronological order of first modification.
    pub fn touched_files(&self) -> Vec<String> {
        let mut touched = Vec::new();
        for action in &self.completed {
            if action.is_error {
                continue;
            }
            if let Some(path) = extract_mutation_path(&action.name, &action.arguments) {
                if !touched.contains(&path) {
                    touched.push(path);
                }
            }
        }
        touched
    }

    /// List of unique files whose mutation was attempted but failed, excluding any
    /// that were subsequently successfully touched.
    pub fn failed_mutations(&self) -> Vec<String> {
        let touched = self.touched_files();
        let mut failed = Vec::new();
        for action in &self.completed {
            if !action.is_error {
                continue;
            }
            if let Some(path) = extract_mutation_path(&action.name, &action.arguments) {
                if !touched.contains(&path) && !failed.contains(&path) {
                    failed.push(path);
                }
            }
        }
        failed
    }

    /// Chronological list of shell/system commands executed and their success status.
    pub fn executed_commands(&self) -> Vec<(String, bool)> {
        let mut commands = Vec::new();
        for action in &self.completed {
            if let Some(cmd) = extract_command(&action.name, &action.arguments) {
                commands.push((cmd, !action.is_error));
            }
        }
        commands
    }

    /// Render compact mutation and execution evidence for a worker subagent.
    /// Format: `[Worker touched: <files>, ran: <commands>]`
    pub fn render_worker_receipt(&self) -> String {
        let touched = self.touched_files();
        let failed_mutations = self.failed_mutations();
        let commands = self.executed_commands();

        let touched_str = if !touched.is_empty() {
            let mut list = if touched.len() > 6 {
                let mut shown: Vec<String> = touched.iter().take(6).cloned().collect();
                shown.push(format!("(+{} more)", touched.len() - 6));
                shown.join(", ")
            } else {
                touched.join(", ")
            };
            if !failed_mutations.is_empty() {
                list.push_str(&format!(" (failed: {})", failed_mutations.join(", ")));
            }
            list
        } else if !failed_mutations.is_empty() {
            format!("(none) (failed: {})", failed_mutations.join(", "))
        } else {
            "(none)".to_string()
        };

        let ran_str = if !commands.is_empty() {
            let formatted: Vec<String> = commands
                .iter()
                .map(|(cmd, ok)| format!("{cmd} ({})", if *ok { "ok" } else { "failed" }))
                .collect();
            if formatted.len() > 5 {
                let mut shown: Vec<String> = formatted.iter().take(5).cloned().collect();
                shown.push(format!("(+{} more)", formatted.len() - 5));
                shown.join(", ")
            } else {
                formatted.join(", ")
            }
        } else {
            "(none)".to_string()
        };

        format!("[Worker touched: {touched_str}, ran: {ran_str}]")
    }

    /// Authoritative list of unique files/paths read or queried during the run,
    /// in chronological order of first access.
    pub fn queried_files(&self) -> Vec<String> {
        let mut queried = Vec::new();
        for action in &self.completed {
            if action.is_error {
                continue;
            }
            if let Some(path) = extract_read_path(&action.name, &action.arguments) {
                if !queried.contains(&path) {
                    queried.push(path);
                }
            }
        }
        queried
    }

    /// List of unique files whose read was attempted but failed, excluding any
    /// that were subsequently successfully read.
    pub fn failed_reads(&self) -> Vec<String> {
        let queried = self.queried_files();
        let mut failed = Vec::new();
        for action in &self.completed {
            if !action.is_error {
                continue;
            }
            if let Some(path) = extract_read_path(&action.name, &action.arguments) {
                if !queried.contains(&path) && !failed.contains(&path) {
                    failed.push(path);
                }
            }
        }
        failed
    }

    /// Chronological list of search queries executed (from grep, glob, code_explore, ast_grep)
    /// and their success status.
    pub fn executed_searches(&self) -> Vec<(String, bool)> {
        let mut searches = Vec::new();
        for action in &self.completed {
            if let Some(query) = extract_search_query(&action.name, &action.arguments) {
                searches.push((query, !action.is_error));
            }
        }
        searches
    }

    /// Render compact read and search evidence for an explore subagent.
    /// Format: `[Explore queried: <files>, searched: <queries>]`
    pub fn render_explore_receipt(&self) -> String {
        let queried = self.queried_files();
        let failed_reads = self.failed_reads();
        let searches = self.executed_searches();

        let queried_str = if !queried.is_empty() {
            let mut list = if queried.len() > 6 {
                let mut shown: Vec<String> = queried.iter().take(6).cloned().collect();
                shown.push(format!("(+{} more)", queried.len() - 6));
                shown.join(", ")
            } else {
                queried.join(", ")
            };
            if !failed_reads.is_empty() {
                list.push_str(&format!(" (failed: {})", failed_reads.join(", ")));
            }
            list
        } else if !failed_reads.is_empty() {
            format!("(none) (failed: {})", failed_reads.join(", "))
        } else {
            "(none)".to_string()
        };

        let searched_str = if !searches.is_empty() {
            let formatted: Vec<String> = searches
                .iter()
                .map(|(q, ok)| format!("{q} ({})", if *ok { "ok" } else { "failed" }))
                .collect();
            if formatted.len() > 5 {
                let mut shown: Vec<String> = formatted.iter().take(5).cloned().collect();
                shown.push(format!("(+{} more)", formatted.len() - 5));
                shown.join(", ")
            } else {
                formatted.join(", ")
            }
        } else {
            "(none)".to_string()
        };

        format!("[Explore queried: {queried_str}, searched: {searched_str}]")
    }
}

fn extract_mutation_path(tool: &str, args_json: &str) -> Option<String> {
    if super::is_search_replace_tool(tool) {
        let val = serde_json::from_str::<serde_json::Value>(args_json)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&super::repair::repair_json(args_json))
            })
            .ok()?;
        let raw = val.get("path").and_then(|x| x.as_str())?;
        normalize_receipt_path(raw)
    } else if super::is_file_mutation_tool(tool) {
        let val = serde_json::from_str::<serde_json::Value>(args_json)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&super::repair::repair_json(args_json))
            })
            .ok()?;
        let raw = json_path_arg(&val)?;
        normalize_receipt_path(raw)
    } else {
        None
    }
}

/// The public edit/write field is `path`; accept the same legacy aliases as the tools.
fn json_path_arg(v: &serde_json::Value) -> Option<&str> {
    ["path", "file_path", "target_file", "filePath", "filename"]
        .iter()
        .find_map(|key| v.get(*key).and_then(|x| x.as_str()))
}

fn normalize_receipt_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unified = trimmed.replace('\\', "/");
    let clean = unified.strip_prefix("./").unwrap_or(&unified);
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

fn extract_command(tool: &str, args_json: &str) -> Option<String> {
    if !(super::is_shell_tool_name(tool) || tool.eq_ignore_ascii_case("cmd")) {
        return None;
    }
    let raw = super::extract_shell_command(args_json)
        .or_else(|| super::extract_shell_command(&super::repair::repair_json(args_json)))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        None
    } else {
        Some(first_line_capped(first_line, 40))
    }
}

fn extract_read_path(tool: &str, args_json: &str) -> Option<String> {
    let t = tool.to_ascii_lowercase();
    if t == "read" || t == "read_file" || t == "view" || t == "view_image" {
        let val = serde_json::from_str::<serde_json::Value>(args_json)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&super::repair::repair_json(args_json))
            })
            .ok()?;
        let raw = val
            .get("path")
            .or_else(|| val.get("file_path"))
            .and_then(|x| x.as_str())?;
        normalize_receipt_path(raw)
    } else if t == "code_explore" {
        let val = serde_json::from_str::<serde_json::Value>(args_json)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&super::repair::repair_json(args_json))
            })
            .ok()?;
        let raw = val.get("path").and_then(|x| x.as_str())?;
        let norm = normalize_receipt_path(raw)?;
        // Only count as queried file if it targets a specific file (has a file extension),
        // not a directory or workspace scope like "." or "crates/jeikcode-coding".
        if std::path::Path::new(&norm).extension().is_some() {
            Some(norm)
        } else {
            None
        }
    } else {
        None
    }
}

fn extract_search_query(tool: &str, args_json: &str) -> Option<String> {
    let t = tool.to_ascii_lowercase();
    if t == "grep" || t == "search" || t == "glob" || t == "find_files" || t == "ast_grep" {
        let val = serde_json::from_str::<serde_json::Value>(args_json)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&super::repair::repair_json(args_json))
            })
            .ok()?;
        let raw = val
            .get("pattern")
            .or_else(|| val.get("query"))
            .and_then(|x| x.as_str())?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            let first_line = trimmed.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                None
            } else {
                Some(first_line_capped(first_line, 40))
            }
        }
    } else if t == "code_explore" {
        let val = serde_json::from_str::<serde_json::Value>(args_json)
            .or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&super::repair::repair_json(args_json))
            })
            .ok()?;
        let raw = val.get("query").and_then(|x| x.as_str())?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            let first_line = trimmed.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                None
            } else {
                Some(first_line_capped(first_line, 40))
            }
        }
    } else {
        None
    }
}

/// Max bytes returned from a subtask before head/tail output compaction triggers (~4k tokens).
pub const MAX_SUBTASK_OUTPUT_BYTES: usize = 16_000;
/// Bytes retained from the head of an oversized subtask output.
pub const SUBTASK_PREVIEW_HEAD_BYTES: usize = 3_000;
/// Bytes retained from the tail of an oversized subtask output.
pub const SUBTASK_PREVIEW_TAIL_BYTES: usize = 4_000;

fn head_boundary(s: &str, n: usize) -> usize {
    let mut i = n.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn tail_boundary(s: &str, n: usize) -> usize {
    let mut i = s.len().saturating_sub(n);
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Compact oversized subtask output to protect the parent context window.
/// Preserves UTF-8 char boundaries and keeps head + tail with a clear compaction banner.
pub fn compact_subtask_output(raw: &str) -> String {
    compact_subtask_output_with_store(raw, None)
}

/// Like [`compact_subtask_output`], spilling the full uncompacted output into `store`
/// (`artifact://<id>`) when `raw.len() > MAX_SUBTASK_OUTPUT_BYTES` so omitted middle
/// sections remain 100% recoverable via `read(path="artifact://<id>")`.
pub fn compact_subtask_output_with_store(raw: &str, store: Option<&ArtifactStore>) -> String {
    if raw.len() <= MAX_SUBTASK_OUTPUT_BYTES {
        return raw.to_string();
    }

    let head_end = head_boundary(raw, SUBTASK_PREVIEW_HEAD_BYTES);
    let tail_start = tail_boundary(raw, SUBTASK_PREVIEW_TAIL_BYTES);

    if head_end >= tail_start {
        return raw.to_string();
    }

    let head = &raw[..head_end];
    let tail = &raw[tail_start..];
    let total_bytes = raw.len();
    let omitted_bytes = total_bytes.saturating_sub(head.len() + tail.len());

    let artifact_note = store
        .and_then(|s| s.put(raw.as_bytes()).ok())
        .map(|id| {
            format!(
                " — full output saved to artifact://{id}; call read(path=\"artifact://{id}\", offset=1, limit=200) or read(path=\"artifact://{id}\", keyword=\"<term>\") to inspect"
            )
        })
        .unwrap_or_default();

    format!(
        "{head}\n\n[... subagent output compacted: {total_bytes} bytes total, {omitted_bytes} bytes omitted to protect context window{artifact_note} ...]\n\n{tail}"
    )
}

/// Child-agent observer that funnels live model and tool activity to the parent's
/// marker-prefixed ephemeral progress stream. The TUI projects the latest state
/// into its fixed Subtasks footer without adding transcript rows.
struct SubtaskProgressHook {
    progress: ProgressSink,
    /// The subtask label, e.g. `explore#1` — so the footer shows WHICH child is acting.
    label: String,
    localized_zh: bool,
    /// The child's cancel token. The child is detached from the parent tool future,
    /// so cancellation propagates through this token; gate emits on it so a
    /// non-cooperative child cannot resurrect stale activity after the parent moved on.
    cancel: tokio_util::sync::CancellationToken,
    started_at: std::time::Instant,
    live: Mutex<SubtaskLiveState>,
    ledger: Arc<Mutex<ActionReceiptLedger>>,
    background_handle: Option<Arc<BackgroundTaskHandle>>,
}

#[derive(Default)]
struct SubtaskLiveState {
    activity: String,
    rounds: u32,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    round_chars: usize,
    text_tail: String,
    active_tools: BTreeMap<String, String>,
    last_emit: Option<std::time::Instant>,
}

impl SubtaskProgressHook {
    #[cfg(test)]
    fn new(
        progress: ProgressSink,
        label: String,
        localized_zh: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self::with_ledger(
            progress,
            label,
            localized_zh,
            cancel,
            Arc::new(Mutex::new(ActionReceiptLedger::default())),
        )
    }

    fn with_ledger(
        progress: ProgressSink,
        label: String,
        localized_zh: bool,
        cancel: tokio_util::sync::CancellationToken,
        ledger: Arc<Mutex<ActionReceiptLedger>>,
    ) -> Self {
        Self {
            progress,
            label,
            localized_zh,
            cancel,
            started_at: std::time::Instant::now(),
            live: Mutex::new(SubtaskLiveState::default()),
            ledger,
            background_handle: None,
        }
    }

    fn snapshot_telemetry(&self) -> SubtaskTelemetry {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        let Ok(live) = self.live.lock() else {
            return SubtaskTelemetry {
                duration_ms: elapsed_ms,
                ..Default::default()
            };
        };
        SubtaskTelemetry {
            rounds: live.rounds,
            duration_ms: elapsed_ms,
            input_tokens: live.input_tokens,
            cached_tokens: live.cached_tokens,
            output_tokens: live.output_tokens.max(live.total_tokens),
        }
    }

    fn with_background_handle(mut self, handle: Arc<BackgroundTaskHandle>) -> Self {
        self.background_handle = Some(handle);
        self
    }

    fn thinking_label(&self) -> &'static str {
        if self.localized_zh {
            "正在分析任务"
        } else {
            "analyzing task"
        }
    }

    fn running_tool_label(&self, tool: &str) -> String {
        if self.localized_zh {
            format!("正在执行 {tool}")
        } else {
            format!("running {tool}")
        }
    }

    fn preparing_tool_label(&self, tool: &str) -> String {
        if self.localized_zh {
            format!("准备执行 {tool}")
        } else {
            format!("preparing {tool}")
        }
    }

    fn finished_tool_label(&self, tool: &str) -> String {
        if self.localized_zh {
            format!("已完成 {tool}，正在分析结果")
        } else {
            format!("finished {tool}; analyzing results")
        }
    }

    fn tool_started(&self, call: &ToolCall) {
        let summary = summarize_tool_call(call);
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            live.active_tools.insert(call.id.clone(), summary.clone());
            if live.active_tools.len() == 1 {
                self.running_tool_label(&summary)
            } else if self.localized_zh {
                format!("正在并行执行 {} 个工具：{summary}", live.active_tools.len())
            } else {
                format!(
                    "running {} tools in parallel: {summary}",
                    live.active_tools.len()
                )
            }
        };
        self.publish(Some(activity), true);
    }

    fn tool_finished(&self, result: &ToolResult) {
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let Some(summary) = live.active_tools.remove(&result.call_id) else {
                return;
            };
            if live.active_tools.is_empty() {
                self.finished_tool_label(&summary)
            } else if self.localized_zh {
                format!(
                    "已完成 {summary}；仍有 {} 个工具运行",
                    live.active_tools.len()
                )
            } else {
                format!(
                    "finished {summary}; {} tool(s) still running",
                    live.active_tools.len()
                )
            }
        };
        self.publish(Some(activity), true);
    }

    fn publish(&self, activity: Option<String>, force: bool) {
        if self.cancel.is_cancelled() {
            return;
        }
        let now = std::time::Instant::now();
        let message = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            if let Some(activity) = activity.filter(|activity| !activity.is_empty()) {
                live.activity = first_line_capped(&activity.replace(" \u{b7} ", " "), 88);
            }
            if live.activity.is_empty() {
                live.activity = self.thinking_label().to_string();
            }
            if !force
                && live.last_emit.is_some_and(|last| {
                    now.duration_since(last) < std::time::Duration::from_millis(350)
                })
            {
                return;
            }
            live.last_emit = Some(now);
            let estimated = (live.round_chars / 4) as u64;
            let tokens = live.total_tokens.saturating_add(estimated);
            if let Some(bg) = &self.background_handle {
                bg.set_activity(format!("{} · tokens={}", live.activity, tokens));
            }
            format!(
                "{SUBAGENT_ACTIVITY_MARKER}{} \u{b7} {} \u{b7} tokens={}",
                self.label, live.activity, tokens
            )
        };
        self.progress.emit(message);
    }

    fn observe_delta(&self, delta: &str, semantic: bool) {
        if self.cancel.is_cancelled() || delta.is_empty() {
            return;
        }
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            live.round_chars = live.round_chars.saturating_add(delta.chars().count());
            if semantic {
                live.text_tail.push_str(delta);
                if live.text_tail.len() > 512 {
                    let keep_from = live
                        .text_tail
                        .char_indices()
                        .rev()
                        .take_while(|(idx, _)| live.text_tail.len().saturating_sub(*idx) <= 512)
                        .last()
                        .map(|(idx, _)| idx)
                        .unwrap_or(0);
                    live.text_tail.drain(..keep_from);
                }
                readable_progress_tail(&live.text_tail)
            } else {
                None
            }
        };
        self.publish(activity, false);
    }

    fn finish_round(&self, response: &Message) {
        let activity = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let estimated = (live.round_chars / 4) as u64;
            live.rounds = live.rounds.saturating_add(1);
            if let Some(meta) = response.meta.as_ref() {
                live.input_tokens = live.input_tokens.saturating_add(meta.tokens.prompt as u64);
                live.cached_tokens = live.cached_tokens.saturating_add(meta.tokens.cached as u64);
                live.output_tokens = live
                    .output_tokens
                    .saturating_add((meta.tokens.completion as u64).max(estimated));
            } else {
                live.output_tokens = live.output_tokens.saturating_add(estimated);
            }
            let reported = response
                .meta
                .as_ref()
                .map(|meta| meta.tokens.completion as u64)
                .unwrap_or(0);
            live.total_tokens = live.total_tokens.saturating_add(reported.max(estimated));
            live.round_chars = 0;
            let semantic = readable_progress_tail(&response.text)
                .or_else(|| readable_progress_tail(&live.text_tail));
            live.text_tail.clear();
            semantic.or_else(|| {
                response
                    .tool_calls
                    .first()
                    .map(|call| self.preparing_tool_label(&summarize_tool_call(call)))
            })
        };
        self.publish(activity, true);
    }
}

#[async_trait]
impl LifecycleHooks for SubtaskProgressHook {
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        if self.cancel.is_cancelled() {
            return;
        }
        self.publish(None, true);
    }

    async fn on_text_delta(&self, delta: &mut String) {
        self.observe_delta(delta, true);
    }

    async fn on_reasoning_delta(&self, delta: &mut String) {
        self.observe_delta(delta, false);
    }

    async fn on_model_response(&self, response: &mut Message) {
        if self.cancel.is_cancelled() {
            return;
        }
        if let Ok(mut l) = self.ledger.lock() {
            for call in &response.tool_calls {
                l.record_call(&call.id, &call.name, &call.arguments);
            }
        }
        self.finish_round(response);
    }
}

fn readable_progress_tail(text: &str) -> Option<String> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let clean = line
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    (!clean.is_empty()).then(|| first_line_capped(&clean, 88))
}

/// One-shot child driver with the same aggregation/failure semantics as
/// `Agent::run_to_completion`, plus truthful execution-boundary progress. Tool
/// middleware `before` is a classification seam and may run for a whole batch
/// before any tool starts, so it cannot own user-facing "running" state.
async fn run_child_to_completion(
    child: Agent,
    input: String,
    policy: AutoRespond,
    progress: Arc<SubtaskProgressHook>,
) -> (Outcome, Option<SessionSnapshot>) {
    let mut handle = child.spawn();
    let _ = handle.commands.send(AgentCommand::SendMessage {
        text: input,
        images: vec![],
    });
    let mut outcome = Outcome::default();
    let mut snapshot = None;
    let mut requested_snapshot = false;
    let ledger = progress.ledger.clone();
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::TextDelta(text) => outcome.text.push_str(&text),
            AgentEvent::ToolBatchStarted { calls, .. } => {
                if let Ok(mut l) = ledger.lock() {
                    for c in &calls {
                        l.record_call(&c.id, &c.name, &c.arguments);
                    }
                }
            }
            AgentEvent::ToolStarted { call } => {
                progress.tool_started(&call);
                if let Ok(mut l) = ledger.lock() {
                    l.record_tool_started(&call);
                }
            }
            AgentEvent::ToolResult { result } => {
                progress.tool_finished(&result);
                if let Ok(mut l) = ledger.lock() {
                    l.record_tool_result(&result);
                }
                outcome.tool_results.push(result);
            }
            AgentEvent::Request {
                id,
                kind: _,
                payload: _,
            } => {
                let value = match policy {
                    AutoRespond::AllowAll => serde_json::json!({ "decision": "allow" }),
                    AutoRespond::DenyAll => serde_json::json!({ "decision": "deny" }),
                };
                let _ = handle.commands.send(AgentCommand::Respond { id, value });
            }
            AgentEvent::Error {
                message,
                http_status,
                code,
            } => {
                outcome.error = Some(message);
                outcome.http_status = http_status;
                outcome.error_code = code;
            }
            AgentEvent::TurnComplete { reason } => {
                outcome.stop = reason;
                if !requested_snapshot {
                    requested_snapshot = true;
                    if handle.commands.send(AgentCommand::Snapshot).is_err() {
                        let _ = handle.commands.send(AgentCommand::Shutdown);
                        break;
                    }
                }
            }
            AgentEvent::Snapshot { snapshot: snap } => {
                snapshot = Some(snap);
                let _ = handle.commands.send(AgentCommand::Shutdown);
                break;
            }
            _ => {}
        }
    }
    let _ = handle.task.await;
    (outcome, snapshot)
}

/// A live-progress line for one subtask: `<head> · <model> · <desc>`. `head` is the
/// already-composed icon+label (`↻ explore#1`, `✓ done · explore#1`, …) so callers keep
/// their own icon/label separator. The description is compacted to its first line,
/// trimmed and length-capped, so a long prompt-like description can't wrap the strip.
/// Emitted on start and completion so the user sees WHICH job each subtask is.
fn subtask_progress_line(head: &str, model: &str, desc: &str) -> String {
    let snippet = first_line_capped(desc, 48);
    if snippet.is_empty() {
        format!("{head} \u{b7} {model}")
    } else {
        format!("{head} \u{b7} {model} \u{b7} {snippet}")
    }
}

/// Wrap a child-agent result in an opencode-style `<task>` block. `model` is the
/// model the subagent actually ran on (surfaced so the user can see which tier/model
/// executed — the strong/weak routing proof), and `agent_id` enables `resume_agent_id`.
fn render_task_block(
    id: &str,
    agent_id: &str,
    summary: &str,
    model: &str,
    state: &str,
    tag: &str,
    body: &str,
) -> String {
    render_task_block_with_telemetry(id, agent_id, summary, model, state, tag, body, None)
}

#[allow(clippy::too_many_arguments)]
fn render_task_block_with_telemetry(
    id: &str,
    agent_id: &str,
    summary: &str,
    model: &str,
    state: &str,
    tag: &str,
    body: &str,
    telemetry: Option<SubtaskTelemetry>,
) -> String {
    let telem_attrs = match telemetry {
        Some(t) => format!(
            " rounds=\"{}\" duration_ms=\"{}\" input_tokens=\"{}\" cached_tokens=\"{}\" output_tokens=\"{}\"",
            t.rounds, t.duration_ms, t.input_tokens, t.cached_tokens, t.output_tokens
        ),
        None => String::new(),
    };
    format!(
        "<task id=\"{id}\" agent_id=\"{agent_id}\" model=\"{model}\"{telem_attrs} state=\"{state}\">\n<summary>{summary}</summary>\n<{tag}>\n{body}\n</{tag}>\n</task>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;
    use jeikcode_kernel::message::Message;
    use jeikcode_kernel::provider::ChatOptions;
    use jeikcode_kernel::stream::{ProviderError, StreamEvent};
    use jeikcode_kernel::tool::{ProgressSink, ToolDef, ToolRegistry};
    use tokio_util::sync::CancellationToken;

    /// Scripted provider: `Some(reply)` → one text turn then clean stop;
    /// `None` → a terminal open error (simulates a failed child).
    struct MockProvider {
        reply: Option<String>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn model_name(&self) -> &str {
            "mock"
        }
        async fn chat_stream(
            &self,
            _m: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            match &self.reply {
                Some(text) => {
                    let evs = vec![
                        StreamEvent::TextDelta(text.clone()),
                        StreamEvent::Done { truncated: false },
                    ];
                    Ok(stream::iter(evs).boxed())
                }
                None => Err(ProviderError {
                    retryable: false,
                    message: "mock open failure".into(),
                    ..Default::default()
                }),
            }
        }
    }

    fn ctx() -> ToolContext {
        // Dedicated EMPTY tempdir — shared std::env::temp_dir() can contain stray
        // build markers that confuse any build-detection logic in child agents.
        let dir = tempfile::tempdir().expect("tempdir").keep();
        ToolContext {
            working_dir: dir,
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }

    fn dummy() -> TaskTool {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        TaskTool::new(
            || unreachable!("provider not built in these tests"),
            || unreachable!("provider not built in these tests"),
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
    }

    #[test]
    fn name_is_task() {
        assert_eq!(dummy().name(), "task");
    }

    #[test]
    fn child_round_limit_is_configurable_and_zero_means_unbounded() {
        assert_eq!(dummy().max_rounds, Some(230));
        assert_eq!(dummy().with_max_rounds(500).max_rounds, Some(500));
        assert_eq!(dummy().with_max_rounds(0).max_rounds, None);
    }

    #[test]
    fn child_exact_loop_policy_can_be_inherited_or_disabled() {
        assert_eq!(dummy().tool_loop_policy, Some(ToolLoopPolicy::default()));
        assert_eq!(dummy().with_tool_loop_policy(None).tool_loop_policy, None);
        let custom = ToolLoopPolicy::new(10, 12).unwrap();
        assert_eq!(
            dummy().with_tool_loop_policy(Some(custom)).tool_loop_policy,
            Some(custom)
        );
    }

    #[test]
    fn summarize_tool_call_picks_concise_arg() {
        let mk = |name: &str, args: &str| ToolCall {
            id: "x".into(),
            name: name.into(),
            arguments: args.into(),
        };
        // Recognised key → "name arg".
        assert_eq!(
            summarize_tool_call(&mk("read_file", r#"{"path":"src/auth.rs"}"#)),
            "read_file src/auth.rs"
        );
        assert_eq!(
            summarize_tool_call(&mk("grep", r#"{"pattern":"unwrap("}"#)),
            "grep unwrap("
        );
        // Long arg → truncated with ellipsis.
        let long = summarize_tool_call(&mk(
            "bash",
            r#"{"command":"cargo test --workspace --all-features --verbose now"}"#,
        ));
        assert!(long.starts_with("bash "), "{long}");
        assert!(long.ends_with('\u{2026}'), "{long}");
        // No recognised key / bad JSON → just the tool name.
        assert_eq!(
            summarize_tool_call(&mk("todo_write", r#"{"todos":[]}"#)),
            "todo_write"
        );
        assert_eq!(summarize_tool_call(&mk("weird", "not json")), "weird");
    }

    #[tokio::test]
    async fn subtask_hook_marks_activity_no_double_ellipsis_and_respects_cancel() {
        use std::sync::Mutex;
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let c = captured.clone();
            ProgressSink::new(Arc::new(move |m: String| c.lock().unwrap().push(m)))
        };
        let cancel = CancellationToken::new();
        let hook = SubtaskProgressHook::new(sink, "explore#1".into(), false, cancel.clone());
        let ctx = TurnCtx {
            session_id: None,
            turn_id: 1,
            request_id: 1,
            round: 1,
            max_rounds: None,
            cache_epoch: 0,
            context_window: 0,
            used_tokens: 0,
        };

        hook.pre_request(&mut Vec::new(), &ctx).await;
        let mut msg = Message::assistant(
            String::new(),
            vec![ToolCall {
                id: "x".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            }],
        );
        hook.on_model_response(&mut msg).await;
        {
            let c = captured.lock().unwrap();
            assert_eq!(c.len(), 2, "expected thinking + tool lines: {c:?}");
            assert!(
                c[0].starts_with(SUBAGENT_ACTIVITY_MARKER),
                "marker-prefixed: {:?}",
                c[0]
            );
            assert!(c[0].contains("analyzing task"), "thinking line: {:?}", c[0]);
            assert!(c[0].contains("tokens=0"), "token line: {:?}", c[0]);
            assert!(
                c[1].contains("preparing read_file a.rs"),
                "tool line: {:?}",
                c[1]
            );
        }

        // A detached child cancelled by its parent must emit nothing further.
        cancel.cancel();
        hook.pre_request(&mut Vec::new(), &ctx).await;
        hook.on_model_response(&mut msg).await;
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "cancelled hook must stay silent"
        );
    }

    #[tokio::test]
    async fn subtask_hook_reports_semantic_progress_and_monotonic_tokens() {
        use jeikcode_kernel::message::MessageMeta;
        use jeikcode_kernel::stream::TokenUsage;
        use std::sync::Mutex;

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            ProgressSink::new(Arc::new(move |message| {
                captured.lock().unwrap().push(message)
            }))
        };
        let hook =
            SubtaskProgressHook::new(sink, "explore#1".into(), true, CancellationToken::new());
        let mut response =
            Message::assistant("已定位命令注册入口，正在核对补全与权限机制", Vec::new());
        response.meta = Some(MessageMeta {
            tokens: TokenUsage {
                prompt: 800,
                completion: 128,
                cached: 700,
            },
            ..MessageMeta::default()
        });

        hook.on_model_response(&mut response).await;
        hook.observe_delta("abcdefghijabcdefghijabcdefghijabcdefghij", true);
        let mut second = Message::assistant("继续核对补全脚本", Vec::new());
        second.meta = Some(MessageMeta {
            tokens: TokenUsage {
                completion: 5,
                ..TokenUsage::default()
            },
            ..MessageMeta::default()
        });
        hook.on_model_response(&mut second).await;

        let captured = captured.lock().unwrap();
        assert!(captured.iter().any(|line| {
            line.contains("已定位命令注册入口，正在核对补全与权限机制")
                && line.contains("tokens=128")
        }));
        let latest = captured.last().expect("second-round progress");
        assert!(latest.contains("继续核对补全脚本"));
        assert!(latest.contains("tokens=138"), "{latest}");
    }

    #[test]
    fn subtask_hook_tracks_parallel_tools_by_call_id() {
        use std::sync::Mutex;

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            ProgressSink::new(Arc::new(move |message| {
                captured.lock().unwrap().push(message)
            }))
        };
        let hook =
            SubtaskProgressHook::new(sink, "explore#1".into(), true, CancellationToken::new());
        let read = ToolCall {
            id: "read-1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        };
        let grep = ToolCall {
            id: "grep-1".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"TODO"}"#.into(),
        };

        hook.tool_started(&read);
        hook.tool_started(&grep);
        hook.tool_finished(&ToolResult {
            call_id: read.id.clone(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        });
        hook.tool_finished(&ToolResult {
            call_id: grep.id.clone(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        });

        let captured = captured.lock().unwrap();
        assert!(captured[1].contains("正在并行执行 2 个工具"));
        assert!(captured[2].contains("已完成 read_file a.rs"));
        assert!(captured[2].contains("仍有 1 个工具运行"));
        assert!(captured[3].contains("已完成 grep TODO"));
    }

    #[test]
    fn subtask_progress_line_includes_desc_and_truncates() {
        // Short description → shown verbatim after the model (start-line head style).
        assert_eq!(
            subtask_progress_line("\u{21bb} explore#1", "deepseek", "review auth.rs"),
            "\u{21bb} explore#1 \u{b7} deepseek \u{b7} review auth.rs"
        );
        // Multi-line / long description → first line only, capped with an ellipsis.
        let long = "audit every unwrap() call across the whole crate for panic safety and report\nsecond line";
        let line = subtask_progress_line("\u{2713} done \u{b7} worker#2", "GLM-5.2", long);
        assert!(line.starts_with("\u{2713} done \u{b7} worker#2 \u{b7} GLM-5.2 \u{b7} "));
        assert!(
            line.ends_with('\u{2026}'),
            "long desc must be ellipsized: {line}"
        );
        assert!(
            !line.contains("second line"),
            "only first line should show: {line}"
        );
        // Empty description → no trailing separator after the model.
        assert_eq!(
            subtask_progress_line("\u{21bb} explore#1", "deepseek", "  "),
            "\u{21bb} explore#1 \u{b7} deepseek"
        );
    }

    #[test]
    fn worker_dispatch_is_risky_explore_is_safe() {
        let t = dummy();
        let worker = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"worker"}]}"#;
        let explore = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        assert!(matches!(t.risk(worker), RiskLevel::Risky));
        assert!(matches!(t.risk(explore), RiskLevel::Safe));
    }

    #[tokio::test]
    async fn explore_task_returns_task_result() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("FOUND: the answer is 42".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("FOUND: the answer is 42".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"find","prompt":"where is X","subagent_type":"explore","difficulty":"simple"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert!(
            out.content.contains("<task_result>"),
            "missing tag: {}",
            out.content
        );
        assert!(
            out.content.contains("FOUND: the answer is 42"),
            "missing reply: {}",
            out.content
        );
        assert!(
            out.content.contains("state=\"completed\""),
            "missing state: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn failed_child_returns_task_error() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(MockProvider { reply: None }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: None }) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(out.is_error, "expected error result, got: {}", out.content);
        assert!(
            out.content.contains("<task_error>"),
            "missing tag: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn parent_cancel_terminates_an_unbounded_subtask() {
        struct HangingProvider {
            opened: Arc<tokio::sync::Notify>,
        }

        #[async_trait]
        impl LlmProvider for HangingProvider {
            fn model_name(&self) -> &str {
                "hanging"
            }

            async fn chat_stream(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _o: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                self.opened.notify_one();
                Ok(stream::pending::<StreamEvent>().boxed())
            }
        }

        let opened = Arc::new(tokio::sync::Notify::new());
        let make_provider = {
            let opened = opened.clone();
            move || {
                Arc::new(HangingProvider {
                    opened: opened.clone(),
                }) as Arc<dyn LlmProvider>
            }
        };
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            make_provider.clone(),
            make_provider,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let context = ctx();
        let cancel = context.cancel.clone();
        let run = tokio::spawn(async move {
            tool.execute(
                r#"{"tasks":[{"description":"wait","prompt":"p","subagent_type":"explore"}]}"#,
                &context,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), opened.notified())
            .await
            .expect("child provider must start");
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("parent cancellation must terminate the task tool")
            .expect("task tool join");

        assert!(result.is_error, "cancelled only child must fail the batch");
        assert!(
            result.content.contains("Cancelled"),
            "cancel cause must remain visible: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn partial_batch_failure_is_not_overall_error() {
        // 2 subtasks, one succeeds + one fails ⇒ overall is_error=false (survivors are
        // actionable), but both a <task_result> and a <task_error> appear (#5).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let mk = {
            let calls = calls.clone();
            move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let reply = if n == 0 {
                    Some("did it".to_string())
                } else {
                    None
                };
                Arc::new(MockProvider { reply }) as Arc<dyn LlmProvider>
            }
        };
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(mk.clone(), mk, move || r1.mount(&[]), move || r2.mount(&[]));
        let args = r#"{"tasks":[{"description":"a","prompt":"p","subagent_type":"explore"},{"description":"b","prompt":"q","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(
            !out.is_error,
            "partial failure must not be overall error: {}",
            out.content
        );
        assert!(
            out.content.contains("<task_result>"),
            "missing success block: {}",
            out.content
        );
        assert!(
            out.content.contains("<task_error>"),
            "missing failure block: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn task_block_carries_provider_model() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("done".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("done".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        // The block surfaces the actual model the subagent ran on (MockProvider::model_name).
        assert!(
            out.content.contains("model=\"mock\""),
            "missing model attr: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn control_char_in_args_is_repaired() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("ok".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("ok".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        // A RAW newline (0x0A) inside the `prompt` string value — serde rejects this
        // outright ("control character found"); the try-then-repair path must recover it.
        let args = "{\"tasks\":[{\"description\":\"d\",\"prompt\":\"line1\nline2\",\"subagent_type\":\"explore\"}]}";
        assert!(
            serde_json::from_str::<serde_json::Value>(args).is_err(),
            "test premise: raw control char must be invalid JSON"
        );
        let out = tool.execute(args, &ctx()).await;
        assert!(
            !out.content.contains("invalid task args"),
            "repair should have recovered the args, got: {}",
            out.content
        );
        assert!(
            out.content.contains("<task_result>"),
            "expected a result: {}",
            out.content
        );
    }

    #[test]
    fn worker_with_control_char_args_still_risky() {
        // A worker dispatch whose args carry a raw control char must NOT be downgraded
        // to Safe (which would skip the approval gate while execute() repairs + spawns).
        let worker = "{\"tasks\":[{\"description\":\"d\",\"prompt\":\"a\nb\",\"subagent_type\":\"worker\"}]}";
        assert!(
            serde_json::from_str::<serde_json::Value>(worker).is_err(),
            "test premise: raw control char must be invalid JSON"
        );
        assert!(matches!(dummy().risk(worker), RiskLevel::Risky));
    }

    #[test]
    fn recursive_dir_prefix_only_grants_roots_for_recursive_scopes() {
        use super::recursive_dir_prefix as p;
        // Recursive dir globs grant a search_replace root at their literal dir.
        assert_eq!(p("src/auth/**"), Some("src/auth".into()));
        assert_eq!(p("**"), Some(String::new())); // whole tree
                                                  // Non-recursive scopes cover only specific files → NO search_replace root.
        assert_eq!(p("src/**/x.rs"), None); // matches only x.rs files, not whole dirs
        assert_eq!(p("src/*.rs"), None);
        assert_eq!(p("*.rs"), None);
        assert_eq!(p("Cargo.toml"), None);
        assert_eq!(p("src/auth"), None); // bare dir matches only itself, not its contents
        assert_eq!(p("src/*/**"), None); // non-literal prefix before /** → not granted
    }

    #[test]
    fn worker_scope_gate_confines_writes_but_not_reads() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new(
            &["src/auth/**".into(), "Cargo.toml".into()],
            Path::new("/w"),
        );

        // in-scope write → allowed (supports both modern "edit"/"write" with "path"
        // and legacy "edit_file"/"write_file" with "file_path" or "path")
        assert!(g
            .violation("edit_file", r#"{"file_path":"src/auth/login.rs"}"#)
            .is_none());
        assert!(g
            .violation("edit", r#"{"path":"src/auth/login.rs"}"#)
            .is_none());
        // in-scope NEW file (need not exist) → allowed
        assert!(g
            .violation("write_file", r#"{"file_path":"src/auth/new_mod.rs"}"#)
            .is_none());
        assert!(g
            .violation("write", r#"{"path":"src/auth/new_mod.rs"}"#)
            .is_none());
        // exact-file scope → allowed
        assert!(g
            .violation("write_file", r#"{"file_path":"Cargo.toml"}"#)
            .is_none());
        assert!(g.violation("write", r#"{"path":"Cargo.toml"}"#).is_none());
        // out-of-scope write → denied, message names the path + scope
        let deny = g
            .violation("edit_file", r#"{"file_path":"src/db/schema.rs"}"#)
            .expect("out-of-scope write denied");
        assert!(deny.contains("src/db/schema.rs"), "{deny}");
        assert!(deny.contains("src/auth/**"), "{deny}");
        let deny_modern = g
            .violation("edit", r#"{"path":"src/db/schema.rs"}"#)
            .expect("out-of-scope modern edit denied");
        assert!(deny_modern.contains("src/db/schema.rs"), "{deny_modern}");
        assert!(deny_modern.contains("src/auth/**"), "{deny_modern}");
        // READS are never gated, even outside scope
        assert!(g
            .violation("read_file", r#"{"file_path":"src/db/schema.rs"}"#)
            .is_none());
        assert!(g
            .violation("read", r#"{"path":"src/db/schema.rs"}"#)
            .is_none());
        assert!(g
            .violation("grep", r#"{"pattern":"x","path":"src/db"}"#)
            .is_none());
        // bash is never gated (dispatch-trust; design §6)
        assert!(g
            .violation("bash", r#"{"command":"rm -rf src/db"}"#)
            .is_none());
        // write with no usable path fails CLOSED (denied), not allowed through
        assert!(g.violation("write_file", r#"{"content":"x"}"#).is_some());
        assert!(g.violation("write", r#"{"content":"x"}"#).is_some());
        assert!(g.violation("edit_file", r#"{"file_path":null}"#).is_some());
        assert!(g.violation("edit", r#"{"path":null}"#).is_some());
    }

    #[test]
    fn worker_scope_gate_denies_workspace_escape_and_absolute_outside() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new(&["**".into()], Path::new("/workspace"));
        // `**` allows anything INSIDE the workspace
        assert!(g
            .violation("write_file", r#"{"file_path":"anything/here.rs"}"#)
            .is_none());
        assert!(g
            .violation("write", r#"{"path":"anything/here.rs"}"#)
            .is_none());
        // ...but a `..` escape is denied even under `**`
        assert!(g
            .violation("write_file", r#"{"file_path":"../outside.rs"}"#)
            .is_some());
        assert!(g
            .violation("write", r#"{"path":"../outside.rs"}"#)
            .is_some());
        assert!(g.violation("edit", r#"{"path":"../outside.rs"}"#).is_some());
        // ...and an absolute path outside the working dir is denied
        assert!(g
            .violation("write_file", r#"{"file_path":"/etc/passwd"}"#)
            .is_some());
        assert!(g.violation("write", r#"{"path":"/etc/passwd"}"#).is_some());
        assert!(g.violation("edit", r#"{"path":"/etc/passwd"}"#).is_some());
        // an absolute path INSIDE the working dir is normalized + allowed
        assert!(g
            .violation("write_file", r#"{"file_path":"/workspace/in.rs"}"#)
            .is_none());
        assert!(g
            .violation("write", r#"{"path":"/workspace/in.rs"}"#)
            .is_none());
    }

    #[test]
    fn worker_scope_gate_confines_search_replace_root() {
        use super::WorkerScopeGate;
        use std::path::Path;
        let g = WorkerScopeGate::new(&["src/auth/**".into()], Path::new("/w"));
        // root inside scope dir → allowed
        assert!(g
            .violation("search_replace", r#"{"path":"src/auth"}"#)
            .is_none());
        assert!(g
            .violation("search_replace", r#"{"path":"src/auth/sub"}"#)
            .is_none());
        // root outside scope → denied
        assert!(g
            .violation("search_replace", r#"{"path":"src/db"}"#)
            .is_some());
        // NO path (whole-tree rewrite) → denied
        let deny = g
            .violation("search_replace", r#"{"pattern":"x","replacement":"y"}"#)
            .expect("whole-tree search_replace denied");
        assert!(
            deny.contains("whole tree") || deny.contains("path"),
            "{deny}"
        );
        // root escaping the workspace → denied
        assert!(g
            .violation("search_replace", r#"{"path":"../outside"}"#)
            .is_some());

        // Regression: a NON-recursive glob scope must NOT grant a wide search_replace root.
        // `["*.rs"]` (root-level .rs files) must not let search_replace rewrite the whole tree,
        // and `["src/*.rs"]` must not let it rewrite all of src/.
        let g_root = WorkerScopeGate::new(&["*.rs".into()], Path::new("/w"));
        assert!(
            g_root
                .violation("search_replace", r#"{"path":"src/db"}"#)
                .is_some(),
            "*.rs scope must not grant a search_replace root under src/"
        );
        assert!(
            g_root
                .violation("search_replace", r#"{"path":"."}"#)
                .is_some(),
            "*.rs scope must not grant a whole-tree search_replace root"
        );
        let g_srcrs = WorkerScopeGate::new(&["src/*.rs".into()], Path::new("/w"));
        assert!(
            g_srcrs
                .violation("search_replace", r#"{"path":"src/db"}"#)
                .is_some(),
            "src/*.rs scope must not grant a search_replace root over src/db"
        );
        // ...but a single-file write still matches the file glob (unchanged).
        assert!(g_srcrs
            .violation("edit_file", r#"{"file_path":"src/main.rs"}"#)
            .is_none());
    }

    #[test]
    fn workers_missing_scope_flags_scopeless_workers_only() {
        use super::{workers_missing_scope, SubTask};
        let mk = |ty: &str, scope: Vec<&str>| SubTask {
            description: "d".into(),
            prompt: "p".into(),
            subagent_type: ty.into(),
            difficulty: None,
            scope: scope.into_iter().map(String::from).collect(),
            resume_agent_id: None,
            run_in_background: false,
            inherit_context: false,
            max_rounds: None,
        };
        let tasks = vec![
            mk("worker", vec!["src/a/**"]), // #1 ok
            mk("explore", vec![]),          // #2 explore — ignored even with no scope
            mk("worker", vec![]),           // #3 missing → flagged
            mk("worker", vec!["   "]),      // #4 whitespace-only → flagged
        ];
        assert_eq!(workers_missing_scope(&tasks), vec![3, 4]);
    }

    #[test]
    fn child_middlewares_add_the_scope_gate_only_for_workers() {
        use super::{child_middlewares, DenySensitivePaths, SubagentRole};
        use std::path::Path;
        let base = 2; // DenySensitivePaths + DenyOfflineNetwork.
        assert_eq!(
            child_middlewares(SubagentRole::Explore, &[], Path::new("/w"), &[]).len(),
            base
        );
        assert_eq!(
            child_middlewares(
                SubagentRole::Worker,
                &["src/**".into()],
                Path::new("/w"),
                &[]
            )
            .len(),
            base + 1
        );
        assert_eq!(
            child_middlewares(SubagentRole::Verifier, &[], Path::new("/w"), &[]).len(),
            base + 1,
            "verifier receives VerifierReadOnlyShellGuard"
        );
        let inherited: Vec<Arc<dyn ToolMiddleware>> = vec![Arc::new(DenySensitivePaths::default())];
        assert_eq!(
            child_middlewares(SubagentRole::Explore, &[], Path::new("/w"), &inherited).len(),
            base,
            "read-only explore children do not need worker execution policy"
        );
        assert_eq!(
            child_middlewares(
                SubagentRole::Worker,
                &["src/**".into()],
                Path::new("/w"),
                &inherited,
            )
            .len(),
            base + 2,
            "worker receives inherited policy plus its scope gate"
        );
    }

    #[tokio::test]
    async fn child_sensitive_path_denial_is_terminal() {
        let gate = DenySensitivePaths::default();
        let tool: Arc<dyn Tool> = Arc::new(super::super::BashTool);
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(events, None);
        let mut call = ToolCall {
            id: "sensitive-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "cat .env" }).to_string(),
        };

        assert!(matches!(
            gate.before(&mut call, &tool, &rt).await,
            BeforeOutcome::DenyTurn { .. }
        ));
    }

    #[test]
    fn subtask_lenient_parsing_and_case_insensitivity() {
        use super::{parse_task_args, workers_missing_scope, RiskLevel, TaskTool};

        // 1. Case-insensitivity in worker scope validation
        let mk = |ty: &str, scope: Vec<&str>| super::SubTask {
            description: "d".into(),
            prompt: "p".into(),
            subagent_type: ty.into(),
            difficulty: None,
            scope: scope.into_iter().map(String::from).collect(),
            resume_agent_id: None,
            run_in_background: false,
            inherit_context: false,
            max_rounds: None,
        };
        let tasks = vec![
            mk("Worker", vec!["src/**"]), // PascalCase ok
            mk("WORKER", vec![]),         // UPPERCASE missing scope -> flagged
            mk(" worker ", vec!["  "]),   // padded missing scope -> flagged
            mk("Explore", vec![]),        // explore -> ignored
        ];
        assert_eq!(workers_missing_scope(&tasks), vec![2, 3]);

        // 2. Field aliases: "type", "agent_type", "role", "desc", "tier"
        let json_aliases = r#"{
            "tasks": [
                {
                    "name": "worker-task",
                    "prompt": "fix login auth\nin module",
                    "type": "Worker",
                    "tier": "Hard",
                    "scope": ["src/auth/**"]
                },
                {
                    "desc": "explore-task",
                    "prompt": "check database",
                    "agent_type": "explore",
                    "level": "simple"
                },
                {
                    "prompt": "inspect router\nsecond line",
                    "role": "explore"
                }
            ]
        }"#;

        let parsed = parse_task_args(json_aliases).expect("should parse aliases leniently");
        assert_eq!(parsed.tasks.len(), 3);

        // Task 1: "type": "Worker", "tier": "Hard", "name": "worker-task"
        assert!(parsed.tasks[0].is_worker());
        assert!(parsed.tasks[0].is_hard());
        assert_eq!(parsed.tasks[0].resolved_description(0), "worker-task");
        assert_eq!(parsed.tasks[0].scope, vec!["src/auth/**"]);

        // Task 2: "agent_type": "explore", "desc": "explore-task"
        assert!(!parsed.tasks[1].is_worker());
        assert!(!parsed.tasks[1].is_hard());
        assert_eq!(parsed.tasks[1].resolved_description(1), "explore-task");

        // Task 3: missing description -> resolved from first line of prompt
        assert!(!parsed.tasks[2].is_worker());
        assert_eq!(parsed.tasks[2].resolved_description(2), "inspect router");

        // 3. RiskLevel detection handles case-insensitive and aliased worker types
        let tool = TaskTool::new(
            || panic!("not called"),
            || panic!("not called"),
            || panic!("not called"),
            || panic!("not called"),
        );
        assert_eq!(tool.risk(json_aliases), RiskLevel::Risky);

        let safe_explore = r#"{"tasks":[{"prompt":"find x","type":"explore"}]}"#;
        assert_eq!(tool.risk(safe_explore), RiskLevel::Safe);
    }

    #[test]
    fn receipt_ledger_records_file_mutations_and_normalizes_paths() {
        let mut ledger = ActionReceiptLedger::default();

        // 1. edit with Windows backslash and redundant edits
        ledger.record_call("1", "edit", r#"{"path":"src\\auth\\login.rs"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        ledger.record_call("2", "edit", r#"{"path":"src/auth/login.rs"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "2".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 2. write_file legacy with leading ./
        ledger.record_call("3", "write_file", r#"{"file_path":"./Cargo.toml"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "3".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 3. create_file alias via single-source FILE_MUTATION_TOOLS
        ledger.record_call("4", "create_file", r#"{"path":"./src/new_mod.rs"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "4".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 4. search_replace directory
        ledger.record_call(
            "5",
            "search_replace",
            r#"{"path":"crates/jeikcode-coding"}"#,
        );
        ledger.record_tool_result(&ToolResult {
            call_id: "5".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 5. read_file is read-only, must not be counted as touched
        ledger.record_call("6", "read_file", r#"{"path":"README.md"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "6".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        let touched = ledger.touched_files();
        assert_eq!(
            touched,
            vec![
                "src/auth/login.rs".to_string(),
                "Cargo.toml".to_string(),
                "src/new_mod.rs".to_string(),
                "crates/jeikcode-coding".to_string(),
            ]
        );
    }

    #[test]
    fn receipt_ledger_records_commands_with_status_and_capping() {
        let mut ledger = ActionReceiptLedger::default();

        ledger.record_call("1", "run_command", r#"{"command":"cargo check"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        ledger.record_call(
            "2",
            "bash",
            r#"{"command":"cargo test --workspace --all-features --verbose --release"}"#,
        );
        ledger.record_tool_result(&ToolResult {
            call_id: "2".into(),
            content: "failed".into(),
            is_error: true,
            images: vec![],
        });

        // Multi-line command should only take first line
        ledger.record_call("3", "cmd", r#"{"command":"npm run build\necho done"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "3".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        let commands = ledger.executed_commands();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], ("cargo check".to_string(), true));
        assert!(commands[1].0.ends_with('\u{2026}')); // truncated with ellipsis
        assert!(!commands[1].1); // failed
        assert_eq!(commands[2], ("npm run build".to_string(), true));
    }

    #[test]
    fn receipt_ledger_renders_worker_receipt_format() {
        // Case 1: Touched file + successful command
        let mut l1 = ActionReceiptLedger::default();
        l1.record_call("1", "edit", r#"{"path":"path/to/file.rs"}"#);
        l1.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        l1.record_call("2", "run_command", r#"{"command":"cargo check"}"#);
        l1.record_tool_result(&ToolResult {
            call_id: "2".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l1.render_worker_receipt(),
            "[Worker touched: path/to/file.rs, ran: cargo check (ok)]"
        );

        // Case 2: Only touched file, no command
        let mut l2 = ActionReceiptLedger::default();
        l2.record_call("1", "write", r#"{"path":"src/lib.rs"}"#);
        l2.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l2.render_worker_receipt(),
            "[Worker touched: src/lib.rs, ran: (none)]"
        );

        // Case 3: No touched files, only command
        let mut l3 = ActionReceiptLedger::default();
        l3.record_call("1", "run_command", r#"{"command":"cargo check"}"#);
        l3.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l3.render_worker_receipt(),
            "[Worker touched: (none), ran: cargo check (ok)]"
        );

        // Case 4: Neither files nor commands
        let l4 = ActionReceiptLedger::default();
        assert_eq!(
            l4.render_worker_receipt(),
            "[Worker touched: (none), ran: (none)]"
        );

        // Case 5: Failed edit
        let mut l5 = ActionReceiptLedger::default();
        l5.record_call("1", "edit", r#"{"path":"src/read_only.rs"}"#);
        l5.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "denied".into(),
            is_error: true,
            images: vec![],
        });
        assert_eq!(
            l5.render_worker_receipt(),
            "[Worker touched: (none) (failed: src/read_only.rs), ran: (none)]"
        );

        // Case 6: Failed edit then succeeded on the same file -> file is touched, not in failed
        let mut l6 = ActionReceiptLedger::default();
        l6.record_call("1", "edit", r#"{"path":"src/retry.rs"}"#);
        l6.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "mismatch".into(),
            is_error: true,
            images: vec![],
        });
        l6.record_call("2", "edit", r#"{"path":"src/retry.rs"}"#);
        l6.record_tool_result(&ToolResult {
            call_id: "2".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l6.render_worker_receipt(),
            "[Worker touched: src/retry.rs, ran: (none)]"
        );
    }

    #[test]
    fn receipt_ledger_capping_prevents_token_bloat() {
        let mut ledger = ActionReceiptLedger::default();
        for i in 1..=8 {
            let id = i.to_string();
            ledger.record_call(&id, "edit", &format!(r#"{{"path":"src/file_{i}.rs"}}"#));
            ledger.record_tool_result(&ToolResult {
                call_id: id,
                content: "ok".into(),
                is_error: false,
                images: vec![],
            });
        }
        for i in 1..=7 {
            let id = (100 + i).to_string();
            ledger.record_call(&id, "run_command", &format!(r#"{{"command":"check_{i}"}}"#));
            ledger.record_tool_result(&ToolResult {
                call_id: id,
                content: "ok".into(),
                is_error: false,
                images: vec![],
            });
        }
        let receipt = ledger.render_worker_receipt();
        assert!(receipt.contains("(+2 more)"));
        assert!(receipt.starts_with("[Worker touched: src/file_1.rs, src/file_2.rs"));
    }

    struct ScriptedMultiRoundProvider {
        rounds: std::sync::Mutex<Vec<Vec<StreamEvent>>>,
    }

    #[async_trait]
    impl LlmProvider for ScriptedMultiRoundProvider {
        fn model_name(&self) -> &str {
            "scripted-mock"
        }

        async fn chat_stream(
            &self,
            _m: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let mut g = self.rounds.lock().unwrap();
            if g.is_empty() {
                Ok(stream::iter(vec![StreamEvent::Done { truncated: false }]).boxed())
            } else {
                let evs = g.remove(0);
                Ok(stream::iter(evs).boxed())
            }
        }
    }

    struct MockEditTool;
    #[async_trait]
    impl Tool for MockEditTool {
        fn name(&self) -> &str {
            "edit"
        }
        fn description(&self) -> &str {
            "mock edit"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: "edited file".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    struct MockRunCommandTool;
    #[async_trait]
    impl Tool for MockRunCommandTool {
        fn name(&self) -> &str {
            "run_command"
        }
        fn description(&self) -> &str {
            "mock command"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: "cargo check ok".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    #[tokio::test]
    async fn worker_task_includes_action_receipt_ledger_in_result() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(MockEditTool));
        reg.register(Arc::new(MockRunCommandTool));
        let reg = Arc::new(reg);
        let r_exp = reg.clone();
        let r_wrk = reg.clone();

        let make_provider = || {
            let rounds = vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "edit".into(),
                        arguments: r#"{"path":"src/auth/login.rs"}"#.into(),
                    }),
                    StreamEvent::ToolCall(ToolCall {
                        id: "call-2".into(),
                        name: "run_command".into(),
                        arguments: r#"{"command":"cargo check"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("Authentication fix verified.".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ];
            Arc::new(ScriptedMultiRoundProvider {
                rounds: std::sync::Mutex::new(rounds),
            }) as Arc<dyn LlmProvider>
        };

        let tool = TaskTool::new(
            make_provider,
            make_provider,
            move || r_exp.mount(&[]),
            move || r_wrk.mount(&["edit", "run_command"]),
        );

        let args = r#"{
            "tasks": [
                {
                    "description": "fix login",
                    "prompt": "fix login auth",
                    "subagent_type": "worker",
                    "scope": ["src/auth/**"]
                }
            ]
        }"#;

        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert!(out.content.contains("<task_result>"));
        assert!(out
            .content
            .contains("[Worker touched: src/auth/login.rs, ran: cargo check (ok)]"));
        assert!(out.content.contains("Authentication fix verified."));
    }

    #[tokio::test]
    async fn explore_task_omits_worker_receipt() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || {
                Arc::new(MockProvider {
                    reply: Some("findings reported".into()),
                }) as Arc<dyn LlmProvider>
            },
            || {
                Arc::new(MockProvider {
                    reply: Some("findings reported".into()),
                }) as Arc<dyn LlmProvider>
            },
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"investigate","prompt":"look into X","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error);
        assert!(out.content.contains("<task_result>"));
        assert!(out.content.contains("findings reported"));
        assert!(
            !out.content.contains("[Worker touched:"),
            "explore subtask must not include worker receipt ledger"
        );
    }

    #[tokio::test]
    async fn worker_task_failure_includes_action_receipt_ledger_in_error() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(MockEditTool));
        let reg = Arc::new(reg);
        let r_exp = reg.clone();
        let r_wrk = reg.clone();

        // Round 1: worker calls edit
        // Round 2: provider encounters an error
        let make_provider = || {
            let rounds = vec![vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "edit".into(),
                    arguments: r#"{"path":"src/auth/login.rs"}"#.into(),
                }),
                StreamEvent::Done { truncated: false },
            ]];
            Arc::new(ScriptedMultiRoundProvider {
                rounds: std::sync::Mutex::new(rounds),
            }) as Arc<dyn LlmProvider>
        };

        let tool = TaskTool::new(
            make_provider,
            make_provider,
            move || r_exp.mount(&[]),
            move || r_wrk.mount(&["edit"]),
        )
        .with_max_rounds(1); // Force MaxRounds stop after first round

        let args = r#"{
            "tasks": [
                {
                    "description": "fix login",
                    "prompt": "fix login auth",
                    "subagent_type": "worker",
                    "scope": ["src/auth/**"]
                }
            ]
        }"#;

        let out = tool.execute(args, &ctx()).await;
        assert!(out.is_error);
        assert!(out.content.contains("<task_error>"));
        assert!(out
            .content
            .contains("[Worker touched: src/auth/login.rs, ran: (none)]"));
        assert!(out.content.contains("subagent stopped early"));
    }

    #[test]
    fn receipt_ledger_records_explore_queries_and_searches() {
        let mut ledger = ActionReceiptLedger::default();

        // 1. read with Windows backslash and redundant reads
        ledger.record_call("1", "read", r#"{"path":"src\\auth\\login.rs"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        ledger.record_call("2", "read_file", r#"{"file_path":"./src/auth/login.rs"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "2".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 2a. code_explore with directory scope: only query is counted in searches, not queried_files
        ledger.record_call("3a", "code_explore", r#"{"path":".","query":"TaskTool"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "3a".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 2b. code_explore targeting a specific file: path IS counted in queried_files
        ledger.record_call(
            "3b",
            "code_explore",
            r#"{"path":"crates/jeikcode-capabilities/src/tools/task.rs","query":"ActionReceipt"}"#,
        );
        ledger.record_tool_result(&ToolResult {
            call_id: "3b".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 3. grep with pattern
        ledger.record_call("4", "grep", r#"{"pattern":"TokenClaims"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "4".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 4. glob with pattern
        ledger.record_call("5", "glob", r#"{"pattern":"*.toml"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "5".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        // 5. ast_grep with pattern
        ledger.record_call("6", "ast_grep", r#"{"pattern":"fn login($$$)"}"#);
        ledger.record_tool_result(&ToolResult {
            call_id: "6".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });

        let queried = ledger.queried_files();
        assert_eq!(
            queried,
            vec![
                "src/auth/login.rs".to_string(),
                "crates/jeikcode-capabilities/src/tools/task.rs".to_string(),
            ]
        );

        let searches = ledger.executed_searches();
        assert_eq!(searches.len(), 5);
        assert_eq!(searches[0], ("TaskTool".to_string(), true));
        assert_eq!(searches[1], ("ActionReceipt".to_string(), true));
        assert_eq!(searches[2], ("TokenClaims".to_string(), true));
        assert_eq!(searches[3], ("*.toml".to_string(), true));
        assert_eq!(searches[4], ("fn login($$$)".to_string(), true));
    }

    #[test]
    fn receipt_ledger_renders_explore_receipt_format() {
        // Case 1: Queried file + search query
        let mut l1 = ActionReceiptLedger::default();
        l1.record_call("1", "read", r#"{"path":"src/main.rs"}"#);
        l1.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        l1.record_call("2", "grep", r#"{"pattern":"Config"}"#);
        l1.record_tool_result(&ToolResult {
            call_id: "2".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l1.render_explore_receipt(),
            "[Explore queried: src/main.rs, searched: Config (ok)]"
        );

        // Case 2: Only queried file, no search
        let mut l2 = ActionReceiptLedger::default();
        l2.record_call("1", "read", r#"{"path":"Cargo.toml"}"#);
        l2.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l2.render_explore_receipt(),
            "[Explore queried: Cargo.toml, searched: (none)]"
        );

        // Case 3: Only search, no read
        let mut l3 = ActionReceiptLedger::default();
        l3.record_call("1", "glob", r#"{"pattern":"*.rs"}"#);
        l3.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        });
        assert_eq!(
            l3.render_explore_receipt(),
            "[Explore queried: (none), searched: *.rs (ok)]"
        );

        // Case 4: Neither
        let l4 = ActionReceiptLedger::default();
        assert_eq!(
            l4.render_explore_receipt(),
            "[Explore queried: (none), searched: (none)]"
        );

        // Case 5: Failed read
        let mut l5 = ActionReceiptLedger::default();
        l5.record_call("1", "read", r#"{"path":"missing.rs"}"#);
        l5.record_tool_result(&ToolResult {
            call_id: "1".into(),
            content: "file not found".into(),
            is_error: true,
            images: vec![],
        });
        assert_eq!(
            l5.render_explore_receipt(),
            "[Explore queried: (none) (failed: missing.rs), searched: (none)]"
        );

        // Case 6: Capping with (+N more)
        let mut l6 = ActionReceiptLedger::default();
        for i in 1..=8 {
            let id = i.to_string();
            l6.record_call(&id, "read", &format!(r#"{{"path":"src/file_{i}.rs"}}"#));
            l6.record_tool_result(&ToolResult {
                call_id: id,
                content: "ok".into(),
                is_error: false,
                images: vec![],
            });
        }
        for i in 1..=7 {
            let id = (100 + i).to_string();
            l6.record_call(&id, "grep", &format!(r#"{{"pattern":"pattern_{i}"}}"#));
            l6.record_tool_result(&ToolResult {
                call_id: id,
                content: "ok".into(),
                is_error: false,
                images: vec![],
            });
        }
        let receipt = l6.render_explore_receipt();
        assert!(receipt.contains("(+2 more)"));
        assert!(receipt.starts_with("[Explore queried: src/file_1.rs, src/file_2.rs"));
    }

    #[test]
    fn compact_subtask_output_bounds_oversized_payload() {
        // Small output passes through untouched
        let small = "findings: all clear";
        assert_eq!(compact_subtask_output(small), small);

        // Oversized output (>16KB) is compacted with head and tail
        let huge = "A".repeat(10_000) + &"B".repeat(10_000);
        let compacted = compact_subtask_output(&huge);
        assert!(compacted.len() < huge.len());
        assert!(compacted.starts_with(&"A".repeat(100)));
        assert!(compacted.ends_with(&"B".repeat(100)));
        assert!(compacted.contains("[... subagent output compacted: 20000 bytes total"));
    }

    struct MockReadTool;
    #[async_trait]
    impl Tool for MockReadTool {
        fn name(&self) -> &str {
            "read"
        }
        fn description(&self) -> &str {
            "mock read"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: "file content preview".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    struct MockGrepTool;
    #[async_trait]
    impl Tool for MockGrepTool {
        fn name(&self) -> &str {
            "grep"
        }
        fn description(&self) -> &str {
            "mock grep"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: "search match found".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    #[tokio::test]
    async fn explore_task_includes_explore_receipt_in_result() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(MockReadTool));
        reg.register(Arc::new(MockGrepTool));
        let reg = Arc::new(reg);
        let r_exp = reg.clone();
        let r_wrk = reg.clone();

        let make_provider = || {
            let rounds = vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "read".into(),
                        arguments: r#"{"path":"src/auth/jwt.rs"}"#.into(),
                    }),
                    StreamEvent::ToolCall(ToolCall {
                        id: "call-2".into(),
                        name: "grep".into(),
                        arguments: r#"{"pattern":"verify_token"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("JWT authentication flow traced.".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ];
            Arc::new(ScriptedMultiRoundProvider {
                rounds: std::sync::Mutex::new(rounds),
            }) as Arc<dyn LlmProvider>
        };

        let tool = TaskTool::new(
            make_provider,
            make_provider,
            move || r_exp.mount(&["read", "grep"]),
            move || r_wrk.mount(&[]),
        );

        let args = r#"{
            "tasks": [
                {
                    "description": "trace auth",
                    "prompt": "trace JWT auth flow",
                    "subagent_type": "explore"
                }
            ]
        }"#;

        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert!(out.content.contains("<task_result>"));
        assert!(out
            .content
            .contains("[Explore queried: src/auth/jwt.rs, searched: verify_token (ok)]"));
        assert!(out.content.contains("JWT authentication flow traced."));
    }

    #[tokio::test]
    #[serial_test::serial(offline_verdict)]
    async fn test_deny_offline_network_blocks_subagent_network_calls() {
        use jeikcode_config::config::offline::{seed_offline_verdict, OfflineMode};

        struct ResetGuard;
        impl Drop for ResetGuard {
            fn drop(&mut self) {
                jeikcode_config::config::offline::seed_offline_verdict(OfflineMode::Off, None);
            }
        }
        let _guard = ResetGuard;

        let gate = DenyOfflineNetwork;
        let mut call = ToolCall {
            id: "call-web".to_string(),
            name: "web_search".to_string(),
            arguments: r#"{"query":"docs"}"#.to_string(),
        };

        // 1. When online, web_search should proceed
        seed_offline_verdict(OfflineMode::Off, None);
        let mock_tool: Arc<dyn Tool> = Arc::new(MockReadTool);
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(events, None);
        let outcome = gate.before(&mut call, &mock_tool, &rt).await;
        assert!(matches!(outcome, BeforeOutcome::Proceed));

        // 2. When offline, it must deny turn
        seed_offline_verdict(OfflineMode::On, None);
        let outcome_offline = gate.before(&mut call, &mock_tool, &rt).await;
        match outcome_offline {
            BeforeOutcome::Deny { reason } => {
                assert!(reason.contains("offline / air-gap mode is active"));
            }
            _ => panic!("Expected Deny when offline, got {:?}", outcome_offline),
        }

        // 3. Test web_fetch and whitespace tolerance
        let mut fetch_call = ToolCall {
            id: "call-fetch".to_string(),
            name: "  Web_Fetch  ".to_string(),
            arguments: r#"{"url":"https://example.com"}"#.to_string(),
        };
        let outcome_fetch = gate.before(&mut fetch_call, &mock_tool, &rt).await;
        match outcome_fetch {
            BeforeOutcome::Deny { reason } => {
                assert!(reason.contains("offline / air-gap mode is active"));
            }
            _ => panic!("Expected Deny for web_fetch when offline"),
        }

        // 4. Test read_url_content alias
        let mut read_url_call = ToolCall {
            id: "call-read-url".to_string(),
            name: "read_url_content".to_string(),
            arguments: r#"{"url":"https://example.com"}"#.to_string(),
        };
        let outcome_read_url = gate.before(&mut read_url_call, &mock_tool, &rt).await;
        assert!(matches!(outcome_read_url, BeforeOutcome::Deny { .. }));

        // 5. Test safe tools (e.g. read / code_explore / code_search) proceed even in offline mode
        let mut safe_read_call = ToolCall {
            id: "call-safe-read".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"src/lib.rs"}"#.to_string(),
        };
        let outcome_safe_read = gate.before(&mut safe_read_call, &mock_tool, &rt).await;
        assert!(matches!(outcome_safe_read, BeforeOutcome::Proceed));

        let mut safe_code_search = ToolCall {
            id: "call-safe-code-search".to_string(),
            name: "code_search".to_string(),
            arguments: r#"{"query":"fn main"}"#.to_string(),
        };
        assert!(matches!(
            gate.before(&mut safe_code_search, &mock_tool, &rt).await,
            BeforeOutcome::Proceed
        ));

        // 6. Test namespaced & prefixed MCP tools (e.g. server/tool, brave_web_search, or mcp__tool)
        seed_offline_verdict(OfflineMode::On, None);
        let mut mcp_call = ToolCall {
            id: "call-mcp-browser".to_string(),
            name: "puppeteer/browser".to_string(),
            arguments: r#"{"url":"https://example.com"}"#.to_string(),
        };
        let outcome_mcp = gate.before(&mut mcp_call, &mock_tool, &rt).await;
        assert!(matches!(outcome_mcp, BeforeOutcome::Deny { .. }));

        let mut mcp_dunder_call = ToolCall {
            id: "call-mcp-dunder".to_string(),
            name: "mcp__web_search".to_string(),
            arguments: r#"{"query":"test"}"#.to_string(),
        };
        let outcome_mcp_dunder = gate.before(&mut mcp_dunder_call, &mock_tool, &rt).await;
        assert!(matches!(outcome_mcp_dunder, BeforeOutcome::Deny { .. }));

        let mut mcp_prefixed_call = ToolCall {
            id: "call-mcp-prefixed".to_string(),
            name: "brave_search/brave_web_search".to_string(),
            arguments: r#"{"query":"test"}"#.to_string(),
        };
        assert!(matches!(
            gate.before(&mut mcp_prefixed_call, &mock_tool, &rt).await,
            BeforeOutcome::Deny { .. }
        ));

        // 7. Test dynamic flip: offline -> online allows egress again
        seed_offline_verdict(OfflineMode::Off, None);
        let outcome_restored = gate.before(&mut fetch_call, &mock_tool, &rt).await;
        assert!(matches!(outcome_restored, BeforeOutcome::Proceed));
    }

    #[tokio::test]
    async fn child_subagent_receives_project_instructions_and_explore_strategy_while_gates_hold() {
        use jeikcode_kernel::message::Role;
        use std::sync::Mutex as StdMutex;

        struct CaptureProvider {
            seen_messages: Arc<StdMutex<Vec<Vec<Message>>>>,
            rounds: StdMutex<Vec<Vec<StreamEvent>>>,
        }

        #[async_trait]
        impl LlmProvider for CaptureProvider {
            fn model_name(&self) -> &str {
                "capture-mock"
            }
            async fn chat_stream(
                &self,
                messages: &[Message],
                _tools: &[ToolDef],
                _opts: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                self.seen_messages.lock().unwrap().push(messages.to_vec());
                let next = self.rounds.lock().unwrap().remove(0);
                Ok(stream::iter(next).boxed())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "SUBAGENT_REPO_RULE_ABC").unwrap();
        std::fs::write(dir.path().join("allowed.txt"), "before").unwrap();

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(crate::tools::ReadFileTool::new(false)));
        reg.register(Arc::new(crate::tools::WriteFileTool));
        let reg = Arc::new(reg);
        let r_exp = reg.clone();
        let r_wrk = reg.clone();

        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let round_idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let make_provider = move || {
            let idx = round_idx.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let rounds = if idx == 0 {
                // explore#1: simple answer
                vec![vec![
                    StreamEvent::TextDelta("explored ok".into()),
                    StreamEvent::Done { truncated: false },
                ]]
            } else {
                // worker#2: attempt out-of-scope write (must be denied by WorkerScopeGate),
                // then attempt sensitive path read (must terminate via DenySensitivePaths).
                vec![
                    vec![
                        StreamEvent::ToolCall(ToolCall {
                            id: "w1".into(),
                            name: "write".into(),
                            arguments: r#"{"path":"outside.txt","content":"bad"}"#.into(),
                        }),
                        StreamEvent::Done { truncated: false },
                    ],
                    vec![
                        StreamEvent::ToolCall(ToolCall {
                            id: "w2".into(),
                            name: "read".into(),
                            arguments: r#"{"path":".env"}"#.into(),
                        }),
                        StreamEvent::Done { truncated: false },
                    ],
                ]
            };
            Arc::new(CaptureProvider {
                seen_messages: seen_clone.clone(),
                rounds: StdMutex::new(rounds),
            }) as Arc<dyn LlmProvider>
        };

        let tool = TaskTool::new(
            make_provider.clone(),
            make_provider,
            move || r_exp.mount(&["read"]),
            move || r_wrk.mount(&["read", "write"]),
        );

        let args = r#"{
            "tasks": [
                {
                    "description": "inspect rules",
                    "prompt": "check project",
                    "subagent_type": "explore"
                },
                {
                    "description": "edit allowed",
                    "prompt": "try edits",
                    "subagent_type": "worker",
                    "scope": ["allowed.txt"]
                }
            ]
        }"#;

        let tool_ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        };
        let out = tool.execute(args, &tool_ctx).await;

        let captured = seen.lock().unwrap();
        assert!(captured.len() >= 2);
        for req in captured.iter() {
            let sys = req
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                sys.contains("SUBAGENT_REPO_RULE_ABC"),
                "child system prompt missing AGENTS.md: {sys}"
            );
            assert!(
                sys.contains("<explore_strategy>"),
                "child system prompt missing <explore_strategy>: {sys}"
            );
            assert!(
                sys.contains("<project_instructions>"),
                "child system prompt missing <project_instructions>: {sys}"
            );
        }

        // Verify worker out-of-scope write was blocked and sensitive path denial triggered.
        assert!(!dir.path().join("outside.txt").exists());
        assert!(
            out.content.contains("PolicyDenied") || out.content.contains("sensitive paths"),
            "DenySensitivePaths must remain active: {}",
            out.content
        );
    }
}
