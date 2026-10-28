//! End-to-end checks for remote MCP HTTP/SSE transport.

use coderex_mcp::channel::{
    InboundRequestHandler, JsonRpcChannel, JsonRpcError, ServerNotificationHandler,
};
use coderex_mcp::config::{McpConfig, McpServerConfig, McpTransport};
use coderex_mcp::manager::McpManager;
use coderex_mcp::oauth::{McpOAuthState, OAuthManager, OAuthTokenProvider, ReauthHandler};
use coderex_mcp::server::{McpServer, ServerStatus};
use coderex_mcp::{EventStreamRetryPolicy, EventStreamState, TokenProvider};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "application/json",
                    "",
                );
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
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
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "application/json",
                    "",
                );
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
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "application/json",
                    "",
                );
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
        &oauth,
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

/// An expired token with no refresh token to replace it must hand off to the
/// installed re-authorization seam exactly once, and use the token it stored,
/// instead of failing until the person runs `McpAuth` by hand.
#[tokio::test]
async fn an_expired_token_without_a_refresh_token_reauthorizes_once() {
    let (mcp_address, mcp_server) = spawn_rpc_server(1);
    let server_url = format!("http://{mcp_address}/mcp");

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("mcp_tokens.json");
    let calls = Arc::new(AtomicUsize::new(0));

    let mut oauth = OAuthManager::with_path(store.clone());
    let mut state = McpOAuthState::new(&server_url, "client1");
    state.access_token = Some("stale_access".into());
    // Expired, and deliberately no refresh token.
    state.expires_at = Some(chrono::Utc::now().timestamp() - 10);
    oauth.register(state);
    oauth.set_reauth_handler(Arc::new(FileReauth {
        path: store.clone(),
        token: "reauth_access",
        calls: Arc::clone(&calls),
    }));
    let oauth = Arc::new(tokio::sync::Mutex::new(oauth));

    let mut channel = JsonRpcChannel::new_http(&server_url, HashMap::new()).unwrap();
    channel.set_token_provider(Arc::new(OAuthTokenProvider::new(
        &oauth,
        server_url.clone(),
        reqwest::Client::new(),
    )));
    channel.spawn().await.unwrap();
    channel.send_request("tools/list", None).await.unwrap();

    let requests = mcp_server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        authorization(&requests[0]).as_deref(),
        Some("Bearer reauth_access"),
        "the request did not carry the re-authorized token: {}",
        requests[0]
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the re-authorization handler must run exactly once"
    );

    channel.terminate().await.unwrap();
}

/// A bearer challenge response, with the `WWW-Authenticate` header the
/// transport inspects before deciding whether recovery is allowed.
fn write_unauthorized(stream: &mut TcpStream, challenge: &str) {
    let body = r#"{"error":"unauthorized"}"#;
    let response = format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nWWW-Authenticate: {challenge}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

/// Answer `responses` POSTs: the first `unauthorized` get a 401 carrying
/// `challenge`, the rest a successful JSON-RPC response.
fn spawn_challenge_server(
    challenge: &'static str,
    responses: usize,
    unauthorized: usize,
) -> (SocketAddr, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..responses {
            let (mut stream, _) = listener.accept().unwrap();
            requests.push(read_request(&mut stream));
            if index < unauthorized {
                write_unauthorized(&mut stream, challenge);
            } else {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                );
            }
        }
        requests
    });
    (address, handle)
}

