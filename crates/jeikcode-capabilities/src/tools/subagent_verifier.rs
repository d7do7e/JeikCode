//! Read-only blind `verifier` subagent role and shell mutation guard.
//!
//! `verifier` is a fresh-context evaluator that inspects workspace files and runs
//! verification commands (`cargo check/test/clippy`, `npm test`, `pytest`, `git diff`,
//! etc.) while strictly blocking file mutations (`write`, `edit`, redirects, mutating
//! shell commands).

use async_trait::async_trait;
use jeikcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use jeikcode_kernel::request::RequestCtx;
use jeikcode_kernel::tool::{Tool, ToolCall};
use std::sync::Arc;

use super::bash::{
    bash_invocations, check_destructive_command, is_read_only_bash, output_redirect_target,
    BashInvocation,
};

/// Subagent execution role parsed from `SubTask.subagent_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SubagentRole {
    Explore,
    Worker,
    Verifier,
}

impl SubagentRole {
    pub(crate) fn parse_opt(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.eq_ignore_ascii_case("worker") {
            Some(Self::Worker)
        } else if trimmed.eq_ignore_ascii_case("verifier")
            || trimmed.eq_ignore_ascii_case("reviewer")
        {
            Some(Self::Verifier)
        } else {
            Some(Self::Explore)
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Worker => "worker",
            Self::Verifier => "verifier",
        }
    }

    pub(crate) fn is_worker(self) -> bool {
        matches!(self, Self::Worker)
    }
}

pub(crate) const VERIFIER_PERSONA: &str = "\
You are a BLIND, FRESH-CONTEXT, READ-ONLY VERIFIER subagent. Your sole job is to \
independently verify whether the workspace actually satisfies the stated acceptance \
criteria and changed-code invariants.

Hard rules:
1. Default to NEEDS_WORK. Never assume a fix works from claims in the prompt or partial reading.
2. Only report `VERDICT: PASS` when you have actually observed passing tool/command output \
or directly read the exact implementation and tests proving every criterion.
3. You are STRICTLY READ-ONLY: do NOT edit, write, format-in-place, commit, or redirect output to files.
4. Always conclude with this exact structured report:
VERDICT: PASS | NEEDS_WORK
SUMMARY: <1-2 sentences grounded in verified output>
CHECKS_RUN:
- <command or file check>: <PASS/FAIL + key output>
ISSUES:
- <none, or [file:line] concrete failure / gap>";

/// Hard middleware guard mounted on `verifier` children: blocks any file-mutation tool
/// and restricts `run_command` to read-only inspection and verification commands.
pub(crate) struct VerifierReadOnlyShellGuard;

#[async_trait]
impl ToolMiddleware for VerifierReadOnlyShellGuard {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if super::is_file_mutation_tool(&call.name) || super::is_search_replace_tool(&call.name) {
            return BeforeOutcome::deny_turn(format!(
                "verifier subagent is read-only and cannot invoke `{}`",
                call.name
            ));
        }
        if super::is_shell_tool_name(&call.name) || call.name.eq_ignore_ascii_case("cmd") {
            let Some(command) = super::extract_shell_command(&call.arguments) else {
                return BeforeOutcome::deny(
                    "verifier run_command call had no parseable `command`".to_string(),
                );
            };
            if let Err(reason) = validate_verifier_shell_command(&command) {
                return BeforeOutcome::deny(reason);
            }
        }
        BeforeOutcome::Proceed
    }
}

pub(crate) fn validate_verifier_shell_command(command: &str) -> Result<(), String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("verifier command is empty".to_string());
    }
    if is_read_only_bash(trimmed) {
        return Ok(());
    }
    if let Some(target) = output_redirect_target(trimmed) {
        return Err(format!(
            "verifier subagent is read-only and may not redirect shell output to `{target}`"
        ));
    }
    if let Some(reason) = check_destructive_command(trimmed) {
        return Err(format!(
            "verifier subagent blocked potentially destructive command ({reason})"
        ));
    }
    let Some(invocations) = bash_invocations(trimmed) else {
        return Err(
            "verifier subagent could not safely parse shell command; keep checks simple and read-only"
                .to_string(),
        );
    };
    if invocations.is_empty() {
        return Err("verifier command contained no executable invocations".to_string());
    }
    for inv in &invocations {
        if let Err(reason) = validate_invocation(inv) {
            return Err(reason);
        }
    }
    Ok(())
}

