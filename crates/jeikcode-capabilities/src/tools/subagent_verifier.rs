//! Read-only blind `verifier` subagent role and shell mutation guard.
//!
//! `verifier` is a fresh-context evaluator that inspects workspace files and runs
//! verification commands (`cargo check/test/clippy`, `npm test`, `pytest`, `git diff`,
//! etc.) while strictly blocking file mutations (`write`, `edit`, redirects, mutating
//! shell commands).

use async_trait::async_trait;
use jeikcode_kernel::agent::{Agent, AutoRespond, ToolLoopPolicy};
use jeikcode_kernel::event::StopReason;
use jeikcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use jeikcode_kernel::provider::LlmProvider;
use jeikcode_kernel::request::RequestCtx;
use jeikcode_kernel::tool::{MountedTools, Tool, ToolCall};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::bash::{
    bash_invocations, check_destructive_command, is_read_only_bash, output_redirect_target,
    BashInvocation,
};
use super::subagent_context::build_child_persona;
use super::task::{DenyOfflineNetwork, DenySensitivePaths};

/// Subagent execution role parsed from `SubTask.subagent_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
        "rustc" => validate_rustc(&args),
        "git" => validate_git(&args),
        "gh" => validate_gh(&args),
        "uv" => validate_uv(&args),
        "dotnet" => validate_dotnet(&args),
        "npm" | "pnpm" | "yarn" | "bun" => validate_node_pkg(&cmd, &args),
        "npx" => validate_npx(&args),
        "pytest" | "ruff" | "mypy" | "pyright" | "flake8" | "tsc" | "eslint" | "vitest"
        | "jest" | "go" | "ctest" => validate_checker(&cmd, &args),
        "node" => validate_node(&args),
        "python" | "python3" | "py" => validate_python(&args),
        "flutter" | "dart" => validate_dart_flutter(&cmd, &args),
        "ninja" => {
            if args.iter().any(|a| *a == "-n" || *a == "--dry-run") {
                Ok(())
            } else {
                Err("verifier `ninja` only allows dry-run (`-n` / `--dry-run`)".to_string())
            }
        }
        "cmake" => {
            if args.iter().any(|a| *a == "--version" || *a == "-V") {
                Ok(())
            } else {
                Err("verifier `cmake` only allows `--version` (use `ctest` for tests)".to_string())
            }
        }
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

/// Skip flags (including value-taking flags) and optional leading `+toolchain` tokens,
/// returning the first positional subcommand and its index in `args`.
fn first_subcommand<'a>(
    args: &'a [&'a str],
    value_flags: &[&str],
    skip_plus_toolchain: bool,
) -> Option<(usize, &'a str)> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            return args.get(i + 1).copied().map(|s| (i + 1, s));
        }
        if skip_plus_toolchain && a.starts_with('+') {
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            let takes_value = !a.contains('=') && value_flags.contains(&a);
            i += 1;
            if takes_value && i < args.len() {
                i += 1;
            }
            continue;
        }
        return Some((i, a));
    }
    None
}

fn validate_cargo(args: &[&str]) -> Result<(), String> {
    if args.iter().any(|a| *a == "--version" || *a == "-V") {
        return Ok(());
    }
    const CARGO_GLOBAL_VALUE_FLAGS: &[&str] = &[
        "-C",
        "--config",
        "-Z",
        "--color",
        "--manifest-path",
        "--target-dir",
    ];
    let sub = first_subcommand(args, CARGO_GLOBAL_VALUE_FLAGS, true).map(|(_, s)| s);
    match sub {
        Some("clippy") => {
            if args.iter().any(|a| *a == "--fix") {
                Err("verifier `cargo clippy` cannot use `--fix` (read-only)".to_string())
            } else {
                Ok(())
            }
        }
        Some(
            "check" | "test" | "nextest" | "mutants" | "tree" | "metadata" | "verify-project"
            | "doc" | "llvm-cov" | "version",
        ) => Ok(()),
        Some("bench") if args.iter().any(|a| *a == "--no-run") => Ok(()),
        Some("fmt") => {
            if args.iter().any(|a| *a == "--check") {
                Ok(())
            } else {
                Err("verifier `cargo fmt` requires `--check` (no in-place formatting)".to_string())
            }
        }
        other => Err(format!(
            "verifier `cargo` subcommand `{}` is not permitted (use check/test/nextest/clippy/fmt --check/mutants/doc)",
            other.unwrap_or("<none>")
        )),
    }
}

