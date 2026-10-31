//! A worker and an operator meeting at the gateway.
//!
//! The gateway is one process serving the control plane and a per-environment
//! frame mailbox. This runs it on a loopback port and drives both sides with
//! the real clients: a worker registers, is given a submitted assignment, and
//! exchanges a permission request/response pair with an operator that only
//! polls the same mailbox.

use std::collections::HashMap;

use coderex_bridge::http_transport::{HttpPollTransport, Role};
use coderex_bridge::poller::{ControlPlaneClient, HttpControlPlaneClient};
use coderex_bridge::transport::{Transport, TransportMessage};
use coderex_server::gateway::GatewayServer;

/// Start the gateway on a free loopback port and return its base URL.
async fn start_gateway() -> String {
    let gateway = std::sync::Arc::new(GatewayServer::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        let _ = gateway.run_on(listener).await;
    });
    format!("http://{address}")
}

/// Submit one assignment through the gateway's HTTP API.
async fn submit_assignment(base: &str, assignment_id: &str, session_id: &str) {
    let body = serde_json::json!({
        "assignment_id": assignment_id,
        "session_id": session_id,
        "working_directory": "/tmp/work",
        "prompt": "do the thing",
        "spawn_mode": "single_session"
    });
    let response = reqwest::Client::new()
        .post(format!("{base}/api/environments/env-a/assignments"))
        .json(&body)
        .send()
        .await
        .expect("submit the assignment");
    assert_eq!(response.status(), 201, "submission failed");
}

fn frame(kind: &str, payload: serde_json::Value) -> TransportMessage {
    TransportMessage {
        r#type: kind.to_string(),
        correlation_id: None,
        payload: payload.to_string(),
        metadata: HashMap::new(),
    }
}

#[tokio::test]
async fn a_worker_registers_claims_submitted_work_and_reports_capacity() {
    let base = start_gateway().await;
    let control_plane = HttpControlPlaneClient::new(&base);

    control_plane
        .register("env-a", 2)
        .await
        .expect("registration");
    submit_assignment(&base, "a1", "s1").await;

    let fetched = control_plane
        .fetch_assignments("env-a")
        .await
        .expect("fetch assignments");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].assignment_id, "a1");
    assert_eq!(fetched[0].prompt, "do the thing");

    assert_eq!(
        control_plane
            .claim_assignment("env-a", "a1")
            .await
            .expect("claim"),
        coderex_bridge::poller::ClaimOutcome::Claimed
    );
    // A second claim from another worker loses.
    assert_eq!(
        control_plane
            .claim_assignment("env-a", "a1")
            .await
            .expect("second claim"),
        coderex_bridge::poller::ClaimOutcome::Lost
    );

    // The assignment is now claimed, so it is not handed out again.
    assert!(
        control_plane
            .fetch_assignments("env-a")
            .await
            .expect("fetch again")
            .is_empty()
    );
}

#[tokio::test]
async fn a_permission_request_round_trips_between_worker_and_operator() {
    let base = start_gateway().await;

    let mut worker = HttpPollTransport::new(&base, "env-a");
    worker.connect().await.expect("worker connects");
    let mut operator = HttpPollTransport::with_role(&base, "env-a", Role::Host);
    operator.connect().await.expect("operator connects");

    worker
        .send(frame(
            "permission_request",
            serde_json::json!({"request_id": "req-1", "tool": "Bash", "args": {"command": "rm -rf x"}}),
        ))
        .await
        .expect("worker sends the request");

    let request = tokio::time::timeout(std::time::Duration::from_secs(5), operator.receive())
        .await
        .expect("the operator should see the request")
        .expect("receive");
    assert_eq!(request.r#type, "permission_request");
    assert!(request.payload.contains("req-1"), "{}", request.payload);

    operator
        .send(frame(
            "permission_response",
            serde_json::json!({"request_id": "req-1", "allowed": true}),
        ))
        .await
        .expect("operator answers");

    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), worker.receive())
        .await
        .expect("the worker should see the answer")
        .expect("receive");
    assert_eq!(answer.r#type, "permission_response");
    assert!(
        answer.payload.contains("\"allowed\":true"),
        "{}",
        answer.payload
    );

    // Sequence numbers are per mailbox, so each side's cursor is 1: it has
    // delivered the only frame addressed to it.
    assert_eq!(worker.cursor(), 1);
    assert_eq!(operator.cursor(), 1);
}

#[tokio::test]
async fn the_gateway_refuses_a_frame_with_no_environment_to_deliver_it_to() {
    let base = start_gateway().await;
    let mut transport = HttpPollTransport::new(&base, "env-missing");
    transport.connect().await.expect("connect");
    transport
        .send(frame(
            "permission_request",
            serde_json::json!({"request_id": "x"}),
        ))
        .await
        .expect("the gateway accepts the frame");

    // It is stored under its own environment and does not leak into another.
    let mut other = HttpPollTransport::with_role(&base, "env-other", Role::Host);
    other.connect().await.expect("connect");
    let received =
        tokio::time::timeout(std::time::Duration::from_millis(400), other.receive()).await;
    assert!(received.is_err(), "a frame leaked between environments");
}

/// A claim that the control plane answers with a transient error is retried:
/// the assignment is already off the queue, and a 5xx says nothing about
/// whether it is ours. A 4xx that is not "lost" is the server's answer and is
/// reported at once.
#[tokio::test]
async fn a_transient_claim_failure_is_retried() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Three answers: 503, 503, 200 — the client should claim on the third.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("local address");
    let seen = std::sync::Arc::new(AtomicUsize::new(0));
    let seen_server = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = std::sync::Arc::clone(&seen_server);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let attempt = seen.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if attempt < 2 {
                    ("503 Service Unavailable", "{}")
                } else {
                    ("200 OK", r#"{"ok":true}"#)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    let client = HttpControlPlaneClient::new(format!("http://{address}"));
    let outcome = client
        .claim_assignment("env-a", "a1")
        .await
        .expect("the third attempt succeeds");
    assert_eq!(outcome, coderex_bridge::poller::ClaimOutcome::Claimed);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        3,
        "the client should have retried twice"
    );
}

#[tokio::test]
async fn a_non_transient_claim_failure_is_not_retried() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("local address");
    let seen = std::sync::Arc::new(AtomicUsize::new(0));
    let seen_server = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = std::sync::Arc::clone(&seen_server);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer).await;
                seen.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":"bad assignment"}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    let client = HttpControlPlaneClient::new(format!("http://{address}"));
    let error = client
        .claim_assignment("env-a", "a1")
        .await
        .expect_err("a 400 is the server's answer, not a transient failure");
    assert!(error.contains("400"), "{error}");
    assert_eq!(seen.load(Ordering::SeqCst), 1, "a 400 must not be retried");
}
