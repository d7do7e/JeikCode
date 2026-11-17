//! Budget-guarded, anti-bloat `memory` tool for persistent cross-session auto-memory.
//!
//! Actions:
//! - `remember`: persist a durable fact (`scope`: `"project"` | `"global"` | `"topic"`)
//! - `recall`: read a `topic` file or search across global + index + topics
//! - `update`: refine/replace an existing `MEMORY.md` entry or rewrite a `topic` file
//! - `prune`: remove obsolete entries by keyword or delete an outdated `topic` file

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use jeikcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};

use super::auto_store::AutoMemoryStore;
use super::store::MemoryStore;

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

#[derive(Deserialize)]
struct MemoryToolArgs {
    action: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    append: Option<bool>,
}

/// Model-facing `memory` tool (mounted ONLY on primary parent agents, never subagents).
pub struct MemoryTool {
    project_root: PathBuf,
    auto_root_override: Option<PathBuf>,
    global_path_override: Option<PathBuf>,
    legacy_project_path_override: Option<PathBuf>,
}

impl MemoryTool {
    pub fn for_project(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            auto_root_override: None,
            global_path_override: None,
            legacy_project_path_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_paths(
        project_root: PathBuf,
        auto_root: PathBuf,
        global_path: PathBuf,
        legacy_project_path: PathBuf,
    ) -> Self {
        Self {
            project_root,
            auto_root_override: Some(auto_root),
            global_path_override: Some(global_path),
            legacy_project_path_override: Some(legacy_project_path),
        }
    }

    fn auto_store(&self) -> AutoMemoryStore {
        match &self.auto_root_override {
            Some(root) => AutoMemoryStore::with_root(root.clone()),
            None => AutoMemoryStore::for_project(&self.project_root),
        }
    }

    fn global_store(&self) -> MemoryStore {
        match &self.global_path_override {
            Some(path) => MemoryStore::new(path.clone()),
            None => MemoryStore::global(),
        }
    }

    fn legacy_project_store(&self) -> MemoryStore {
        match &self.legacy_project_path_override {
            Some(path) => MemoryStore::new(path.clone()),
            None => MemoryStore::project(&self.project_root),
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Persist, recall, update, or prune durable cross-session memory (`MEMORY.md` compact index + on-demand `topics/*.md`). Store only durable, non-obvious preferences, conventions, or gotchas — never secrets or ephemeral task logs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["remember", "recall", "update", "prune"],
                    "description": "Operation to perform."
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global", "topic"],
                    "description": "Target store for `remember`/`update`/`prune` (default: `project`, or `topic` when `topic` is set)."
                },
                "content": {
                    "type": "string",
                    "description": "Concise one-line fact (for index/global) or markdown body (for `topic`)."
                },
                "topic": {
                    "type": "string",
                    "description": "Topic file slug (e.g. `build_rules`, `architecture`) stored under `topics/<slug>.md`."
                },
                "summary": {
                    "type": "string",
                    "description": "Optional short summary line placed in `MEMORY.md` pointing to the topic file."
                },
                "query": {
                    "type": "string",
                    "description": "Keyword filter for `recall` or `prune`."
                },
                "target": {
                    "type": "string",
                    "description": "Existing entry substring to replace during `update`."
                },
                "append": {
                    "type": "boolean",
                    "description": "When true with `topic`, append to `topics/<slug>.md` instead of overwriting (default: false)."
                }
            },
            "required": ["action"]
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let parsed: MemoryToolArgs = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return tool_err(format!("invalid memory tool arguments: {e}")),
        };

        let action = parsed.action.trim().to_ascii_lowercase();
        let auto = self.auto_store();
        let global = self.global_store();
        let legacy_project = self.legacy_project_store();

