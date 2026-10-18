//! End-to-end checks for remote MCP HTTP/SSE transport.

use coderex_mcp::channel::{InboundRequestHandler, JsonRpcChannel, JsonRpcError};
use coderex_mcp::config::{McpConfig, McpServerConfig, McpTransport};
use coderex_mcp::manager::McpManager;
use coderex_mcp::oauth::{McpOAuthState, OAuthManager, OAuthTokenProvider};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);

        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then_some(())?;
                    value.trim().parse::<usize>().ok()
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

/// Whether a recorded request is the standalone stream's GET.
fn is_get(request: &str) -> bool {
    request.starts_with("GET ")
}

/// The value of `name` in a recorded HTTP request's headers.
fn header_value(request: &str, name: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (header, value) = line.split_once(':')?;
        header
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// A server that answers the transport-level methods and refuses the
/// standalone stream, recording every request it saw.
///
/// The refusal is deliberate: these tests exercise the POST paths, and a 405
/// is the specified quiet fallback.
fn spawn_session_server() -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                write_response(&mut stream, "405 Method Not Allowed", "application/json", "");
            } else if request.contains("\"method\":\"initialize\"") {
                let body =
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-alpha\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"remote_read"}]}}"#,
                );
            } else if request.contains("\"method\":\"resources/list\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":3,"result":{"resources":[]}}"#,
                );
            } else {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":4,"result":{}}"#,
                );
            }
        }
    });

    (address, requests)
}

fn spawn_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            if is_get(&request) {
                write_response(&mut stream, "405 Method Not Allowed", "application/json", "");
                continue;
            }
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test"),
                "unexpected request without the configured header: {request}"
            );

            if request.contains("\"method\":\"initialize\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
                );
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    concat!(
                        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"remote_read\"}]}}\n\n"
                    ),
                );
            } else if request.contains("\"method\":\"resources/list\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":3,"result":{"resources":[]}}"#,
                );
            } else {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":4,"result":{}}"#,
                );
            }
        }
    });
    address
}

#[tokio::test]
async fn http_transport_supports_json_and_sse_responses() {
    let address = spawn_server();
    let mut channel = JsonRpcChannel::new_http(
        &format!("http://{address}/mcp"),
        HashMap::from([("authorization".into(), "Bearer test".into())]),
    )
    .unwrap();

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    let tools = channel.list_tools().await.unwrap();
    assert_eq!(tools["tools"][0]["name"], "remote_read");

    channel.terminate().await.unwrap();
    assert!(!channel.is_running());
}

/// A server that asks the client a question in the middle of an SSE response
/// and does not finish that response until the answer arrives.
fn spawn_asking_server() -> (SocketAddr, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut answer = String::new();
        let mut open_stream: Option<TcpStream> = None;

        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);

            if is_get(&request) {
                // The per-response path is what this server exercises; the
                // standalone stream is refused quietly.
                write_response(&mut stream, "405 Method Not Allowed", "application/json", "");
                continue;
            }

            if request.contains("\"method\":\"initialize\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
                );
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                // The server asks before answering this request.
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                stream
                    .write_all(
                        b"data: {\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"elicitation/create\",\"params\":{\"message\":\"pick one\"}}\n\n",
                    )
                    .unwrap();
                stream.flush().unwrap();
                open_stream = Some(stream);
            } else if request.contains("\"id\":99") {
                // The answer has to arrive on its own connection while the
                // request's stream is still open.
                answer = request;
                write_response(&mut stream, "202 Accepted", "application/json", "");
                if let Some(ref mut open) = open_stream {
                    // Only now can the original request complete.
                    open.write_all(
                        b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"remote_read\"}]}}\n\n",
                    )
                    .unwrap();
                    open.flush().unwrap();
                }
                break;
            }
        }

        answer
    });
    (address, handle)
}

#[derive(Debug)]
struct AcceptingHandler;

#[async_trait::async_trait]
impl InboundRequestHandler for AcceptingHandler {
    async fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, JsonRpcError> {
        assert_eq!(method, "elicitation/create");
        Ok(serde_json::json!({"action": "accept", "content": {"answer": 42}}))
    }
}