/// A provider that stands in for the OAuth manager, counting the times the
/// transport asks it to recover from a 401.
#[derive(Debug)]
struct CountingProvider {
    token: std::sync::Mutex<Option<String>>,
    replacement: &'static str,
    recoveries: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl TokenProvider for CountingProvider {
    async fn access_token(&self) -> Option<String> {
        self.token.lock().unwrap().clone()
    }

    async fn handle_unauthorized(
        &self,
        _www_authenticate: Option<&str>,
        _rejected_token: Option<&str>,
    ) -> Result<Option<String>, String> {
        self.recoveries.fetch_add(1, Ordering::SeqCst);
        *self.token.lock().unwrap() = Some(self.replacement.to_string());
        Ok(Some(self.replacement.to_string()))
    }
}

fn counting_provider(recoveries: &Arc<AtomicUsize>) -> Arc<CountingProvider> {
    Arc::new(CountingProvider {
        token: std::sync::Mutex::new(Some("stale_access".into())),
        replacement: "fresh_access",
        recoveries: Arc::clone(recoveries),
    })
}

/// A re-authorization seam that persists a token the way the real `McpAuth`
/// flow does: by writing the shared token store the manager reads back.
#[derive(Debug)]
struct FileReauth {
    path: std::path::PathBuf,
    token: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ReauthHandler for FileReauth {
    async fn reauthorize(&self, server_url: &str) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let document = serde_json::json!({
            "states": [{
                "server_url": server_url,
                "client_id": "client1",
                "access_token": self.token,
                "expires_at": chrono::Utc::now().timestamp() + 3600,
            }],
        });
        std::fs::write(&self.path, document.to_string()).map_err(|error| error.to_string())
    }
}

/// A 401 whose bearer challenge says `invalid_token` is recoverable: the
/// provider is asked once, and the retried request carries the new token.
#[tokio::test]
async fn an_invalid_token_401_refreshes_once_and_retries_with_the_new_token() {
    let (address, server) = spawn_challenge_server(
        r#"Bearer error="invalid_token", error_description="expired""#,
        2,
        1,
    );
    let server_url = format!("http://{address}/mcp");
    let recoveries = Arc::new(AtomicUsize::new(0));

    let mut channel = JsonRpcChannel::new_http(&server_url, HashMap::new()).unwrap();
    channel.set_token_provider(counting_provider(&recoveries));
    channel.spawn().await.unwrap();

    channel.send_request("tools/list", None).await.unwrap();

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "the request was not retried exactly once"
    );
    assert_eq!(
        authorization(&requests[0]).as_deref(),
        Some("Bearer stale_access")
    );
    assert_eq!(
        authorization(&requests[1]).as_deref(),
        Some("Bearer fresh_access"),
        "the retry did not carry the refreshed token: {}",
        requests[1]
    );
    assert_eq!(
        recoveries.load(Ordering::SeqCst),
        1,
        "the refresh token was spent more than once"
    );

    channel.terminate().await.unwrap();
}

/// A server that issued no refresh token still recovers when the CLI installed
/// a re-authorization handler: the handler runs once, stores a token, and the
/// request is retried with that token. The three requests are the original
/// POST, the resource-metadata GET from the 401 policy, and the retry.
#[tokio::test]
async fn an_invalid_token_401_without_a_refresh_token_reauthorizes_and_retries() {
    let (address, server) = spawn_challenge_server(r#"Bearer error="invalid_token""#, 3, 1);
    let server_url = format!("http://{address}/mcp");
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("mcp_tokens.json");

    let mut oauth = OAuthManager::with_path(&token_path);
    let mut state = McpOAuthState::new(&server_url, "client1");
    state.access_token = Some("stale_access".into());
    state.expires_at = Some(chrono::Utc::now().timestamp() + 3600);
    oauth.register(state);
    let calls = Arc::new(AtomicUsize::new(0));
    oauth.set_reauth_handler(Arc::new(FileReauth {
        path: token_path,
        token: "reauth_access",
        calls: Arc::clone(&calls),
    }));
    let oauth = Arc::new(tokio::sync::Mutex::new(oauth));

    let mut channel = JsonRpcChannel::new_http(&server_url, HashMap::new()).unwrap();
    channel.set_token_provider(Arc::new(OAuthTokenProvider::new(
        &oauth,
        server_url.clone(),
        reqwest::Client::new(),
    )));
    channel.spawn().await.unwrap();

    channel.send_request("tools/list", None).await.unwrap();

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3, "expected POST, metadata GET, retry POST");
    assert_eq!(
        authorization(&requests[0]).as_deref(),
        Some("Bearer stale_access")
    );
    assert!(
        is_get(&requests[1]),
        "the 401 policy did not re-read metadata"
    );
    assert_eq!(
        authorization(&requests[2]).as_deref(),
        Some("Bearer reauth_access"),
        "the retry did not carry the re-authorized token: {}",
        requests[2]
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the re-authorization handler was called more than once"
    );

    channel.terminate().await.unwrap();
}

/// A 401 that is not `invalid_token` (here `insufficient_scope`) is an
/// ordinary failure: recovery is not attempted and no refresh token is spent.
#[tokio::test]
async fn a_non_invalid_token_401_does_not_spend_the_refresh_token() {
    let (address, server) = spawn_challenge_server(
        r#"Bearer error="insufficient_scope", scope="files:read""#,
        1,
        1,
    );
    let server_url = format!("http://{address}/mcp");
    let recoveries = Arc::new(AtomicUsize::new(0));

    let mut channel = JsonRpcChannel::new_http(&server_url, HashMap::new()).unwrap();
    channel.set_token_provider(counting_provider(&recoveries));
    channel.spawn().await.unwrap();

    let error = channel
        .send_request("tools/list", None)
        .await
        .expect_err("a non-recoverable 401 must surface");
    assert!(error.contains("401"), "{error}");
    assert_eq!(
        recoveries.load(Ordering::SeqCst),
        0,
        "a non-invalid_token 401 must not refresh"
    );
    assert_eq!(server.join().unwrap().len(), 1);

    channel.terminate().await.unwrap();
}