fn validate_rustc(args: &[&str]) -> Result<(), String> {
    if args
        .iter()
        .any(|a| *a == "--version" || *a == "-V" || *a == "--print")
    {
        Ok(())
    } else {
        Err("verifier `rustc` only allows `--version`, `-V`, or `--print`".to_string())
    }
}

fn validate_dotnet(args: &[&str]) -> Result<(), String> {
    if args.iter().any(|a| *a == "--version" || *a == "--info") {
        return Ok(());
    }
    let sub = first_subcommand(args, &[], false).map(|(_, s)| s);
    match sub {
        Some("test" | "list") => Ok(()),
        other => Err(format!(
            "verifier `dotnet` subcommand `{}` is not permitted (use `dotnet test` / `dotnet list`)",
            other.unwrap_or("<none>")
        )),
    }
}

fn validate_gh(args: &[&str]) -> Result<(), String> {
    const GH_GLOBAL_VALUE_FLAGS: &[&str] = &["-R", "--repo"];
    let Some((idx, group)) = first_subcommand(args, GH_GLOBAL_VALUE_FLAGS, false) else {
        return Err("verifier `gh` requires a read-only subcommand".to_string());
    };
    if group == "status" || group == "version" {
        return Ok(());
    }
    if !matches!(group, "pr" | "issue" | "run" | "workflow" | "release") {
        return Err(format!("verifier `gh {group}` is not permitted"));
    }
    let action = first_subcommand(&args[idx + 1..], GH_GLOBAL_VALUE_FLAGS, false).map(|(_, s)| s);
    if matches!(action, Some("view" | "list" | "status" | "diff" | "checks")) {
        Ok(())
    } else {
        Err(format!(
            "verifier `gh {group}` only allows read-only view/list/status/diff/checks, got `{}`",
            action.unwrap_or("<none>")
        ))
    }
}

fn validate_uv(args: &[&str]) -> Result<(), String> {
    if args.iter().any(|a| *a == "--version" || *a == "-V") {
        return Ok(());
    }
    const UV_GLOBAL_VALUE_FLAGS: &[&str] = &[
        "--project",
        "--directory",
        "-C",
        "--config-file",
        "--cache-dir",
        "--python",
        "-p",
    ];
    const UV_RUN_VALUE_FLAGS: &[&str] = &[
        "--extra",
        "--with",
        "--with-editable",
        "--with-requirements",
        "--python",
        "-p",
        "--directory",
        "--project",
        "--group",
        "--env-file",
        "--package",
        "--cwd",
        "--from",
        "-w",
        "-m",
        "-C",
    ];
    let Some((sub_idx, sub)) = first_subcommand(args, UV_GLOBAL_VALUE_FLAGS, false) else {
        return Err("verifier `uv` requires a subcommand".to_string());
    };
    match sub {
        "version" => Ok(()),
        "run" => {
            let after_run = &args[sub_idx + 1..];
            let Some((rel_idx, runner)) = first_subcommand(after_run, UV_RUN_VALUE_FLAGS, false)
            else {
                return Err("verifier `uv run` requires a target checker command".to_string());
            };
            let runner_name = std::path::Path::new(runner)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(runner)
                .trim_end_matches(".exe")
                .to_ascii_lowercase();
            let tail = &after_run[rel_idx + 1..];
            match runner_name.as_str() {
                "pytest" | "mypy" | "pyright" | "flake8" => Ok(()),
                "ruff" => validate_checker("ruff", tail),
                "python" | "python3" | "py" => validate_python(tail),
                _ => Err(format!(
                    "verifier `uv run` only allows pytest/mypy/pyright/flake8/ruff/python -m, got `{runner}`"
                )),
            }
        }
        other => Err(format!(
            "verifier `uv` subcommand `{other}` is not permitted (use `uv run pytest|mypy|ruff check`)"
        )),
    }
}

