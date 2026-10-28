//! Bridge integration tests — covering environment registration, work assignment,
//! permission forwarding, worktree spawn, and disconnect recovery.
//!
//! These tests correspond to the equivalence test plan Module 12:
//! 12.1 environment registration, 12.2 work assignment, 12.3 permission forwarding,
//! 12.4 worktree spawn, 12.5 disconnect recovery

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use coderex_bridge::archive::UsageStats;
use coderex_bridge::heartbeat::{HeartbeatConfig, HeartbeatStatus, WsReconnector};
use coderex_bridge::orchestrator::{Bridge, BridgeConfig, BridgeEvent, ManagedSession};
use coderex_bridge::permission::{BridgePermissionRouter, PermissionResponse};
use coderex_bridge::poller::{BridgeWorkPoller, SpawnMode, WorkAssignment};
use coderex_bridge::relay::{
    APPROVED_INTENT_HEADER, ApprovedIntent, RelayDecision, TOOL_USE_ID_HEADER,
};
use coderex_bridge::spawner::{BridgeSessionSpawner, SpawnConfig};
use coderex_bridge::state::{BridgeSessionState, BridgeSessionStateMachine};
use coderex_bridge::transport::{InProcessTransport, Transport, TransportMessage};
use coderex_bridge::ws_transport::{WebSocketTransport, WsTransportConfig};

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn test_config(base: &std::path::Path) -> BridgeConfig {
    BridgeConfig {
        env_id: "test_env".into(),
        max_concurrent: 3,
        control_plane_url: "http://localhost:3000".into(),
        repo_root: base.to_path_buf(),
        worktree_base: base.join("worktrees"),
        session_base: base.join("sessions"),
        archive_base: base.join("archives"),
        heartbeat: HeartbeatConfig::default(),
    }
}

fn test_bridge(base: &std::path::Path) -> Bridge {
    let config = test_config(base);
    let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
    Bridge::new(config, Box::new(a))
}

fn init_git_repo(path: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .output()
        .expect("git init failed");
    std::fs::write(path.join("README"), "root").unwrap();
    std::process::Command::new("git")
        .args(["add", "README"])
        .current_dir(path)
        .output()
        .expect("git add failed");
    std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "init"])
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .current_dir(path)
        .output()
        .expect("git commit failed");
}

fn sample_assignment(session_id: &str, work_dir: &str, mode: SpawnMode) -> WorkAssignment {
    WorkAssignment {
        assignment_id: format!("a_{session_id}"),
        session_id: session_id.into(),
        working_directory: work_dir.into(),
        prompt: "test prompt".into(),
        spawn_mode: mode,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.1 Environment Registration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn env_registration_connect_establishes_transport() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    assert!(!bridge.is_running());
    bridge.connect().await.unwrap();
    assert!(bridge.is_running());
    assert_eq!(bridge.connection_id(), Some("bridge"));

    bridge.disconnect().await.unwrap();
}

#[test]
fn env_registration_poller_capacity_report() {
    let poller = BridgeWorkPoller::new("env_1".into(), 5);
    let report = poller.report_capacity();

    assert_eq!(report.env_id, "env_1");
    assert_eq!(report.max_concurrent, 5);
    assert_eq!(report.available_slots, 5);
    assert_eq!(report.active_sessions, 0);
}

#[test]
fn env_registration_poller_with_custom_url() {
    let poller = BridgeWorkPoller::with_url(
        "prod_env".into(),
        10,
        "https://control.example.com:8443".into(),
    );
    assert_eq!(poller.env_id, "prod_env");
    assert_eq!(poller.max_concurrent, 10);
}

#[tokio::test]
async fn env_registration_fails_without_server() {
    let poller = BridgeWorkPoller::new("env_1".into(), 5);
    let result = poller.register().await;
    assert!(
        result.is_err(),
        "registration should fail without a control plane server"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.2 Work Assignment — receive, queue, spawn
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn work_assignment_receive_and_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    bridge.receive_assignment(sample_assignment("s1", "/tmp/s1", SpawnMode::SingleSession));
    bridge.receive_assignment(sample_assignment("s2", "/tmp/s2", SpawnMode::SameDir));

    assert_eq!(bridge.pending_assignment_count(), 2);
}

#[test]
fn work_assignment_take_respects_capacity() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    // max_concurrent = 3
    bridge.receive_assignment(sample_assignment("s1", "/tmp/s1", SpawnMode::SingleSession));
    bridge.receive_assignment(sample_assignment("s2", "/tmp/s2", SpawnMode::SingleSession));
    bridge.receive_assignment(sample_assignment("s3", "/tmp/s3", SpawnMode::SingleSession));
    bridge.receive_assignment(sample_assignment("s4", "/tmp/s4", SpawnMode::SingleSession));

    assert!(bridge.take_assignment().is_some());
    assert!(bridge.take_assignment().is_some());
    assert!(bridge.take_assignment().is_some());
    // At capacity — should return None
    assert!(bridge.take_assignment().is_none());
}

#[tokio::test]
async fn work_assignment_spawn_single_session() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
    let mut bridge = test_bridge(tmp.path());
    bridge.connect().await.unwrap();

    let assignment = sample_assignment(
        "s1",
        tmp.path().join("work").to_str().unwrap(),
        SpawnMode::SingleSession,
    );

    let result = bridge.spawn_session(&assignment).await;
    assert!(result.is_ok());
    let spawn_result = result.unwrap();
    assert_eq!(spawn_result.session_id, "s1");
    assert!(!spawn_result.worktree_created);

    bridge.disconnect().await.unwrap();
}

