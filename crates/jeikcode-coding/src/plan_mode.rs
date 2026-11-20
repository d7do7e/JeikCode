//! Plan mode — read-only exploration, no edits.
//!
//! v1 exposed `/plan` (and `SetPlanMode`): the agent explores and presents a plan
//! WITHOUT mutating anything. This re-implements the ENFORCEMENT on the new stack as
//! a [`ToolMiddleware`] that, while active, blocks every `Risky` tool (the kernel's
//! own risk metadata already marks the mutating ones: write/edit/bash). Read-only
//! tools (read_file, grep, list_*, symbols, web, …) stay available.
//!
//! The flag is an `Arc<AtomicBool>` so `CodingRuntime::set_mode` can toggle it live
//! without a respawn — like the shared cwd handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use jeikcode_capabilities::tools::{
    ApprovalRequest, PermissionDecision, PermissionStore, APPROVAL_KIND,
};
use jeikcode_kernel::hook::LifecycleHooks;
use jeikcode_kernel::message::Conversation;
use jeikcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use jeikcode_kernel::request::RequestCtx;
use jeikcode_kernel::tool::{RiskLevel, Tool, ToolCall};

/// Enforces plan mode (read-only exploration) while active. Share the `Arc<AtomicBool>`
/// with the driver to toggle it live.
///
/// Policy while active (mirrors codex `readOnlyHint` + Claude Code's prompt):
/// - built-in **`Risky`** tools (bash/edit/write) → **hard-blocked** (plan's local
///   read-only guarantee — the model must present a plan first);
/// - **MCP tools declared `readOnlyHint: true`** → **allowed** (an external read-only
///   query has no side effects and is exactly what planning research needs);
/// - **other MCP tools** (mutating / unannotated, incl. `trust: true` servers) →
///   **prompt** instead of hard-block, so the user can allow a needed external call or
///   deny a risky one. A trusted server can't silently write here because we prompt
///   regardless of trust.
pub struct PlanModeGate {
    active: Arc<AtomicBool>,
    /// Session grants for mutating MCP tools the user approved "always" while in plan
    /// mode — keyed by the tool's full name so a repeat call skips the prompt. Supplied
    /// by `CodingParts` (shared, not rebuilt in `assemble`) so it survives a respawn /
    /// model-swap, matching how the write gate and approval middleware persist grants.
    mcp_grants: Arc<dyn PermissionStore>,
}

impl PlanModeGate {
    pub fn new(active: Arc<AtomicBool>, mcp_grants: Arc<dyn PermissionStore>) -> Self {
        Self { active, mcp_grants }
    }

    /// The hard-block message for a built-in mutating tool under plan mode.
    fn blocked(name: &str) -> BeforeOutcome {
        BeforeOutcome::deny(format!(
            "plan mode is active — `{name}` would modify the workspace and is blocked. Only \
             read-only tools are allowed: explore and present a plan for the user to approve \
             before making changes."
        ))
    }
}

#[async_trait]
impl ToolMiddleware for PlanModeGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        if !self.active.load(Ordering::Relaxed) {
            return BeforeOutcome::Proceed;
        }

        if call.name.starts_with("mcp__") {
            // A server-declared read-only external query or a read-only action on a
            // multiplex MCP tool (e.g. `vault_manage_board(action="get"|"trace_graph")`)
            // can't modify anything → proceed without prompting.
            if tool.read_only_hint()
                || jeikcode_capabilities::mcp::is_mcp_call_read_only(
                    &call.name,
                    false,
                    &call.arguments,
                )
            {
                return BeforeOutcome::Proceed;
            }
            // Mutating / unannotated MCP tool: prompt instead of hard-blocking. Owns the
            // decision (returns Allow/Deny) so the generic ApprovalMiddleware after it
            // never double-prompts — same pattern as the write gate.
            if self.mcp_grants.is_granted(&call.name) {
                return BeforeOutcome::Allow {
                    reason: Some("approved this session".into()),
                };
            }
            let payload = serde_json::to_value(ApprovalRequest {
                call_id: call.id.clone(),
                tool: tool.name().to_string(),
                args: call.arguments.clone(),
            })
            .unwrap_or(serde_json::Value::Null);
            return match PermissionDecision::from_value(&rt.request(APPROVAL_KIND, payload).await) {
                PermissionDecision::AllowOnce => BeforeOutcome::Allow {
                    reason: Some("approved once (plan mode)".into()),
                },
                PermissionDecision::AllowAlways => {
                    self.mcp_grants.grant(&call.name);
                    BeforeOutcome::Allow {
                        reason: Some("approved always (plan mode)".into()),
                    }
                }
                PermissionDecision::Deny => BeforeOutcome::deny(format!(
                    "plan mode: `{}` was not approved — present a plan and switch to build mode \
                     to run it.",
                    call.name
                )),
            };
        }

        // Built-in mutating tools (bash/edit/write) stay hard-blocked.
        if tool.risk(&call.arguments) == RiskLevel::Risky {
            return Self::blocked(&call.name);
        }
        BeforeOutcome::Proceed
    }
}

