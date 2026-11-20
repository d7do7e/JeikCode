//! Unit and integration tests for Stage 1 Sub-Agent v2:
//! - Read-only blind `verifier` role & `VerifierReadOnlyShellGuard`
//! - Resumable subagents via `resume_agent_id`
//! - Async/background dispatch via `run_in_background` + `TaskOutputTool`

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use jeikcode_kernel::message::{Message, Role};
use jeikcode_kernel::provider::{ChatOptions, LlmProvider};
use jeikcode_kernel::stream::{ProviderError, StreamEvent};
use jeikcode_kernel::tool::{
    ProgressSink, RiskLevel, Tool, ToolCall, ToolContext, ToolDef, ToolRegistry,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{BashTool, EditFileTool, ReadFileTool, TaskTool, WriteFileTool};

struct ScriptedProvider {
    model: &'static str,
    rounds: Mutex<Vec<Vec<StreamEvent>>>,
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
    seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
}

impl ScriptedProvider {
    fn new(
        rounds: Vec<Vec<StreamEvent>>,
        seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
        seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
    ) -> Self {
        Self {
            model: "mock-subagent-v2",
            rounds: Mutex::new(rounds),
            seen_messages,
            seen_tools,
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn model_name(&self) -> &str {
        self.model
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _opts: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.seen_messages.lock().unwrap().push(messages.to_vec());
        self.seen_tools
            .lock()
            .unwrap()
            .push(tools.iter().map(|t| t.name.clone()).collect());
        let next = {
            let mut g = self.rounds.lock().unwrap();
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

fn tool_ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        working_dir: dir.to_path_buf(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    }
}

fn build_test_registry() -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool::new(false)));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(EditFileTool));
    reg.register(Arc::new(BashTool));
    Arc::new(reg)
}

fn extract_attr(xml: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = xml.find(&needle)? + needle.len();
    let end = xml[start..].find('"')?;
    Some(xml[start..start + end].to_string())
}

#[tokio::test]
async fn verifier_role_is_read_only_allows_checks_and_emits_structured_verdict() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("src_sample.txt"),
        "fn answer() -> i32 { 42 }\n",
    )
    .unwrap();

    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();
    let r_v = reg.clone();

    let seen_msgs = Arc::new(Mutex::new(Vec::new()));
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let sm = seen_msgs.clone();
    let st = seen_tools.clone();

    let make_provider = move || {
        Arc::new(ScriptedProvider::new(
            vec![
                // Round 1: read file + attempt disallowed shell write + allowed read-only git/echo check
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "v1".into(),
                        name: "read".into(),
                        arguments: r#"{"path":"src_sample.txt"}"#.into(),
                    }),
                    StreamEvent::ToolCall(ToolCall {
                        id: "v2".into(),
                        name: "run_command".into(),
                        arguments: r#"{"command":"echo hacked > pwn.txt"}"#.into(),
                    }),
                    StreamEvent::ToolCall(ToolCall {
                        id: "v3".into(),
                        name: "run_command".into(),
                        arguments: r#"{"command":"echo check-ok"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                // Round 2: final structured verdict
                vec![
                    StreamEvent::TextDelta(
                        "VERDICT: PASS\nSUMMARY: Checked src_sample.txt and check-ok.\nCHECKS_RUN:\n- echo check-ok: PASS\nISSUES:\n- none".into(),
                    ),
                    StreamEvent::Done { truncated: false },
                ],
            ],
            sm.clone(),
            st.clone(),
        )) as Arc<dyn LlmProvider>
    };

    let tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read", "write", "edit", "run_command"]),
    )
    .with_verifier_tools(move || r_v.mount(&["read", "run_command"]));

    let args = r#"{
        "tasks": [{
            "description": "verify sample",
            "prompt": "Verify src_sample.txt returns 42.",
            "subagent_type": "verifier"
        }]
    }"#;

    assert_eq!(tool.risk(args), RiskLevel::Safe);
    let out = tool.execute(args, &tool_ctx(dir.path())).await;
    assert!(!out.is_error, "verifier failed: {}", out.content);
    assert!(out.content.contains("VERDICT: PASS"), "{}", out.content);
    assert!(
        !dir.path().join("pwn.txt").exists(),
        "verifier shell redirect must be blocked"
    );

    let tools_list = seen_tools.lock().unwrap();
    assert!(!tools_list.is_empty());
    assert!(
        !tools_list[0].iter().any(|t| t == "write" || t == "edit"),
        "verifier must not mount write/edit: {:?}",
        tools_list[0]
    );
    assert!(tools_list[0].iter().any(|t| t == "run_command"));

    let msgs = seen_msgs.lock().unwrap();
    let sys_prompt = msgs[0]
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(sys_prompt.contains("BLIND, FRESH-CONTEXT, READ-ONLY VERIFIER"));

    // Verify the blocked shell redirect returned an error tool message to the model
    let second_round = &msgs[1];
    assert!(
        second_round.iter().any(|m| {
            m.role == Role::Tool && m.is_error && m.text.contains("verifier subagent is read-only")
        }),
        "expected read-only shell guard error in round 2: {second_round:?}"
    );
}

