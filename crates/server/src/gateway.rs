//! The bridge gateway process.
//!
//! A bridge worker needs three things from the machine the operator watches:
//! somewhere to register and report capacity, somewhere to pull assignments
//! from, and a way to exchange the frames it already produces (session output,
//! state changes, and permission requests waiting for a person). The first two
//! are the control plane; this module adds the third and serves both from one
//! process.
//!
//! The frame relay is deliberately opaque: a frame is a `TransportMessage`
//! from the bridge crate — correlation id, type and a JSON payload — and the
//! gateway stores it under a
//! per-environment sequence number. It does not interpret permission requests,
//! decide anything, or rewrite a payload. The worker keeps its own
//! `PermissionFrameRelay`, which is what makes an answer resolve exactly once;
//! an operator client reads `permission_request` frames and writes
//! `permission_response` frames.
//!
//! Endpoints:
//!
//! - Everything in [`crate::control_plane`] (register, assignments, claim,
//!   capacity, listing), plus `POST /api/environments/{env}/assignments` to
//!   submit work.
//! - `POST /api/environments/{env}/frames?to=host|worker` — enqueue one frame
//!   addressed to a role, returns its sequence number. A worker sends its
//!   output and permission requests to `host`; an operator answers to
//!   `worker`.
//! - `GET /api/environments/{env}/frames?for=host|worker&after=N&wait=S` —
//!   the frames addressed to that role after `N`, long-polling up to `S`
//!   seconds. Reading does not consume: `after` is the cursor, so a retried
//!   request cannot lose a frame, and a worker never reads back its own.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::control_plane::{ControlPlaneState, route};

/// The path frames hang off, below `/api/environments/{env}`.
pub const FRAMES_PATH: &str = "frames";

/// How many unread frames one environment may have before the gateway refuses
/// more. A worker whose operator is gone should get an error, not a memory
/// leak.
pub const MAX_PENDING_FRAMES: usize = 4096;

/// The longest a `wait` may ask the gateway to hold a request open.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// One environment's unread frames.
#[derive(Debug, Default)]
struct Mailbox {
    frames: VecDeque<(u64, Value)>,
    next_seq: u64,
}

impl Mailbox {
    fn push(&mut self, frame: Value) -> Option<u64> {
        if self.frames.len() >= MAX_PENDING_FRAMES {
            return None;
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        self.frames.push_back((seq, frame));
        Some(seq)
    }

    /// Frames after `after`, oldest first, with their sequence numbers.
    fn after(&self, after: u64) -> Vec<(u64, Value)> {
        self.frames
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect()
    }
}

/// The gateway: the control plane plus frame mailboxes, one process.
#[derive(Debug)]
pub struct GatewayServer {
    port: u16,
    control_plane: Arc<Mutex<ControlPlaneState>>,
    mailboxes: Arc<Mutex<HashMap<String, Mailbox>>>,
}

impl GatewayServer {
    /// A gateway listening on `port` (0 picks a free one).
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            control_plane: Arc::new(Mutex::new(ControlPlaneState::default())),
            mailboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The control plane state, for an embedding process that wants to submit
    /// work directly.
    #[must_use]
    pub fn control_plane(&self) -> Arc<Mutex<ControlPlaneState>> {
        Arc::clone(&self.control_plane)
    }

    /// Bind and serve until the process stops.
    pub async fn run(&self) -> Result<(), String> {
        let addr: SocketAddr = ([0, 0, 0, 0], self.port).into();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|error| format!("failed to bind the gateway on {addr}: {error}"))?;
        self.run_on(listener).await
    }

    /// Serve an already-bound listener (tests bind port 0).
    pub async fn run_on(&self, listener: TcpListener) -> Result<(), String> {
        let address = listener
            .local_addr()
            .map_err(|error| format!("could not read the listener address: {error}"))?;
        info!("bridge gateway listening on http://{address}");
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(error) => {
                    warn!("gateway accept error: {error}");
                    continue;
                }
            };
            let control_plane = Arc::clone(&self.control_plane);
            let mailboxes = Arc::clone(&self.mailboxes);
            tokio::spawn(async move {
                if let Err(error) = handle(stream, control_plane, mailboxes).await {
                    warn!("gateway request failed: {error}");
                }
            });
        }
    }

    /// Enqueue a frame addressed to `role` (the HTTP route calls this).
    pub async fn enqueue_frame(&self, env_id: &str, role: &str, frame: Value) -> Option<u64> {
        enqueue(&self.mailboxes, env_id, role, frame).await
    }

    /// Frames addressed to `role` after `after`, waiting up to `wait`.
    pub async fn frames_after(
        &self,
        env_id: &str,
        role: &str,
        after: u64,
        wait: Duration,
    ) -> Vec<(u64, Value)> {
        frames_after(&self.mailboxes, env_id, role, after, wait).await
    }
}