/// A handler that waits, then declines — the answer still has to be delivered
/// on a new POST while the SSE stream is held open.
#[derive(Debug)]
struct SlowDecliningHandler;

#[async_trait::async_trait]
impl InboundRequestHandler for SlowDecliningHandler {
    async fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, JsonRpcError> {
        assert_eq!(method, "elicitation/create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(serde_json::json!({"action": "decline"}))
    }
}

#[tokio::test]
async fn a_server_request_in_an_sse_stream_is_answered_while_it_is_open() {
    let (address, server) = spawn_asking_server();
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_inbound_handler(Arc::new(AcceptingHandler));

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    let tools = channel.list_tools().await.unwrap();
    assert_eq!(tools["tools"][0]["name"], "remote_read");

    let answer = server.join().unwrap();
    assert!(answer.contains("\"id\":99"), "wrong answer: {answer}");
    assert!(
        answer.contains("\"result\""),
        "the handler's answer was not delivered: {answer}"
    );
    assert!(
        answer.contains("\"answer\":42"),
        "the handler's result was not in the answer: {answer}"
    );

    channel.terminate().await.unwrap();
}

#[tokio::test]
async fn an_sse_server_request_awaits_a_slow_decline() {
    let (address, server) = spawn_asking_server();
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_inbound_handler(Arc::new(SlowDecliningHandler));

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    let tools = channel.list_tools().await.unwrap();
    assert!(tools.get("tools").is_some(), "{tools}");

    let answer = server.join().unwrap();
    assert!(answer.contains("\"id\":99"), "wrong answer: {answer}");
    assert!(
        answer.contains("\"action\":\"decline\""),
        "the handler's decline was not delivered: {answer}"
    );

    channel.terminate().await.unwrap();
}

#[tokio::test]
async fn a_server_request_in_an_sse_stream_with_no_handler_is_refused() {
    let (address, server) = spawn_asking_server();
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    let tools = channel.list_tools().await.unwrap();
    assert!(tools.get("tools").is_some(), "{tools}");

    let answer = server.join().unwrap();
    assert!(answer.contains("\"id\":99"), "wrong answer: {answer}");
    assert!(
        answer.contains("-32601"),
        "a request nothing can answer should be refused: {answer}"
    );

    channel.terminate().await.unwrap();
}

/// The authorization header of one recorded request, if it had one.
fn authorization(request: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    })
}

/// A server that answers `count` JSON-RPC requests, recording each one.
fn spawn_rpc_server(count: usize) -> (SocketAddr, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            requests.push(request);
            let body = format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, index + 1);
            write_response(&mut stream, "200 OK", "application/json", &body);
        }
        requests
    });
    (address, handle)
}

/// A one-shot token endpoint, recording the request it was sent.
fn spawn_token_endpoint(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        write_response(&mut stream, "200 OK", "application/json", body);
        request
    });
    (format!("http://{address}/oauth/token"), handle)
}