#[tokio::test]
async fn resume_agent_id_restores_history_preserves_scope_and_enforces_session_role_guards() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("allowed.txt"), "initial").unwrap();

    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();

    let seen_msgs = Arc::new(Mutex::new(Vec::new()));
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let call_idx = Arc::new(AtomicUsize::new(0));
    let sm = seen_msgs.clone();
    let st = seen_tools.clone();
    let ci = call_idx.clone();

    let make_provider = move || {
        let n = ci.fetch_add(1, Ordering::SeqCst);
        let rounds = if n == 0 {
            // Turn 1 (worker): edit allowed.txt and stop
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "w1".into(),
                        name: "write".into(),
                        arguments: r#"{"path":"allowed.txt","content":"step1"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("TURN1_MEMORY_TOKEN".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ]
        } else {
            // Turn 2 (resumed worker): try out-of-scope write (must still be blocked!), then finish
            vec![
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "w2".into(),
                        name: "write".into(),
                        arguments: r#"{"path":"forbidden.txt","content":"bad"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ],
                vec![
                    StreamEvent::TextDelta("TURN2_RESUMED_DONE".into()),
                    StreamEvent::Done { truncated: false },
                ],
            ]
        };
        Arc::new(ScriptedProvider::new(rounds, sm.clone(), st.clone())) as Arc<dyn LlmProvider>
    };

    let tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read", "write"]),
    );
    tool.bind_session("sess-A");

    let args1 = r#"{
        "tasks": [{
            "description": "step 1",
            "prompt": "write step1 to allowed.txt",
            "subagent_type": "worker",
            "scope": ["allowed.txt"]
        }]
    }"#;
    let out1 = tool.execute(args1, &tool_ctx(dir.path())).await;
    assert!(!out1.is_error, "turn 1 failed: {}", out1.content);
    let agent_id = extract_attr(&out1.content, "agent_id").expect("agent_id in <task> header");

    // Resume WITHOUT repeating subagent_type or scope — must inherit worker + scope AND detect Risky!
    let args2 = serde_json::json!({
        "tasks": [{
            "description": "step 2 resume",
            "prompt": "continue from turn 1",
            "resume_agent_id": agent_id
        }]
    })
    .to_string();
    assert_eq!(
        tool.risk(&args2),
        RiskLevel::Risky,
        "resuming a saved worker must remain Risky even when subagent_type is omitted"
    );

    let out2 = tool.execute(&args2, &tool_ctx(dir.path())).await;
    assert!(!out2.is_error, "turn 2 failed: {}", out2.content);
    assert!(out2.content.contains("TURN2_RESUMED_DONE"));
    assert!(
        !dir.path().join("forbidden.txt").exists(),
        "resumed worker must still enforce saved WorkerScopeGate"
    );

    // Check that resumed call's provider received Turn 1 messages!
    let msgs = seen_msgs.lock().unwrap();
    let resumed_req = &msgs[2];
    assert!(
        resumed_req
            .iter()
            .any(|m| m.text.contains("TURN1_MEMORY_TOKEN")),
        "resumed child must retain prior assistant history: {resumed_req:?}"
    );
    assert!(
        resumed_req
            .iter()
            .any(|m| m.text.contains("continue from turn 1")),
        "resumed child must carry new prompt"
    );

    // Role mismatch rejected
    let bad_role = serde_json::json!({
        "tasks": [{
            "description": "wrong role",
            "prompt": "try explore",
            "subagent_type": "explore",
            "resume_agent_id": agent_id
        }]
    })
    .to_string();
    let bad_role_out = tool.execute(&bad_role, &tool_ctx(dir.path())).await;
    assert!(bad_role_out.is_error);
    assert!(bad_role_out
        .content
        .contains("saved subagent role is `worker`"));

    // Cross-session resume rejected
    tool.bind_session("sess-B");
    let cross_sess_out = tool.execute(&args2, &tool_ctx(dir.path())).await;
    assert!(cross_sess_out.is_error);
    assert!(cross_sess_out
        .content
        .contains("belongs to a different session"));
}