#[tokio::test]
async fn work_assignment_spawn_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let wt = tmp.path().join("worktrees");
    let sess = tmp.path().join("sessions");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&sess).unwrap();
    init_git_repo(&repo);

    let config = BridgeConfig {
        env_id: "test_env".into(),
        max_concurrent: 3,
        control_plane_url: "http://localhost:3000".into(),
        repo_root: repo.clone(),
        worktree_base: wt.clone(),
        session_base: sess.clone(),
        archive_base: tmp.path().join("archives"),
        heartbeat: HeartbeatConfig::default(),
    };
    let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
    let mut bridge = Bridge::new(config, Box::new(a));
    bridge.connect().await.unwrap();

    let assignment = WorkAssignment {
        assignment_id: "a1".into(),
        session_id: "wt_session".into(),
        working_directory: tmp.path().to_str().unwrap().into(),
        prompt: "worktree test".into(),
        spawn_mode: SpawnMode::Worktree,
    };

    let result = bridge.spawn_session(&assignment).await;
    assert!(result.is_ok());
    let spawn_result = result.unwrap();
    assert!(spawn_result.worktree_created);
    assert!(spawn_result.work_dir.ends_with("worktrees/wt_session"));

    bridge.disconnect().await.unwrap();
}

#[test]
fn work_assignment_events_are_broadcast() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = test_bridge(tmp.path());
    let mut rx = bridge.subscribe();

    bridge.emit(BridgeEvent::SessionSpawned {
        session_id: "s1".into(),
        work_dir: "/tmp/s1".into(),
    });

    let event = rx.try_recv().unwrap();
    assert!(matches!(event, BridgeEvent::SessionSpawned { session_id, .. } if session_id == "s1"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.3 Permission Forwarding — request, route, respond
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn permission_forwarding_request_creates_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    let request_id = bridge.request_permission("s1", "Bash", "rm -rf /tmp");
    assert!(!bridge.is_permission_resolved(&request_id));
    assert_eq!(bridge.pending_permission_count("s1"), 1);
}

#[test]
fn permission_forwarding_respond_grants() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    let request_id = bridge.request_permission("s1", "Read", "secret.txt");
    assert!(!bridge.is_permission_resolved(&request_id));

    let resolved =
        bridge.respond_to_permission(&request_id, PermissionResponse::Allow { always: false });
    assert!(resolved);
    assert!(bridge.is_permission_resolved(&request_id));

    let response = bridge.get_permission_response(&request_id);
    assert!(matches!(
        response,
        Some(PermissionResponse::Allow { always: false })
    ));
}