/// If the retried request is also rejected, the transport reports the second
/// 401 instead of retrying forever.
#[tokio::test]
async fn a_second_401_after_the_retry_is_an_error_not_a_loop() {
    let (address, server) = spawn_challenge_server(r#"Bearer error="invalid_token""#, 2, 2);
    let server_url = format!("http://{address}/mcp");
    let recoveries = Arc::new(AtomicUsize::new(0));

    let mut channel = JsonRpcChannel::new_http(&server_url, HashMap::new()).unwrap();
    channel.set_token_provider(counting_provider(&recoveries));
    channel.spawn().await.unwrap();

    let error = channel
        .send_request("tools/list", None)
        .await
        .expect_err("a second 401 must be an error");
    assert!(
        error.contains("after refreshing the access token"),
        "unexpected error: {error}"
    );

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2, "the transport looped past one retry");
    assert_eq!(
        authorization(&requests[0]).as_deref(),
        Some("Bearer stale_access")
    );
    assert_eq!(
        authorization(&requests[1]).as_deref(),
        Some("Bearer fresh_access")
    );
    assert_eq!(recoveries.load(Ordering::SeqCst), 1);

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
        assert_eq!(
            header_value(request, "mcp-protocol-version").as_deref(),
            Some("2024-11-05"),
            "{method} did not carry the negotiated protocol version: {request}"
        );
    }

    let get = recorded
        .iter()
        .find(|request| is_get(request))
        .expect("GET");
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
    assert_eq!(
        header_value(get, "mcp-protocol-version").as_deref(),
        Some("2024-11-05"),
        "the GET stream did not carry the negotiated protocol version: {get}"
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

    // The client observes the 405 and stops the task permanently.
    wait_until(|| !channel.event_stream_active()).await;
    assert_eq!(
        channel.event_stream_state(),
        Some(EventStreamState::Unsupported),
        "405 must be a quiet, permanent stop"
    );

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
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "application/json",
                    "",
                );
            } else if request.contains("\"method\":\"initialize\"") {
                initializes += 1;
                let session = if initializes == 1 {
                    "session-1"
                } else {
                    "session-2"
                };
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2024-11-05"}}}}"#,
                    i32::from(!request.contains("\"id\":0"))
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
        // The vector is written and never read: dropping it closes them.
        #[allow(clippy::collection_is_never_read)]
        // Held open on purpose; see the note on the first server below.
        #[allow(clippy::collection_is_never_read)]
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
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"")
                || request.contains("\"id\":77")
            {
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

    let get = recorded
        .iter()
        .find(|request| is_get(request))
        .expect("GET");
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

/// Records how many server requests it was asked to answer.
#[derive(Debug, Default)]
struct CountingHandler {
    answers: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl InboundRequestHandler for CountingHandler {
    async fn handle(
        &self,
        _method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, JsonRpcError> {
        self.answers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(serde_json::json!({"action": "accept", "content": {}}))
    }
}

/// The same server request id delivered once on the POST response stream and
/// once on the standalone GET stream must be answered exactly once.
#[tokio::test]
async fn a_request_delivered_on_both_streams_is_answered_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        // Held open on purpose; see the note on the first server below.
        #[allow(clippy::collection_is_never_read)]
        let mut open_streams: Vec<TcpStream> = Vec::new();
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\nid: evt-dup\ndata: {\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"elicitation/create\",\"params\":{\"message\":\"once\"}}\n\n",
                    )
                    .unwrap();
                stream.flush().unwrap();
                open_streams.push(stream);
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-dup\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                // The duplicate arrives on the response stream too.
                write_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    concat!(
                        "data: {\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"elicitation/create\",\"params\":{\"message\":\"once\"}}\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n"
                    ),
                );
            } else if request.contains("\"id\":77") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else {
                write_response(&mut stream, "200 OK", "application/json", "{}");
            }
        }
    });

    let handler = Arc::new(CountingHandler::default());
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_inbound_handler(handler.clone());
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    channel.list_tools().await.unwrap();

    let recorded_ref = Arc::clone(&requests);
    wait_until(|| {
        recorded_ref
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains("\"id\":77"))
    })
    .await;

    // Give the second copy time to arrive and be (wrongly) answered.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        handler.answers.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the handler must run once for a duplicated request id"
    );
    let recorded = requests.lock().unwrap();
    let answers: Vec<&String> = recorded
        .iter()
        .filter(|request| {
            !is_get(request)
                && !request.contains("\"method\":\"tools/list\"")
                && request.contains("\"id\":77")
        })
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "the request must be answered exactly once: {answers:?}"
    );

    drop(recorded);
    channel.terminate().await.unwrap();
}

