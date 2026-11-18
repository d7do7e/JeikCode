//! Two-stage `/goal` verification harness combining:
//! 1. Fast text-only evaluator (`controllers::evaluate_goal`)
//! 2. Change-driven git-tree fingerprint + blind fresh-context `verifier` subagent gate
//!    (`jeikcode_capabilities::tools::run_blind_verifier`)

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::Stream;
use jeikcode_capabilities::codeintel::{register_codeintel_tools_for_subagent, CodeIntelMode};
use jeikcode_capabilities::session::instructions::render_project_instructions;
use jeikcode_capabilities::tools::{
    register_coding_tools_with_vision_and_bash_state, run_blind_verifier_with_context,
    BashRuntimeState, InheritedSubagentContext,
};
use jeikcode_kernel::message::{Message, SessionSnapshot};
use jeikcode_kernel::provider::{ChatOptions, LlmProvider};
use jeikcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use jeikcode_kernel::tool::{MountedTools, ToolDef, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// Lightweight provider wrapper that accumulates `StreamEvent::Usage` across every
/// blind-verifier round so verifier token spend is folded into `EvalOutcome::usage`
/// and enforced by `GoalState::tokens_used` / `goal_max_tokens` (#57).
struct TokenMeteringProvider {
    inner: Arc<dyn LlmProvider>,
    totals: Arc<Mutex<TokenUsage>>,
}

impl TokenMeteringProvider {
    fn new(inner: Arc<dyn LlmProvider>) -> (Arc<Self>, Arc<Mutex<TokenUsage>>) {
        let totals = Arc::new(Mutex::new(TokenUsage::default()));
        (
            Arc::new(Self {
                inner,
                totals: Arc::clone(&totals),
            }),
            totals,
        )
    }
}

struct MeteringStream {
    inner: BoxStream<'static, StreamEvent>,
    totals: Arc<Mutex<TokenUsage>>,
    round_usage: TokenUsage,
    flushed: bool,
}

impl MeteringStream {
    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        if self.round_usage.prompt > 0
            || self.round_usage.completion > 0
            || self.round_usage.cached > 0
        {
            if let Ok(mut g) = self.totals.lock() {
                g.prompt = g.prompt.saturating_add(self.round_usage.prompt);
                g.completion = g.completion.saturating_add(self.round_usage.completion);
                g.cached = g.cached.saturating_add(self.round_usage.cached);
            }
        }
    }
}

impl Drop for MeteringStream {
    fn drop(&mut self) {
        self.flush();
    }
}

impl Stream for MeteringStream {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(ev)) => {
                if let StreamEvent::Usage(u) = &ev {
                    self.round_usage.merge_max(*u);
                }
                Poll::Ready(Some(ev))
            }
            Poll::Ready(None) => {
                self.flush();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[async_trait]
impl LlmProvider for TokenMeteringProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }
    fn bind_session_id(&self, session_id: &str) {
        self.inner.bind_session_id(session_id);
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let inner = self.inner.chat_stream(messages, tools, options).await?;
        Ok(Box::pin(MeteringStream {
            inner,
            totals: Arc::clone(&self.totals),
            round_usage: TokenUsage::default(),
            flushed: false,
        }))
    }
}

use crate::controllers::{evaluate_goal, summarize_for_goal, EvalOutcome, GoalResult};
use crate::goal_harness::{
    compute_git_diff_stat_async, compute_workspace_fingerprint_async,
    create_milestone_checkpoint_async, GoalOptions, WorkspaceFingerprint,
};

const BLIND_VERIFIER_MAX_ROUNDS: u32 = 24;

/// Inputs captured off the runtime loop before spawning asynchronous `/goal` evaluation.
pub(crate) struct GoalEvalJob {
    pub generation: u64,
    pub controller_id: u64,
    pub condition: String,
    pub previous_reason: Option<String>,
    pub snapshot: Arc<SessionSnapshot>,
    pub working_dir: PathBuf,
    pub bash_runtime: Arc<BashRuntimeState>,
    pub evaluator_provider: Arc<dyn LlmProvider>,
    pub make_verifier_provider: Box<dyn FnOnce() -> Option<Arc<dyn LlmProvider>> + Send>,
    pub baseline_fingerprint: Option<String>,
    pub baseline_fingerprint_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    pub options: GoalOptions,
    pub round: u32,
    pub checkpoint_requested: bool,
    pub cancel: CancellationToken,
}