/// A token that expires between two requests must be refreshed for the second
/// one. This is the difference between "authenticated at startup" and
/// "authenticated for the life of the session".
#[tokio::test]
async fn an_expiring_token_is_refreshed_before_the_second_request() {
    let (mcp_address, mcp_server) = spawn_rpc_server(2);
    let (token_endpoint, token_server) = spawn_token_endpoint(
        r#"{"access_token":"new_access","refresh_token":"rotated","expires_in":3600}"#,
    );
    let server_url = format!("http://{mcp_address}/mcp");

    let dir = tempfile::tempdir().unwrap();
    let mut oauth = OAuthManager::with_path(dir.path().join("mcp_tokens.json"));
    let mut state = McpOAuthState::new(&server_url, "client1");
    state.access_token = Some("old_access".into());
    state.refresh_token = Some("refresh_me".into());
    state.expires_at = Some(chrono::Utc::now().timestamp() + 3600);
    state.token_endpoint = Some(token_endpoint);
    oauth.register(state);
    let oauth = Arc::new(tokio::sync::Mutex::new(oauth));

    let mut channel = JsonRpcChannel::new_http(&server_url, HashMap::new()).unwrap();
    channel.set_token_provider(Arc::new(OAuthTokenProvider::new(
        Arc::clone(&oauth),
        server_url.clone(),
        reqwest::Client::new(),
    )));
    channel.spawn().await.unwrap();

    channel.send_request("tools/list", None).await.unwrap();

    // Move the token inside the 5-minute window. The next request has to
    // notice and refresh before sending.
    {
        let mut oauth = oauth.lock().await;
        oauth.get_mut(&server_url).expect("state").expires_at =
            Some(chrono::Utc::now().timestamp() + 60);
    }
    channel.send_request("tools/list", None).await.unwrap();

    let requests = mcp_server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        authorization(&requests[0]).as_deref(),
        Some("Bearer old_access")
    );
    assert_eq!(
        authorization(&requests[1]).as_deref(),
        Some("Bearer new_access"),
        "the second request did not use the refreshed token: {}",
        requests[1]
    );

    let token_request = token_server.join().unwrap();
    assert!(
        token_request.contains("grant_type=refresh_token"),
        "{token_request}"
    );
    assert!(
        token_request.contains("refresh_token=refresh_me"),
        "{token_request}"
    );

    channel.terminate().await.unwrap();
}

/// Poll `condition` until it holds or five seconds pass.
async fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition did not hold within five seconds"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The initialize response's `Mcp-Session-Id` must be carried by every later
/// POST, including the `initialized` notification and the standalone GET.
#[tokio::test]
async fn the_session_id_is_sent_on_every_later_request() {
    let (address, requests) = spawn_session_server();
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    assert_eq!(channel.session_id().as_deref(), Some("session-alpha"));

    channel.list_tools().await.unwrap();

    // Wait for the standalone stream's GET, which also carries the session id.
    let recorded = Arc::clone(&requests);
    wait_until(|| {
        recorded
            .lock()
            .unwrap()
            .iter()
            .any(|request| is_get(request))
    })
    .await;

    let recorded = requests.lock().unwrap();
    let initialize = recorded
        .iter()
        .find(|request| request.contains("\"method\":\"initialize\""))
        .expect("initialize was sent");
    assert_eq!(
        header_value(initialize, "mcp-session-id"),
        None,
        "the initialize request cannot carry a session it does not know yet: {initialize}"
    );

    for method in ["initialized", "tools/list"] {
        let request = recorded
            .iter()
            .find(|request| request.contains(&format!("\"method\":\"{method}\"")))
            .unwrap_or_else(|| panic!("{method} was not sent"));
        assert_eq!(
            header_value(request, "mcp-session-id").as_deref(),
            Some("session-alpha"),
            "{method} did not carry the session id: {request}"
        );
    }

    let get = recorded.iter().find(|request| is_get(request)).expect("GET");
    assert_eq!(
        header_value(get, "mcp-session-id").as_deref(),
        Some("session-alpha"),
        "the GET stream did not carry the session id: {get}"
    );
    assert_eq!(
        header_value(get, "accept").as_deref(),
        Some("text/event-stream"),
        "the GET stream did not ask for SSE: {get}"
    );

    channel.terminate().await.unwrap();
}

/// A 405 to the standalone GET is the specified quiet fallback: the stream
/// stops and the server remains usable.
#[tokio::test]
async fn a_get_405_stops_the_stream_without_failing_the_server() {
    let (address, requests) = spawn_session_server();
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();

    let recorded = Arc::clone(&requests);
    wait_until(|| {
        recorded
            .lock()
            .unwrap()
            .iter()
            .any(|request| is_get(request))
    })
    .await;

    // The client observes the 405 and stops the task.
    wait_until(|| !channel.event_stream_active()).await;

    let tools = channel.list_tools().await.unwrap();
    assert_eq!(tools["tools"][0]["name"], "remote_read");

    channel.terminate().await.unwrap();
}