/// Counts the server notifications it was handed.
#[derive(Debug, Default)]
struct CountingNotifications {
    seen: AtomicUsize,
}

#[async_trait::async_trait]
impl ServerNotificationHandler for CountingNotifications {
    async fn handle_notification(&self, _method: &str, _params: Option<&serde_json::Value>) {
        self.seen.fetch_add(1, Ordering::SeqCst);
    }
}

/// The same notification event id delivered once on the standalone GET stream
/// and once on a POST response stream must be dispatched exactly once. A
/// notification has no id to dedup on, so the event id is the key.
#[tokio::test]
async fn a_notification_delivered_on_both_streams_is_dispatched_once() {
    const EVENT: &str = concat!(
        "id: evt-note\n",
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"once\"}}\n\n"
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        // Held open on purpose; see the note on the first server below.
        #[allow(clippy::collection_is_never_read)]
        let mut open_streams: Vec<TcpStream> = Vec::new();
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n{EVENT}"
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                stream.flush().unwrap();
                open_streams.push(stream);
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-note\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                // The duplicate rides the response stream, with the same id.
                write_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    &format!(
                        "{EVENT}data: {{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":[]}}}}\n\n"
                    ),
                );
            } else {
                write_response(&mut stream, "200 OK", "application/json", "{}");
            }
        }
    });

    let notifications = Arc::new(CountingNotifications::default());
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_notification_handler(notifications.clone());
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    channel.list_tools().await.unwrap();

    // Give both copies time to arrive and be (wrongly) dispatched.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        notifications.seen.load(Ordering::SeqCst),
        1,
        "a notification with a repeated event id must be dispatched once"
    );

    channel.terminate().await.unwrap();
}

/// A server request replayed after the event stream reconnects must not be
/// asked a second time: the resume point advances past the events that were
/// actually processed, and the bounded dedup catches the replay itself.
#[tokio::test]
async fn a_replayed_server_request_after_reconnect_is_not_asked_again() {
    const EVENT: &str = concat!(
        "id: evt-req\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"elicitation/create\",\"params\":{\"message\":\"ask once\"}}\n\n"
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gets = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&gets);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        // Held open on purpose: dropping a stream closes it.
        #[allow(clippy::collection_is_never_read)]
        let mut held: Vec<TcpStream> = Vec::new();
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                let attempt = counted.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // Deliver the request, then end the body so the client
                    // reconnects and resumes.
                    write_response(&mut stream, "200 OK", "text/event-stream", EVENT);
                } else {
                    // The reconnect replays the same event id.
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n{EVENT}"
                            )
                            .as_bytes(),
                        )
                        .unwrap();
                    stream.flush().unwrap();
                    held.push(stream);
                }
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-replay\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            }
        }
    });

    let handler = Arc::new(CountingHandler::default());
    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_inbound_handler(handler.clone());
    channel.set_event_stream_retry_policy(EventStreamRetryPolicy {
        max_attempts: 5,
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(20),
        max_duration: std::time::Duration::from_secs(10),
    });
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();

    wait_until(|| gets.load(Ordering::SeqCst) >= 2).await;
    // Let the replayed event arrive and be (wrongly) answered.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(
        handler.answers.load(Ordering::SeqCst),
        1,
        "the replayed server request must be asked once"
    );

    let recorded = requests.lock().unwrap();
    let gets_recorded: Vec<&String> = recorded.iter().filter(|request| is_get(request)).collect();
    assert_eq!(
        header_value(gets_recorded[1], "last-event-id").as_deref(),
        Some("evt-req"),
        "the reconnect must resume from the event that was processed: {}",
        gets_recorded[1]
    );
    let answers: Vec<&String> = recorded
        .iter()
        .filter(|request| !is_get(request) && request.contains("\"id\":77"))
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "the replayed request must be answered exactly once: {answers:?}"
    );

    drop(recorded);
    channel.terminate().await.unwrap();
}

