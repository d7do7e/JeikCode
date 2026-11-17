//! Cross-process live session mesh & peer talk integration suite.
//!
//! Runs in its own isolated `$JEIKCODE_HOME` test binary so live descriptor files under
//! `$JEIKCODE_HOME/live/*.json` never race sibling test binaries.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use jeikcode_coding::{
    discover_live_sessions, live_registry_dir, CodingAgentConfig, CodingProviderFactory,
    CodingRuntime, CodingRuntimeEvent, CodingRuntimeStart, PrepareOptions, ProviderBuildError,
    SessionMode, StaticPluginHookSource, UserInput,
};
use jeikcode_kernel::message::{Message, Role};
use jeikcode_kernel::provider::{ChatOptions, LlmProvider};
use jeikcode_kernel::stream::{ProviderError, StreamEvent};
use jeikcode_kernel::tool::{ToolCall, ToolDef};

#[ctor::ctor]
fn _isolate_jeikcode_home() {
    jeikcode_kernel::test_support::isolate_home();
}

struct ScriptedFactory {
    turns: Mutex<Vec<Vec<StreamEvent>>>,
    seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ScriptedFactory {
    fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns),
            seen_tools: Arc::new(Mutex::new(Vec::new())),
            seen_messages: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<StreamEvent>>>>,
    seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn model_name(&self) -> &str {
        "scripted-mesh-model"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.seen_tools
            .lock()
            .unwrap()
            .push(tools.iter().map(|t| t.name.clone()).collect());
        self.seen_messages.lock().unwrap().push(messages.to_vec());
        let next = {
            let mut guard = self.turns.lock().unwrap();
            if guard.is_empty() {
                vec![
                    StreamEvent::TextDelta("ok".into()),
                    StreamEvent::Done { truncated: false },
                ]
            } else {
                guard.remove(0)
            }
        };
        Ok(stream::iter(next).boxed())
    }
}

impl CodingProviderFactory for ScriptedFactory {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        let turns = std::mem::take(&mut *self.turns.lock().unwrap());
        Ok(Arc::new(ScriptedProvider {
            turns: Arc::new(Mutex::new(turns)),
            seen_tools: Arc::clone(&self.seen_tools),
            seen_messages: Arc::clone(&self.seen_messages),
        }))
    }
}

fn start_cfg(working_dir: &Path, factory: Arc<dyn CodingProviderFactory>) -> CodingRuntimeStart {
    let mut cfg = CodingAgentConfig::new("k", "http://unused", "scripted-mesh-model", working_dir);
    cfg.timeouts = cfg.timeouts.with_content_liveness(Duration::from_secs(5));
    cfg.request_timeout = Some(Duration::from_secs(5));
    CodingRuntimeStart {
        agent: cfg,
        prepare: PrepareOptions {
            session: SessionMode::Fresh,
            skill_dirs: Some(vec![working_dir.join("no_skills")]),
            mcp: false,
            memory: false,
            web: false,
            review: false,
            request_user_input: false,
            ..Default::default()
        },
        provider_factory: factory,
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    }
}

async fn wait_turn_finished(runtime: &mut CodingRuntime) -> Vec<CodingRuntimeEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut seen = Vec::new();
        while let Some(ev) = runtime.events.recv().await {
            let done = matches!(ev.event, CodingRuntimeEvent::TurnFinished(_));
            seen.push(ev.event);
            if done {
                return seen;
            }
        }
        panic!("runtime event stream closed before TurnFinished");
    })
    .await
    .expect("timed out waiting for TurnFinished")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(jeikcode_home)]