/// The standing plan-mode reminder BODY. Kept OUT of the system prompt (so toggling plan
/// mode never perturbs the cached prefix). [`PlanModeReminderHook`] appends it to the
/// current real user block (same placement as the date reminder) so the constraint is
/// the last thing in that turn's query. The [`PlanModeGate`] blocks mutating TOOLS, but
/// nothing stops the model from writing the implementation straight into its reply —
/// this keeps it planning.
const PLAN_MODE_REMINDER_BODY: &str = "\
PLAN MODE is active. Do NOT create, edit, or delete files, and do NOT write out the \
implementation — not even as code blocks in your reply. Investigate with read-only tools, \
then present a concise implementation plan and STOP, waiting for the user to review and \
switch to build mode. Writing the full solution now defeats the purpose of plan mode.";

const PLAN_MODE_NEEDLE: &str = "PLAN MODE is active";

/// Appends the wrapped [`PLAN_MODE_REMINDER_BODY`] to the current real user query
/// while plan mode is active. Stored on `turn_start` so tool-loop rounds do not
/// rewrite the prefix. Register this hook AFTER [`StatusReminderHook`] so PLAN MODE
/// sits below the date line and keeps recency.
pub struct PlanModeReminderHook {
    active: Arc<AtomicBool>,
}

impl PlanModeReminderHook {
    pub fn new(active: Arc<AtomicBool>) -> Self {
        Self { active }
    }
}

#[async_trait]
impl LifecycleHooks for PlanModeReminderHook {
    async fn turn_start(&self, convo: &mut Conversation) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let Some(query) = convo.active_turn_prompt_mut() else {
            return;
        };
        if query.text.contains(PLAN_MODE_NEEDLE) {
            return;
        }
        if !query.text.is_empty() {
            query.text.push_str("\n\n");
        }
        query
            .text
            .push_str(&jeikcode_capabilities::reminder::system_reminder(
                PLAN_MODE_REMINDER_BODY,
            ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeikcode_capabilities::tools::InMemoryPermissionStore;
    use jeikcode_kernel::message::Message;
    use jeikcode_kernel::testkit::{EchoTool, RiskyWriteTool};
    use jeikcode_kernel::tool::{ToolContext, ToolResult};

    fn rt() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, None)
    }

    fn grants() -> Arc<dyn PermissionStore> {
        Arc::new(InMemoryPermissionStore::new())
    }