/// A stream that keeps failing exhausts its retry budget and surfaces
/// `Degraded` instead of retrying forever.
#[tokio::test]
async fn a_stream_that_keeps_failing_degrades_after_the_retry_cap() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&gets);

    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            if is_get(&request) {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A complete, empty event stream: the client sees EOF and
                // reconnects until its budget runs out.
                write_response(&mut stream, "200 OK", "text/event-stream", "");
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-retry\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_event_stream_retry_policy(EventStreamRetryPolicy {
        max_attempts: 3,
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(20),
        max_duration: std::time::Duration::from_secs(10),
    });
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();

    wait_until(|| channel.event_stream_state() == Some(EventStreamState::Degraded)).await;
    assert!(
        gets.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "the stream should have made its bounded attempts"
    );

    channel.terminate().await.unwrap();
}

/// Reaching the retry cap is not permanent: the stream enters `Degraded`,
/// waits one probe interval, and makes another GET. A successful probe puts
/// the stream back into `Active` (the "running" state).
#[tokio::test]
async fn a_degraded_stream_probes_and_recovers_to_active() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gets = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&gets);

    std::thread::spawn(move || {
        // Hold recovered streams open, or the client would see EOF and
        // treat the probe as another failure.
        // Held open on purpose: dropping a stream closes it.
        #[allow(clippy::collection_is_never_read)]
        let mut held: Vec<TcpStream> = Vec::new();
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            if is_get(&request) {
                let attempt = counted.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt <= 2 {
                    // A complete, empty event stream: the client sees EOF and
                    // spends its retry budget.
                    write_response(&mut stream, "200 OK", "text/event-stream", "");
                } else {
                    // The probe's GET connects and stays open.
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
                        )
                        .unwrap();
                    stream.flush().unwrap();
                    held.push(stream);
                }
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-probe\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_event_stream_retry_policy(EventStreamRetryPolicy {
        max_attempts: 2,
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(20),
        max_duration: std::time::Duration::from_secs(10),
    });
    // Short enough to keep the test fast, long enough for the 10 ms poll in
    // `wait_until` to observe the `Degraded` state.
    channel.set_event_stream_probe_interval(Some(std::time::Duration::from_millis(100)));
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();

    wait_until(|| channel.event_stream_state() == Some(EventStreamState::Degraded)).await;
    let attempts_before_probe = gets.load(Ordering::SeqCst);
    assert!(
        attempts_before_probe >= 2,
        "the stream should have exhausted its retry budget first"
    );

    wait_until(|| channel.event_stream_state() == Some(EventStreamState::Active)).await;
    assert!(
        gets.load(Ordering::SeqCst) > attempts_before_probe,
        "the probe did not make another GET"
    );

    channel.terminate().await.unwrap();
}

/// A 404 to the standalone GET means the session expired: the client clears
/// it, re-initializes once, and reconnects under the new session.
#[tokio::test]
async fn a_get_404_reinitializes_once_and_resumes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        // Held open on purpose; see the note on the first server below.
        #[allow(clippy::collection_is_never_read)]
        let mut open_streams: Vec<TcpStream> = Vec::new();
        let mut initializes = 0_usize;
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                let attempt = initializes;
                if attempt == 1 {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        "application/json",
                        r#"{"error":"unknown session"}"#,
                    );
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
                        )
                        .unwrap();
                    stream.flush().unwrap();
                    open_streams.push(stream);
                }
            } else if request.contains("\"method\":\"initialize\"") {
                initializes += 1;
                let session = format!("session-{initializes}");
                let id = i32::from(!request.contains("\"id\":0"));
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2024-11-05"}}}}"#
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: {session}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_event_stream_retry_policy(EventStreamRetryPolicy {
        max_attempts: 3,
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(20),
        max_duration: std::time::Duration::from_secs(10),
    });
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    assert_eq!(channel.session_id().as_deref(), Some("session-1"));

    wait_until(|| channel.event_stream_state() == Some(EventStreamState::Active)).await;
    assert_eq!(
        channel.session_id().as_deref(),
        Some("session-2"),
        "the GET stream should have re-initialized the session"
    );

    let recorded = requests.lock().unwrap();
    let initializes = recorded
        .iter()
        .filter(|request| request.contains("\"method\":\"initialize\""))
        .count();
    assert_eq!(
        initializes, 2,
        "the session must be re-initialized exactly once"
    );
    let gets: Vec<&String> = recorded.iter().filter(|request| is_get(request)).collect();
    assert_eq!(
        header_value(gets.last().unwrap(), "mcp-session-id").as_deref(),
        Some("session-2"),
        "the reconnected stream did not use the new session"
    );

    drop(recorded);
    channel.terminate().await.unwrap();
}

