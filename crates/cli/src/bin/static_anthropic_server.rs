//! Static Anthropic API server for E2E testing.
//!
//! Serves pre-recorded LLM responses (produced by `record-anthropic-responses`)
//! instead of hard-coded placeholder text.  Tool-selection logic is kept
//! deterministic so E2E tests receive the exact `tool_use` blocks they expect,
//! but the human-readable text inside those blocks comes from a real model.
//!
//! Usage:
//!   cargo run --bin record-anthropic-responses -- responses.json
//!   cargo run --bin static-anthropic-server -- --port 9876 --responses responses.json
//!
//!   ANTHROPIC_API_KEY=mock ANTHROPIC_BASE_URL=http://127.0.0.1:9876 \
//!       cargo test -p coderex-cli --test e2e_code_agent -- --ignored

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

/// Pre-recorded text responses keyed by prompt keyword.
struct StaticResponses {
    texts: HashMap<String, String>,
}

impl StaticResponses {
    fn load(path: &str) -> Self {
        let content = fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
        let texts: HashMap<String, String> =
            serde_json::from_str(&content).unwrap_or_default();
        Self { texts }
    }

    fn resolve(&self, key: &str, fallback: &str) -> String {
        self.texts
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.into())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let port = args
        .windows(2)
        .find(|w| w[0] == "--port")
        .and_then(|w| w[1].parse::<u16>().ok())
        .unwrap_or(9876);

    let responses_path = args
        .windows(2)
        .find(|w| w[0] == "--responses")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "responses.json".into());

    let responses = StaticResponses::load(&responses_path);
    let responses = std::sync::Arc::new(responses);

    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    eprintln!("[static-anthropic-server] http://127.0.0.1:{port}/v1/messages");
    eprintln!("[static-anthropic-server] responses loaded from: {responses_path}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let r = std::sync::Arc::clone(&responses);
                thread::spawn(move || handle_connection(stream, r));
            }
            Err(e) => eprintln!("[static-anthropic-server] accept error: {e}"),
        }
    }
}