#[tokio::test]
async fn background_dispatch_runs_async_collects_via_task_output_and_cancels_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();

    let gate = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());

    struct GatedProvider {
        gate: Arc<Notify>,
        started: Arc<Notify>,
        reply: &'static str,
    }

    #[async_trait]
    impl LlmProvider for GatedProvider {
        fn model_name(&self) -> &str {
            "gated-mock"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _opts: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.started.notify_waiters();
            self.gate.notified().await;
            let evs = vec![
                StreamEvent::TextDelta(self.reply.to_string()),
                StreamEvent::Done { truncated: false },
            ];
            Ok(stream::iter(evs).boxed())
        }
    }

    let g1 = gate.clone();
    let s1 = started.clone();
    let make_provider = move || {
        Arc::new(GatedProvider {
            gate: g1.clone(),
            started: s1.clone(),
            reply: "ASYNC_BG_FINISHED",
        }) as Arc<dyn LlmProvider>
    };

    let task_tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read", "write"]),
    );
    let output_tool = task_tool.output_tool();
    assert_eq!(output_tool.risk("{}"), RiskLevel::Safe);

    let bg_args = r#"{
        "run_in_background": true,
        "tasks": [{
            "description": "bg explore",
            "prompt": "scan repo",
            "subagent_type": "explore"
        }]
    }"#;
    let ctx = tool_ctx(dir.path());
    let spawn_res = tokio::time::timeout(Duration::from_secs(2), task_tool.execute(bg_args, &ctx))
        .await
        .expect("background dispatch must return immediately without awaiting child");
    assert!(!spawn_res.is_error, "{}", spawn_res.content);
    assert!(spawn_res.content.contains("<task_spawned"));
    let task_id = extract_attr(&spawn_res.content, "task_id").expect("task_id present");
    let agent_id = extract_attr(&spawn_res.content, "agent_id").expect("agent_id present");

    // Non-blocking poll while still running
    started.notified().await;
    let poll = output_tool
        .execute(
            &serde_json::json!({"task_id": task_id, "block": false}).to_string(),
            &ctx,
        )
        .await;
    assert!(!poll.is_error);
    assert!(poll.content.contains("status=\"running\""));

    // List summary shows active background task
    let listed = output_tool.execute(r#"{"list": true}"#, &ctx).await;
    assert!(listed.content.contains(&task_id));
    assert!(listed.content.contains(&agent_id));

    // Release gate and block via task_output
    gate.notify_waiters();
    let awaited = output_tool
        .execute(
            &serde_json::json!({"task_id": task_id, "block": true, "timeout_secs": 5}).to_string(),
            &ctx,
        )
        .await;
    assert!(!awaited.is_error, "{}", awaited.content);
    assert!(awaited.content.contains("ASYNC_BG_FINISHED"));
    assert!(awaited
        .content
        .contains(&format!("agent_id=\"{agent_id}\"")));
    assert!(awaited.content.contains("state=\"completed\""));

    // Cancel-on-drop test: park a second background task on a never-notified gate,
    // drop TaskTool + TaskOutputTool, and verify cancellation propagates to child!
    let drop_cancelled = Arc::new(Notify::new());
    let dc_clone = drop_cancelled.clone();
    let started2 = Arc::new(Notify::new());
    let s2_clone = started2.clone();

    struct NeverProvider {
        started: Arc<Notify>,
        cancelled_observed: Arc<Notify>,
    }

    #[async_trait]
    impl LlmProvider for NeverProvider {
        fn model_name(&self) -> &str {
            "never-mock"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _opts: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.started.notify_waiters();
            // Return an infinite pending stream so Agent checks cancel token cleanly
            let cancelled = self.cancelled_observed.clone();
            struct DropNotify(Arc<Notify>);
            impl Drop for DropNotify {
                fn drop(&mut self) {
                    self.0.notify_waiters();
                }
            }
            let guard = DropNotify(cancelled);
            let s = stream::pending::<StreamEvent>().map(move |x| {
                let _ = &guard;
                x
            });
            Ok(s.boxed())
        }
    }

    let reg2 = build_test_registry();
    let r2e = reg2.clone();
    let r2w = reg2.clone();
    let make_never = move || {
        Arc::new(NeverProvider {
            started: s2_clone.clone(),
            cancelled_observed: dc_clone.clone(),
        }) as Arc<dyn LlmProvider>
    };
    let tool2 = TaskTool::new(
        make_never.clone(),
        make_never,
        move || r2e.mount(&["read"]),
        move || r2w.mount(&["read"]),
    );
    let _ = tool2.execute(bg_args, &ctx).await;
    started2.notified().await;
    drop(tool2);

    tokio::time::timeout(Duration::from_secs(2), drop_cancelled.notified())
        .await
        .expect("dropping TaskTool must cancel detached background child");
}

