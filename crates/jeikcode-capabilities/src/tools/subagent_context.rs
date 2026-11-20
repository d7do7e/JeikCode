//! Lean workspace context and exploration-discipline builder for `TaskTool` subagents.
//!
//! Keeps child agents token-lean while ensuring they inherit essential project rules
//! (`AGENTS.md` / `<project_instructions>`) and `<explore_strategy>` tool discipline
//! without weakening `DenySensitivePaths`, `DenyOfflineNetwork`, or `WorkerScopeGate`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use jeikcode_config::config::instructions::{GLOBAL_INSTRUCTION_FILES, PROJECT_INSTRUCTION_FILES};
use jeikcode_kernel::hook::LifecycleHooks;
use jeikcode_kernel::message::Conversation;

use super::subagent_verifier::SubagentRole;

/// Maximum inner byte budget for `<project_instructions>` passed to a subagent child
/// when `inherit_context` is `false` (the default lean mode).
pub(crate) const SUBAGENT_PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 4096;

pub(crate) type ProjectContextLoader = dyn Fn(&Path) -> Option<String> + Send + Sync;

/// Full parent-aligned read-only context blocks injected into a subagent when
/// `inherit_context: true` is passed to `TaskTool`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InheritedSubagentContext {
    pub memory: Option<String>,
    pub user_preferences: Option<String>,
    pub project_instructions: Option<String>,
}

pub(crate) type InheritedContextLoader = dyn Fn(&Path) -> InheritedSubagentContext + Send + Sync;

const EXPLORE_TOOL_GUIDANCE: &str = "\
<explore_strategy>
- Prefer dedicated tools over shell commands: use `code_explore` for semantic call flows (pass a directory or `.` to `path`, never a single file), `grep` for exact symbols/regex, `glob` for file patterns, and `read` in generous chunks (several hundred lines or full files).
- Concurrency first: batch independent `read`, `grep`, `glob`, and `code_explore` calls in the same turn.
- You are strictly READ-ONLY. Cite exact file paths, line numbers, and verified symbols in your findings.
</explore_strategy>";

const EXPLORE_TOOL_GUIDANCE_NO_CODEGRAPH: &str = "\
<explore_strategy>
- Prefer dedicated tools over shell commands: use `grep` for exact symbols/regex, `glob` for file patterns, and `read` in generous chunks (several hundred lines or full files).
- Concurrency first: batch independent `read`, `grep`, and `glob` calls in the same turn.
- You are strictly READ-ONLY. Cite exact file paths, line numbers, and verified symbols in your findings.
</explore_strategy>";

const WORKER_TOOL_GUIDANCE: &str = "\
<explore_strategy>
- Prefer dedicated tools: use `code_explore` (directory scope), `grep`, `glob`, and generous `read` calls before editing; use `edit` / `write` (never `sed`/`awk`/redirects) strictly within your declared `scope`.
- Concurrency first: batch independent reads/searches or independent in-scope edits in the same turn.
- Use `run_command` only for builds, tests, or checks dedicated tools cannot cover; run one focused verification check after edits, and never run pagers or interactive/watch commands.
</explore_strategy>";

const WORKER_TOOL_GUIDANCE_NO_CODEGRAPH: &str = "\
<explore_strategy>
- Prefer dedicated tools: use `grep`, `glob`, and generous `read` calls before editing; use `edit` / `write` (never `sed`/`awk`/redirects) strictly within your declared `scope`.
- Concurrency first: batch independent reads/searches or independent in-scope edits in the same turn.
- Use `run_command` only for builds, tests, or checks dedicated tools cannot cover; run one focused verification check after edits, and never run pagers or interactive/watch commands.
</explore_strategy>";