/// A standalone GET stream that delivers an event and then ends must reconnect
/// with `Last-Event-ID` set to that event's id, so the server can resume after
/// the last message the client actually processed.
#[tokio::test]
async fn a_reconnecting_get_stream_sends_the_last_event_id() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let gets = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&gets);

    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Deliver one event carrying an id, then let the response
                    // body end so the client sees EOF and reconnects.
                    let body = concat!(
                        "id: evt-42\n",
                        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"hello\"}}\n\n"
                    );
                    write_response(&mut stream, "200 OK", "text/event-stream", body);
                } else {
                    // The reconnect has been observed; stop the retry loop.
                    write_response(
                        &mut stream,
                        "405 Method Not Allowed",
                        "application/json",
                        "",
                    );
                }
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-resume\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.set_event_stream_retry_policy(EventStreamRetryPolicy {
        max_attempts: 3,
        base_delay: std::time::Duration::from_millis(10),
        max_delay: std::time::Duration::from_millis(20),
        max_duration: std::time::Duration::from_secs(10),
    });
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();

    // The server counts a GET after recording it, so two GETs means the
    // reconnecting request is in `requests`.
    wait_until(|| gets.load(Ordering::SeqCst) >= 2).await;

    let recorded = requests.lock().unwrap();
    let gets: Vec<&String> = recorded.iter().filter(|request| is_get(request)).collect();
    assert!(
        gets.len() >= 2,
        "the client did not reconnect the GET stream: {recorded:?}"
    );
    assert_eq!(
        header_value(gets[0], "last-event-id"),
        None,
        "the first GET has no event to resume from: {}",
        gets[0]
    );
    assert_eq!(
        header_value(gets[1], "last-event-id").as_deref(),
        Some("evt-42"),
        "the reconnecting GET did not resume from the last event: {}",
        gets[1]
    );

    drop(recorded);
    channel.terminate().await.unwrap();
}

/// The JSON-RPC id of a recorded POST request.
fn jsonrpc_id(request: &str) -> u64 {
    request
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
        .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64))
        .unwrap_or(2)
}

/// Push `tools/list_changed` once on the open GET stream.
fn send_tools_changed(open_stream: &mut Option<TcpStream>, ready: &mut bool) {
    if !*ready {
        return;
    }
    if let Some(stream) = open_stream.as_mut() {
        let _ = stream.write_all(
            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
        );
        let _ = stream.flush();
        *ready = false;
    }
}

/// `notifications/tools/list_changed` must trigger a real re-discovery, not
/// just a log line.
#[tokio::test]
async fn tools_list_changed_triggers_rediscovery() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let list_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&list_calls);

    std::thread::spawn(move || {
        let mut open_stream: Option<TcpStream> = None;
        let mut first_list_seen = false;
        let mut stream_ready = false;

        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            if is_get(&request) {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
                    )
                    .unwrap();
                stream.flush().unwrap();
                open_stream = Some(stream);
                stream_ready = true;
                if first_list_seen {
                    send_tools_changed(&mut open_stream, &mut stream_ready);
                }
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-notify\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                let call = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let tools = if call == 0 {
                    r#"[{"name":"one"}]"#
                } else {
                    r#"[{"name":"one"},{"name":"two"}]"#
                };
                // Echo the actual request id: the re-discovery is a later
                // request than the initial discovery.
                let id = jsonrpc_id(&request);
                let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":{tools}}}}}"#);
                write_response(&mut stream, "200 OK", "application/json", &body);
                if !first_list_seen {
                    first_list_seen = true;
                    send_tools_changed(&mut open_stream, &mut stream_ready);
                }
            } else if request.contains("\"method\":\"resources/list\"") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":3,"result":{"resources":[]}}"#,
                );
            } else {
                write_response(&mut stream, "200 OK", "application/json", "{}");
            }
        }
    });

    let mut config = McpConfig::new();
    config.add_server(
        "remote",
        McpServerConfig {
            transport: McpTransport::Http,
            url: Some(format!("http://{address}/mcp")),
            ..Default::default()
        },
    );
    let manager = McpManager::new();
    manager.load_config(&config);
    manager.start_all().await;

    let tools = manager.list_tools();
    assert!(
        tools.contains(&"mcp__remote__one".to_string()),
        "initial discovery failed: {tools:?}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !manager
        .list_tools()
        .contains(&"mcp__remote__two".to_string())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "tools/list_changed did not trigger a re-discovery: {:?}",
            manager.list_tools()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        list_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the notification did not cause another tools/list"
    );

    manager.stop_all().await;
}

