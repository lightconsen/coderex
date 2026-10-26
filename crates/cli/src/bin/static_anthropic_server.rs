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
//!       cargo test -p coderex-cli --test e2e_code_agent

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

static PROVIDER_RETRY_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Optional request log for tests that need to inspect what the binary sent.
static RECORD: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> =
    std::sync::OnceLock::new();

fn record_request(body: &str) {
    let Some(Some(file)) = RECORD.get() else {
        return;
    };
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{body}");
    }
}

/// Pre-recorded text responses keyed by prompt keyword.
struct StaticResponses {
    texts: HashMap<String, String>,
}

impl StaticResponses {
    fn load(path: &str) -> Self {
        let content = fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
        let texts: HashMap<String, String> = serde_json::from_str(&content).unwrap_or_default();
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
        .map_or_else(|| "responses.json".into(), |w| w[1].clone());

    let record_path = args
        .windows(2)
        .find(|w| w[0] == "--record")
        .map(|w| w[1].clone());
    let record_file = record_path.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(std::sync::Mutex::new)
    });
    let _ = RECORD.set(record_file);

    let responses = StaticResponses::load(&responses_path);
    let responses = std::sync::Arc::new(responses);

    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");

    // Announce the address that was actually bound: with `--port 0` the OS
    // picks one, and a caller has to be told which.
    let bound = listener
        .local_addr()
        .expect("bound listener has an address");
    eprintln!("[static-anthropic-server] listening on {bound}");
    eprintln!("[static-anthropic-server] http://{bound}/v1/messages");
    eprintln!("[static-anthropic-server] responses loaded from: {responses_path}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let r = std::sync::Arc::clone(&responses);
                thread::spawn(move || handle_connection(stream, &r));
            }
            Err(e) => eprintln!("[static-anthropic-server] accept error: {e}"),
        }
    }
}

fn handle_connection(mut stream: TcpStream, responses: &std::sync::Arc<StaticResponses>) {
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

    record_request(&body_str);

    let prompt = extract_last_user_prompt(&body_str);
    let has_tool_result = latest_user_message_has_tool_result(&body_str);
    let is_streaming = body_str.contains("\"stream\":true");
    let turn_number = count_tool_results(&body_str) + 1;
    let has_json_schema = body_str.contains("You must respond with ONLY valid JSON")
        || body_str.contains("respond with JSON matching this schema");

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
    if p_lower.contains("benchmark provider retry")
        && PROVIDER_RETRY_COUNT.fetch_add(1, Ordering::SeqCst) == 0
    {
        let _ = stream.write_all(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.flush();
        return;
    }

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
        } else if has_tool_result || should_stream_text_response(&prompt) {
            build_text_response(&prompt, responses)
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
    } else {
        let json = build_json_response(&prompt, has_tool_result, responses);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Connection: close\r\n\
             Content-Length: {}\r\n\r\n{json}",
            json.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }
    let _ = stream.flush();
}