#[test]
fn permission_forwarding_respond_denies() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    let request_id = bridge.request_permission("s1", "Write", "/etc/passwd");
    bridge.respond_to_permission(&request_id, PermissionResponse::Deny);

    let response = bridge.get_permission_response(&request_id);
    assert!(matches!(response, Some(PermissionResponse::Deny)));
}

#[test]
fn permission_forwarding_multiple_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    let r1 = bridge.request_permission("s1", "Bash", "ls");
    let _r2 = bridge.request_permission("s2", "Read", "a.txt");
    let _r3 = bridge.request_permission("s1", "Write", "b.txt");

    assert_eq!(bridge.pending_permission_count("s1"), 2);
    assert_eq!(bridge.pending_permission_count("s2"), 1);

    bridge.respond_to_permission(&r1, PermissionResponse::Allow { always: true });
    assert_eq!(bridge.pending_permission_count("s1"), 1);
}

#[test]
fn permission_forwarding_event_broadcast() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = test_bridge(tmp.path());
    let mut rx = bridge.subscribe();

    bridge.emit(BridgeEvent::PermissionRequested {
        request_id: "perm_0".into(),
        session_id: "s1".into(),
        tool: "Bash".into(),
    });

    let event = rx.try_recv().unwrap();
    assert!(
        matches!(event, BridgeEvent::PermissionRequested { request_id, .. } if request_id == "perm_0")
    );
}

#[test]
fn permission_router_prune_resolved() {
    let mut router = BridgePermissionRouter::new();
    let id = router.create_request("s1".into(), "Bash".into(), "ls".into());
    router.respond(&id, PermissionResponse::Allow { always: true });
    assert_eq!(router.pending_count(), 0);

    router.prune_resolved();
    assert_eq!(router.pending_count(), 0);
    assert!(router.get_response(&id).is_some()); // response kept for audit
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.4 Worktree Spawn — create, track, cleanup
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn worktree_spawner_creates_worktree() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let wt = base.path().join("worktrees");
    let sess = base.path().join("sessions");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&sess).unwrap();

    let spawner = BridgeSessionSpawner::new(repo, wt.clone(), sess);
    let rt = tokio::runtime::Runtime::new().unwrap();

    let config = SpawnConfig {
        session_id: "wt1".into(),
        work_dir: PathBuf::from("/tmp"),
        mode: SpawnMode::Worktree,
        branch: Some("HEAD".into()),
    };

    let result = rt.block_on(spawner.spawn(config)).unwrap();
    assert!(result.worktree_created);
    assert!(wt.join("wt1").exists());
    assert!(result.work_dir.ends_with("worktrees/wt1"));

    // Cleanup
    spawner.cleanup_worktree("wt1").unwrap();
    assert!(!wt.join("wt1").exists());
}

#[test]
fn worktree_spawner_same_dir_does_not_create_worktree() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let wt = base.path().join("worktrees");
    let sess = base.path().join("sessions");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&sess).unwrap();

    let spawner = BridgeSessionSpawner::new(repo, wt, sess);
    let rt = tokio::runtime::Runtime::new().unwrap();

    let config = SpawnConfig {
        session_id: "s1".into(),
        work_dir: PathBuf::from("/tmp/work"),
        mode: SpawnMode::SameDir,
        branch: None,
    };

    let result = rt.block_on(spawner.spawn(config)).unwrap();
    assert!(!result.worktree_created);
    assert_eq!(result.work_dir, PathBuf::from("/tmp/work"));
}