fn validate_invocation(inv: &BashInvocation) -> Result<(), String> {
    let cmd = inv
        .command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&inv.command)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    let args: Vec<&str> = inv.arguments.iter().map(String::as_str).collect();

    if has_inline_write_redirect_or_inplace_flag(&cmd, &args) {
        return Err(format!(
            "verifier subagent blocked in-place mutation flag in `{}`",
            inv.command
        ));
    }

    match cmd.as_str() {
        // Pure read-only / inspection utilities & shell builtins
        "ls" | "dir" | "cat" | "head" | "tail" | "wc" | "sort" | "uniq" | "cut" | "tr" | "grep"
        | "rg" | "find" | "fd" | "which" | "where" | "pwd" | "echo" | "printf" | "env"
        | "printenv" | "stat" | "file" | "diff" | "cmp" | "cd" | "true" | "false" | "test"
        | "[" | "jq" => Ok(()),
        "sed" | "awk" | "gawk" => {
            if args.iter().any(|a| *a == "-i" || a.starts_with("-i")) {
                Err(format!("verifier subagent cannot run `{cmd} -i`"))
            } else {
                Ok(())
            }
        }
        "cargo" => validate_cargo(&args),
        "git" => validate_git(&args),
        "npm" | "pnpm" | "yarn" | "bun" => validate_node_pkg(&cmd, &args),
        "npx" => validate_npx(&args),
        "pytest" | "ruff" | "mypy" | "pyright" | "flake8" | "tsc" | "eslint" | "vitest"
        | "jest" | "go" | "ctest" => validate_checker(&cmd, &args),
        "node" => validate_node(&args),
        "python" | "python3" | "py" => validate_python(&args),
        "flutter" | "dart" => validate_dart_flutter(&cmd, &args),
        "make" => {
            if args.iter().any(|a| matches!(*a, "test" | "check" | "lint")) {
                Ok(())
            } else {
                Err("verifier `make` only allows `test`, `check`, or `lint`".to_string())
            }
        }
        _ => Err(format!(
            "verifier subagent blocked non-allowlisted command `{}`; only read-only inspection and verification checks are permitted",
            inv.command
        )),
    }
}

fn has_inline_write_redirect_or_inplace_flag(cmd: &str, args: &[&str]) -> bool {
    if matches!(cmd, "perl" | "ruby" | "sed")
        && args.iter().any(|a| *a == "-i" || a.starts_with("-i"))
    {
        return true;
    }
    false
}

fn validate_cargo(args: &[&str]) -> Result<(), String> {
    let sub = args.iter().copied().find(|a| !a.starts_with('-'));
    match sub {
        Some("check" | "test" | "clippy" | "mutants" | "tree" | "metadata" | "verify-project"
        | "doc") => Ok(()),
        Some("fmt") => {
            if args.iter().any(|a| *a == "--check") {
                Ok(())
            } else {
                Err("verifier `cargo fmt` requires `--check` (no in-place formatting)".to_string())
            }
        }
        other => Err(format!(
            "verifier `cargo` subcommand `{}` is not permitted (use check/test/clippy/fmt --check/mutants)",
            other.unwrap_or("<none>")
        )),
    }
}

fn validate_git(args: &[&str]) -> Result<(), String> {
    let sub = args.iter().copied().find(|a| !a.starts_with('-'));
    match sub {
        Some(
            "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "grep" | "blame"
            | "describe",
        ) => Ok(()),
        Some("branch") => {
            if args
                .iter()
                .any(|a| matches!(*a, "-d" | "-D" | "-m" | "-M" | "--delete" | "--move"))
            {
                Err("verifier `git branch` cannot delete or rename branches".to_string())
            } else {
                Ok(())
            }
        }
        other => Err(format!(
            "verifier `git` subcommand `{}` is not read-only",
            other.unwrap_or("<none>")
        )),
    }
}

fn validate_node_pkg(cmd: &str, args: &[&str]) -> Result<(), String> {
    let sub = args.iter().copied().find(|a| !a.starts_with('-'));
    match sub {
        Some("test" | "typecheck" | "lint" | "audit" | "list" | "ls" | "check") => Ok(()),
        Some("run") => {
            let script = args
                .iter()
                .copied()
                .filter(|a| !a.starts_with('-'))
                .nth(1)
                .unwrap_or("");
            if matches!(
                script,
                "test" | "build" | "typecheck" | "lint" | "check" | "verify"
            ) {
                Ok(())
            } else {
                Err(format!(
                    "verifier `{cmd} run` only allows test/build/typecheck/lint/check/verify scripts"
                ))
            }
        }
        other => Err(format!(
            "verifier `{cmd}` subcommand `{}` is not permitted",
            other.unwrap_or("<none>")
        )),
    }
}