const VERIFIER_TOOL_GUIDANCE: &str = "\
<explore_strategy>
- Prefer dedicated tools (`read` in generous chunks, `code_explore` on directory scopes, `grep`, `glob`) to inspect changed code and callers/callees concurrently.
- Use `run_command` ONLY for read-only inspection and verification checks (`cargo check`, `cargo test`, `cargo fmt --check`, `npm test`, `pytest`, `git diff`, etc.). File mutations and output redirects are hard-blocked.
- Verify independently from fresh workspace state; cite exact commands run, pass/fail output, and file:line evidence.
</explore_strategy>";

const VERIFIER_TOOL_GUIDANCE_NO_CODEGRAPH: &str = "\
<explore_strategy>
- Prefer dedicated tools (`read` in generous chunks, `grep`, `glob`) to inspect changed code and callers/callees concurrently.
- Use `run_command` ONLY for read-only inspection and verification checks (`cargo check`, `cargo test`, `cargo fmt --check`, `npm test`, `pytest`, `git diff`, etc.). File mutations and output redirects are hard-blocked.
- Verify independently from fresh workspace state; cite exact commands run, pass/fail output, and file:line evidence.
</explore_strategy>";

/// Load bounded `<project_instructions>` for `working_dir`, preferring an embedder-supplied
/// loader (e.g. `jeikcode-coding` with knowledge packs) and falling back to the canonical
/// `PROJECT_INSTRUCTION_FILES` lookup.
pub(crate) fn resolve_project_instructions(
    working_dir: &Path,
    custom_loader: Option<&Arc<ProjectContextLoader>>,
) -> Option<String> {
    if let Some(loader) = custom_loader {
        if let Some(raw) = loader(working_dir) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(format_bounded_project_instructions(
                    trimmed,
                    SUBAGENT_PROJECT_INSTRUCTIONS_MAX_BYTES,
                ));
            }
        }
        return None;
    }

    for name in PROJECT_INSTRUCTION_FILES {
        let candidate = working_dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&candidate) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                let labeled = format!("# PROJECT INSTRUCTIONS ({name})\n{trimmed}");
                return Some(format_bounded_project_instructions(
                    &labeled,
                    SUBAGENT_PROJECT_INSTRUCTIONS_MAX_BYTES,
                ));
            }
        }
    }
    None
}

/// Resolve full uncapped parent context blocks (`<memory>`, `<user_preferences>`,
/// `<project_instructions>`) for `inherit_context: true`, preferring an embedder-supplied
/// loader (e.g. `jeikcode-coding` with `SessionContextHook` + `MemoryHook`) and falling
/// back to reading global/project instruction files and memory directly.
pub(crate) fn resolve_inherited_context(
    working_dir: &Path,
    custom_loader: Option<&Arc<InheritedContextLoader>>,
) -> InheritedSubagentContext {
    if let Some(loader) = custom_loader {
        return loader(working_dir);
    }

    let home = crate::paths::config_dir();
    let user_preferences = GLOBAL_INSTRUCTION_FILES.iter().find_map(|name| {
        let candidate = home.join(name);
        if !candidate.is_file() {
            return None;
        }
        let content = std::fs::read_to_string(&candidate).ok()?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(format!(
            "<user_preferences>\n\n# USER PREFERENCES ({name})\n{trimmed}\n</user_preferences>"
        ))
    });

    let project_instructions = PROJECT_INSTRUCTION_FILES.iter().find_map(|name| {
        let candidate = working_dir.join(name);
        if !candidate.is_file() {
            return None;
        }
        let content = std::fs::read_to_string(&candidate).ok()?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(format!(
            "<project_instructions>\n\n# PROJECT INSTRUCTIONS ({name})\n{trimmed}\n</project_instructions>"
        ))
    });

    #[cfg(feature = "memory")]
    let memory = crate::memory::MemoryHook::for_project(working_dir).render_block();
    #[cfg(not(feature = "memory"))]
    let memory = None;

    InheritedSubagentContext {
        memory,
        user_preferences,
        project_instructions,
    }
}