#[tokio::test]
async fn inherit_context_flag_defaults_lean_and_injects_full_ordered_blocks_when_true() {
    let dir = tempfile::tempdir().unwrap();
    let long_agents = format!("{}\nTAIL_RULE_ABC", "rule line\n".repeat(700));
    std::fs::write(dir.path().join("AGENTS.md"), &long_agents).unwrap();

    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();
    let seen_msgs = Arc::new(Mutex::new(Vec::new()));
    let seen_msgs_p = seen_msgs.clone();
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let make_p = move || {
        Arc::new(ScriptedProvider::new(
            vec![vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Done { truncated: false },
            ]],
            seen_msgs_p.clone(),
            seen_tools.clone(),
        )) as Arc<dyn LlmProvider>
    };
    let tool = TaskTool::new(
        make_p.clone(),
        make_p,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read"]),
    );

    // Schema exposes inherit_context
    let schema = tool.parameters_schema();
    assert_eq!(
        schema["properties"]["tasks"]["items"]["properties"]["inherit_context"]["type"],
        "boolean"
    );

    // 1. Default (inherit_context omitted => false): capped in Role::System, zero synthetic user blocks
    let out1 = tool
        .execute(
            r#"{"tasks":[{"description":"lean","prompt":"lean prompt","subagent_type":"explore"}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out1.is_error, "{}", out1.content);

    // 2. inherit_context: true: uncapped <project_instructions> as frozen synthetic user block
    let out2 = tool
        .execute(
            r#"{"tasks":[{"description":"full","prompt":"full prompt","subagent_type":"explore","inherit_context":true}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out2.is_error, "{}", out2.content);

    let msgs = seen_msgs.lock().unwrap();
    assert_eq!(msgs.len(), 2);

    // Call 1 (lean): <project_instructions> is inside Role::System and truncated
    let lean_sys = &msgs[0][0].text;
    assert!(lean_sys.contains("<project_instructions>"));
    assert!(!lean_sys.contains("TAIL_RULE_ABC"));
    assert!(!msgs[0].iter().any(|m| m.role == Role::User && m.synthetic));

    // Call 2 (inherit_context: true): Role::System has no <project_instructions>; synthetic User carries full uncapped body before prompt
    assert!(!msgs[1][0].text.contains("<project_instructions>"));
    let proj_idx = msgs[1]
        .iter()
        .position(|m| {
            m.role == Role::User && m.synthetic && m.text.starts_with("<project_instructions>")
        })
        .expect("missing frozen <project_instructions>");
    let prompt_idx = msgs[1]
        .iter()
        .position(|m| m.role == Role::User && !m.synthetic && m.text == "full prompt")
        .expect("missing real prompt");
    assert!(proj_idx < prompt_idx);
    assert!(msgs[1][proj_idx].text.contains("TAIL_RULE_ABC"));
}

#[tokio::test]
async fn resume_agent_id_preserves_hard_difficulty_when_omitted_and_allows_explicit_override() {
    struct TierProvider {
        model: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LlmProvider for TierProvider {
        fn model_name(&self) -> &str {
            self.model
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _opts: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(stream::iter(vec![
                StreamEvent::TextDelta(format!("ok from {}", self.model)),
                StreamEvent::Done { truncated: false },
            ])
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();
    let fast_calls = Arc::new(AtomicUsize::new(0));
    let capable_calls = Arc::new(AtomicUsize::new(0));
    let fast_calls_c = fast_calls.clone();
    let capable_calls_c = capable_calls.clone();

    let tool = TaskTool::new(
        move || {
            Arc::new(TierProvider {
                model: "fast-tier-model",
                calls: fast_calls_c.clone(),
            }) as Arc<dyn LlmProvider>
        },
        move || {
            Arc::new(TierProvider {
                model: "capable-tier-model",
                calls: capable_calls_c.clone(),
            }) as Arc<dyn LlmProvider>
        },
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read"]),
    );

    // Round 1: spawn with difficulty="hard" -> must hit capable-tier-model
    let out1 = tool
        .execute(
            r#"{"tasks":[{"description":"r1","prompt":"critique r1","subagent_type":"explore","difficulty":"hard"}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out1.is_error, "{}", out1.content);
    assert!(out1.content.contains("capable-tier-model"));
    assert_eq!(capable_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fast_calls.load(Ordering::SeqCst), 0);

    let agent_id = extract_attr(&out1.content, "agent_id").expect("agent_id in header");

    // Round 2: resume WITHOUT difficulty -> must inherit "hard" and stay on capable-tier-model
    let args2 = serde_json::json!({
        "tasks": [{
            "description": "r2",
            "prompt": "critique r2",
            "resume_agent_id": agent_id
        }]
    })
    .to_string();
    let out2 = tool.execute(&args2, &tool_ctx(dir.path())).await;
    assert!(!out2.is_error, "{}", out2.content);
    assert!(
        out2.content.contains("capable-tier-model"),
        "omitted difficulty on resume must inherit capable tier: {}",
        out2.content
    );
    assert_eq!(capable_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fast_calls.load(Ordering::SeqCst), 0);

    // Round 3: resume with blank difficulty ("   ") -> still inherits "hard"
    let args3 = serde_json::json!({
        "tasks": [{
            "description": "r3",
            "prompt": "critique r3",
            "difficulty": "   ",
            "resume_agent_id": agent_id
        }]
    })
    .to_string();
    let out3 = tool.execute(&args3, &tool_ctx(dir.path())).await;
    assert!(!out3.is_error, "{}", out3.content);
    assert!(out3.content.contains("capable-tier-model"));
    assert_eq!(capable_calls.load(Ordering::SeqCst), 3);
    assert_eq!(fast_calls.load(Ordering::SeqCst), 0);

    // Round 4: explicit difficulty="simple" overrides saved "hard"
    let args4 = serde_json::json!({
        "tasks": [{
            "description": "r4",
            "prompt": "quick summary",
            "difficulty": "simple",
            "resume_agent_id": agent_id
        }]
    })
    .to_string();
    let out4 = tool.execute(&args4, &tool_ctx(dir.path())).await;
    assert!(!out4.is_error, "{}", out4.content);
    assert!(out4.content.contains("fast-tier-model"));
    assert_eq!(capable_calls.load(Ordering::SeqCst), 3);
    assert_eq!(fast_calls.load(Ordering::SeqCst), 1);

    // SavedSubagentState deserializes legacy JSON lacking `difficulty` / `inherit_context`
    let legacy_json = serde_json::json!({
        "agent_id": "agent-explore-99",
        "session_scope": "sess-legacy",
        "role": "explore",
        "scope": [],
        "working_dir": dir.path(),
        "model": "legacy-model",
        "snapshot": jeikcode_kernel::message::SessionSnapshot::new(Vec::new()),
        "updated_seq": 1
    });
    let decoded: super::subagent_store::SavedSubagentState =
        serde_json::from_value(legacy_json).expect("legacy SavedSubagentState must deserialize");
    assert_eq!(decoded.difficulty, None);
    assert!(!decoded.inherit_context);
}

#[tokio::test]
async fn subagent_spills_oversized_output_to_artifact_store_and_emits_telemetry_and_guards_concurrent_resume(
) {
    let dir = tempfile::tempdir().unwrap();
    let artifact_store = Arc::new(
        crate::tools::output_artifact::ArtifactStore::new_standalone(dir.path().join("artifacts")),
    );
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();

    let huge_middle = "MIDDLE_CRITIQUE_EVIDENCE_TOKEN_".repeat(2_500);
    let huge_reply = format!("HEAD_START\n{}\nTAIL_END", huge_middle);
    let huge_reply_c = huge_reply.clone();

    let make_provider = move || {
        let reply = huge_reply_c.clone();
        Arc::new(ScriptedProvider::new(
            vec![vec![
                StreamEvent::TextDelta(reply),
                StreamEvent::Usage(jeikcode_kernel::stream::TokenUsage {
                    prompt: 1200,
                    completion: 450,
                    cached: 900,
                }),
                StreamEvent::Done { truncated: false },
            ]],
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        )) as Arc<dyn LlmProvider>
    };

    let tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read"]),
    )
    .with_optional_artifact_store(Some(artifact_store.clone()));
    tool.bind_session("sess-main");

    let out = tool
        .execute(
            r#"{"tasks":[{"description":"big critique","prompt":"audit","subagent_type":"explore","max_rounds":1}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("rounds=\"1\"")
            && out.content.contains("input_tokens=\"1200\"")
            && out.content.contains("cached_tokens=\"900\"")
            && out.content.contains(" state=\"completed\">"),
        "task header must carry telemetry attributes and end with state: {}",
        out.content
    );
    assert!(
        out.content.contains("full output saved to artifact://"),
        "oversized subagent output must spill to artifact store: {}",
        out.content
    );

    let agent_id = extract_attr(&out.content, "agent_id").expect("agent_id in header");

    // Concurrent resume of the same agent_id in one batch must reject the second task cleanly
    let batch_resume = serde_json::json!({
        "tasks": [
            {
                "description": "r1",
                "prompt": "first resume",
                "resume_agent_id": agent_id
            },
            {
                "description": "r2",
                "prompt": "concurrent duplicate resume",
                "resume_agent_id": agent_id
            }
        ]
    })
    .to_string();
    let batch_out = tool.execute(&batch_resume, &tool_ctx(dir.path())).await;
    assert!(
        batch_out.content.contains("already running"),
        "second concurrent resume of same agent_id must be rejected: {}",
        batch_out.content
    );
}

#[tokio::test]
async fn deny_sensitive_paths_blocks_codegraphignore_graph_and_include_sensitive() {
    use super::task::DenySensitivePaths;
    use jeikcode_kernel::middleware::ToolMiddleware;
    use jeikcode_kernel::request::RequestCtx;
    use jeikcode_kernel::tool::ToolCall;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".codegraphignore.graph"), "secrets/**\n").unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(dir.path().join("secrets/api.txt"), "TOKEN=123").unwrap();

    let mw = DenySensitivePaths::new(dir.path());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let rt = RequestCtx::new(tx, None);
    let read_tool: Arc<dyn Tool> = Arc::new(crate::tools::ReadFileTool::default());
    let grep_tool: Arc<dyn Tool> = Arc::new(crate::tools::GrepTool);

    let mut read_call = ToolCall {
        id: "t1".into(),
        name: "read".into(),
        arguments: r#"{"path":"secrets/api.txt"}"#.into(),
    };
    let out1 = mw.before(&mut read_call, &read_tool, &rt).await;
    assert!(
        out1.is_deny(),
        "subagent DenySensitivePaths must block .codegraphignore.graph paths"
    );

    let mut grep_call = ToolCall {
        id: "t2".into(),
        name: "grep".into(),
        arguments: r#"{"pattern":"TOKEN","include_sensitive":true}"#.into(),
    };
    let out2 = mw.before(&mut grep_call, &grep_tool, &rt).await;
    assert!(
        out2.is_deny(),
        "subagent DenySensitivePaths must block grep include_sensitive=true"
    );
}

#[tokio::test]
async fn task_max_rounds_runs_graceful_tool_less_synthesis_preserving_partial_marker() {
    use jeikcode_kernel::provider::ToolChoice;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn alpha() {}\n").unwrap();
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();

    let seen_choices: Arc<Mutex<Vec<(usize, ToolChoice)>>> = Arc::new(Mutex::new(Vec::new()));
    let sc = seen_choices.clone();

    struct SynthesisProvider {
        step: AtomicUsize,
        seen: Arc<Mutex<Vec<(usize, ToolChoice)>>>,
    }

    #[async_trait]
    impl LlmProvider for SynthesisProvider {
        fn model_name(&self) -> &str {
            "synth-mock"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            tools: &[ToolDef],
            opts: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push((tools.len(), opts.tool_choice.clone()));
            if n == 0 {
                Ok(stream::iter(vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: r#"{"path":"src.rs"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ])
                .boxed())
            } else {
                Ok(stream::iter(vec![
                    StreamEvent::TextDelta("FINAL_SYNTHESIS_OK: alpha verified".into()),
                    StreamEvent::Done { truncated: false },
                ])
                .boxed())
            }
        }
    }

    let make_provider = move || {
        Arc::new(SynthesisProvider {
            step: AtomicUsize::new(0),
            seen: sc.clone(),
        }) as Arc<dyn LlmProvider>
    };
    let tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read"]),
    );

    let out = tool
        .execute(
            r#"{"tasks":[{"description":"bounded audit","prompt":"check src.rs","subagent_type":"explore","max_rounds":1}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("partial=\"max_rounds_synthesis\""),
        "must preserve explicit partial marker: {}",
        out.content
    );
    assert!(
        out.content.contains("FINAL_SYNTHESIS_OK: alpha verified"),
        "must deliver synthesized prose: {}",
        out.content
    );
    let choices = seen_choices.lock().unwrap();
    assert_eq!(choices.len(), 2);
    assert_eq!(choices[1].0, 1, "tools must stay mounted on synthesis turn");
    assert_eq!(choices[1].1, ToolChoice::None);
}

#[tokio::test]
async fn task_reserve_final_synthesis_round_synthesizes_within_cap_on_max_rounds_3() {
    use jeikcode_kernel::provider::ToolChoice;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("src.rs"), "pub fn beta() {}\n").unwrap();
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();

    let seen_choices: Arc<Mutex<Vec<(usize, ToolChoice)>>> = Arc::new(Mutex::new(Vec::new()));
    let sc = seen_choices.clone();

    struct ReservedRoundProvider {
        step: AtomicUsize,
        seen: Arc<Mutex<Vec<(usize, ToolChoice)>>>,
    }

    #[async_trait]
    impl LlmProvider for ReservedRoundProvider {
        fn model_name(&self) -> &str {
            "reserved-synth-mock"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            tools: &[ToolDef],
            opts: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push((tools.len(), opts.tool_choice.clone()));
            if n < 2 {
                Ok(stream::iter(vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: format!("c{n}"),
                        name: "read".into(),
                        arguments: r#"{"path":"src.rs"}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ])
                .boxed())
            } else {
                Ok(stream::iter(vec![
                    StreamEvent::TextDelta("WITHIN_CAP_SYNTHESIS: beta checked".into()),
                    StreamEvent::Done { truncated: false },
                ])
                .boxed())
            }
        }
    }

    let make_provider = move || {
        Arc::new(ReservedRoundProvider {
            step: AtomicUsize::new(0),
            seen: sc.clone(),
        }) as Arc<dyn LlmProvider>
    };
    let tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read"]),
    );

    let out = tool
        .execute(
            r#"{"tasks":[{"description":"reserved synthesis","prompt":"check src.rs","subagent_type":"explore","max_rounds":3}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("WITHIN_CAP_SYNTHESIS: beta checked"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("rounds=\"3\""),
        "must complete within max_rounds=3 without needing a 4th round: {}",
        out.content
    );
    let choices = seen_choices.lock().unwrap();
    assert_eq!(choices.len(), 3);
    assert_eq!(choices[0].1, ToolChoice::Auto);
    assert_eq!(choices[1].1, ToolChoice::Auto);
    assert_eq!(choices[2].1, ToolChoice::None);
}

#[tokio::test]
async fn bind_session_switch_cancels_prior_background_subagent_while_turn_cancel_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();

    let started = Arc::new(Notify::new());
    let cancelled_observed = Arc::new(Notify::new());
    let s_clone = started.clone();
    let c_clone = cancelled_observed.clone();

    struct PendingBgProvider {
        started: Arc<Notify>,
        cancelled_observed: Arc<Notify>,
    }

    #[async_trait]
    impl LlmProvider for PendingBgProvider {
        fn model_name(&self) -> &str {
            "bg-pending"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _opts: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.started.notify_waiters();
            struct DropNotify(Arc<Notify>);
            impl Drop for DropNotify {
                fn drop(&mut self) {
                    self.0.notify_waiters();
                }
            }
            let guard = DropNotify(self.cancelled_observed.clone());
            Ok(stream::pending::<StreamEvent>()
                .map(move |x| {
                    let _ = &guard;
                    x
                })
                .boxed())
        }
    }

    let make_p = move || {
        Arc::new(PendingBgProvider {
            started: s_clone.clone(),
            cancelled_observed: c_clone.clone(),
        }) as Arc<dyn LlmProvider>
    };
    let tool = TaskTool::new(
        make_p.clone(),
        make_p,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read"]),
    );
    tool.bind_session("sess-A");

    let turn_ctx = tool_ctx(dir.path());
    let _ = tool
        .execute(
            r#"{"run_in_background":true,"tasks":[{"description":"bg","prompt":"work","subagent_type":"explore"}]}"#,
            &turn_ctx,
        )
        .await;
    started.notified().await;

    // Cancelling the single foreground turn must NOT kill the detached background child.
    turn_ctx.cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(80), cancelled_observed.notified())
            .await
            .is_err(),
        "single-turn ctx.cancel must not terminate a detached background subagent"
    );

    // Switching sessions via bind_session MUST cancel the prior session's background child.
    tool.bind_session("sess-B");
    tokio::time::timeout(Duration::from_secs(2), cancelled_observed.notified())
        .await
        .expect("bind_session switch must cancel prior session background subagents");
}