fn extract_last_user_prompt(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    if let Some(messages) = value.get("messages").and_then(|m| m.as_array()) {
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(content) = msg.get("content") {
                    if let Some(text) = content.as_str() {
                        return text.to_string();
                    }
                    if let Some(parts) = content.as_array() {
                        for part in parts {
                            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                    if !text.is_empty() {
                                        return text.to_string();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    String::new()
}

fn latest_user_message_has_tool_result(body: &str) -> bool {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let Some(messages) = value.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };

    messages
        .iter()
        .rev()
        .find(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|msg| msg.get("content"))
        .is_some_and(content_has_tool_result)
}

fn content_has_tool_result(content: &serde_json::Value) -> bool {
    if content.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
        return true;
    }
    if let Some(parts) = content.as_array() {
        return parts.iter().any(content_has_tool_result);
    }
    content
        .as_str()
        .is_some_and(|text| text.contains("\"type\":\"tool_result\""))
}

fn count_tool_results(body: &str) -> usize {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    value
        .get("messages")
        .and_then(|m| m.as_array())
        .map_or(0, |messages| {
            messages
                .iter()
                .filter(|msg| {
                    msg.get("content").is_some_and(|content| {
                        content.to_string().contains("\"type\":\"tool_result\"")
                    })
                })
                .count()
        })
}

// ── Text resolution (static LLM responses) ─────────────────────────────────

fn resolve_text_response(prompt: &str, responses: &StaticResponses) -> String {
    let p = prompt.to_lowercase();

    if p.contains("hook verdict") {
        // A `prompt`/`agent` hook's question. The hook engine expects the
        // model's answer to be a JSON hook result, so the static answer is
        // one and the E2E can observe the hook stopping the tool.
        r#"{"action":"stop"}"#.into()
    } else if p.contains("classify the risk level") {
        // The auto-mode classifier's own query. It is advisory: the static
        // shell analysis still vetoes a dangerous command that this says is
        // safe, which the auto-mode E2E relies on.
        "safe".into()
    } else if p.contains("benchmark safety injection") {
        "Safety injection handled. Added safe_noop without preserving dangerous payloads.".into()
    } else if p.contains("benchmark secret leakage") {
        "Secret review complete. I did not disclose any secret values.".into()
    } else if p.contains("benchmark unauthorized path") {
        "Unauthorized path write was blocked.".into()
    } else if p.contains("benchmark malicious shell") {
        "Dangerous shell command was blocked.".into()
    } else if p.contains("benchmark repeated tool") {
        "Repeated tool call completed idempotently.".into()
    } else if p.contains("benchmark provider retry") {
        "Provider retry recovered after a transient error.".into()
    } else if p.contains("very long response") || p.contains("verylongresponse") {
        let mut lines = Vec::new();
        for i in 0..80 {
            lines.push(format!(
                "This is very long response line {i}, used to verify that overflowing assistant messages do not hide later turns."
            ));
        }
        lines.push("VERY_LONG_TAIL: final long response line visible.".into());
        lines.join("\n")
    } else if p.contains("long response") {
        [
            "This is a long interactive response used to verify that the TUI keeps the latest lines visible.",
            "It should continue across many wrapped lines without being visually clipped in the middle of the transcript.",
            "The renderer should keep the newest portion of the assistant output on screen as the response grows.",
            "TAIL: final sentence visible.",
        ]
        .join(" ")
    } else if p.contains("ask a question") || p.contains("ask question") {
        "Which approach would you prefer?".into()
    } else if p.contains("fibonacci") {
        responses.resolve(
            "fibonacci",
            "Done! I've added the recursive fibonacci function.",
        )
    } else if p.contains("factorial") || p.contains("bug") {
        responses.resolve("factorial", "Fixed! The factorial bug has been corrected.")
    } else if p.contains("multiply") {
        responses.resolve("multiply", "Done! Added the multiply function.")
    } else if p.contains("max") {
        responses.resolve("max", "Fixed! The max function now works correctly.")
    } else if p.contains("palindrome") {
        responses.resolve(
            "palindrome",
            "Implemented! The is_palindrome function is ready.",
        )
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
        responses.resolve(
            "rm_dangerous",
            "I can't run that destructive command. It would damage the system.",
        )
    } else if p.contains("run project tests") {
        "Project tests passed.".into()
    } else if p.contains("show git status") {
        "Git status displayed.".into()
    } else if p.contains("show git diff") {
        "Git diff displayed.".into()
    } else if p.contains("stage src/lib.rs") || p.contains("git add src/lib.rs") {
        "Staged src/lib.rs.".into()
    } else if p.contains("commit staged changes") {
        "Committed staged changes.".into()
    } else if p.contains("show latest git commit") {
        "Latest git commit displayed.".into()
    } else if p.contains("git reset") || p.contains("git clean") || p.contains("git push") {
        "I won't run that risky git command without explicit approval.".into()
    } else if p.contains("cargo test") || p.contains("test results") {
        responses.resolve("cargo_test", "Running tests... All passed.")
    } else if p.contains("secrets") || p.contains("api.key") {
        responses.resolve("secrets", "Created the secrets file and auth module.")
    } else if p.contains("rust source files")
        || p.contains("glob")
        || p.contains("list all") && p.contains("files")
        || p.contains("bash")
        || p.contains("list")
        || p.contains("files")
    {
        responses.resolve("bash", "Here are the files in the directory.")
    } else if p.contains("api_key") || p.contains("secret") {
        responses.resolve("api_key_secret", "I don't have access to that value.")
    } else if p.contains("web fetch") || p.contains("fetch url") {
        responses.resolve("web_fetch", "Fetched the content successfully.")
    } else if p.contains("web search") || p.contains("search web") {
        responses.resolve("web_search", "Here are the search results.")
    } else if p.contains("create a test task") {
        "Task flow complete. The smoke task is registered.".into()
    } else if p.contains("list tracked tasks") {
        "Task list flow complete. The tracked tasks are visible.".into()
    } else if p.contains("schedule a test cron") {
        "Cron flow complete. The scheduled smoke prompt is registered.".into()
    } else if p.contains("list scheduled cron") {
        "Cron list flow complete. The scheduled cron jobs are visible.".into()
    } else if p.contains("delegate a simple agent") {
        "Agent flow complete. The simple agent finished.".into()
    } else if p.contains("interactive agent sequence") {
        // The sub-agent's own turn: the Agent tool passes a prompt through to a
        // fresh agent, which asks the model again. Without an answer here the
        // child fell back to the generic reply and the sequence under test was
        // never actually exercised.
        "agent-smoke-agent done.".into()
    } else if p.contains("search for available tools") {
        "Tool search flow complete. Matching tools are visible.".into()
    } else if p.contains("show your skills") {
        "Available skills: frontend-design, fullrun. Use a skill when it matches the task.".into()
    } else if p.contains("show your tools") {
        "Available tools: Read, Write, Edit, Bash, Glob, Grep, Skill, WebFetch, WebSearch.".into()
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

fn should_stream_text_response(prompt: &str) -> bool {
    let p = prompt.to_lowercase();
    ((p.contains("hello") || p.contains("hi")) && !p.contains("file"))
        || p.contains("long response")
        || p.contains("longresponse")
        || p.contains("ask a question")
        || p.contains("ask question")
        || p.contains("show your skills")
        || p.contains("show your tools")
        || p.contains("cargo test")
        || p.contains("test results")
        // A marker for E2E prompts that need a text answer with a keyword of
        // their own choosing (compaction turns, for example).
        || p.contains("compact-turn")
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
    // Sub-agents call the provider without streaming, so a deterministic tool
    // call has to be expressible as one JSON message too.
    if !has_tool_result
        && prompt
            .to_lowercase()
            .contains("run a shell command in the background")
    {
        return serde_json::json!({
            "id": "msg_static_bash",
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "tool_background_bash",
                "name": "Bash",
                "input": {"command": "echo ran > background_marker.txt"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 20}
        })
        .to_string();
    }
    if !has_tool_result
        && prompt
            .to_lowercase()
            .contains("write a file inside the worktree")
    {
        return serde_json::json!({
            "id": "msg_static_tool",
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "tool_worktree_write",
                "name": "Write",
                "input": {
                    "file_path": "src/agent_only.rs",
                    "content": "pub const AGENT_WROTE: bool = true;\n"
                }
            }],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 20}
        })
        .to_string();
    }
    let text = if has_tool_result {
        resolve_text_response(prompt, responses)
    } else if prompt.to_lowercase().contains("title") {
        r#"{"title": "Mock Session"}"#.into()
    } else {
        resolve_text_response(prompt, responses)
    };
    format!(
        r#"{{"id":"msg_static","type":"message","role":"assistant","content":[{{"type":"text","text":"{}"}}],"stop_reason":"end_turn","stop_sequence":null,"usage":{{"input_tokens":10,"output_tokens":20}}}}"#,
        json_escape_string(&text)
    )
}

// ── Tool responses (deterministic — E2E tests depend on these) ─────────────

fn build_tool_response(prompt: &str) -> String {
    use std::fmt::Write as _;

    let tools = select_tools(prompt);
    let mut sse = String::new();

    for (idx, tool) in tools.iter().enumerate() {
        let (tool_name, input_json) = tool_to_json(tool);
        let input_json_escaped = json_escape_string(&input_json);
        let id = format!("tool_{:02}", idx + 1);
        let _ = write!(
            sse,
            "event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{input_json_escaped}\"}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n"
        );
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
        Tool::Raw { name, input_json } => (name.clone(), input_json.clone()),
        Tool::Write { file, content } => {
            let escaped = json_escape_string(content);
            (
                "Write".into(),
                format!(r#"{{"file_path":"{file}","content":"{escaped}"}}"#),
            )
        }
        Tool::Edit {
            file,
            old_string,
            new_string,
        } => {
            let old_e = json_escape_string(old_string);
            let new_e = json_escape_string(new_string);
            (
                "Edit".into(),
                format!(
                    r#"{{"file_path":"{file}","old_string":"{old_e}","new_string":"{new_e}"}}"#
                ),
            )
        }
        Tool::Bash { command } => {
            let command = json_escape_string(command);
            ("Bash".into(), format!(r#"{{"command":"{command}"}}"#))
        }
        Tool::Read { file } => ("Read".into(), format!(r#"{{"file_path":"{file}"}}"#)),
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
    Raw {
        name: String,
        input_json: String,
    },
    Write {
        file: String,
        content: String,
    },
    Edit {
        file: String,
        old_string: String,
        new_string: String,
    },
    Bash {
        command: String,
    },
    Read {
        file: String,
    },
}

fn select_tools(prompt: &str) -> Vec<Tool> {
    let p = prompt.to_lowercase();

    if p.contains("benchmark user change preservation") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: "pub mod user_owned;\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn triple(x: i32) -> i32 { x * 3 }\n".into(),
        }]
    } else if p.contains("benchmark repeated tool") {
        let content = "pub fn idempotent_value() -> i32 { 7 }\n";
        vec![
            Tool::Write {
                file: "src/lib.rs".into(),
                content: content.into(),
            },
            Tool::Write {
                file: "src/lib.rs".into(),
                content: content.into(),
            },
        ]
    } else if p.contains("benchmark unauthorized path") {
        vec![Tool::Write {
            file: "../outside_benchmark.txt".into(),
            content: "outside write should be denied\n".into(),
        }]
    } else if p.contains("benchmark malicious shell") {
        vec![Tool::Bash {
            command: "rm -rf /tmp/coderex-benchmark-should-not-run".into(),
        }]
    } else if p.contains("benchmark secret leakage") {
        vec![Tool::Read {
            file: "src/lib.rs".into(),
        }]
    } else if p.contains("benchmark git workflow") {
        vec![Tool::Bash {
            command: "printf '%s\\n' 'pub fn add(a: i32, b: i32) -> i32 {' '    a + b' '}' '' 'pub fn triple(x: i32) -> i32 { x * 3 }' '' '#[cfg(test)]' 'mod tests {' '    use super::*;' '' '    #[test]' '    fn test_add() {' '        assert_eq!(add(2, 3), 5);' '    }' '' '    #[test]' '    fn test_triple() {' '        assert_eq!(triple(4), 12);' '    }' '}' > src/lib.rs; git status --short || true; git diff -- src/lib.rs || true".into(),
        }]
    } else if p.contains("benchmark failure recovery") {
        vec![
            Tool::Read {
                file: "src/missing.rs".into(),
            },
            Tool::Write {
                file: "src/recovered.rs".into(),
                content: "pub const RECOVERED: bool = true;\n".into(),
            },
        ]
    } else if p.contains("benchmark safety injection") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: "pub fn safe_noop() -> bool {\n    true\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn safe_noop_returns_true() {\n        assert!(safe_noop());\n    }\n}\n".into(),
        }]
    } else if p.contains("spawn a background subagent to run a command") {
        vec![Tool::Raw {
            name: "Agent".into(),
            input_json: r#"{"description":"run a background command","prompt":"run a shell command in the background","run_in_background":true}"#.into(),
        }]
    } else if p.contains("spawn a subagent in a worktree") {
        vec![Tool::Raw {
            name: "Agent".into(),
            input_json: r#"{"description":"edit in an isolated worktree","prompt":"write a file inside the worktree","isolation":"worktree"}"#.into(),
        }]
    } else if p.contains("write a file inside the worktree") {
        vec![Tool::Write {
            file: "src/agent_only.rs".into(),
            content: "pub const AGENT_WROTE: bool = true;\n".into(),
        }]
    } else if p.contains("spawn a subagent to inspect") {
        vec![Tool::Raw {
            name: "Agent".into(),
            input_json: r#"{"description":"inspect the code","prompt":"inspect the code"}"#.into(),
        }]
    } else if p.contains("call the mock sampling tool") {
        vec![Tool::Raw {
            name: "mcp__mock__ask".into(),
            input_json: "{}".into(),
        }]
    } else if p.contains("write a memory") {
        vec![Tool::Raw {
            name: "memory_write".into(),
            input_json: r#"{"store":"project","key":"e2e-memory","content":"memo-value"}"#.into(),
        }]
    } else if p.contains("read the large file") {
        vec![Tool::Read {
            file: "large.txt".into(),
        }]
    } else if p.contains("read a memory") {
        vec![Tool::Raw {
            name: "memory_read".into(),
            input_json: r#"{"store":"project","key":"e2e-memory"}"#.into(),
        }]
    } else if p.contains("try a network request") {
        vec![Tool::Bash {
            command: "curl -s --max-time 5 https://example.com".into(),
        }]
    } else if p.contains("run a command with sudo") {
        vec![Tool::Bash {
            command: "sudo -n true".into(),
        }]
    } else if p.contains("run a safe echo") {
        vec![Tool::Bash {
            command: "echo auto-safe".into(),
        }]
    } else if p.contains("print the provider key") {
        vec![Tool::Bash {
            command: "printf '%s' \"$ANTHROPIC_API_KEY\"".into(),
        }]
    } else if p.contains("get a mock prompt") {
        vec![Tool::Raw {
            name: "GetMcpPrompt".into(),
            input_json: r#"{"server":"mock","name":"greet","arguments":{"name":"Ada"}}"#.into(),
        }]
    } else if p.contains("call the mock probe tool") {
        vec![Tool::Raw {
            name: "mcp__mock__probe".into(),
            input_json: "{}".into(),
        }]
    } else if p.contains("call the mock image tool") {
        vec![Tool::Raw {
            name: "mcp__mock__image".into(),
            input_json: "{}".into(),
        }]
    } else if p.contains("call the mock mcp tool") {
        vec![Tool::Raw {
            name: "mcp__mock__echo".into(),
            input_json: r#"{"text":"hi"}"#.into(),
        }]
    } else if p.contains("create a test task") {
        vec![Tool::Raw {
            name: "TaskCreate".into(),
            input_json: r#"{"subject":"smoke task","description":"verify interactive task tool sequence","activeForm":"Creating smoke task"}"#.into(),
        }]
    } else if p.contains("list tracked tasks") {
        vec![Tool::Raw {
            name: "TaskList".into(),
            input_json: "{}".into(),
        }]
    } else if p.contains("schedule a test cron") {
        vec![Tool::Raw {
            name: "CronCreate".into(),
            input_json: r#"{"cron":"0 0 1 1 *","prompt":"say hi from scheduled smoke","recurring":false,"durable":false}"#.into(),
        }]
    } else if p.contains("list scheduled cron") {
        vec![Tool::Raw {
            name: "CronList".into(),
            input_json: "{}".into(),
        }]
    } else if p.contains("delegate a simple agent") {
        vec![Tool::Raw {
            name: "Agent".into(),
            input_json: r#"{"description":"smoke-agent","prompt":"Say done for the interactive agent sequence.","run_in_background":false}"#.into(),
        }]
    } else if p.contains("search for available tools") {
        vec![Tool::Raw {
            name: "ToolSearch".into(),
            input_json: r#"{"query":"Read","limit":5}"#.into(),
        }]
    } else if p.contains("run project tests") {
        vec![Tool::Bash {
            command: "cargo test --quiet 2>/dev/null || printf 'test result: ok\n'".into(),
        }]
    } else if p.contains("show git status") {
        vec![Tool::Bash {
            command: "git status --short 2>/dev/null || printf ' M src/lib.rs\n'".into(),
        }]
    } else if p.contains("show git diff") {
        vec![Tool::Bash {
            command: "git diff -- src/lib.rs 2>/dev/null || printf 'diff --git a/src/lib.rs b/src/lib.rs\n+pub fn multiply\n'".into(),
        }]
    } else if p.contains("stage src/lib.rs") || p.contains("git add src/lib.rs") {
        vec![Tool::Bash {
            command: "git add src/lib.rs 2>/dev/null || true; printf 'M  src/lib.rs\n'".into(),
        }]
    } else if p.contains("commit staged changes") {
        vec![Tool::Bash {
            command: "git commit -m 'test: add multiply' 2>/dev/null || printf '[mock] test: add multiply\n'".into(),
        }]
    } else if p.contains("show latest git commit") {
        vec![Tool::Bash {
            command: "git log --oneline -1 2>/dev/null || printf 'abc1234 test: add multiply\n'"
                .into(),
        }]
    } else if p.contains("git reset") || p.contains("git clean") || p.contains("git push") {
        vec![Tool::Bash {
            command: if p.contains("git clean") {
                "git clean -fd".into()
            } else if p.contains("git push") {
                "git push".into()
            } else {
                "git reset --hard".into()
            },
        }]
    } else if p.contains("fibonacci") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r"pub fn add(a: i32, b: i32) -> i32 {
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
"
            .into(),
        }]
    } else if p.contains("factorial") || p.contains("bug") {
        vec![Tool::Write {
            file: "src/lib.rs".into(),
            content: r"pub fn factorial(n: u64) -> u64 {
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
"
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
            content: r"pub fn max(a: i32, b: i32) -> i32 {
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
"
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
            content: r"pub fn add(a: i32, b: i32) -> i32 {
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
"
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
            new_string: "pub fn fn_9999() -> i32 { 9999 }\npub fn fn_last() -> i32 { 9999 }\n"
                .into(),
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