/// Lifecycle hook mounted on `inherit_context: true` subagents so `<memory>` (50),
/// `<user_preferences>` (55), and `<project_instructions>` (60) land as frozen
/// synthetic-user blocks in the exact parent order before the task prompt.
pub(crate) struct SubagentInheritedContextHook {
    ctx: InheritedSubagentContext,
}

impl SubagentInheritedContextHook {
    pub(crate) fn new(ctx: InheritedSubagentContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LifecycleHooks for SubagentInheritedContextHook {
    async fn session_start(&self, convo: &mut Conversation, _resumed: bool) {
        convo.reconcile_frozen_user_block("<memory>", self.ctx.memory.clone());
        convo.reconcile_frozen_user_block("<user_preferences>", self.ctx.user_preferences.clone());
        convo.reconcile_frozen_user_block(
            "<project_instructions>",
            self.ctx.project_instructions.clone(),
        );
    }
}

/// Assemble the child subagent system prompt from its base role persona, declared worker
/// scope, `<explore_strategy>` tool guidance, and bounded `<project_instructions>`.
#[allow(dead_code)]
pub(crate) fn build_child_persona(
    base_persona: &str,
    role: SubagentRole,
    scope: &[String],
    project_instructions: Option<&str>,
) -> String {
    build_child_persona_with_codegraph(base_persona, role, scope, project_instructions, true)
}

/// Assemble the child subagent system prompt, selecting `<explore_strategy>` guidance
/// that avoids `code_explore` nudges when the workspace lacks a codegraph index.
pub(crate) fn build_child_persona_with_codegraph(
    base_persona: &str,
    role: SubagentRole,
    scope: &[String],
    project_instructions: Option<&str>,
    has_codegraph: bool,
) -> String {
    let mut parts = vec![base_persona.to_string()];
    match role {
        SubagentRole::Worker => {
            let non_empty: Vec<&str> = scope
                .iter()
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !non_empty.is_empty() {
                parts.push(format!(
                    "Declared writable scope (enforced by WorkerScopeGate): {}",
                    non_empty.join(", ")
                ));
            }
            parts.push(
                if has_codegraph {
                    WORKER_TOOL_GUIDANCE
                } else {
                    WORKER_TOOL_GUIDANCE_NO_CODEGRAPH
                }
                .to_string(),
            );
        }
        SubagentRole::Verifier => {
            parts.push(
                if has_codegraph {
                    VERIFIER_TOOL_GUIDANCE
                } else {
                    VERIFIER_TOOL_GUIDANCE_NO_CODEGRAPH
                }
                .to_string(),
            );
        }
        SubagentRole::Explore => {
            parts.push(
                if has_codegraph {
                    EXPLORE_TOOL_GUIDANCE
                } else {
                    EXPLORE_TOOL_GUIDANCE_NO_CODEGRAPH
                }
                .to_string(),
            );
        }
    }
    if let Some(instr) = project_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(instr.to_string());
    }
    parts.join("\n\n")
}

fn format_bounded_project_instructions(raw: &str, max_bytes: usize) -> String {
    let inner = raw
        .trim()
        .strip_prefix("<project_instructions>")
        .unwrap_or(raw.trim())
        .trim();
    let inner = inner
        .strip_suffix("</project_instructions>")
        .unwrap_or(inner)
        .trim();

    let body = if inner.len() <= max_bytes {
        inner.to_string()
    } else {
        let cut = truncate_utf8_at_boundary(inner, max_bytes);
        format!("{cut}\n[... truncated for subagent context budget ...]")
    };
    format!("<project_instructions>\n{body}\n</project_instructions>")
}

fn truncate_utf8_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let slice = &s[..end];
    if let Some(nl) = slice.rfind('\n') {
        if nl >= end / 2 {
            return slice[..nl].trim_end();
        }
    }
    slice.trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_agents_md_and_bounds_oversized_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let long_body = "rule line\n".repeat(700);
        std::fs::write(dir.path().join("AGENTS.md"), &long_body).unwrap();

