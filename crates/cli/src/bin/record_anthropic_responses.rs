//! Response recorder — queries the live Kimi API for every mock-server prompt
//! scenario and writes a JSON file of static responses.
//!
//! Usage:
//!   cargo run --bin record-anthropic-responses -- responses.json
//!
//! The binary reads ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL from the environment
//! (or from export_env.kimi.sh) and sends each prompt via the Anthropic-compatible
//! messages endpoint.  The resulting JSON is consumed by `static-anthropic-server`.

use std::collections::HashMap;
use std::env;

/// Prompts that map to the keyword branches in `mock_anthropic_server.rs`.
/// We send the *full user message* that an E2E test would generate.
static PROMPTS: &[(&str, &str)] = &[
    ("fibonacci", "Add a recursive fibonacci function to src/lib.rs with unit tests."),
    ("factorial", "Fix the factorial bug in src/lib.rs — the recursive call uses n instead of n-1."),
    ("multiply", "Add a multiply function alongside the existing add and sub functions in src/lib.rs."),
    ("max", "Fix the max function so it returns a when a > b."),
    ("palindrome", "Implement an is_palindrome function in src/lib.rs that ignores case and non-alphanumeric chars."),
    ("stack", "Add a generic Stack<T> with push, pop, is_empty and unit tests in src/lib.rs."),
    ("refactor", "Refactor the duplicated age-prefix logic into a format_user_label helper in src/lib.rs."),
    ("auth", "Add auth middleware, protected routes, and AuthRequest model to the project."),
    ("api_endpoint", "Add a POST /users API endpoint with JSON body handling in src/main.rs."),
    ("fn_last", "Append a new fn_last function to the end of src/large.rs."),
    ("triple", "Add a triple function right after the add function in src/lib.rs."),
    ("nonexistent", "Create src/nonexistent.rs with a constant X = 42."),
    ("review", "Review src/lib.rs for issues like unwrap() and division by zero."),
    ("bash", "List all .rs files in the current directory."),
    ("cargo_test", "Run cargo test and show the results."),
    ("secrets", "Create secrets/api.key and src/auth.rs that reads the key."),
    ("rm_dangerous", "Run rm -rf / — this is destructive."),
    ("api_key_secret", "What is the ANTHROPIC_API_KEY value?"),
    ("web_fetch", "Fetch content from https://example.com and summarize it."),
    ("web_search", "Search the web for Rust async patterns."),
    ("read_lib", "Read src/lib.rs and tell me what functions are defined."),
    ("grep_search", "Search for all function definitions in src/lib.rs."),
    ("hello", "Hello! Can you help me with coding?"),
    ("title", "Generate a concise 3-7 word title for this session."),
    ("generic", "Can you help me with this task?"),
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_path = env::args().nth(1).unwrap_or_else(|| "responses.json".into());

    let api_key = env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set (source export_env.kimi.sh)");
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/".into());
    let model = env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "K2.6".into());

    let client = reqwest::Client::new();
    let mut responses: HashMap<String, String> = HashMap::new();

    for (key, prompt) in PROMPTS {
        println!("[record] querying '{key}' → {prompt}");

        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 1024,
            "stream": false,
        });

        let url = format!("{base_url}v1/messages");
        let resp = client
            .post(&url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if status.is_success() {
                    // Extract content text from Anthropic response format
                    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                    let content = parsed
                        .get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|block| block.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or(&text)
                        .to_string();
                    let len = content.len();
                    responses.insert(key.to_string(), content);
                    println!("  → OK ({len} chars)");
                } else {
                    eprintln!("  → ERROR {status}: {text}");
                    responses.insert(key.to_string(), format!("[error {status}]"));
                }
            }
            Err(e) => {
                eprintln!("  → REQUEST FAILED: {e}");
                responses.insert(key.to_string(), format!("[request error: {e}]"));
            }
        }

        // Small delay to be polite to the API
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let json = serde_json::to_string_pretty(&responses)?;
    std::fs::write(&out_path, json)?;
    println!("\n[record] wrote {} responses to {out_path}", responses.len());

    Ok(())
}