fn handle_connection(mut stream: TcpStream, responses: std::sync::Arc<StaticResponses>) {
    let mut buf_reader = BufReader::new(&mut stream);
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if buf_reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line);
    }

    let first_line = headers.first().cloned().unwrap_or_default();
    if !first_line.starts_with("POST /v1/messages") {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    }

    let content_length = headers
        .iter()
        .find_map(|h| {
            let h = h.to_lowercase();
            h.strip_prefix("content-length: ")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        buf_reader.read_exact(&mut body).expect("read body");
    }
    let body_str = String::from_utf8_lossy(&body);

    let prompt = extract_last_user_prompt(&body_str);
    let has_tool_result = body_str.contains("\"type\":\"tool_result\"");
    let is_streaming = body_str.contains("\"stream\":true");
    let turn_number = count_tool_results(&body_str) + 1;
    let has_json_schema = body_str.contains("\"type\":\"object\"")
        || body_str.contains("\\\"type\\\": \\\"object\\\"")
        || body_str.contains("\\\"type\\\":\\\"object\\\"")
        || body_str.contains("json_schema");

    eprintln!(
        "[static] prompt='{}' has_tool_result={} is_streaming={} turn={} json_schema={}",
        prompt.chars().take(80).collect::<String>(),
        has_tool_result,
        is_streaming,
        turn_number,
        has_json_schema
    );

    // Simulate API errors for retry testing
    let p_lower = prompt.to_lowercase();
    if p_lower.contains("simulate error")
        || p_lower.contains("force error")
        || p_lower.contains("trigger error")
    {
        let _ = stream.write_all(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.flush();
        return;
    }

    if is_streaming {
        let sse = if p_lower.contains("trigger loop") {
            build_loop_tool_response()
        } else if p_lower.contains("multi-turn sequence") || p_lower.contains("do 5 things") {
            build_sequence_tool_response(turn_number)
        } else if has_json_schema {
            build_json_sse_response(r#"{"greeting": "Hello!"}"#)
        } else if has_tool_result {
            build_text_response(&prompt, &responses)
        } else {
            build_tool_response(&prompt)
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\r\n{sse}"
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    } else {
        let json = build_json_response(&prompt, has_tool_result, &responses);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Connection: close\r\n\
             Content-Length: {}\r\n\r\n{json}",
            json.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

fn extract_last_user_prompt(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if let Some(messages) = value.get("messages").and_then(|m| m.as_array()) {
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    return text.to_string();
                }
            }
        }
    }
    String::new()
}

fn count_tool_results(body: &str) -> usize {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if let Some(messages) = value.get("messages").and_then(|m| m.as_array()) {
        messages
            .iter()
            .filter(|msg| {
                if let Some(content) = msg.get("content") {
                    content.to_string().contains("\"type\":\"tool_result\"")
                } else {
                    false
                }
            })
            .count()
    } else {
        0
    }
}

// ── Text resolution (static LLM responses) ─────────────────────────────────

fn resolve_text_response(prompt: &str, responses: &StaticResponses) -> String {
    let p = prompt.to_lowercase();

    if p.contains("fibonacci") {
        responses.resolve("fibonacci", "Done! I've added the recursive fibonacci function.")
    } else if p.contains("factorial") || p.contains("bug") {
        responses.resolve("factorial", "Fixed! The factorial bug has been corrected.")
    } else if p.contains("multiply") {
        responses.resolve("multiply", "Done! Added the multiply function.")
    } else if p.contains("max") {
        responses.resolve("max", "Fixed! The max function now works correctly.")
    } else if p.contains("palindrome") {
        responses.resolve("palindrome", "Implemented! The is_palindrome function is ready.")
    } else if p.contains("stack") {
        responses.resolve("stack", "Done! Added the generic Stack<T> with tests.")
    } else if p.contains("format_user_label") || p.contains("refactor") {
        responses.resolve("refactor", "Refactored! Extracted the shared logic.")
    } else if p.contains("auth") && p.contains("protected") {
        responses.resolve("auth", "Done! Added auth middleware and protected routes.")
    } else if p.contains("api endpoint") || p.contains("/users") {
        responses.resolve("api_endpoint", "Done! Added the POST /users endpoint.")
    } else if p.contains("large") || p.contains("fn_last") {
        responses.resolve("fn_last", "Done! Appended fn_last to the file.")
    } else if p.contains("triple") {
        responses.resolve("triple", "Done! Added the triple function.")
    } else if p.contains("nonexistent") || p.contains("error") {
        responses.resolve("nonexistent", "Done! Created the new file.")
    } else if p.contains("review") {
        responses.resolve("review", "Review complete. Found potential issues.")
    } else if p.contains("rm -rf") || p.contains("dangerous") {
        responses.resolve("rm_dangerous", "I can't run that destructive command. It would damage the system.")
    } else if p.contains("cargo test") || p.contains("test results") {
        responses.resolve("cargo_test", "Running tests... All passed.")
    } else if p.contains("secrets") || p.contains("api.key") {
        responses.resolve("secrets", "Created the secrets file and auth module.")
    } else if p.contains("rust source files") || p.contains("glob") || p.contains("list all") && p.contains("files") {
        responses.resolve("bash", "Here are the files in the directory.")
    } else if p.contains("bash") || p.contains("list") || p.contains("files") {
        responses.resolve("bash", "Here are the files in the directory.")
    } else if p.contains("api_key") || p.contains("secret") {
        responses.resolve("api_key_secret", "I don't have access to that value.")
    } else if p.contains("web fetch") || p.contains("fetch url") {
        responses.resolve("web_fetch", "Fetched the content successfully.")
    } else if p.contains("web search") || p.contains("search web") {
        responses.resolve("web_search", "Here are the search results.")
    } else if p.contains("read") && p.contains("lib.rs") {
        responses.resolve("read_lib", "The file defines an add function.")
    } else if p.contains("grep") || p.contains("search for") || p.contains("search pattern") {
        responses.resolve("grep_search", "Found the matching functions.")
    } else if p.contains("hello") || p.contains("hi") {
        responses.resolve("hello", "Hello! I'm ready to help.")
    } else {
        responses.resolve("generic", "Done!")
    }
}

fn build_text_response(prompt: &str, responses: &StaticResponses) -> String {
    sse_text_block(&resolve_text_response(prompt, responses))
}

fn sse_text_block(text: &str) -> String {
    let escaped = json_escape_string(text);
    format!(
        "event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{escaped}\"}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":20}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn build_json_sse_response(json: &str) -> String {
    sse_text_block(json)
}

fn build_json_response(prompt: &str, has_tool_result: bool, responses: &StaticResponses) -> String {
    let text = if has_tool_result {
        resolve_text_response(prompt, responses)
    } else {
        if prompt.to_lowercase().contains("title") {
            r#"{"title": "Mock Session"}"#.into()
        } else {
            resolve_text_response(prompt, responses)
        }
    };
    format!(
        r#"{{"id":"msg_static","type":"message","role":"assistant","content":[{{"type":"text","text":"{}"}}],"stop_reason":"end_turn","stop_sequence":null,"usage":{{"input_tokens":10,"output_tokens":20}}}}"#,
        json_escape_string(&text)
    )
}

// ── Tool responses (deterministic — E2E tests depend on these) ─────────────

fn build_tool_response(prompt: &str) -> String {
    let tools = select_tools(prompt);
    let mut sse = String::new();

    for (idx, tool) in tools.iter().enumerate() {
        let (tool_name, input_json) = tool_to_json(tool);
        let input_json_escaped = json_escape_string(&input_json);
        let id = format!("tool_{:02}", idx + 1);
        sse.push_str(&format!(
            "event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{input_json_escaped}\"}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n"
        ));
    }

    sse.push_str(
        "event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n\n",
    );

    sse
}

fn build_loop_tool_response() -> String {
    let tool = Tool::Bash {
        command: "echo 'loop_iteration'".into(),
    };
    let (tool_name, input_json) = tool_to_json(&tool);
    let input_json_escaped = json_escape_string(&input_json);
    let id = "tool_loop";
    format!(
        "event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{input_json_escaped}\"}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":20}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn build_sequence_tool_response(turn_number: usize) -> String {
    let command = format!("echo 'turn_{turn_number}'");
    let tool = Tool::Bash { command };
    let (tool_name, input_json) = tool_to_json(&tool);
    let input_json_escaped = json_escape_string(&input_json);
    let id = format!("tool_seq_{turn_number:02}");
    format!(
        "event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{input_json_escaped}\"}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":20}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn tool_to_json(tool: &Tool) -> (String, String) {
    match tool {
        Tool::Write { file, content } => {
            let escaped = json_escape_string(content);
            ("Write".into(), format!(r#"{{"file_path":"{file}","content":"{escaped}"}}"#))
        }
        Tool::Edit { file, old_string, new_string } => {
            let old_e = json_escape_string(old_string);
            let new_e = json_escape_string(new_string);
            ("Edit".into(), format!(r#"{{"file_path":"{file}","old_string":"{old_e}","new_string":"{new_e}"}}"#))
        }
        Tool::Bash { command } => {
            ("Bash".into(), format!(r#"{{"command":"{command}"}}"#))
        }
        Tool::Read { file } => {
            ("Read".into(), format!(r#"{{"file_path":"{file}"}}"#))
        }
    }
}

fn json_escape_string(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '"' => vec!['\\', '"'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            c => vec![c],
        })
        .collect()
}

#[derive(Debug)]
enum Tool {
    Write { file: String, content: String },
    Edit { file: String, old_string: String, new_string: String },
    Bash { command: String },
    Read { file: String },
}

fn select_tools(prompt: &str) -> Vec<Tool> {
    let p = prompt.to_lowercase();

    if p.contains("fibonacci") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn fibonacci(n: u32) -> u32 {
    if n <= 1 { n } else { fibonacci(n - 1) + fibonacci(n - 2) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_fibonacci() {
        assert_eq!(fibonacci(0), 0);
        assert_eq!(fibonacci(1), 1);
        assert_eq!(fibonacci(5), 5);
    }
}
"#
            .into(),
        }]
    } else if p.contains("factorial") || p.contains("bug") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r#"pub fn factorial(n: u64) -> u64 {
    if n <= 1 { 1 } else { n * factorial(n - 1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_factorial() {
        assert_eq!(factorial(5), 120);
    }
}
"#
            .into(),
        }]
    } else if p.contains("multiply") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }\npub fn multiply(a: i32, b: i32) -> i32 { a * b }\n".into(),
        }]
    } else if p.contains("max") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r#"pub fn max(a: i32, b: i32) -> i32 {
    if a > b { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max() {
        assert_eq!(max(5, 3), 5);
        assert_eq!(max(2, 10), 10);
    }
}
"#
            .into(),
        }]
    } else if p.contains("palindrome") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r#"pub fn is_palindrome(s: &str) -> bool {
    let cleaned: String = s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    cleaned == cleaned.chars().rev().collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_palindrome() {
        assert!(is_palindrome("racecar"));
        assert!(is_palindrome("A man a plan a canal Panama"));
        assert!(!is_palindrome("hello"));
    }
}
"#
            .into(),
        }]
    } else if p.contains("stack") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct Stack<T> {
    items: Vec<T>,
}