#[tokio::test]
async fn task_tool_mounts_recall_tool_only_for_inherit_context_explore_and_verifier() {
    struct MockRecallTool;
    #[async_trait]
    impl Tool for MockRecallTool {
        fn name(&self) -> &str {
            "memory"
        }
        fn description(&self) -> &str {
            "Read-only memory recall mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["recall"] },
                    "topic": { "type": "string" },
                    "query": { "type": "string" }
                },
                "required": ["action"]
            })
        }
        fn read_only_hint(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _args: &str,
            _ctx: &ToolContext,
        ) -> jeikcode_kernel::tool::ToolResult {
            jeikcode_kernel::tool::ToolResult {
                call_id: String::new(),
                content: "ok".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let reg = build_test_registry();
    let r_e = reg.clone();
    let r_w = reg.clone();
    let r_v = reg.clone();

    let seen_msgs = Arc::new(Mutex::new(Vec::new()));
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let sm = seen_msgs.clone();
    let st = seen_tools.clone();

    let make_provider = move || {
        Arc::new(ScriptedProvider::new(
            vec![vec![
                StreamEvent::TextDelta("VERDICT: PASS\nSUMMARY: ok".into()),
                StreamEvent::Done { truncated: false },
            ]],
            sm.clone(),
            st.clone(),
        )) as Arc<dyn LlmProvider>
    };

    let tool = TaskTool::new(
        make_provider.clone(),
        make_provider,
        move || r_e.mount(&["read"]),
        move || r_w.mount(&["read", "write"]),
    )
    .with_verifier_tools(move || r_v.mount(&["read", "run_command"]))
    .with_recall_tool(Arc::new(MockRecallTool));

    // 1. explore + inherit_context: false -> NO memory tool
    let out = tool
        .execute(
            r#"{"tasks":[{"description":"explore lean","prompt":"p","subagent_type":"explore"}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);

    // 2. explore + inherit_context: true -> mounts memory tool
    let out = tool
        .execute(
            r#"{"tasks":[{"description":"explore inherit","prompt":"p","subagent_type":"explore","inherit_context":true}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);

    // 3. verifier + inherit_context: false -> NO memory tool
    let out = tool
        .execute(
            r#"{"tasks":[{"description":"verifier lean","prompt":"p","subagent_type":"verifier"}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);

    // 4. verifier + inherit_context: true -> mounts memory tool
    let out = tool
        .execute(
            r#"{"tasks":[{"description":"verifier inherit","prompt":"p","subagent_type":"verifier","inherit_context":true}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);

    // 5. worker + inherit_context: true -> NO memory tool
    let out = tool
        .execute(
            r#"{"tasks":[{"description":"worker inherit","prompt":"p","subagent_type":"worker","scope":["src/**"],"inherit_context":true}]}"#,
            &tool_ctx(dir.path()),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);

    let recorded = seen_tools.lock().unwrap();
    assert_eq!(recorded.len(), 5);
    assert!(
        !recorded[0].iter().any(|n| n == "memory"),
        "explore lean: {:?}",
        recorded[0]
    );
    assert!(
        recorded[1].iter().any(|n| n == "memory"),
        "explore inherit: {:?}",
        recorded[1]
    );
    assert!(
        !recorded[2].iter().any(|n| n == "memory"),
        "verifier lean: {:?}",
        recorded[2]
    );
    assert!(
        recorded[3].iter().any(|n| n == "memory"),
        "verifier inherit: {:?}",
        recorded[3]
    );
    assert!(
        !recorded[4].iter().any(|n| n == "memory"),
        "worker inherit: {:?}",
        recorded[4]
    );
}