/// A classic MCP SSE server: a long-lived GET stream announces the POST
/// message endpoint with an `endpoint` event, and JSON-RPC responses are
/// written back on that same stream, correlated by id.
fn spawn_legacy_sse_server() -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let open_stream: Arc<std::sync::Mutex<Option<TcpStream>>> =
        Arc::new(std::sync::Mutex::new(None));

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let recorded = Arc::clone(&recorded);
            let open_stream = Arc::clone(&open_stream);
            std::thread::spawn(move || {
                let mut stream = stream;
                let request = read_request(&mut stream);
                recorded.lock().unwrap().push(request.clone());

                if is_get(&request) {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
                        )
                        .unwrap();
                    stream.flush().unwrap();
                    // Store the stream before announcing the endpoint, so a
                    // POST triggered by that event cannot find it missing.
                    *open_stream.lock().unwrap() = Some(stream);
                    let mut guard = open_stream.lock().unwrap();
                    if let Some(sse) = guard.as_mut() {
                        sse.write_all(b"event: endpoint\ndata: /messages?sessionId=legacy-1\n\n")
                            .unwrap();
                        sse.flush().unwrap();
                    }
                    return;
                }

                let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                let message: serde_json::Value =
                    serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
                let method = message
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let id = message.get("id").and_then(serde_json::Value::as_u64);

                // The initialize response sets a session header; every later
                // POST must echo it along with the negotiated protocol version.
                if method == "initialize" {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: legacy-session\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                }
                stream.flush().unwrap();

                let event = match (method, id) {
                    ("initialize", Some(id)) => Some(format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2024-11-05\"}}}}\n\n"
                    )),
                    ("tools/list", Some(id)) => Some(format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"legacy_read\"}}]}}}}\n\n"
                    )),
                    ("resources/list", Some(id)) => Some(format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"resources\":[]}}}}\n\n"
                    )),
                    _ => None,
                };
                if let Some(event) = event
                    && let Some(sse) = open_stream.lock().unwrap().as_mut()
                {
                    sse.write_all(event.as_bytes()).unwrap();
                    sse.flush().unwrap();
                }
            });
        }
    });

    (address, requests)
}

/// `type: "sse"` must use classic HTTP+SSE: GET an event stream, learn the
/// separate POST endpoint from its `endpoint` event, and correlate responses
/// by id — not POST to the stream URL as streamable HTTP would.
#[tokio::test]
async fn legacy_sse_uses_the_announced_message_endpoint_and_stream_responses() {
    let (address, requests) = spawn_legacy_sse_server();
    let mut channel = JsonRpcChannel::new_sse(
        &format!("http://{address}/sse"),
        HashMap::from([("authorization".into(), "Bearer legacy".into())]),
    )
    .unwrap();

    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    let tools = channel.list_tools().await.unwrap();
    assert_eq!(tools["tools"][0]["name"], "legacy_read");

    let recorded = requests.lock().unwrap();
    let get = recorded
        .iter()
        .find(|request| is_get(request))
        .expect("no GET event stream:");
    assert_eq!(
        header_value(get, "accept").as_deref(),
        Some("text/event-stream"),
        "the SSE stream must ask for text/event-stream: {get}"
    );
    assert_eq!(
        authorization(get).as_deref(),
        Some("Bearer legacy"),
        "the GET stream did not carry the configured auth header: {get}"
    );

    let initialize = recorded
        .iter()
        .find(|request| request.contains("\"method\":\"initialize\""))
        .expect("no initialize POST: ");
    assert!(
        initialize.starts_with("POST /messages?sessionId=legacy-1 "),
        "the request did not go to the announced endpoint: {initialize}"
    );
    assert_eq!(
        authorization(initialize).as_deref(),
        Some("Bearer legacy"),
        "the POST did not carry the configured auth header: {initialize}"
    );
    assert_eq!(
        header_value(initialize, "mcp-session-id"),
        None,
        "initialize cannot carry a session it does not know yet: {initialize}"
    );

    let tools_list = recorded
        .iter()
        .find(|request| request.contains("\"method\":\"tools/list\""))
        .expect("no tools/list POST: ");
    assert!(
        tools_list.starts_with("POST /messages?sessionId=legacy-1 "),
        "tools/list did not use the message endpoint: {tools_list}"
    );
    assert_eq!(
        header_value(tools_list, "mcp-session-id").as_deref(),
        Some("legacy-session"),
        "the POST did not carry the session id learned from initialize: {tools_list}"
    );
    assert_eq!(
        header_value(tools_list, "mcp-protocol-version").as_deref(),
        Some("2024-11-05"),
        "the POST did not carry the negotiated protocol version: {tools_list}"
    );

    drop(recorded);
    // Graceful shutdown cancels the long-lived stream.
    channel.terminate().await.unwrap();
}