#[tokio::test]
async fn worktree_bridge_full_session_with_worktree() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let wt = base.path().join("worktrees");
    let sess = base.path().join("sessions");
    let archives = base.path().join("archives");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::create_dir_all(&archives).unwrap();

    let config = BridgeConfig {
        env_id: "test_env".into(),
        max_concurrent: 3,
        control_plane_url: "http://localhost:3000".into(),
        repo_root: repo,
        worktree_base: wt.clone(),
        session_base: sess.clone(),
        archive_base: archives,
        heartbeat: HeartbeatConfig::default(),
    };
    let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
    let mut bridge = Bridge::new(config, Box::new(a));
    bridge.connect().await.unwrap();

    // Receive work assignment with Worktree spawn mode
    bridge.receive_assignment(WorkAssignment {
        assignment_id: "a1".into(),
        session_id: "wt_sess".into(),
        working_directory: base.path().to_str().unwrap().into(),
        prompt: "fix the bug".into(),
        spawn_mode: SpawnMode::Worktree,
    });

    let assignment = bridge.take_assignment().unwrap();
    let spawn_result = bridge.spawn_session(&assignment).await.unwrap();
    assert!(spawn_result.worktree_created);
    assert!(wt.join("wt_sess").exists());

    // Permission request during session
    let perm_id = bridge.request_permission("wt_sess", "Read", "src/lib.rs");
    bridge.respond_to_permission(&perm_id, PermissionResponse::Allow { always: false });

    // Complete session lifecycle
    let usage = UsageStats {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: 0.01,
    };
    bridge
        .complete_session("wt_sess", "{\"type\":\"result\"}\n", usage)
        .await
        .unwrap();

    // Session cleaned up
    assert!(!bridge.sessions.contains_key("wt_sess"));

    bridge.disconnect().await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.5 Disconnect Recovery — heartbeat, reconnection, state recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn disconnect_recovery_transport_disconnect_reconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    bridge.connect().await.unwrap();
    assert!(bridge.is_running());

    // Simulate disconnect
    bridge.disconnect().await.unwrap();
    assert!(!bridge.is_running());

    // Reconnect
    bridge.connect().await.unwrap();
    assert!(bridge.is_running());
}

#[test]
fn disconnect_recovery_heartbeat_status_transitions() {
    use coderex_bridge::heartbeat::Heartbeat;

    let mut hb = Heartbeat::new(HeartbeatConfig {
        degraded_threshold: 2,
        lost_threshold: 5,
        ..Default::default()
    });

    assert_eq!(hb.status(), HeartbeatStatus::Healthy);

    hb.record_failure();
    hb.record_failure();
    assert_eq!(hb.status(), HeartbeatStatus::Degraded);

    for _ in 0..3 {
        hb.record_failure();
    }
    assert_eq!(hb.status(), HeartbeatStatus::Lost);

    // Reset on reconnect
    hb.reset();
    assert_eq!(hb.status(), HeartbeatStatus::Healthy);
}

#[test]
fn disconnect_recovery_reconnector_backoff() {
    let mut rc = WsReconnector::new(5);

    let d1 = rc.next_delay().unwrap();
    let d2 = rc.next_delay().unwrap();
    let d3 = rc.next_delay().unwrap();

    assert!(d2 > d1);
    assert!(d3 > d2);
    assert!(rc.can_reconnect());

    // Exhaust attempts
    let _ = rc.next_delay();
    let _ = rc.next_delay();
    assert!(rc.next_delay().is_none());
    assert!(!rc.can_reconnect());

    // Reset after successful reconnect
    rc.reset();
    assert_eq!(rc.attempt_count(), 0);
    assert!(rc.can_reconnect());
}

#[test]
fn disconnect_recovery_session_state_after_disconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    // Manually inject a session
    bridge.sessions.insert(
        "s1".into(),
        ManagedSession {
            state_machine: BridgeSessionStateMachine::new("s1".into()),
            work_dir: PathBuf::from("/tmp/s1"),
            worktree_created: false,
        },
    );

    let recovery = bridge.create_recovery_state("s1");
    assert!(recovery.is_some());
    let state = recovery.unwrap();
    assert_eq!(state.session_id, "s1");
    assert!(!state.has_pending_tools());
}

#[test]
fn disconnect_recovery_all_active_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bridge = test_bridge(tmp.path());

    bridge.sessions.insert(
        "s1".into(),
        ManagedSession {
            state_machine: BridgeSessionStateMachine::new("s1".into()),
            work_dir: PathBuf::from("/tmp/s1"),
            worktree_created: false,
        },
    );
    bridge.sessions.insert(
        "s2".into(),
        coderex_bridge::orchestrator::ManagedSession {
            state_machine: BridgeSessionStateMachine::new("s2".into()),
            work_dir: PathBuf::from("/tmp/s2"),
            worktree_created: true,
        },
    );

    let states = bridge.all_recovery_states();
    assert_eq!(states.len(), 2);
}

