//! End-to-end checks for remote MCP HTTP/SSE transport.

use coderex_mcp::channel::{InboundRequestHandler, JsonRpcChannel, JsonRpcError};
use coderex_mcp::config::{McpConfig, McpServerConfig, McpTransport};
use coderex_mcp::manager::McpManager;
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

fn spawn_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (index, expected_method) in ["initialize", "initialized", "tools/list"]
            .into_iter()
            .enumerate()
        {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(
                request.contains(&format!("\"method\":\"{expected_method}\"")),
                "unexpected request: {request}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test")
            );

            match index {
                0 => write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
                ),
                1 => write_response(&mut stream, "202 Accepted", "application/json", ""),
                _ => write_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    concat!(
                        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"remote_read\"}]}}\n\n"
                    ),
                ),
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
        // initialize
        {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.contains("\"method\":\"initialize\""), "{request}");
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
            );
        }

        // initialized notification
        {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.contains("\"method\":\"initialized\""), "{request}");
            write_response(&mut stream, "202 Accepted", "application/json", "");
        }

        // tools/list: the server asks before answering.
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        assert!(request.contains("\"method\":\"tools/list\""), "{request}");
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

        // The answer has to arrive on its own connection while this stream is
        // still open; the client must not wait for the response body to end.
        let (mut answer_stream, _) = listener.accept().unwrap();
        let answer = read_request(&mut answer_stream);
        write_response(&mut answer_stream, "202 Accepted", "application/json", "");

        // Only now can the original request complete.
        stream
            .write_all(
                b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"remote_read\"}]}}\n\n",
            )
            .unwrap();
        stream.flush().unwrap();

        answer
    });
    (address, handle)
}

#[derive(Debug)]
struct AcceptingHandler;

impl InboundRequestHandler for AcceptingHandler {
    fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, JsonRpcError> {
        assert_eq!(method, "elicitation/create");
        Ok(serde_json::json!({"action": "accept", "content": {"answer": 42}}))
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