/// Evaluate one `/goal` round with change-driven fingerprint verification and blind
/// fresh-context `verifier` gating when workspace edits occurred (or `--verify` is set).
pub(crate) async fn evaluate_goal_with_harness(mut job: GoalEvalJob) -> EvalOutcome {
    let summary = summarize_for_goal(&job.snapshot.messages, job.previous_reason.as_deref());
    let had_snapshot_mutations = snapshot_has_file_mutations(&job.snapshot.messages);
    let baseline_wait = async {
        if let Some(hash) = job.baseline_fingerprint {
            return Some(hash);
        }
        let mut rx = job.baseline_fingerprint_rx?;
        let ready = rx.borrow().clone();
        if ready.is_some() {
            return ready;
        }
        let wait_res = tokio::select! {
            biased;
            _ = job.cancel.cancelled() => Err(()),
            changed = tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed()) => {
                changed.ok().and_then(|r| r.ok()).ok_or(())
            }
        };
        if wait_res.is_ok() {
            rx.borrow().clone()
        } else {
            None
        }
    };
    let (baseline_fingerprint, fp_before, mut outcome) = tokio::join!(
        baseline_wait,
        compute_workspace_fingerprint_async(job.working_dir.clone()),
        evaluate_goal(
            job.generation,
            job.controller_id,
            job.evaluator_provider,
            job.condition.clone(),
            summary,
            job.cancel.clone(),
        )
    );
    job.baseline_fingerprint = baseline_fingerprint;

    outcome.fingerprint = Some(fp_before.clone());
    outcome.had_snapshot_mutations = had_snapshot_mutations;

    let fast_reason = match &outcome.result {
        GoalResult::Met(reason) => reason.clone(),
        GoalResult::NotMet(_) => {
            if job.checkpoint_requested {
                if let Ok(Some(cp_ref)) = create_milestone_checkpoint_async(
                    job.working_dir.clone(),
                    job.controller_id,
                    job.round.saturating_add(1),
                )
                .await
                {
                    outcome.checkpoint_ref = Some(cp_ref);
                }
            }
            return outcome;
        }
        GoalResult::Error(_) => return outcome,
    };

    if !should_run_blind_verifier(
        &job.options,
        &fp_before,
        job.baseline_fingerprint.as_deref(),
        had_snapshot_mutations,
    ) {
        return outcome;
    }

    let Some(raw_verifier_provider) = (job.make_verifier_provider)() else {
        return outcome;
    };
    let (metered_verifier, verifier_totals) = TokenMeteringProvider::new(raw_verifier_provider);

    let verifier_tools = build_verifier_tools(Arc::clone(&job.bash_runtime));
    let project_instructions = render_project_instructions(&job.working_dir);
    let ctx_hook = jeikcode_capabilities::session::SessionContextHook::new(&job.working_dir);
    let inherited_context = InheritedSubagentContext {
        memory: jeikcode_capabilities::memory::MemoryHook::for_project(&job.working_dir)
            .render_block(),
        user_preferences: ctx_hook.render_user_preferences_block(),
        project_instructions: ctx_hook
            .render_project_instructions_block()
            .or_else(|| project_instructions.clone()),
    };
    let diff_stat = compute_git_diff_stat_async(job.working_dir.clone()).await;
    let prompt = build_blind_verifier_prompt(
        &job.condition,
        &fp_before,
        diff_stat.as_deref(),
        &fast_reason,
    );

    let verdict = match run_blind_verifier_with_context(
        metered_verifier,
        verifier_tools,
        job.working_dir.clone(),
        prompt,
        project_instructions,
        Some(inherited_context),
        BLIND_VERIFIER_MAX_ROUNDS,
        job.cancel,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            merge_usage_totals(&mut outcome.usage, &verifier_totals);
            outcome.result = GoalResult::NotMet(format!(
                "blind verifier failed to run ({err}); refusing unverified completion"
            ));
            return outcome;
        }
    };
    merge_usage_totals(&mut outcome.usage, &verifier_totals);

    // Re-compute workspace fingerprint after verification to reject stale passes if files
    // changed mid-verification (or if the verifier somehow violated read-only invariants).
    let fp_after = compute_workspace_fingerprint_async(job.working_dir.clone()).await;
    if fp_after.hash != fp_before.hash {
        outcome.fingerprint = Some(fp_after);
        outcome.result = GoalResult::NotMet(
            "workspace state changed during blind verification; re-verify on the latest tree"
                .to_string(),
        );
        return outcome;
    }

    outcome.verifier_report = Some(verdict.report.clone());
    if !verdict.passed {
        outcome.result = GoalResult::NotMet(format!(
            "blind verifier rejected completion (NEEDS_WORK): {}\n\n{}",
            verdict.summary, verdict.report
        ));
        return outcome;
    }

    if job.options.checkpoint_milestones {
        if let Ok(Some(cp_ref)) = create_milestone_checkpoint_async(
            job.working_dir,
            job.controller_id,
            job.round.saturating_add(1),
        )
        .await
        {
            outcome.checkpoint_ref = Some(cp_ref);
        }
    }

    outcome.verified_fingerprint = Some(fp_after.hash);
    outcome.result = GoalResult::Met(format!(
        "{fast_reason} [blind-verified: {}]",
        verdict.summary
    ));
    outcome
}