/// A config entry with `type: "sse"` must drive the whole manager over the
/// classic transport: the initialize POST goes to the endpoint announced by
/// the GET event stream, not to the configured URL.
#[tokio::test]
async fn config_type_sse_drives_the_manager_over_the_legacy_transport() {
    let (address, requests) = spawn_legacy_sse_server();
    let config: McpConfig = serde_json::from_str(&format!(
        r#"{{
            "mcpServers": {{
                "legacy": {{
                    "type": "sse",
                    "url": "http://{address}/sse",
                    "headers": {{ "Authorization": "Bearer legacy" }}
                }}
            }}
        }}"#
    ))
    .unwrap();

    let manager = McpManager::new();
    manager.load_config(&config);
    assert_eq!(
        manager.get_server("legacy").unwrap().transport,
        McpTransport::Sse
    );

    manager.start_all().await;
    assert!(manager.is_running("legacy"));
    assert!(
        manager
            .list_tools()
            .contains(&"mcp__legacy__legacy_read".to_string()),
        "tools were not discovered over legacy SSE: {:?}",
        manager.list_tools()
    );

    let recorded = requests.lock().unwrap();
    let initialize = recorded
        .iter()
        .find(|request| request.contains("\"method\":\"initialize\""))
        .expect("no initialize POST: ");
    assert!(
        initialize.starts_with("POST /messages?sessionId=legacy-1 "),
        "`type: \"sse\"` was not sent to the announced message endpoint: {initialize}"
    );

    drop(recorded);
    manager.stop_all().await;
}

/// An endpoint configured as `type: "sse"` that refuses the GET event stream
/// must be rejected loudly, not silently treated as streamable HTTP.
#[tokio::test]
async fn an_sse_server_without_an_event_stream_is_rejected_loudly() {
    let (address, _requests) = spawn_session_server();
    let mut server = McpServer::new_http(
        "legacy",
        format!("http://{address}/mcp"),
        McpTransport::Sse,
        HashMap::new(),
    );

    let error = server
        .spawn()
        .await
        .expect_err("a streamable-only endpoint must not be accepted as SSE");
    assert!(
        error.to_string().contains("405"),
        "the rejection should name the 405: {error}"
    );
    assert_eq!(
        server.status,
        ServerStatus::Errored,
        "a server that cannot serve SSE must not be reported as running"
    );
}

/// A 405 to a POST is not the session-expired signal: it must surface as an
/// ordinary HTTP error, keep the session id, and must not re-initialize.
#[tokio::test]
async fn a_post_405_is_not_reported_as_a_session_expiry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            recorded.lock().unwrap().push(request.clone());

            if is_get(&request) {
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "application/json",
                    "",
                );
            } else if request.contains("\"method\":\"initialize\"") {
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: session-405\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else if request.contains("\"method\":\"initialized\"") {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            } else if request.contains("\"method\":\"tools/list\"") {
                write_response(
                    &mut stream,
                    "405 Method Not Allowed",
                    "application/json",
                    "",
                );
            } else {
                write_response(&mut stream, "202 Accepted", "application/json", "");
            }
        }
    });

    let mut channel =
        JsonRpcChannel::new_http(&format!("http://{address}/mcp"), HashMap::new()).unwrap();
    channel.spawn().await.unwrap();
    channel.initialize().await.unwrap();
    assert_eq!(channel.session_id().as_deref(), Some("session-405"));

    let error = channel
        .send_request("tools/list", None)
        .await
        .expect_err("a 405 on POST must be an error");
    assert!(error.contains("405"), "unexpected error: {error}");
    assert!(
        !error.contains("after re-initializing"),
        "a POST 405 must not trigger re-initialization: {error}"
    );
    assert_eq!(
        channel.session_id().as_deref(),
        Some("session-405"),
        "a POST 405 must not clear the session id"
    );

    let recorded = requests.lock().unwrap();
    let initializes = recorded
        .iter()
        .filter(|request| request.contains("\"method\":\"initialize\""))
        .count();
    assert_eq!(
        initializes, 1,
        "a POST 405 must not re-initialize the session: {recorded:?}"
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