fn validate_npx(args: &[&str]) -> Result<(), String> {
    let tool = args.iter().copied().find(|a| !a.starts_with('-'));
    match tool {
        Some("tsc" | "eslint" | "vitest" | "jest") => Ok(()),
        Some("prettier") if args.iter().any(|a| *a == "--check") => Ok(()),
        other => Err(format!(
            "verifier `npx` tool `{}` is not permitted",
            other.unwrap_or("<none>")
        )),
    }
}

fn validate_checker(cmd: &str, args: &[&str]) -> Result<(), String> {
    match cmd {
        "ruff" => {
            let sub = args.iter().copied().find(|a| !a.starts_with('-'));
            match sub {
                Some("check") | None => Ok(()),
                Some("format") if args.iter().any(|a| *a == "--check" || *a == "--diff") => Ok(()),
                _ => Err("verifier `ruff` only allows `check` or `format --check`".to_string()),
            }
        }
        "go" => {
            let sub = args.iter().copied().find(|a| !a.starts_with('-'));
            match sub {
                Some("test" | "vet" | "version" | "list") => Ok(()),
                _ => Err("verifier `go` only allows `test`, `vet`, `list`, `version`".to_string()),
            }
        }
        _ => Ok(()),
    }
}

fn validate_node(args: &[&str]) -> Result<(), String> {
    if args
        .iter()
        .any(|a| *a == "--test" || *a == "--check" || *a == "--version" || *a == "-v")
    {
        Ok(())
    } else {
        Err("verifier `node` requires `--test`, `--check`, or `--version`".to_string())
    }
}

fn validate_python(args: &[&str]) -> Result<(), String> {
    if args.iter().any(|a| *a == "--version" || *a == "-V") {
        return Ok(());
    }
    if let Some(pos) = args.iter().position(|a| *a == "-m") {
        if let Some(module) = args.get(pos + 1).copied() {
            if matches!(
                module,
                "pytest" | "unittest" | "py_compile" | "mypy" | "ruff"
            ) {
                return Ok(());
            }
        }
    }
    Err(
        "verifier `python` only allows `-m pytest|unittest|py_compile|mypy|ruff` or `--version`"
            .to_string(),
    )
}

fn validate_dart_flutter(cmd: &str, args: &[&str]) -> Result<(), String> {
    let sub = args.iter().copied().find(|a| !a.starts_with('-'));
    match sub {
        Some("test" | "analyze") => Ok(()),
        Some("format")
            if args.iter().any(|a| *a == "--output=none")
                && args.iter().any(|a| *a == "--set-exit-if-changed") =>
        {
            Ok(())
        }
        other => Err(format!(
            "verifier `{cmd}` subcommand `{}` is not permitted",
            other.unwrap_or("<none>")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_shell_guard_allows_checks_and_blocks_mutations() {
        assert!(validate_verifier_shell_command("cargo test -p jeikcode-coding --locked").is_ok());
        assert!(validate_verifier_shell_command("cargo fmt --all -- --check").is_ok());
        assert!(validate_verifier_shell_command("git diff --stat && git status --short").is_ok());
        assert!(validate_verifier_shell_command("python -m pytest tests/").is_ok());
        assert!(validate_verifier_shell_command("npm run typecheck").is_ok());

        assert!(validate_verifier_shell_command("cargo fmt --all").is_err());
        assert!(validate_verifier_shell_command("rm -rf target").is_err());
        assert!(validate_verifier_shell_command("echo hi > out.txt").is_err());
        assert!(validate_verifier_shell_command("git commit -m test").is_err());
        assert!(validate_verifier_shell_command("sed -i 's/a/b/' file.rs").is_err());
        assert!(validate_verifier_shell_command("python -c \"open('x','w').write('y')\"").is_err());
    }

    #[test]
    fn role_parser_accepts_verifier_and_reviewer_alias() {
        assert_eq!(SubagentRole::parse_opt(""), None);
        assert_eq!(
            SubagentRole::parse_opt("explore"),
            Some(SubagentRole::Explore)
        );
        assert_eq!(
            SubagentRole::parse_opt("Worker"),
            Some(SubagentRole::Worker)
        );
        assert_eq!(
            SubagentRole::parse_opt("verifier"),
            Some(SubagentRole::Verifier)
        );
        assert_eq!(
            SubagentRole::parse_opt("reviewer"),
            Some(SubagentRole::Verifier)
        );
    }
}