fn should_run_blind_verifier(
    options: &GoalOptions,
    fp: &WorkspaceFingerprint,
    baseline_fingerprint: Option<&str>,
    had_snapshot_mutations: bool,
) -> bool {
    if options.skip_verifier {
        return false;
    }
    if options.force_verifier || had_snapshot_mutations {
        return true;
    }
    match baseline_fingerprint {
        Some(base) => base != fp.hash,
        None => fp.has_workspace_changes(),
    }
}

fn merge_usage_totals(dest: &mut Option<TokenUsage>, added: &Arc<Mutex<TokenUsage>>) {
    let Ok(u) = added.lock() else {
        return;
    };
    if u.prompt == 0 && u.completion == 0 && u.cached == 0 {
        return;
    }
    match dest {
        Some(existing) => {
            existing.prompt = existing.prompt.saturating_add(u.prompt);
            existing.completion = existing.completion.saturating_add(u.completion);
            existing.cached = existing.cached.saturating_add(u.cached);
        }
        None => *dest = Some(*u),
    }
}

fn build_blind_verifier_prompt(
    condition: &str,
    fp: &WorkspaceFingerprint,
    diff_stat: Option<&str>,
    fast_reason: &str,
) -> String {
    let files_section = if fp.changed_files.is_empty() {
        "(clean git tree / no listed changed files — inspect repository tests and target files directly)".to_string()
    } else {
        fp.changed_files
            .iter()
            .take(40)
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let diff_stat_section = diff_stat
        .map(|stat| format!("\nDiff summary (`git diff --stat HEAD`):\n```\n{stat}\n```\n"))
        .unwrap_or_default();
    let head = fp.head_sha.as_deref().unwrap_or("none");
    format!(
        "Independently verify whether the workspace satisfies this goal.\n\n\
         Goal condition:\n{condition}\n\n\
         Workspace fingerprint: {} (HEAD: {head})\n\
         Changed/untracked files:\n{files_section}\n\
         {diff_stat_section}\n\
         Note: The main agent believes the goal is complete ({fast_reason}). \
         Do NOT trust this claim — inspect the actual code and run real build/test/check commands.",
        fp.hash
    )
}

fn build_verifier_tools(bash_runtime: Arc<BashRuntimeState>) -> MountedTools {
    let mut reg = ToolRegistry::new();
    register_coding_tools_with_vision_and_bash_state(&mut reg, false, bash_runtime);
    let codeintel_mode = CodeIntelMode::from_env_or_config(
        std::env::var("JEIKCODE_CODEINTEL_MODE").ok().as_deref(),
        None,
    );
    register_codeintel_tools_for_subagent(&mut reg, &codeintel_mode);
    reg.mount(&["read", "code_explore", "run_command", "grep", "glob"])
}

pub(crate) fn snapshot_has_file_mutations(messages: &[Message]) -> bool {
    let start = messages
        .iter()
        .rposition(|m| {
            m.role == jeikcode_kernel::message::Role::User
                && (!m.synthetic || m.is_compaction_turn_start())
        })
        .unwrap_or(0);
    messages[start..].iter().any(|m| {
        m.tool_calls.iter().any(|call| {
            matches!(
                call.name.as_str(),
                "write"
                    | "write_file"
                    | "edit"
                    | "edit_file"
                    | "global_search_replace"
                    | "search_replace"
                    | "parallel_edit_files"
                    | "create_file"
            ) || (call.name.eq_ignore_ascii_case("task")
                && call.arguments.to_ascii_lowercase().contains("worker"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;
    use jeikcode_kernel::provider::ChatOptions;
    use jeikcode_kernel::stream::{ProviderError, StreamEvent};
    use jeikcode_kernel::testkit::MockProvider;
    use jeikcode_kernel::tool::{ToolCall, ToolDef};
    use std::sync::Mutex;

    struct VerifierMockProvider {
        responses: Mutex<Vec<Vec<StreamEvent>>>,
        seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl LlmProvider for VerifierMockProvider {
        fn model_name(&self) -> &str {
            "mock-verifier"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.seen_tools
                .lock()
                .unwrap()
                .push(tools.iter().map(|t| t.name.clone()).collect());
            let next = {
                let mut g = self.responses.lock().unwrap();
                if g.is_empty() {
                    vec![
                        StreamEvent::TextDelta("VERDICT: NEEDS_WORK\nSUMMARY: exhausted".into()),
                        StreamEvent::Done { truncated: false },
                    ]
                } else {
                    g.remove(0)
                }
            };
            Ok(stream::iter(next).boxed())
        }
    }

    fn edit_snapshot() -> Arc<SessionSnapshot> {
        Arc::new(SessionSnapshot::new(vec![
            Message::user("make lib.rs return 42"),
            Message::assistant(
                "updated lib.rs",
                vec![ToolCall {
                    id: "c1".into(),
                    name: "edit".into(),
                    arguments: r#"{"file_path":"lib.rs"}"#.into(),
                }],
            ),
        ]))
    }

    #[tokio::test]
    async fn blind_verifier_blocks_premature_fast_evaluator_met_and_forbids_write_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn answer() -> i32 { 0 }\n").unwrap();

        let fast_eval = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("Verdict: yes looks done".into()),
            StreamEvent::Done { truncated: false },
        ]]));

        let seen_tools = Arc::new(Mutex::new(Vec::new()));
        let verifier = Arc::new(VerifierMockProvider {
            responses: Mutex::new(vec![vec![
                StreamEvent::TextDelta(
                    "VERDICT: NEEDS_WORK\nSUMMARY: lib.rs still returns 0 instead of 42\nCHECKS_RUN:\n- read lib.rs: FAIL\nISSUES:\n- lib.rs:1 returns 0".into(),
                ),
                StreamEvent::Done { truncated: false },
            ]]),
            seen_tools: seen_tools.clone(),
        });

        let outcome = evaluate_goal_with_harness(GoalEvalJob {
            generation: 1,
            controller_id: 9,
            condition: "make answer() return 42".into(),
            previous_reason: None,
            snapshot: edit_snapshot(),
            working_dir: dir.path().to_path_buf(),
            bash_runtime: Arc::new(BashRuntimeState::new()),
            evaluator_provider: fast_eval,
            make_verifier_provider: Box::new(move || Some(verifier)),
            baseline_fingerprint: None,
            baseline_fingerprint_rx: None,
            options: GoalOptions::default(),
            round: 0,
            checkpoint_requested: false,
            cancel: CancellationToken::new(),
        })
        .await;

        match outcome.result {
            GoalResult::NotMet(reason) => {
                assert!(
                    reason.contains("NEEDS_WORK") && reason.contains("lib.rs still returns 0"),
                    "unexpected NotMet reason: {reason}"
                );
            }
            other => panic!(
                "expected blind verifier to block Met, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        let mounted = seen_tools.lock().unwrap();
        assert!(!mounted.is_empty());
        assert!(!mounted[0].iter().any(|t| t == "write" || t == "edit"));
        assert!(mounted[0].iter().any(|t| t == "run_command"));
    }

    #[tokio::test]
    async fn blind_verifier_passes_when_verified_and_records_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn answer() -> i32 { 42 }\n").unwrap();

        let fast_eval = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("Verdict: yes done".into()),
            StreamEvent::Done { truncated: false },
        ]]));

        let seen_tools = Arc::new(Mutex::new(Vec::new()));
        let verifier = Arc::new(VerifierMockProvider {
            responses: Mutex::new(vec![vec![
                StreamEvent::TextDelta(
                    "VERDICT: PASS\nSUMMARY: answer() returns 42\nCHECKS_RUN:\n- read lib.rs: PASS\nISSUES:\n- none".into(),
                ),
                StreamEvent::Done { truncated: false },
            ]]),
            seen_tools,
        });

        let outcome = evaluate_goal_with_harness(GoalEvalJob {
            generation: 1,
            controller_id: 9,
            condition: "make answer() return 42".into(),
            previous_reason: None,
            snapshot: edit_snapshot(),
            working_dir: dir.path().to_path_buf(),
            bash_runtime: Arc::new(BashRuntimeState::new()),
            evaluator_provider: fast_eval,
            make_verifier_provider: Box::new(move || Some(verifier)),
            baseline_fingerprint: None,
            baseline_fingerprint_rx: None,
            options: GoalOptions::default(),
            round: 0,
            checkpoint_requested: false,
            cancel: CancellationToken::new(),
        })
        .await;

        match outcome.result {
            GoalResult::Met(reason) => {
                assert!(
                    reason.contains("blind-verified: answer() returns 42"),
                    "{reason}"
                );
            }
            _ => panic!("expected GoalResult::Met"),
        }
        assert!(outcome.verified_fingerprint.is_some());
    }
}