fn validate_git(args: &[&str]) -> Result<(), String> {
    if args.iter().any(|a| *a == "--version" || *a == "-v") {
        return Ok(());
    }
    const GIT_GLOBAL_VALUE_FLAGS: &[&str] = &[
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--super-prefix",
        "--config-env",
        "--exec-path",
    ];
    let Some((sub_idx, sub)) = first_subcommand(args, GIT_GLOBAL_VALUE_FLAGS, false) else {
        return Err("verifier `git` requires a read-only subcommand".to_string());
    };
    let tail = &args[sub_idx + 1..];
    match sub {
        "diff" => {
            if tail
                .iter()
                .any(|a| *a == "--output" || a.starts_with("--output="))
            {
                Err("verifier `git diff` cannot write `--output` files".to_string())
            } else {
                Ok(())
            }
        }
        "status" | "log" | "show" | "rev-parse" | "ls-files" | "grep" | "blame" | "describe"
        | "merge-base" | "rev-list" | "cat-file" | "check-ignore" | "diff-tree" | "show-ref"
        | "symbolic-ref" | "shortlog" => Ok(()),
        "branch" => {
            if tail
                .iter()
                .any(|a| matches!(*a, "-d" | "-D" | "-m" | "-M" | "--delete" | "--move"))
            {
                Err("verifier `git branch` cannot delete or rename branches".to_string())
            } else {
                Ok(())
            }
        }
        other => Err(format!(
            "verifier `git` subcommand `{other}` is not read-only"
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
    const NPX_VALUE_FLAGS: &[&str] = &["-p", "--package", "-c", "--call"];
    let Some((idx, tool)) = first_subcommand(args, NPX_VALUE_FLAGS, false) else {
        return Err("verifier `npx` requires a checker tool".to_string());
    };
    let tail = &args[idx + 1..];
    match tool {
        "tsc" | "eslint" | "vitest" | "jest" => validate_checker(tool, tail),
        "prettier" => {
            if tail.iter().any(|a| *a == "--write" || *a == "-w") {
                Err("verifier `npx prettier` cannot use `--write`/`-w`".to_string())
            } else if tail
                .iter()
                .any(|a| *a == "--check" || *a == "--list-different" || *a == "-l")
            {
                Ok(())
            } else {
                Err("verifier `npx prettier` requires `--check`".to_string())
            }
        }
        other => Err(format!("verifier `npx` tool `{other}` is not permitted")),
    }
}

fn validate_checker(cmd: &str, args: &[&str]) -> Result<(), String> {
    match cmd {
        "ruff" => {
            let sub = args.iter().copied().find(|a| !a.starts_with('-'));
            match sub {
                Some("check") | None => {
                    if args.iter().any(|a| {
                        matches!(*a, "--fix" | "--unsafe-fixes" | "--add-noqa")
                            || a.starts_with("--fix=")
                    }) {
                        Err("verifier `ruff check` cannot use `--fix`/`--unsafe-fixes`".to_string())
                    } else {
                        Ok(())
                    }
                }
                Some("format") if args.iter().any(|a| *a == "--check" || *a == "--diff") => Ok(()),
                _ => Err("verifier `ruff` only allows `check` or `format --check`".to_string()),
            }
        }
        "eslint" => {
            if args.iter().any(|a| {
                matches!(*a, "--fix" | "--output-file" | "-o")
                    || a.starts_with("--fix=")
                    || a.starts_with("--output-file=")
            }) {
                Err("verifier `eslint` cannot use `--fix` or `--output-file`".to_string())
            } else {
                Ok(())
            }
        }
        "tsc" => {
            if args
                .iter()
                .any(|a| matches!(*a, "--noEmit" | "--version" | "-v" | "--showConfig"))
            {
                Ok(())
            } else {
                Err("verifier `tsc` requires `--noEmit` (no JS emit to disk)".to_string())
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

/// Structured verdict produced by a blind fresh-context `verifier` run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifierVerdict {
    pub passed: bool,
    pub summary: String,
    pub report: String,
}

/// Parse a `verifier` subagent report strictly: defaults to `passed: false` unless an
/// explicit `VERDICT: PASS` line is present and no `VERDICT: NEEDS_WORK` overrides it.
pub fn parse_verifier_verdict(text: &str) -> VerifierVerdict {
    let mut verdict_pass: Option<bool> = None;
    let mut summary: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("verdict:") {
            let token = rest.trim();
            if token.starts_with("pass") {
                verdict_pass = Some(true);
            } else if token.starts_with("needs_work")
                || token.starts_with("needs work")
                || token.starts_with("fail")
            {
                verdict_pass = Some(false);
            }
        } else if summary.is_none() && lower.starts_with("summary:") {
            let s = line["summary:".len()..].trim();
            if !s.is_empty() {
                summary = Some(s.to_string());
            }
        }
    }
    let passed = verdict_pass.unwrap_or(false);
    let summary = summary.unwrap_or_else(|| {
        if passed {
            "blind verifier confirmed PASS".to_string()
        } else {
            "blind verifier did not emit VERDICT: PASS".to_string()
        }
    });
    VerifierVerdict {
        passed,
        summary,
        report: text.trim().to_string(),
    }
}

/// Run an isolated, fresh-context, read-only `verifier` agent against `working_dir`
/// using the provided `MountedTools` (which must omit file-mutation tools; `VerifierReadOnlyShellGuard`,
/// `DenySensitivePaths`, and `DenyOfflineNetwork` are always enforced).
pub async fn run_blind_verifier(
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    working_dir: PathBuf,
    prompt: String,
    project_instructions: Option<String>,
    max_rounds: u32,
    cancel: CancellationToken,
) -> Result<VerifierVerdict, String> {
    run_blind_verifier_with_context(
        provider,
        tools,
        working_dir,
        prompt,
        project_instructions,
        None,
        max_rounds,
        cancel,
    )
    .await
}

/// Like [`run_blind_verifier`], with optional full parent-ordered [`InheritedSubagentContext`]
/// (`<memory>`, `<user_preferences>`, and uncapped `<project_instructions>`).
#[allow(clippy::too_many_arguments)]
pub async fn run_blind_verifier_with_context(
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    working_dir: PathBuf,
    prompt: String,
    project_instructions: Option<String>,
    inherited_context: Option<super::subagent_context::InheritedSubagentContext>,
    max_rounds: u32,
    cancel: CancellationToken,
) -> Result<VerifierVerdict, String> {
    let persona = build_child_persona(
        VERIFIER_PERSONA,
        SubagentRole::Verifier,
        &[],
        if inherited_context.is_some() {
            None
        } else {
            project_instructions.as_deref()
        },
    );
    let mut builder = Agent::builder()
        .provider(provider)
        .tools(tools)
        .persona(persona)
        .working_dir(working_dir.clone())
        .cancel_token(cancel)
        .tool_loop_policy(ToolLoopPolicy::default())
        .middleware(Arc::new(DenySensitivePaths::new(working_dir)))
        .middleware(Arc::new(DenyOfflineNetwork))
        .middleware(Arc::new(VerifierReadOnlyShellGuard));
    if let Some(ctx) = inherited_context {
        builder = builder.hook(Arc::new(
            super::subagent_context::SubagentInheritedContextHook::new(ctx),
        ));
    }
    if max_rounds != 0 {
        builder = builder.max_rounds(max_rounds);
    }
    let agent = builder.build();
    let outcome =
        tokio::spawn(async move { agent.run_to_completion(prompt, AutoRespond::AllowAll).await })
            .await
            .map_err(|e| format!("blind verifier task failed: {e}"))?;

    if outcome.stop != StopReason::Stopped {
        let err_detail = outcome
            .error
            .unwrap_or_else(|| format!("stopped with {:?}", outcome.stop));
        return Ok(VerifierVerdict {
            passed: false,
            summary: format!("blind verifier did not complete cleanly ({err_detail})"),
            report: outcome.text.trim().to_string(),
        });
    }
    Ok(parse_verifier_verdict(&outcome.text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_shell_guard_allows_checks_and_blocks_mutations() {
        assert!(validate_verifier_shell_command("cargo test -p jeikcode-coding --locked").is_ok());
        assert!(validate_verifier_shell_command("cargo nextest run --workspace").is_ok());
        assert!(validate_verifier_shell_command("cargo fmt --all -- --check").is_ok());
        assert!(validate_verifier_shell_command("git diff --stat && git status --short").is_ok());
        assert!(validate_verifier_shell_command("python -m pytest tests/").is_ok());
        assert!(validate_verifier_shell_command("uv run pytest tests/").is_ok());
        assert!(validate_verifier_shell_command("uv run ruff check .").is_ok());
        assert!(validate_verifier_shell_command("uv run ruff format --check .").is_ok());
        assert!(validate_verifier_shell_command("uv run --extra dev pytest tests/").is_ok());
        assert!(
            validate_verifier_shell_command("uv --project sub run --with pytest pytest").is_ok()
        );
        assert!(validate_verifier_shell_command(
            "git -C C:/Users/depc/Projects/Personal/JeikCode status --short -uall"
        )
        .is_ok());
        assert!(
            validate_verifier_shell_command("git -c core.quotepath=false diff --stat HEAD").is_ok()
        );
        assert!(validate_verifier_shell_command("git -C /repo merge-base HEAD main").is_ok());
        assert!(validate_verifier_shell_command("cargo +nightly check --workspace").is_ok());
        assert!(validate_verifier_shell_command("dotnet test --no-restore").is_ok());
        assert!(validate_verifier_shell_command("gh pr view 90").is_ok());
        assert!(validate_verifier_shell_command("gh run list --limit 5").is_ok());
        assert!(validate_verifier_shell_command("rustc --version").is_ok());
        assert!(validate_verifier_shell_command("npx tsc --noEmit").is_ok());
        assert!(validate_verifier_shell_command(
            "dart format --output=none --set-exit-if-changed ."
        )
        .is_ok());
        assert!(validate_verifier_shell_command("npm run typecheck").is_ok());

        // Block mutating flags and write-emitting checks
        assert!(validate_verifier_shell_command("cargo clippy --fix --allow-dirty").is_err());
        assert!(validate_verifier_shell_command("ruff check --fix .").is_err());
        assert!(validate_verifier_shell_command("uv run ruff check --fix .").is_err());
        assert!(validate_verifier_shell_command("eslint --fix src/").is_err());
        assert!(validate_verifier_shell_command("npx eslint --fix src/").is_err());
        assert!(validate_verifier_shell_command("tsc").is_err());
        assert!(validate_verifier_shell_command("npx tsc").is_err());
        assert!(validate_verifier_shell_command("git -C /repo commit -m bad").is_err());
        assert!(validate_verifier_shell_command("git diff --output=patch.diff").is_err());
        assert!(validate_verifier_shell_command("gh pr merge 90").is_err());
        assert!(validate_verifier_shell_command("uv run ruff format .").is_err());
        assert!(validate_verifier_shell_command("uv pip install requests").is_err());
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

    #[test]
    fn parse_verifier_verdict_defaults_fail_and_parses_structured_report() {
        let pass = parse_verifier_verdict(
            "VERDICT: PASS\nSUMMARY: All unit tests passed.\nCHECKS_RUN:\n- cargo test: PASS\nISSUES:\n- none",
        );
        assert!(pass.passed);
        assert_eq!(pass.summary, "All unit tests passed.");

        let fail = parse_verifier_verdict(
            "VERDICT: NEEDS_WORK\nSUMMARY: 2 tests failing.\nCHECKS_RUN:\n- cargo test: FAIL\nISSUES:\n- src/lib.rs:10",
        );
        assert!(!fail.passed);
        assert_eq!(fail.summary, "2 tests failing.");

        let ambiguous = parse_verifier_verdict("Looks good to me!");
        assert!(!ambiguous.passed);
    }
}