/// A frame's addressee. Anything else is refused rather than stored in a
/// mailbox nobody reads.
fn valid_role(role: &str) -> bool {
    matches!(role, "host" | "worker")
}

/// The mailbox key: one mailbox per environment and addressee.
fn mailbox_key(env_id: &str, role: &str) -> String {
    format!("{env_id}/{role}")
}

/// Store one frame, refusing when the environment already has too many unread.
async fn enqueue(
    mailboxes: &Arc<Mutex<HashMap<String, Mailbox>>>,
    env_id: &str,
    role: &str,
    frame: Value,
) -> Option<u64> {
    mailboxes
        .lock()
        .await
        .entry(mailbox_key(env_id, role))
        .or_default()
        .push(frame)
}

/// Frames after `after`, waiting up to `wait` for one to arrive.
async fn frames_after(
    mailboxes: &Arc<Mutex<HashMap<String, Mailbox>>>,
    env_id: &str,
    role: &str,
    after: u64,
    wait: Duration,
) -> Vec<(u64, Value)> {
    let deadline = Instant::now() + wait.min(MAX_WAIT);
    let key = mailbox_key(env_id, role);
    loop {
        let frames = mailboxes
            .lock()
            .await
            .get(&key)
            .map_or_else(Vec::new, |mailbox| mailbox.after(after));
        if !frames.is_empty() || Instant::now() >= deadline {
            return frames;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Serve one HTTP request.
async fn handle(
    mut stream: TcpStream,
    control_plane: Arc<Mutex<ControlPlaneState>>,
    mailboxes: Arc<Mutex<HashMap<String, Mailbox>>>,
) -> Result<(), String> {
    let (method, path, query, body) = read_request(&mut stream).await?;
    let response = route_request(&control_plane, &mailboxes, &method, &path, &query, &body).await;
    write_response(&mut stream, response.0, &response.1).await
}

/// Route one request: frames here, everything else to the control plane.
async fn route_request(
    control_plane: &Arc<Mutex<ControlPlaneState>>,
    mailboxes: &Arc<Mutex<HashMap<String, Mailbox>>>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
) -> (u16, Value) {
    if let Some(env_id) = frames_environment(path) {
        return match method {
            "POST" => {
                let role = query.get("to").map_or("host", String::as_str);
                if !valid_role(role) {
                    return (400, json!({"error": format!("unknown role {role:?}")}));
                }
                match serde_json::from_slice::<Value>(body) {
                    Ok(frame) => match enqueue(mailboxes, env_id, role, frame).await {
                        Some(seq) => (200, json!({"seq": seq})),
                        None => (
                            429,
                            json!({"error": "the environment has too many unread frames"}),
                        ),
                    },
                    Err(error) => (400, json!({"error": format!("invalid frame: {error}")})),
                }
            }
            "GET" => {
                let role = query.get("for").map_or("host", String::as_str);
                if !valid_role(role) {
                    return (400, json!({"error": format!("unknown role {role:?}")}));
                }
                let after = query
                    .get("after")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                let wait = query
                    .get("wait")
                    .and_then(|value| value.parse::<u64>().ok())
                    .map_or(Duration::ZERO, Duration::from_secs);
                let frames: Vec<Value> = frames_after(mailboxes, env_id, role, after, wait)
                    .await
                    .into_iter()
                    .map(|(seq, frame)| json!({"seq": seq, "frame": frame}))
                    .collect();
                (
                    200,
                    json!({"env_id": env_id, "frames": frames, "after": after}),
                )
            }
            _ => (404, json!({"error": "not found"})),
        };
    }

    let body = String::from_utf8_lossy(body).to_string();
    route(method, path, &body, control_plane).await
}

/// The environment whose frames a path names, when it names one.
fn frames_environment(path: &str) -> Option<&str> {
    let rest = path
        .strip_prefix("/api/environments/")?
        .strip_suffix(&format!("/{FRAMES_PATH}"))?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest)
}

/// Read one HTTP request into method, path, query and body.
async fn read_request(
    stream: &mut TcpStream,
) -> Result<(String, String, HashMap<String, String>, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("the connection closed before the request head".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Err("the request head is larger than 64 KiB".to_string());
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target, HashMap::new()),
    };

    let content_length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if content_length > 64 * 1024 * 1024 {
        return Err("the request body is larger than 64 MiB".to_string());
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok((method, path, query, body))
}

/// Parse `a=1&b=2` into a map. Repeated keys keep the first value.
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.entry(key.to_string())
            .or_insert_with(|| value.to_string());
    }
    out
}