    /// An rt whose approval requests time out fast (no driver answers in tests),
    /// degrading to `Null` → `Deny` — so the prompt path resolves instead of hanging.
    fn rt_timeout() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, Some(std::time::Duration::from_millis(20)))
    }

    /// A `Safe`, read-only tool with an `mcp__*` name — mimics an MCP tool the server
    /// annotated `readOnlyHint: true`.
    struct ReadOnlyMcpTool;
    #[async_trait]
    impl Tool for ReadOnlyMcpTool {
        fn name(&self) -> &str {
            "mcp__docs__query"
        }
        fn description(&self) -> &str {
            "read-only query"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn read_only_hint(&self) -> bool {
            true
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: String::new(),
                is_error: false,
                images: vec![],
            }
        }
    }

    #[tokio::test]
    async fn blocks_risky_only_when_active() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = PlanModeGate::new(flag.clone(), grants());
        let risky: Arc<dyn Tool> = Arc::new(RiskyWriteTool); // always Risky
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // Safe
        let mut call = ToolCall {
            id: "c".into(),
            name: "risky_write".into(),
            arguments: "{}".into(),
        };

        // Inactive: nothing blocked.
        assert!(!gate.before(&mut call, &risky, &rt()).await.is_deny());

        // Active: Risky blocked, Safe allowed.
        flag.store(true, Ordering::Relaxed);
        assert!(gate.before(&mut call, &risky, &rt()).await.is_deny());
        let mut safe_call = ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: "{}".into(),
        };
        assert!(!gate.before(&mut safe_call, &safe, &rt()).await.is_deny());
    }

    /// A read-only MCP tool (`readOnlyHint: true`) is ALLOWED in plan mode — an external
    /// read-only query can't modify anything, and it's exactly what planning research needs.
    #[tokio::test]
    async fn read_only_mcp_allowed_in_plan_mode() {
        let flag = Arc::new(AtomicBool::new(true));
        let gate = PlanModeGate::new(flag, grants());
        let ro: Arc<dyn Tool> = Arc::new(ReadOnlyMcpTool);
        let mut call = ToolCall {
            id: "c".into(),
            name: "mcp__docs__query".into(),
            arguments: "{}".into(),
        };
        let out = gate.before(&mut call, &ro, &rt()).await;
        assert!(
            !out.is_deny(),
            "read-only MCP must be allowed in plan mode, got {out:?}"
        );
    }

    /// A mutating / unannotated MCP tool is NOT hard-blocked in plan mode — it PROMPTS
    /// (Claude Code parity). With no driver to answer, the prompt times out → deny, but
    /// the deny reason proves it went through the approval path ("not approved"), not the
    /// hard-block path ("blocked").
    #[tokio::test]
    async fn mutating_mcp_prompts_not_hard_blocked_in_plan_mode() {
        let flag = Arc::new(AtomicBool::new(true));
        let gate = PlanModeGate::new(flag, grants());
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // read_only_hint()==false, mcp__ name
        let mut call = ToolCall {
            id: "c".into(),
            name: "mcp__docs__delete".into(),
            arguments: "{}".into(),
        };
        let out = gate.before(&mut call, &safe, &rt_timeout()).await;
        assert!(out.is_deny(), "un-answered prompt degrades to deny");
        assert!(
            out.deny_reason().unwrap().contains("not approved"),
            "must reach the PROMPT path, not the hard-block path: {:?}",
            out.deny_reason()
        );
    }

    /// The "always allow" grant lives in a store supplied by `CodingParts`, so it
    /// survives a respawn (model-swap / MCP-reload) that rebuilds the gate. Two gates
    /// sharing ONE store model that: a grant seen by the second gate skips the prompt.
    #[tokio::test]
    async fn always_grant_survives_gate_rebuild_via_shared_store() {
        let flag = Arc::new(AtomicBool::new(true));
        let store = grants();
        // First gate records an "always" grant (as the AllowAlways arm would).
        store.grant("mcp__docs__delete");
        // A freshly-built gate (post-respawn) sharing the SAME store honors it — and
        // short-circuits BEFORE rt.request, so the no-timeout rt() can't hang.
        let gate = PlanModeGate::new(flag, store);
        let safe: Arc<dyn Tool> = Arc::new(EchoTool);
        let mut call = ToolCall {
            id: "c".into(),
            name: "mcp__docs__delete".into(),
            arguments: "{}".into(),
        };
        let out = gate.before(&mut call, &safe, &rt()).await;
        assert!(
            matches!(out, BeforeOutcome::Allow { .. }),
            "shared grant must skip the prompt after a respawn, got {out:?}"
        );
    }

    #[tokio::test]
    async fn reminder_appends_to_the_real_query_when_active() {
        let flag = Arc::new(AtomicBool::new(false));
        let hook = PlanModeReminderHook::new(flag.clone());
        let mut convo = Conversation::default();
        convo.messages = vec![Message::system("sys"), Message::user("hi")];
        let before = convo.messages.clone();

        hook.turn_start(&mut convo).await;
        assert_eq!(
            convo.messages, before,
            "build mode must not inject a plan reminder"
        );

        flag.store(true, Ordering::Relaxed);
        hook.turn_start(&mut convo).await;
        hook.turn_start(&mut convo).await;
        assert_eq!(
            convo.messages.len(),
            2,
            "must not insert a separate user block"
        );
        assert_eq!(convo.messages[0], before[0], "system prefix stays frozen");
        assert!(!convo.messages[1].synthetic);
        assert!(
            convo.messages[1]
                .text
                .starts_with("hi\n\n<system-reminder>")
                && convo.messages[1].text.contains(PLAN_MODE_NEEDLE)
                && convo.messages[1].text.to_lowercase().contains("stop"),
            "plan reminder is a tail on the real query: {:?}",
            convo.messages[1].text
        );
        assert_eq!(
            convo.messages[1].text.matches("<system-reminder>").count(),
            1,
            "idempotent: {:?}",
            convo.messages[1].text
        );
    }

    #[tokio::test]
    async fn reminder_preserves_prefix_during_autonomous_and_assistant_terminated_turns() {
        let flag = Arc::new(AtomicBool::new(true));
        let hook = PlanModeReminderHook::new(flag);

        // Assistant-terminated conversation must be a no-op so earlier user messages
        // inside sacred_floor are never mutated.
        let mut terminated = Conversation::default();
        terminated.messages = vec![
            Message::system("sys"),
            Message::user("first task"),
            Message::assistant("done", vec![]),
        ];
        let before_terminated = terminated.messages.clone();
        hook.turn_start(&mut terminated).await;
        assert_eq!(terminated.messages, before_terminated);

        // Autonomous synthetic_turn after assistant history gets the reminder on the
        // trailing synthetic_turn; earlier real user prompt stays byte-identical.
        let mut auto = Conversation::default();
        auto.messages = vec![
            Message::system("sys"),
            Message::user("first task"),
            Message::assistant("done", vec![]),
            Message::synthetic_turn("continue goal"),
        ];
        let frozen_prefix = auto.messages[..3].to_vec();
        hook.turn_start(&mut auto).await;
        assert_eq!(
            &auto.messages[..3],
            &frozen_prefix[..],
            "earlier prefix must stay byte-identical"
        );
        assert!(
            auto.messages[3]
                .text
                .starts_with("continue goal\n\n<system-reminder>")
                && auto.messages[3].text.contains(PLAN_MODE_NEEDLE),
            "plan reminder must land on active synthetic_turn: {:?}",
            auto.messages[3].text
        );
    }

    #[tokio::test]
    async fn plan_mode_auto_allows_vault_manage_board_read_actions_and_gates_writes() {
        struct MultiplexBoardMcpTool;
        #[async_trait]
        impl Tool for MultiplexBoardMcpTool {
            fn name(&self) -> &str {
                "mcp__datumref__vault_manage_board"
            }
            fn description(&self) -> &str {
                "multiplex board tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
                ToolResult::default()
            }
        }

        let flag = Arc::new(AtomicBool::new(true));
        let gate = PlanModeGate::new(flag, grants());
        let tool: Arc<dyn Tool> = Arc::new(MultiplexBoardMcpTool);

        for action in [
            "get",
            "list",
            "list_boards",
            "trace_graph",
            "search",
            "status",
            "validate",
        ] {
            let mut call = ToolCall {
                id: "c1".into(),
                name: "mcp__datumref__vault_manage_board".into(),
                arguments:
                    serde_json::json!({ "action": action, "asset_id": "JeikCode_dblueprint" })
                        .to_string(),
            };
            let out = gate.before(&mut call, &tool, &rt()).await;
            assert_eq!(
                out,
                BeforeOutcome::Proceed,
                "read-only action `{action}` must auto-allow in plan mode without prompting"
            );
        }

        for mutating in [
            "batch_upsert",
            "auto_layout",
            "create",
            "delete",
            "update",
            "clear",
        ] {
            let mut call = ToolCall {
                id: "c2".into(),
                name: "mcp__datumref__vault_manage_board".into(),
                arguments:
                    serde_json::json!({ "action": mutating, "asset_id": "JeikCode_dblueprint" })
                        .to_string(),
            };
            let out = gate.before(&mut call, &tool, &rt_timeout()).await;
            assert!(
                out.is_deny(),
                "mutating action `{mutating}` must remain gated in plan mode: {out:?}"
            );
        }
    }
}