async fn live_session_mesh_allows_cross_repo_by_default_returns_replies_and_emits_ui_echo() {
    let home = tempfile::tempdir().unwrap();
    let _home = jeikcode_config::test_util::IsolatedHome::at(home.path());

    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();

    // Runtime B (same-repo peer in repo_a)
    let factory_b = Arc::new(ScriptedFactory::new(vec![vec![
        StreamEvent::TextDelta("B reply: auth.rs verified".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let b_messages = Arc::clone(&factory_b.seen_messages);
    let mut rt_b = CodingRuntime::start(start_cfg(repo_a.path(), factory_b))
        .await
        .unwrap();
    let sid_b = rt_b.session.as_ref().unwrap().id.clone();

    // Runtime C (cross-repo peer in repo_b — allowed by default with EXTERNAL REPO annotations!)
    let factory_c = Arc::new(ScriptedFactory::new(vec![vec![
        StreamEvent::TextDelta("C reply from external repo".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let c_messages = Arc::clone(&factory_c.seen_messages);
    let mut rt_c = CodingRuntime::start(start_cfg(repo_b.path(), factory_c))
        .await
        .unwrap();
    let sid_c = rt_c.session.as_ref().unwrap().id.clone();

    // Runtime A (originator in repo_a):
    // 1. list_live_sessions
    // 2. send_to_session -> B (same repo, waits & receives B's reply)
    // 3. send_to_session -> C (external repo by default, waits & receives C's reply)
    // 4. read_session_turns -> C (external repo by default, annotated [EXTERNAL REPO: ...])
    // 5. task(explore) -> child must NOT receive mesh tools
    let factory_a = Arc::new(ScriptedFactory::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "list_live_sessions".into(),
                arguments: "{}".into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "t2".into(),
                name: "send_to_session".into(),
                arguments: serde_json::json!({
                    "session_id": sid_b,
                    "message": "please inspect auth.rs"
                })
                .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "t3".into(),
                name: "send_to_session".into(),
                arguments: serde_json::json!({
                    "session_id": sid_c,
                    "message": "check cross-repo contract"
                })
                .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "t4".into(),
                name: "read_session_turns".into(),
                arguments: serde_json::json!({
                    "session_id": sid_c
                })
                .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "t5".into(),
                name: "task".into(),
                arguments: serde_json::json!({
                    "tasks": [{
                        "description": "check mesh isolation",
                        "prompt": "report ok",
                        "subagent_type": "explore"
                    }]
                })
                .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        // Child explore turn
        vec![
            StreamEvent::TextDelta("explore ok".into()),
            StreamEvent::Done { truncated: false },
        ],
        // Final parent A answer
        vec![
            StreamEvent::TextDelta("A finished".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let a_tools = Arc::clone(&factory_a.seen_tools);
    let a_messages = Arc::clone(&factory_a.seen_messages);
    let mut rt_a = CodingRuntime::start(start_cfg(repo_a.path(), factory_a))
        .await
        .unwrap();
    let sid_a = rt_a.session.as_ref().unwrap().id.clone();

    // Verify discovery annotates same_repo vs EXTERNAL REPO.
    let live =
        discover_live_sessions(&live_registry_dir(), Some(&sid_a), Some(repo_a.path())).await;
    assert_eq!(live.len(), 3, "expected 3 live sessions: {live:?}");
    let view_b = live.iter().find(|v| v.session_id == sid_b).unwrap();
    assert!(view_b.same_repo);
    assert_eq!(view_b.repo_scope, "same_repo");
    let view_c = live.iter().find(|v| v.session_id == sid_c).unwrap();
    assert!(!view_c.same_repo);
    assert!(view_c.repo_scope.starts_with("EXTERNAL REPO:"));

    // Submit A's turn: send_to_session waits for B and C replies synchronously!
    rt_a.handle
        .submit(UserInput::from("coordinate with peers"))
        .await
        .unwrap();
    let _a_events = wait_turn_finished(&mut rt_a).await;
    let b_events = wait_turn_finished(&mut rt_b).await;
    let c_events = wait_turn_finished(&mut rt_c).await;

    // Verify B and C both emitted CodingRuntimeEvent::PeerMessageReceived for TUI/Daemon visibility.
    assert!(
        b_events.iter().any(|e| matches!(
            e,
            CodingRuntimeEvent::PeerMessageReceived { from_session_id, same_repo: true, text, .. }
                if from_session_id == &sid_a && text == "please inspect auth.rs"
        )),
        "B must emit PeerMessageReceived(same_repo=true)"
    );
    assert!(
        c_events.iter().any(|e| matches!(
            e,
            CodingRuntimeEvent::PeerMessageReceived { from_session_id, same_repo: false, text, .. }
                if from_session_id == &sid_a && text == "check cross-repo contract"
        )),
        "C must emit PeerMessageReceived(same_repo=false)"
    );

    // Verify B received <peer_message> (same_repo) and C received [EXTERNAL REPO: ...] annotation.
    let b_seen = b_messages.lock().unwrap();
    assert!(b_seen[0].iter().any(|m| m.role == Role::User
        && m.text.contains("<peer_message")
        && m.text.contains("repo_scope=\"same_repo\"")
        && m.text.contains("please inspect auth.rs")));
    let c_seen = c_messages.lock().unwrap();
    assert!(c_seen[0].iter().any(|m| m.role == Role::User
        && m.text.contains("<peer_message")
        && m.text.contains("repo_scope=\"external_repo\"")
        && m.text.contains("[EXTERNAL REPO:")
        && m.text.contains("check cross-repo contract")));

    // Verify A's parent tools include the 3 mesh tools, while child explore turn does NOT.
    let tools_calls = a_tools.lock().unwrap();
    assert!(tools_calls[0].iter().any(|t| t == "list_live_sessions"));
    assert!(tools_calls[0].iter().any(|t| t == "read_session_turns"));
    assert!(tools_calls[0].iter().any(|t| t == "send_to_session"));
    let child_tools = &tools_calls[5];
    assert!(
        !child_tools.iter().any(|t| t == "list_live_sessions"
            || t == "read_session_turns"
            || t == "send_to_session"),
        "subagent child must NEVER mount mesh tools: {child_tools:?}"
    );

    // Verify A received BOTH peer replies directly in tool outputs, plus [EXTERNAL REPO:] annotations!
    let a_hist = a_messages.lock().unwrap().last().unwrap().clone();
    let tool_outputs: Vec<String> = a_hist
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.text.clone())
        .collect();
    assert!(
        tool_outputs
            .iter()
            .any(|o| o.contains("B reply: auth.rs verified") && o.contains("[SAME REPO]")),
        "A must receive B's actual reply: {tool_outputs:?}"
    );
    assert!(
        tool_outputs
            .iter()
            .any(|o| o.contains("C reply from external repo") && o.contains("[EXTERNAL REPO:")),
        "A must receive C's cross-repo reply with [EXTERNAL REPO:] tag: {tool_outputs:?}"
    );
    assert!(
        tool_outputs
            .iter()
            .any(|o| o.contains("[EXTERNAL REPO:") && o.contains("check cross-repo contract")),
        "read_session_turns on C must annotate [EXTERNAL REPO:] and return turns: {tool_outputs:?}"
    );

    rt_a.handle.shutdown().await.unwrap();
    rt_b.handle.shutdown().await.unwrap();
    rt_c.handle.shutdown().await.unwrap();
    let remaining = discover_live_sessions(&live_registry_dir(), None, None).await;
    assert!(
        remaining.is_empty(),
        "live descriptors should be removed on shutdown: {remaining:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(jeikcode_home)]
async fn mesh_cycle_and_read_turns_enforce_guardrails_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let _home = jeikcode_config::test_util::IsolatedHome::at(home.path());
    let repo = tempfile::tempdir().unwrap();

    let sid_a_cell = Arc::new(std::sync::OnceLock::<String>::new());
    let sid_b_cell = Arc::new(std::sync::OnceLock::<String>::new());

    struct BounceBackFactory {
        target: Arc<std::sync::OnceLock<String>>,
        seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
        step: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct BounceBackProvider {
        target: Arc<std::sync::OnceLock<String>>,
        seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
        step: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl LlmProvider for BounceBackProvider {
        fn model_name(&self) -> &str {
            "bounce-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            self.seen_messages.lock().unwrap().push(messages.to_vec());
            let idx = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if idx == 0 {
                let target_id = self.target.get().unwrap().clone();
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "bounce_1".into(),
                        name: "send_to_session".into(),
                        arguments: serde_json::json!({
                            "session_id": target_id,
                            "message": "echo bounce"
                        })
                        .to_string(),
                    }),
                    StreamEvent::Done { truncated: false },
                ]
            } else {
                vec![
                    StreamEvent::TextDelta("B handled cycle guard".into()),
                    StreamEvent::Done { truncated: false },
                ]
            };
            Ok(stream::iter(events).boxed())
        }
    }

    impl CodingProviderFactory for BounceBackFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
            Ok(Arc::new(BounceBackProvider {
                target: Arc::clone(&self.target),
                seen_messages: Arc::clone(&self.seen_messages),
                step: Arc::clone(&self.step),
            }))
        }
    }

    let b_msgs = Arc::new(Mutex::new(Vec::new()));
    let factory_b = Arc::new(BounceBackFactory {
        target: Arc::clone(&sid_a_cell),
        seen_messages: Arc::clone(&b_msgs),
        step: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let mut rt_b = CodingRuntime::start(start_cfg(repo.path(), factory_b))
        .await
        .unwrap();
    let sid_b = rt_b.session.as_ref().unwrap().id.clone();
    let _ = sid_b_cell.set(sid_b.clone());

    let a_msgs = Arc::new(Mutex::new(Vec::new()));
    let factory_a = Arc::new(BounceBackFactory {
        target: Arc::clone(&sid_b_cell),
        seen_messages: Arc::clone(&a_msgs),
        step: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let mut rt_a = CodingRuntime::start(start_cfg(repo.path(), factory_a))
        .await
        .unwrap();
    let sid_a = rt_a.session.as_ref().unwrap().id.clone();
    let _ = sid_a_cell.set(sid_a.clone());

    rt_a.handle
        .submit(UserInput::from("send ping to B"))
        .await
        .unwrap();
    let _ = wait_turn_finished(&mut rt_a).await;
    let _ = wait_turn_finished(&mut rt_b).await;

    let b_last = b_msgs.lock().unwrap().last().unwrap().clone();
    let b_tool_outputs: Vec<String> = b_last
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.text.clone())
        .collect();
    assert!(
        b_tool_outputs
            .iter()
            .any(|t| t.contains("mesh cycle detected")),
        "B bouncing back to A must trigger cycle guard: {b_tool_outputs:?}"
    );

    // Also confirm A received B's final reply cleanly!
    let a_last = a_msgs.lock().unwrap().last().unwrap().clone();
    let a_tool_outputs: Vec<String> = a_last
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.text.clone())
        .collect();
    assert!(
        a_tool_outputs
            .iter()
            .any(|t| t.contains("B handled cycle guard")),
        "A must receive B's reply after cycle block: {a_tool_outputs:?}"
    );

    rt_a.handle.shutdown().await.unwrap();
    rt_b.handle.shutdown().await.unwrap();
}