/// A request that meets an expired session (404/405) drops the id and
/// re-initializes once before retrying.
#[tokio::test]
async fn an_expired_session_is_cleared_and_reinitialized_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        let mut initializes = 0_usize;
        let mut tools_attempts = 0_usize;

        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                write_response(&mut stream, "405 Method Not Allowed", "application/json", "");
            } else if request.contains("\"method\":\"initialize\"") {
                initializes += 1;
                let session = if initializes == 1 {
                    "session-1"
                } else {
                    "session-2"
                };
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2024-11-05"}}}}"#,
                    if request.contains("\"id\":0") { 0 } else { 1 }
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: {session}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                tools_attempts += 1;
                if tools_attempts == 1 {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        "application/json",
                        r#"{"error":"unknown session"}"#,
                    );
                } else {
                    write_response(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"remote_read"}]}}"#,
                    );
                }
            } else {
                write_response(&mut stream, "200 OK", "application/json", "{}");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    assert_eq!(channel.session_id().as_deref(), Some("session-1"));

    let tools = channel.list_tools().await.unwrap();
    assert_eq!(tools["tools"][0]["name"], "remote_read");
    assert_eq!(
        channel.session_id().as_deref(),
        Some("session-2"),
        "the retry did not use the re-initialized session"
    );

    let recorded = requests.lock().unwrap();
    let tools_requests: Vec<&String> = recorded
        .iter()
        .filter(|request| request.contains("\"method\":\"tools/list\""))
        .collect();
    assert_eq!(tools_requests.len(), 2, "the request must be retried once");
    assert_eq!(
        header_value(tools_requests[0], "mcp-session-id").as_deref(),
        Some("session-1")
    );
    assert_eq!(
        header_value(tools_requests[1], "mcp-session-id").as_deref(),
        Some("session-2")
    );

    channel.terminate().await.unwrap();
}

/// A request pushed on the standalone GET stream is answered over POST with
/// the session id, while the stream is still open.
#[tokio::test]
async fn a_question_on_the_get_stream_is_answered_over_post() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        // Keep GET streams alive while answers arrive on other connections.
        let mut open_streams: Vec<TcpStream> = Vec::new();

        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                // Hold this connection open and push a question on it.
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\nid: evt-1\ndata: {\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"elicitation/create\",\"params\":{\"message\":\"pick a number\",\"requestedSchema\":{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"integer\"}},\"required\":[\"answer\"]}}}\n\n",
                    )
                    .unwrap();
                stream.flush().unwrap();
                open_streams.push(stream);
            } else if request.contains("\"method\":\"initialize\"") {
                let body =
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"id\":77") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else {
                write_response(&mut stream, "200 OK", "application/json", "{}");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_inbound_handler(Arc::new(AcceptingHandler));
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();

    let recorded = Arc::clone(&requests);
    wait_until(|| {
        recorded
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains("\"id\":77"))
    })
    .await;

    let recorded = requests.lock().unwrap();
    let answer = recorded
        .iter()
        .find(|request| request.contains("\"id\":77"))
        .expect("the answer was posted");
    assert!(
        answer.contains("\"answer\":42"),
        "the handler's content was not delivered: {answer}"
    );
    assert_eq!(
        header_value(answer, "mcp-session-id").as_deref(),
        Some("session-stream"),
        "the answer did not carry the session id: {answer}"
    );

    let get = recorded.iter().find(|request| is_get(request)).expect("GET");
    assert_eq!(
        header_value(get, "mcp-session-id").as_deref(),
        Some("session-stream")
    );
    assert_eq!(
        header_value(get, "accept").as_deref(),
        Some("text/event-stream")
    );

    drop(recorded);
    channel.terminate().await.unwrap();
}

#[tokio::test]
async fn remote_server_config_drives_discovery_and_qualified_names() {
    let address = spawn_server();
    let mut config = McpConfig::new();
    config.add_server(
        "remote",
        McpServerConfig {
            transport: McpTransport::Http,
            url: Some(format!("http://{address}/mcp")),
            headers: HashMap::from([("authorization".into(), "Bearer test".into())]),
            ..Default::default()
        },
    );
    let manager = McpManager::new();
    manager.load_config(&config);

    manager.start_all().await;
    assert!(manager.is_running("remote"));
    assert!(
        manager
            .list_tools()
            .contains(&"mcp__remote__remote_read".to_string())
    );
    manager.stop_all().await;
}
