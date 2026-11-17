//! Lean workspace context and exploration-discipline builder for `TaskTool` subagents.
//!
//! Keeps child agents token-lean while ensuring they inherit essential project rules
//! (`AGENTS.md` / `<project_instructions>`) and `<explore_strategy>` tool discipline
//! without weakening `DenySensitivePaths`, `DenyOfflineNetwork`, or `WorkerScopeGate`.

use std::path::Path;
use std::sync::Arc;

use jeikcode_config::config::instructions::PROJECT_INSTRUCTION_FILES;

use super::subagent_verifier::SubagentRole;

/// Maximum inner byte budget for `<project_instructions>` passed to a subagent child.
pub(crate) const SUBAGENT_PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 4096;

pub(crate) type ProjectContextLoader = dyn Fn(&Path) -> Option<String> + Send + Sync;

const EXPLORE_TOOL_GUIDANCE: &str = "\
<explore_strategy>
- Prefer dedicated tools over shell commands: use `code_explore` for semantic call flows (pass a directory or `.` to `path`, never a single file), `grep` for exact symbols/regex, `glob` for file patterns, and `read` in generous chunks (several hundred lines or full files).
- Concurrency first: batch independent `read`, `grep`, `glob`, and `code_explore` calls in the same turn.
- You are strictly READ-ONLY. Cite exact file paths, line numbers, and verified symbols in your findings.
</explore_strategy>";

const WORKER_TOOL_GUIDANCE: &str = "\
<explore_strategy>
- Prefer dedicated tools: use `code_explore` (directory scope), `grep`, `glob`, and generous `read` calls before editing; use `edit` / `write` (never `sed`/`awk`/redirects) strictly within your declared `scope`.
- Concurrency first: batch independent reads/searches or independent in-scope edits in the same turn.
- Use `run_command` only for builds, tests, or checks dedicated tools cannot cover; run one focused verification check after edits, and never run pagers or interactive/watch commands.
</explore_strategy>";

const VERIFIER_TOOL_GUIDANCE: &str = "\
<explore_strategy>
- Prefer dedicated tools (`read` in generous chunks, `code_explore` on directory scopes, `grep`, `glob`) to inspect changed code and callers/callees concurrently.
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

/// Assemble the child subagent system prompt from its base role persona, declared worker
/// scope, `<explore_strategy>` tool guidance, and bounded `<project_instructions>`.
pub(crate) fn build_child_persona(
    base_persona: &str,
    role: SubagentRole,
    scope: &[String],
    project_instructions: Option<&str>,
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
            parts.push(WORKER_TOOL_GUIDANCE.to_string());
        }
        SubagentRole::Verifier => {
            parts.push(VERIFIER_TOOL_GUIDANCE.to_string());
        }
        SubagentRole::Explore => {
            parts.push(EXPLORE_TOOL_GUIDANCE.to_string());
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
        assert!(verifier.contains("File mutations and output redirects are hard-blocked"));
        assert!(!verifier.contains("Declared writable scope"));
    }
}
