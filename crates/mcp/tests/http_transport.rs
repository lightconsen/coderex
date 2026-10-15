//! End-to-end checks for remote MCP HTTP/SSE transport.

use coderex_mcp::channel::JsonRpcChannel;
use coderex_mcp::config::{McpConfig, McpServerConfig, McpTransport};
use coderex_mcp::manager::McpManager;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

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
