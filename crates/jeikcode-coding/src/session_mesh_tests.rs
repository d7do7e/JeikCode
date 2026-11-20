use super::*;
use crate::runtime::{coding_runtime_control_channel, CodingRuntimeControlReceiver};

fn test_local_state() -> (MeshLocalState, CodingRuntimeControlReceiver) {
    let (handle, controls) = coding_runtime_control_channel();
    let (turn_outcomes, _) = broadcast::channel(8);
    let local = MeshLocalState {
        live_dir: PathBuf::from("/unused-live"),
        session_id: Arc::new(RwLock::new("target-session".to_string())),
        name: Arc::new(RwLock::new(Some("backend".to_string()))),
        working_dir: Arc::new(RwLock::new(PathBuf::from("/repo"))),
        repo_root: Arc::new(RwLock::new(PathBuf::from("/repo"))),
        model: Arc::new(RwLock::new("test-model".to_string())),
        latest_snapshot: Arc::new(RwLock::new(None)),
        active_hop: Arc::new(RwLock::new(None)),
        turn_outcomes,
        handle,
    };
    (local, controls)
}

fn send_req(from: &str, steer_if_busy: bool) -> MeshRequest {
    MeshRequest::SendMessage {
        from_session_id: from.to_string(),
        from_session_name: None,
        from_working_dir: PathBuf::from("/repo"),
        from_repo_root: PathBuf::from("/repo"),
        text: "ping".to_string(),
        hop_depth: 0,
        visited_sessions: vec![],
        waiting_sessions: vec![],
        steer_if_busy,
        wait_for_reply: Some(false),
        timeout_secs: Some(5),
        max_reply_chars: None,
    }
}

#[tokio::test]
async fn rejected_send_message_preserves_active_turn_hop_context() {
    let active_ctx = MeshHopContext {
        hop_depth: 2,
        visited_sessions: vec!["origin-a".into(), "origin-b".into()],
        waiting_sessions: vec!["origin-a".into()],
    };

    // 1. Busy rejection (`InTurn` with `steer_if_busy = false`) leaves `active_hop` untouched.
    let (local, controls) = test_local_state();
    controls.set_phase_for_test(RuntimePhase::InTurn);
    local.bind_hop_context(Some(7), active_ctx.clone());
    let resp = dispatch_ipc_request(&local, send_req("caller-c", false)).await;
    assert!(matches!(resp, MeshResponse::Error { .. }), "got {resp:?}");
    assert_eq!(local.current_hop_context(), active_ctx);

    // 2. Steer submission failing at `submit_peer_message` (`Unavailable`) leaves `active_hop` untouched.
    drop(controls);
    let (local2, controls2) = test_local_state();
    controls2.set_phase_for_test(RuntimePhase::InTurn);
    local2.bind_hop_context(Some(7), active_ctx.clone());
    drop(controls2);
    let resp2 = dispatch_ipc_request(&local2, send_req("caller-c", true)).await;
    assert!(matches!(resp2, MeshResponse::Error { .. }), "got {resp2:?}");
    assert_eq!(local2.current_hop_context(), active_ctx);

    // 3. `Ready` submission failing at `submit_peer_message` (`Unavailable`) leaves `active_hop` untouched.
    let (local3, controls3) = test_local_state();
    local3.bind_hop_context(Some(7), active_ctx.clone());
    drop(controls3);
    let resp3 = dispatch_ipc_request(&local3, send_req("caller-c", false)).await;
    assert!(matches!(resp3, MeshResponse::Error { .. }), "got {resp3:?}");
    assert_eq!(local3.current_hop_context(), active_ctx);
}

#[test]
fn bind_hop_context_merges_same_turn_steer_and_replaces_new_turn() {
    let (local, _controls) = test_local_state();
    local.bind_hop_context(
        Some(10),
        MeshHopContext {
            hop_depth: 2,
            visited_sessions: vec!["s1".into(), "s2".into()],
            waiting_sessions: vec!["s1".into()],
        },
    );
    // Steer on the same active turn (`Some(10)`) with lower hop_depth merges union + max depth.
    local.bind_hop_context(
        Some(10),
        MeshHopContext {
            hop_depth: 1,
            visited_sessions: vec!["s2".into(), "s3".into()],
            waiting_sessions: vec!["s3".into()],
        },
    );
    assert_eq!(
        local.current_hop_context(),
        MeshHopContext {
            hop_depth: 2,
            visited_sessions: vec!["s1".into(), "s2".into(), "s3".into()],
            waiting_sessions: vec!["s1".into(), "s3".into()],
        }
    );

    // Binding a subsequent turn (`Some(11)`) replaces the prior turn's context cleanly.
    let next = MeshHopContext {
        hop_depth: 1,
        visited_sessions: vec!["s4".into()],
        waiting_sessions: vec![],
    };
    local.bind_hop_context(Some(11), next.clone());
    assert_eq!(local.current_hop_context(), next);
}