impl<T> Stack<T> {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }
    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_stack() {
        let mut s = Stack::new();
        s.push(1);
        s.push(2);
        assert_eq!(s.pop(), Some(2));
        assert!(!s.is_empty());
    }
}
"#
            .into(),
        }]
    } else if p.contains("format_user_label") || p.contains("refactor") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r#"pub fn format_user_label(name: &str, age: u32) -> String {
    let prefix = if age >= 18 { "Adult" } else { "Minor" };
    format!("{}: {}", prefix, name)
}

pub fn process_user_a(name: &str, age: u32) -> String {
    format_user_label(name, age)
}

pub fn process_user_b(name: &str, age: u32) -> String {
    format_user_label(name, age)
}
"#
            .into(),
        }]
    } else if p.contains("auth") && p.contains("protected") {
        vec![
            Tool::Write {
                file: "src/middleware.rs".into(),
                content: "pub fn check_auth(token: &str) -> bool { !token.is_empty() }\npub fn log_request(path: &str) { println!(\"request: {}\", path); }\n".into(),
            },
            Tool::Write {
                file: "src/routes.rs".into(),
                content: "use crate::models::User;\nuse crate::middleware::check_auth;\n\npub fn get_user(id: u64) -> User {\n    User { id, name: \"anon\".into() }\n}\n\npub fn protected_route(token: &str) -> String {\n    if check_auth(token) { \"Access granted\".into() } else { \"Access denied\".into() }\n}\n".into(),
            },
            Tool::Write {
                file: "src/models.rs".into(),
                content: "#[derive(Debug)]\npub struct User {\n    pub id: u64,\n    pub name: String,\n}\n\n#[derive(Debug)]\npub struct AuthRequest {\n    pub token: String,\n}\n".into(),
            },
        ]
    } else if p.contains("api endpoint") || p.contains("/users") {
        vec![Tool::Write {
            file: "src/main.rs".into(),
            content: r#"use axum::{routing::{get, post}, Router, http::StatusCode};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { "Hello" }))
        .route("/users", post(|| async { (StatusCode::CREATED, "User created") }));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
"#
            .into(),
        }]
    } else if p.contains("fn_last") {
        vec![Tool::Edit {
            file: "src/large.rs".into(),
            old_string: "pub fn fn_9999() -> i32 { 9999 }\n".into(),
            new_string: "pub fn fn_9999() -> i32 { 9999 }\npub fn fn_last() -> i32 { 9999 }\n".into(),
        }]
    } else if p.contains("triple") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn triple(x: i32) -> i32 { x * 3 }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn test_add() {\n        assert_eq!(add(2, 3), 5);\n    }\n}\n".into(),
        }]
    } else if p.contains("nonexistent") || p.contains("error") {
        vec![Tool::Write {
            file: "src/nonexistent.rs".into(),
            content: "pub const X: i32 = 42;\n".into(),
        }]
    } else if p.contains("secrets") || p.contains("api.key") {
        vec![Tool::Bash {
            command: "mkdir -p secrets && echo 'super-secret-key' > secrets/api.key && echo 'pub fn read_key() -> String { std::fs::read_to_string(\"secrets/api.key\").unwrap() }' > src/auth.rs".into(),
        }]
    } else if p.contains("review") {
        vec![Tool::Bash {
            command: "echo 'review complete'".into(),
        }]
    } else if p.contains("bash") || p.contains("list") {
        vec![Tool::Bash {
            command: "find . -name '*.rs' -type f".into(),
        }]
    } else if p.contains("glob") || p.contains("find all") || p.contains("list all files") {
        vec![Tool::Bash {
            command: "find . -name '*.rs' -type f | sort".into(),
        }]
    } else if p.contains("grep") || p.contains("search pattern") || p.contains("search for") {
        vec![Tool::Bash {
            command: "grep -rn 'fn ' src/".into(),
        }]
    } else if p.contains("read file") || p.contains("show content") || p.contains("file content") {
        vec![Tool::Read {
            file: "src/lib.rs".into(),
        }]
    } else if p.contains("image.bin") || p.contains("binary") {
        vec![Tool::Bash {
            command: "ls -la".into(),
        }]
    } else {
        vec![Tool::Bash {
            command: "echo 'hello from static server'".into(),
        }]
    }
}