        match action.as_str() {
            "remember" => {
                let Some(raw_content) = parsed.content.as_deref().map(str::trim) else {
                    return tool_err("`remember` requires `content`");
                };
                if let Err(reason) = validate_admission(raw_content) {
                    return tool_err(format!("memory admission rejected: {reason}"));
                }
                let topic = parsed
                    .topic
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let scope = parsed
                    .scope
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_else(|| {
                        if topic.is_some() {
                            "topic".to_string()
                        } else {
                            "project".to_string()
                        }
                    });

                if scope == "topic" || topic.is_some() {
                    let Some(slug) = topic else {
                        return tool_err("`scope:\"topic\"` requires `topic` slug");
                    };
                    let append = parsed.append.unwrap_or(false);
                    match auto.write_topic(slug, raw_content, parsed.summary.as_deref(), append) {
                        Ok(path) => {
                            tool_ok(format!("Saved topic memory `{slug}` ({})", path.display()))
                        }
                        Err(e) => tool_err(e),
                    }
                } else if scope == "global" {
                    if raw_content.contains('\n') || raw_content.len() > 240 {
                        return tool_err(
                            "global memory entries must be a single concise line (<= 240 chars)",
                        );
                    }
                    match global.append_deduped(raw_content) {
                        Ok(true) => tool_ok(format!("Remembered (global): {raw_content}")),
                        Ok(false) => tool_ok(format!("Already remembered (global): {raw_content}")),
                        Err(e) => tool_err(format!("failed to write global memory: {e}")),
                    }
                } else if scope == "project" {
                    match auto.remember_index_entry(raw_content) {
                        Ok(true) => {
                            tool_ok(format!("Remembered in project MEMORY.md: {raw_content}"))
                        }
                        Ok(false) => tool_ok(format!(
                            "Already remembered in project MEMORY.md: {raw_content}"
                        )),
                        Err(e) => tool_err(e),
                    }
                } else {
                    tool_err(format!(
                        "unsupported scope `{scope}` (expected `project`, `global`, or `topic`)"
                    ))
                }
            }
            "recall" => {
                let topic = parsed
                    .topic
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let query = parsed
                    .query
                    .as_deref()
                    .or(parsed.target.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if topic.is_some() {
                    return match auto.recall(None, topic) {
                        Ok(body) => tool_ok(body),
                        Err(e) => tool_err(e),
                    };
                }
                let mut sections = Vec::new();
                match auto.recall(query, None) {
                    Ok(s) if !s.trim().is_empty() => sections.push(s),
                    Ok(_) => {}
                    Err(e) => return tool_err(e),
                }
                let g_hits = match query {
                    Some(q) => global.find_matching(q),
                    None => global.load(),
                };
                if !g_hits.is_empty() {
                    sections.push(format!(
                        "[Global memory.md]\n{}",
                        g_hits
                            .into_iter()
                            .map(|e| format!("- {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                let p_hits = match query {
                    Some(q) => legacy_project.find_matching(q),
                    None => legacy_project.load(),
                };
                if !p_hits.is_empty() {
                    sections.push(format!(
                        "[Project memory.md]\n{}",
                        p_hits
                            .into_iter()
                            .map(|e| format!("- {e}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                tool_ok(sections.join("\n\n"))
            }
            "update" => {
                let Some(raw_content) = parsed.content.as_deref().map(str::trim) else {
                    return tool_err("`update` requires `content`");
                };
                if let Err(reason) = validate_admission(raw_content) {
                    return tool_err(format!("memory admission rejected: {reason}"));
                }
                if let Some(slug) = parsed
                    .topic
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    match auto.write_topic(slug, raw_content, parsed.summary.as_deref(), false) {
                        Ok(path) => tool_ok(format!("Updated topic `{slug}` ({})", path.display())),
                        Err(e) => tool_err(e),
                    }
                } else {
                    let Some(target) = parsed
                        .target
                        .as_deref()
                        .or(parsed.query.as_deref())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        return tool_err(
                            "`update` without `topic` requires `target` substring to match in MEMORY.md",
                        );
                    };
                    let scope = parsed
                        .scope
                        .as_deref()
                        .map(str::trim)
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_else(|| "project".to_string());
                    if scope == "global" {
                        if raw_content.contains('\n') || raw_content.len() > 240 {
                            return tool_err(
                                "global memory entries must be a single concise line (<= 240 chars)",
                            );
                        }
                        match global.update_matching(target, raw_content) {
                            Ok(true) => tool_ok(format!(
                                "Updated global memory.md entry matching `{target}`"
                            )),
                            Ok(false) => {
                                tool_err(format!("no global memory.md entry matched `{target}`"))
                            }
                            Err(e) => tool_err(format!("failed to update global memory: {e}")),
                        }
                    } else {
                        match auto.update_index_entry(target, raw_content) {
                            Ok(true) => {
                                tool_ok(format!("Updated MEMORY.md entry matching `{target}`"))
                            }
                            Ok(false) => {
                                match legacy_project.update_matching(target, raw_content) {
                                    Ok(true) => tool_ok(format!(
                                        "Updated project memory.md entry matching `{target}`"
                                    )),
                                    Ok(false) => {
                                        tool_err(format!("no MEMORY.md entry matched `{target}`"))
                                    }
                                    Err(e) => tool_err(e.to_string()),
                                }
                            }
                            Err(e) => tool_err(e),
                        }
                    }
                }
            }
            "prune" => {
                if let Some(slug) = parsed
                    .topic
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    match auto.delete_topic(slug) {
                        Ok(true) => tool_ok(format!(
                            "Deleted topic `{slug}` and pruned its index pointer"
                        )),
                        Ok(false) => tool_ok(format!("Topic `{slug}` did not exist")),
                        Err(e) => tool_err(e),
                    }
                } else {
                    let Some(kw) = parsed
                        .query
                        .as_deref()
                        .or(parsed.target.as_deref())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        return tool_err("`prune` requires `query` or `topic`");
                    };
                    let scope = parsed
                        .scope
                        .as_deref()
                        .map(str::trim)
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_else(|| "project".to_string());
                    let mut count = 0usize;
                    if scope == "global" {
                        if let Ok(removed) = global.remove_matching(kw) {
                            count += removed.len();
                        }
                    } else if scope == "project" || scope == "topic" {
                        if let Ok(removed) = auto.prune_index(kw) {
                            count += removed.len();
                        }
                        if let Ok(removed) = legacy_project.remove_matching(kw) {
                            count += removed.len();
                        }
                    } else {
                        return tool_err(format!(
                            "unsupported scope `{scope}` (expected `project`, `global`, or `topic`)"
                        ));
                    }
                    tool_ok(format!("Pruned {count} matching memory entries for `{kw}`"))
                }
            }
            other => tool_err(format!(
                "unsupported memory action `{other}` (expected remember, recall, update, or prune)"
            )),
        }
    }
}

/// Anti-bloat & secret-hygiene admission filter for agent-written memory entries.
pub(crate) fn validate_admission(content: &str) -> Result<(), &'static str> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("empty memory content");
    }
    if trimmed.len() < 6 {
        return Err("entry is too short/vague to be durable");
    }
    let lower = trimmed.to_ascii_lowercase();
    for secret_marker in [
        "sk-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "akia",
        "-----begin private key-----",
        "-----begin openssh private key-----",
        "password=",
        "api_key=",
        "secret_key=",
    ] {
        if lower.contains(secret_marker) {
            return Err("refusing to persist potential secret/credential material");
        }
    }
    for ephemeral in [
        "working on step ",
        "currently running ",
        "just ran cargo ",
        "todowrite",
    ] {
        if lower.starts_with(ephemeral) {
            return Err("ephemeral turn narration does not belong in persistent memory");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tool(dir: &Path) -> MemoryTool {
        MemoryTool::with_test_paths(
            dir.to_path_buf(),
            dir.join("auto"),
            dir.join("global_memory.md"),
            dir.join("project_memory.md"),
        )
    }

    fn test_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: jeikcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn memory_tool_remember_recall_update_and_prune_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let ctx = test_ctx(dir.path());

        let res = tool
            .execute(
                r#"{"action":"remember","content":"Use --locked on all workspace cargo commands"}"#,
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);

        let topic_res = tool
            .execute(
                r#"{"action":"remember","topic":"windows_tests","summary":"Windows parallel test caveats","content":"Run capabilities unit tests with --test-threads=4 on Windows."}"#,
                &ctx,
            )
            .await;
        assert!(!topic_res.is_error, "{}", topic_res.content);

        let recalled = tool
            .execute(r#"{"action":"recall","topic":"windows_tests"}"#, &ctx)
            .await;
        assert!(!recalled.is_error);
        assert!(recalled.content.contains("--test-threads=4"));

        let updated = tool
            .execute(
                r#"{"action":"update","target":"--locked","content":"Always pass --locked on cargo check/test"}"#,
                &ctx,
            )
            .await;
        assert!(!updated.is_error, "{}", updated.content);
        let search = tool
            .execute(r#"{"action":"recall","query":"--locked"}"#, &ctx)
            .await;
        assert!(search.content.contains("Always pass --locked"));

        let pruned = tool
            .execute(r#"{"action":"prune","topic":"windows_tests"}"#, &ctx)
            .await;
        assert!(!pruned.is_error);
        assert!(!dir.path().join("auto/topics/windows_tests.md").exists());
    }

    #[tokio::test]
    async fn memory_tool_rejects_secrets_and_oversized_index_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let ctx = test_ctx(dir.path());

        let secret = tool
            .execute(
                r#"{"action":"remember","content":"api_key=sk-1234567890abcdef"}"#,
                &ctx,
            )
            .await;
        assert!(secret.is_error);
        assert!(secret.content.contains("secret"));

        let huge = format!(r#"{{"action":"remember","content":"{}"}}"#, "x".repeat(300));
        let oversized = tool.execute(&huge, &ctx).await;
        assert!(oversized.is_error);
        assert!(oversized.content.contains("topic"));
    }

    #[tokio::test]
    async fn memory_tool_scopes_prune_and_update_and_includes_global_in_queryless_recall() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let ctx = test_ctx(dir.path());

        tool.execute(
            r#"{"action":"remember","scope":"global","content":"Shared token: global-pref"}"#,
            &ctx,
        )
        .await;
        tool.execute(
            r#"{"action":"remember","scope":"project","content":"Shared token: project-pref"}"#,
            &ctx,
        )
        .await;

        let all_recalled = tool.execute(r#"{"action":"recall"}"#, &ctx).await;
        assert!(!all_recalled.is_error);
        assert!(
            all_recalled.content.contains("global-pref")
                && all_recalled.content.contains("project-pref"),
            "queryless recall must include both global and project memory: {}",
            all_recalled.content
        );

        // Project-scoped prune must NOT delete global memory
        let pruned_proj = tool
            .execute(r#"{"action":"prune","query":"Shared token"}"#, &ctx)
            .await;
        assert!(!pruned_proj.is_error);
        let after_prune = tool.execute(r#"{"action":"recall"}"#, &ctx).await;
        assert!(
            after_prune.content.contains("global-pref")
                && !after_prune.content.contains("project-pref"),
            "default project prune must leave global memory untouched: {}",
            after_prune.content
        );

        // Global update works
        let updated_global = tool
            .execute(
                r#"{"action":"update","scope":"global","target":"global-pref","content":"Shared token: global-pref-v2"}"#,
                &ctx,
            )
            .await;
        assert!(!updated_global.is_error, "{}", updated_global.content);
        let after_update = tool.execute(r#"{"action":"recall"}"#, &ctx).await;
        assert!(after_update.content.contains("global-pref-v2"));
    }
}