/// Write one JSON response.
async fn write_response(stream: &mut TcpStream, status: u16, body: &Value) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        _ => "Unknown",
    };
    let payload = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(body).map_err(|error| error.to_string())?
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&payload)
        .await
        .map_err(|error| error.to_string())?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_frames_path_names_an_environment() {
        assert_eq!(
            frames_environment("/api/environments/env-a/frames"),
            Some("env-a")
        );
        assert_eq!(frames_environment("/api/environments/env-a"), None);
        assert_eq!(frames_environment("/api/environments/a/b/frames"), None);
        assert_eq!(frames_environment("/api/environments/frames"), None);
        assert_eq!(frames_environment("/other"), None);
    }

    #[test]
    fn a_query_keeps_its_first_value() {
        let query = parse_query("after=3&wait=2&after=9&flag");
        assert_eq!(query.get("after").map(String::as_str), Some("3"));
        assert_eq!(query.get("wait").map(String::as_str), Some("2"));
        assert_eq!(query.get("flag").map(String::as_str), Some(""));
    }

    #[tokio::test]
    async fn frames_are_ordered_and_a_cursor_cannot_lose_one() {
        let gateway = GatewayServer::new(0);
        let first = gateway
            .enqueue_frame("env-a", "host", json!({"type": "one"}))
            .await
            .unwrap();
        let second = gateway
            .enqueue_frame("env-a", "host", json!({"type": "two"}))
            .await
            .unwrap();
        assert_eq!((first, second), (1, 2));

        let all = gateway
            .frames_after("env-a", "host", 0, Duration::ZERO)
            .await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], (1, json!({"type": "one"})));
        let rest = gateway
            .frames_after("env-a", "host", first, Duration::ZERO)
            .await;
        assert_eq!(rest, vec![(2, json!({"type": "two"}))]);
        // Reading twice returns the same frames: the cursor, not the call,
        // decides what is consumed.
        assert_eq!(
            gateway
                .frames_after("env-a", "host", 0, Duration::ZERO)
                .await
                .len(),
            2
        );
        // The worker's mailbox is separate.
        assert!(
            gateway
                .frames_after("env-a", "worker", 0, Duration::ZERO)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_long_poll_returns_as_soon_as_a_frame_arrives() {
        let gateway = Arc::new(GatewayServer::new(0));
        let waiter = Arc::clone(&gateway);
        let handle = tokio::spawn(async move {
            waiter
                .frames_after("env-a", "host", 0, Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gateway
            .enqueue_frame("env-a", "host", json!({"type": "wake"}))
            .await
            .unwrap();
        let frames = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the long poll should wake")
            .unwrap();
        assert_eq!(frames, vec![(1, json!({"type": "wake"}))]);
    }

    #[tokio::test]
    async fn an_environment_with_too_many_unread_frames_is_refused() {
        let gateway = GatewayServer::new(0);
        for _ in 0..MAX_PENDING_FRAMES {
            gateway
                .enqueue_frame("env-a", "host", json!({}))
                .await
                .unwrap();
        }
        assert!(
            gateway
                .enqueue_frame("env-a", "host", json!({}))
                .await
                .is_none()
        );
        // A different environment is unaffected.
        assert!(
            gateway
                .enqueue_frame("env-b", "host", json!({}))
                .await
                .is_some()
        );
    }
}