        let rendered = resolve_project_instructions(dir.path(), None).expect("AGENTS.md loaded");
        assert!(rendered.starts_with("<project_instructions>\n"));
        assert!(rendered.ends_with("\n</project_instructions>"));
        assert!(rendered.contains("# PROJECT INSTRUCTIONS (AGENTS.md)"));
        assert!(rendered.contains("[... truncated for subagent context budget ...]"));
        assert!(rendered.len() <= SUBAGENT_PROJECT_INSTRUCTIONS_MAX_BYTES + 160);
    }

    #[test]
    fn builds_role_specific_persona_with_scope_and_explore_strategy() {
        let explore = build_child_persona(
            "EXPLORE",
            SubagentRole::Explore,
            &[],
            Some("<project_instructions>\nx\n</project_instructions>"),
        );
        assert!(explore.contains("<explore_strategy>"));
        assert!(explore.contains("READ-ONLY"));
        assert!(explore.contains("<project_instructions>"));
        assert!(!explore.contains("Declared writable scope"));

        let worker = build_child_persona(
            "WORKER",
            SubagentRole::Worker,
            &["crates/jeikcode-coding/**".into()],
            None,
        );
        assert!(worker.contains("<explore_strategy>"));
        assert!(worker.contains(
            "Declared writable scope (enforced by WorkerScopeGate): crates/jeikcode-coding/**"
        ));
        assert!(!worker.contains("<project_instructions>"));

        let verifier = build_child_persona("VERIFIER", SubagentRole::Verifier, &[], None);
        assert!(verifier.contains("<explore_strategy>"));
        assert!(verifier.contains("code_explore"));
        assert!(verifier.contains("File mutations and output redirects are hard-blocked"));
        assert!(!verifier.contains("Declared writable scope"));

        for role in [
            SubagentRole::Explore,
            SubagentRole::Worker,
            SubagentRole::Verifier,
        ] {
            let no_cg = build_child_persona_with_codegraph("BASE", role, &[], None, false);
            assert!(no_cg.contains("<explore_strategy>"));
            assert!(
                !no_cg.contains("code_explore"),
                "no-codegraph guidance must omit code_explore for {role:?}: {no_cg}"
            );
            assert!(no_cg.contains("grep") && no_cg.contains("glob") && no_cg.contains("read"));
        }
    }

    #[tokio::test]
    async fn inherited_context_hook_injects_parent_ordered_frozen_blocks() {
        use jeikcode_kernel::message::{Message, Role};

        let hook = SubagentInheritedContextHook::new(InheritedSubagentContext {
            memory: Some("<memory>\n- use uv\n</memory>".to_string()),
            user_preferences: Some("<user_preferences>\n# global\n</user_preferences>".to_string()),
            project_instructions: Some(
                "<project_instructions>\n# project\n</project_instructions>".to_string(),
            ),
        });
        let mut convo = Conversation::new();
        convo.push(Message::system("EXPLORE_PERSONA"));
        hook.session_start(&mut convo, false).await;
        convo.push(Message::user("Critique this plan"));

        assert_eq!(convo.messages.len(), 5);
        assert_eq!(convo.messages[0].role, Role::System);
        assert_eq!(convo.messages[1].role, Role::User);
        assert!(convo.messages[1].synthetic);
        assert!(convo.messages[1].text.starts_with("<memory>"));
        assert_eq!(convo.messages[2].role, Role::User);
        assert!(convo.messages[2].synthetic);
        assert!(convo.messages[2].text.starts_with("<user_preferences>"));
        assert_eq!(convo.messages[3].role, Role::User);
        assert!(convo.messages[3].synthetic);
        assert!(convo.messages[3].text.starts_with("<project_instructions>"));
        assert_eq!(convo.messages[4].role, Role::User);
        assert!(!convo.messages[4].synthetic);
        assert_eq!(convo.messages[4].text, "Critique this plan");

        // Idempotent on resume
        hook.session_start(&mut convo, true).await;
        assert_eq!(convo.messages.len(), 5);
    }
}