#[tokio::test]
async fn disconnect_recovery_full_lifecycle_with_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
    std::fs::create_dir_all(tmp.path().join("archives")).unwrap();
    let mut bridge = test_bridge(tmp.path());

    bridge.connect().await.unwrap();

    // Spawn a session
    let assignment = sample_assignment(
        "s1",
        tmp.path().join("work").to_str().unwrap(),
        SpawnMode::SingleSession,
    );
    bridge.spawn_session(&assignment).await.unwrap();

    // Permission flow
    let perm_id = bridge.request_permission("s1", "Read", "file.txt");
    bridge.respond_to_permission(&perm_id, PermissionResponse::Allow { always: false });

    // Create recovery state before disconnect
    let recovery = bridge.create_recovery_state("s1").unwrap();
    assert_eq!(recovery.session_id, "s1");

    // Disconnect
    bridge.disconnect().await.unwrap();
    assert!(!bridge.is_running());

    // Reconnect
    bridge.connect().await.unwrap();
    assert!(bridge.is_running());

    // Complete session after reconnect
    let usage = UsageStats {
        input_tokens: 200,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: 0.02,
    };
    bridge
        .complete_session("s1", "{\"type\":\"result\"}\n", usage)
        .await
        .unwrap();
    assert!(!bridge.has_session("s1"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.6 Relay endpoint — the approval chain on the live protocol
// ═══════════════════════════════════════════════════════════════════════════

/// Send a frame over an in-process transport and read the reply to it.
async fn transport_round_trip(
    transport: &mut InProcessTransport,
    frame: &TransportMessage,
) -> TransportMessage {
    transport.send(frame.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), transport.receive())
        .await
        .expect("round trip timed out")
        .expect("transport ended")
}

/// Build the `relay_call` frame the gateway sends for a tool call.
fn relay_frame(
    session_id: &str,
    tool: &str,
    args: &str,
    headers: &[(&str, &str)],
) -> TransportMessage {
    TransportMessage {
        r#type: "relay_call".into(),
        correlation_id: Some("call_1".into()),
        payload: serde_json::json!({
            "session_id": session_id,
            "tool": tool,
            "args": args,
        })
        .to_string(),
        metadata: headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

/// Send a frame over a real WebSocket and read the reply to it.
async fn ws_round_trip(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    frame: &TransportMessage,
) -> TransportMessage {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    ws.send(Message::Text(serde_json::to_string(frame).unwrap().into()))
        .await
        .unwrap();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("relay round trip timed out")
            .expect("websocket ended")
            .expect("websocket error");
        if let Message::Text(text) = next {
            return serde_json::from_str(&text).expect("reply is a transport message");
        }
    }
}

/// The whole point of the relay endpoint: a call the person confirmed is
/// forwarded once, a replay of the same pair is refused, and the header
/// allowlist decides what the gate even sees — all over the wire.
#[tokio::test]
async fn a_relayed_call_is_authorized_over_the_websocket_protocol() {
    use tokio::net::TcpListener;

    let tmp = tempfile::tempdir().unwrap();
    let args = r#"{"command":"ls"}"#;

    // The gateway side of the relay: a real WebSocket peer.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    });

    let url = format!("ws://{addr}");
    let mut transport = WebSocketTransport::new(WsTransportConfig::new(url.clone()));
    transport
        .connect_ws(WsTransportConfig::new(url))
        .await
        .unwrap();

    let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(transport));
    let request_id = bridge.request_permission("sess_1", "Bash", args);
    assert!(bridge.respond_to_permission(&request_id, PermissionResponse::Allow { always: false }));

    let serving = tokio::spawn(async move { bridge.serve_transport().await });
    let mut ws = gateway.await.unwrap();

    let intent = ApprovedIntent::for_call("sess_1", "Bash", args);
    let encoded = intent.encode();
    let approved = relay_frame(
        "sess_1",
        "Bash",
        args,
        &[
            (TOOL_USE_ID_HEADER, request_id.as_str()),
            (APPROVED_INTENT_HEADER, encoded.as_str()),
            // A header the relay does not own: it must not cross the boundary.
            ("authorization", "Bearer smuggled"),
        ],
    );

    let reply = ws_round_trip(&mut ws, &approved).await;
    assert_eq!(reply.r#type, "relay_result");
    let decision: RelayDecision = serde_json::from_str(&reply.payload).unwrap();
    match decision {
        RelayDecision::Forward { headers, .. } => {
            assert!(headers.contains_key(TOOL_USE_ID_HEADER), "{headers:?}");
            assert!(
                !headers.contains_key("authorization"),
                "the forwarding policy let a foreign header through: {headers:?}"
            );
        }
        other => panic!("the confirmed call should have been forwarded, got {other:?}"),
    }

    // One confirmation authorises one call.
    let replay = ws_round_trip(&mut ws, &approved).await;
    let decision: RelayDecision = serde_json::from_str(&replay.payload).unwrap();
    match decision {
        RelayDecision::Refuse { status, code, .. } => {
            assert_eq!(status, 403);
            assert_eq!(code, "write_relay_unapproved");
        }
        other => panic!("a replayed header pair must be refused, got {other:?}"),
    }

    // A call whose pair headers were smuggled under names the relay does not
    // forward arrives with no binding at all, and is refused like any other.
    let smuggled = relay_frame(
        "sess_1",
        "Bash",
        args,
        &[
            ("x-pair-id", request_id.as_str()),
            ("x-pair-intent", encoded.as_str()),
        ],
    );
    let reply = ws_round_trip(&mut ws, &smuggled).await;
    let decision: RelayDecision = serde_json::from_str(&reply.payload).unwrap();
    assert!(
        matches!(decision, RelayDecision::Refuse { status: 403, .. }),
        "{decision:?}"
    );

    serving.abort();
}

#[tokio::test]
async fn a_relayed_call_is_refused_when_the_person_denied_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (bridge_transport, mut gateway) =
        InProcessTransport::create_pair("bridge".into(), "gateway".into());
    gateway.connect().await.unwrap();
    let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
    bridge.connect().await.unwrap();
    let request_id = bridge.request_permission("sess_1", "Bash", "ls");
    assert!(bridge.respond_to_permission(&request_id, PermissionResponse::Deny));

    let serving = tokio::spawn(async move { bridge.serve_transport().await });

    let intent = ApprovedIntent::for_call("sess_1", "Bash", "ls");
    let encoded = intent.encode();
    let frame = relay_frame(
        "sess_1",
        "Bash",
        "ls",
        &[
            (TOOL_USE_ID_HEADER, request_id.as_str()),
            (APPROVED_INTENT_HEADER, encoded.as_str()),
        ],
    );

    let reply = transport_round_trip(&mut gateway, &frame).await;
    let decision: RelayDecision = serde_json::from_str(&reply.payload).unwrap();
    match decision {
        RelayDecision::Refuse { status, code, .. } => {
            assert_eq!(status, 403);
            assert_eq!(code, "write_relay_unapproved");
        }
        other => panic!("a denied call must not be forwarded, got {other:?}"),
    }

    serving.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.7 Worker epoch write fence
// ═══════════════════════════════════════════════════════════════════════════

fn session_write_frame(epoch: u64, state: &str) -> TransportMessage {
    TransportMessage {
        r#type: "session_write".into(),
        correlation_id: Some(format!("w{epoch}")),
        payload: serde_json::json!({ "session_id": "s1", "state": state }).to_string(),
        metadata: HashMap::from([("worker_epoch".to_string(), epoch.to_string())]),
    }
}

/// A worker that has been replaced must not be able to write back over the
/// worker that replaced it.
#[tokio::test]
async fn a_stale_worker_epoch_cannot_write_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (bridge_transport, mut gateway) =
        InProcessTransport::create_pair("bridge".into(), "gateway".into());
    gateway.connect().await.unwrap();
    let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
    bridge.connect().await.unwrap();

    let serving = tokio::spawn(async move { bridge.serve_transport().await });

    // The replacement worker takes the session.
    let reply = transport_round_trip(&mut gateway, &session_write_frame(2, "running")).await;
    assert_eq!(reply.r#type, "session_write_result");
    let payload: serde_json::Value = serde_json::from_str(&reply.payload).unwrap();
    assert_eq!(payload["accepted"], true, "{payload}");

    // The worker it replaced is still finishing its last request.
    let reply = transport_round_trip(&mut gateway, &session_write_frame(1, "archiving")).await;
    let payload: serde_json::Value = serde_json::from_str(&reply.payload).unwrap();
    assert_eq!(payload["accepted"], false, "{payload}");
    assert_eq!(payload["current_epoch"], 2, "{payload}");
    assert!(
        payload["reason"].as_str().unwrap().contains("stale"),
        "{payload}"
    );

    serving.abort();
}

#[tokio::test]
async fn a_write_without_an_epoch_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (bridge_transport, mut gateway) =
        InProcessTransport::create_pair("bridge".into(), "gateway".into());
    gateway.connect().await.unwrap();
    let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
    bridge.connect().await.unwrap();

    let serving = tokio::spawn(async move { bridge.serve_transport().await });

    let frame = TransportMessage {
        r#type: "session_write".into(),
        correlation_id: Some("w0".into()),
        payload: serde_json::json!({ "session_id": "s1", "state": "running" }).to_string(),
        metadata: HashMap::new(),
    };
    let reply = transport_round_trip(&mut gateway, &frame).await;
    assert_eq!(reply.r#type, "error");
    assert!(reply.payload.contains("worker_epoch"), "{}", reply.payload);

    serving.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
//  12.8 Stream resume — the sequence cursor on reconnect
// ═══════════════════════════════════════════════════════════════════════════

/// A reconnect asks for the gap, not the whole stream: the worker comes back
/// with the sequence number it got to, in both spellings.
#[tokio::test]
async fn a_reconnect_resumes_from_the_last_delivered_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let (bridge_transport, mut gateway) =
        InProcessTransport::create_pair("bridge".into(), "gateway".into());
    gateway.connect().await.unwrap();
    let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
    bridge.connect().await.unwrap();

    let serving = tokio::spawn(async move { bridge.serve_transport().await });

    for seq in 1..=3u64 {
        let beat = TransportMessage {
            r#type: "heartbeat".into(),
            correlation_id: Some(format!("h{seq}")),
            payload: "{}".into(),
            metadata: HashMap::from([("sequence_num".to_string(), seq.to_string())]),
        };
        let reply = transport_round_trip(&mut gateway, &beat).await;
        assert_eq!(reply.r#type, "heartbeat_ack");
    }

    // The gateway says the downstream ended.
    let end = TransportMessage {
        r#type: "stream_end".into(),
        correlation_id: None,
        payload: "{}".into(),
        metadata: HashMap::new(),
    };
    gateway.send(end).await.unwrap();

    let resume = tokio::time::timeout(Duration::from_secs(10), gateway.receive())
        .await
        .expect("the worker should have reconnected")
        .expect("transport ended");

    assert_eq!(resume.r#type, "resume");
    let payload: serde_json::Value = serde_json::from_str(&resume.payload).unwrap();
    assert_eq!(payload["from_sequence_num"], 4);
    assert_eq!(
        resume.metadata.get("last-event-id").map(String::as_str),
        Some("3")
    );

    serving.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Archive, cleanup, state machine
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn archive_and_cleanup_integration() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
    std::fs::create_dir_all(tmp.path().join("archives")).unwrap();
    let mut bridge = test_bridge(tmp.path());
    bridge.connect().await.unwrap();

    let assignment = sample_assignment(
        "s1",
        tmp.path().join("work").to_str().unwrap(),
        SpawnMode::SingleSession,
    );
    bridge.spawn_session(&assignment).await.unwrap();

    let usage = UsageStats {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        cost_usd: 0.01,
    };
    let transcript = "{\"type\":\"result\",\"subtype\":\"success\"}\n";
    bridge
        .complete_session("s1", transcript, usage)
        .await
        .unwrap();

    // Session should be archived and cleaned up
    assert!(!bridge.has_session("s1"));
    let archived =
        std::fs::read_to_string(tmp.path().join("archives/s1/transcript.jsonl")).unwrap();
    assert_eq!(archived, transcript);
    bridge.disconnect().await.unwrap();
}

#[test]
fn state_machine_valid_transitions() {
    let mut sm = BridgeSessionStateMachine::new("s1".into());
    assert!(matches!(sm.current, BridgeSessionState::Assigned));

    sm.transition(BridgeSessionState::Preparing).unwrap();
    sm.transition(BridgeSessionState::CreatingWorktree).unwrap();
    sm.transition(BridgeSessionState::Spawning).unwrap();
    sm.transition(BridgeSessionState::Running).unwrap();
    sm.transition(BridgeSessionState::AwaitingPermission)
        .unwrap();
    sm.transition(BridgeSessionState::Running).unwrap();
    sm.transition(BridgeSessionState::Archiving).unwrap();
    sm.transition(BridgeSessionState::CleaningUp).unwrap();
    sm.transition(BridgeSessionState::Completed).unwrap();

    assert!(sm.is_terminal());
    assert_eq!(sm.transition_count(), 10);
}

#[test]
fn state_machine_invalid_transition_blocked() {
    let mut sm = BridgeSessionStateMachine::new("s1".into());
    sm.transition(BridgeSessionState::Preparing).unwrap();

    // Cannot go from Preparing to Running directly
    let result = sm.transition(BridgeSessionState::Running);
    assert!(result.is_err());
}

#[test]
fn state_machine_fail_from_any_state() {
    let mut sm = BridgeSessionStateMachine::new("s1".into());
    sm.transition(BridgeSessionState::Preparing).unwrap();
    sm.transition(BridgeSessionState::Spawning).unwrap();
    sm.fail("spawn timeout".into());

    assert!(sm.is_terminal());
    assert!(matches!(sm.current, BridgeSessionState::Failed(_)));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Process cleanup — the registry a spawn site fills, and what cleanup does
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
#[test]
fn cleanup_terminates_a_registered_child() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = coderex_bridge::archive::BridgeCleanupManager::new(
        dir.path().join("worktrees"),
        dir.path().join("sessions"),
    );
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    manager.track_process("session-1", pid);
    assert!(manager.has_tracked_process("session-1"));

    let report = manager.cleanup("session-1").expect("cleanup");

    assert!(report.process_terminated);
    let _ = child.wait();
    assert!(!alive(pid), "the registered child is gone");
    assert_eq!(manager.tracked_process_count(), 0);
}

#[cfg(unix)]
#[test]
fn cleanup_of_a_tracked_tree_kills_the_grandchild() {
    use std::io::{BufRead, BufReader};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let mut manager = coderex_bridge::archive::BridgeCleanupManager::new(
        dir.path().join("worktrees"),
        dir.path().join("sessions"),
    );

    let tree = coderex_core::process_tree::ProcessTree::create(Default::default()).unwrap();
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 60 & echo $!; wait")
        .stdout(std::process::Stdio::piped());
    tree.prepare(&mut command);
    let mut child = command.spawn().expect("spawn shell");
    tree.attach(&child).expect("attach");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("pid");

    manager.track_tree("session-tree", Arc::new(tree));
    let report = manager.cleanup("session-tree").expect("cleanup");
    let _ = child.wait();

    assert!(report.process_terminated);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!alive(grandchild), "the grandchild is gone too");
}

#[test]
fn cleanup_of_an_untracked_session_reports_no_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = coderex_bridge::archive::BridgeCleanupManager::new(
        dir.path().join("worktrees"),
        dir.path().join("sessions"),
    );

    let report = manager.cleanup("unknown").expect("cleanup");

    assert!(!report.process_terminated);
    assert!(!report.worktree_removed);
    assert!(!report.session_dir_removed);
}

#[test]
fn the_shared_registry_sees_registrations_from_the_spawn_site() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = coderex_bridge::archive::BridgeCleanupManager::new(
        dir.path().join("worktrees"),
        dir.path().join("sessions"),
    );
    let registry = manager.process_registry();
    registry.track_process("shared", u32::MAX);

    assert!(manager.has_tracked_process("shared"));
    assert_eq!(manager.tracked_process_count(), 1);
    manager.untrack_process("shared");
    assert!(!manager.has_tracked_process("shared"));
}
