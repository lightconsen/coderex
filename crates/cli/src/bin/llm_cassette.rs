//! Record and replay an LLM endpoint for the end-to-end test suites.
//!
//! `--mode record` proxies each request to a real Anthropic-compatible
//! endpoint and stores the response under `<dir>/<key>.json`. `--mode replay`
//! serves those recordings. Requests are keyed by a canonicalized body:
//! system prompts, tool-result text, images, thinking blocks and volatile
//! values (temp paths, timestamps, UUIDs) are removed, so a replayed run
//! matches even though its temp directory and tool outputs differ.
//!
//! Usage:
//!   llm-cassette --mode replay --dir tests/cassettes/e2e_equivalence --port 0
//!   llm-cassette --mode record --dir ... --upstream https://… --api-key … --port 0
//!
//! The bound address is printed on stderr as `llm-cassette listening on …`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordedResponse {
    status: u16,
    content_type: String,
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CassetteFile {
    /// The canonical key, repeated for a readable filename/record.
    key: String,
    /// One or more responses seen for this key (first is replayed).
    responses: Vec<RecordedResponse>,
    /// The (canonicalized) request, for debugging what a recording answers.
    request: serde_json::Value,
    /// How and when this recording was produced. Older recordings predate the
    /// field, so it defaults to empty and `scripts/record-cassettes.sh
    /// --backfill` can fill it in.
    #[serde(default)]
    metadata: CassetteMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CassetteMetadata {
    /// UTC recording time, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded_at: Option<String>,
    /// The cassette suite (`e2e_code_agent`, `live`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suite: Option<String>,
    /// The test that issued the request, when the harness reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test: Option<String>,
    /// Upstream host only; credentials and paths are deliberately absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint_host: Option<String>,
    /// The model sent upstream while recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// True when the values came from an older recording, not from the
    /// recorder itself. `endpoint_host` and `test` may be absent in that case.
    #[serde(default, skip_serializing_if = "is_false")]
    backfilled: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Record,
    Replay,
}

struct Args {
    mode: Mode,
    dir: PathBuf,
    port: u16,
    upstream: String,
    api_key: String,
    model: String,
    suite: String,
    exit_on_stdin_eof: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut mode = Mode::Replay;
        let mut dir = None;
        let mut port = 0;
        let mut upstream = String::new();
        let mut api_key = String::new();
        let mut model = String::new();
        let mut suite = String::new();
        let mut exit_on_stdin_eof = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--mode" => {
                    mode = match args.next().ok_or("--mode needs a value")?.as_str() {
                        "record" => Mode::Record,
                        "replay" => Mode::Replay,
                        other => return Err(format!("unknown mode {other}")),
                    }
                }
                "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir needs a value")?)),
                "--port" => {
                    port = args
                        .next()
                        .ok_or("--port needs a value")?
                        .parse()
                        .map_err(|_| "--port must be a number".to_string())?;
                }
                "--upstream" => {
                    upstream = args.next().ok_or("--upstream needs a value")?;
                }
                "--api-key" => api_key = args.next().ok_or("--api-key needs a value")?,
                "--model" => model = args.next().ok_or("--model needs a value")?,
                "--suite" => suite = args.next().ok_or("--suite needs a value")?,
                "--exit-on-stdin-eof" => exit_on_stdin_eof = true,
                "-h" | "--help" => {
                    println!(
                        "Usage: llm-cassette --mode record|replay --dir DIR [--port N]\n\
                         \x20      [--upstream URL --api-key KEY]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        let dir = dir.ok_or("--dir is required")?;
        if mode == Mode::Record {
            if upstream.trim().is_empty() {
                return Err("--upstream is required in record mode".into());
            }
            if api_key.trim().is_empty() {
                return Err("--api-key is required in record mode".into());
            }
        }
        Ok(Self {
            mode,
            dir,
            port,
            upstream,
            api_key,
            model,
            suite,
            exit_on_stdin_eof,
        })
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("llm-cassette: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args = Args::parse()?;
    if args.exit_on_stdin_eof {
        // The test harness keeps our stdin pipe open for the life of its
        // process; EOF means the harness is gone and this helper should not
        // outlive it.
        tokio::spawn(async {
            let mut stdin = tokio::io::stdin();
            let mut buffer = [0_u8; 64];
            while let Ok(read) = stdin.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
            }
            std::process::exit(0);
        });
    }
    std::fs::create_dir_all(&args.dir).map_err(|error| error.to_string())?;
    let cassettes = Arc::new(Mutex::new(load_cassettes(&args.dir)));
    let upstream = Arc::new(args.upstream.trim_end_matches('/').to_string());
    let api_key = Arc::new(args.api_key);

    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .await
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    eprintln!("llm-cassette listening on {address}");
    eprintln!(
        "llm-cassette: {} mode, cassettes in {}",
        match args.mode {
            Mode::Record => "record",
            Mode::Replay => "replay",
        },
        args.dir.display()
    );

    loop {
        let (socket, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let cassettes = Arc::clone(&cassettes);
        let upstream = Arc::clone(&upstream);
        let api_key = Arc::clone(&api_key);
        let dir = args.dir.clone();
        let mode = args.mode;
        let model = args.model.clone();
        let suite = args.suite.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(
                socket, mode, &dir, &cassettes, &upstream, &api_key, &model, &suite,
            )
            .await
            {
                eprintln!("llm-cassette: connection error: {error}");
            }
        });
    }
}

fn load_cassettes(dir: &Path) -> BTreeMap<String, CassetteFile> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(cassette) = serde_json::from_str::<CassetteFile>(&content) {
            out.insert(cassette.key.clone(), cassette);
        }
    }
    out
}

async fn handle(
    mut socket: TcpStream,
    mode: Mode,
    dir: &Path,
    cassettes: &Mutex<BTreeMap<String, CassetteFile>>,
    upstream: &str,
    api_key: &str,
    model: &str,
    suite: &str,
) -> Result<(), String> {
    let Some((method, path, headers, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };

    let json = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
    let canonical = canonicalize(&json);
    let key = fingerprint(&path, &canonical);
    let cwd = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-cassette-cwd"))
        .map(|(_, value)| value.as_str());
    let test = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-cassette-test"))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty());
    trace(&format!(
        "{mode:?} {method} {path} key={key} test={test:?} cwd={cwd:?}"
    ));

    let recorded = if mode == Mode::Replay {
        let guard = cassettes.lock().await;
        guard
            .get(&key)
            .and_then(|cassette| cassette.responses.first().cloned())
    } else {
        None
    };

    if let Some(recorded) = recorded {
        write_response(&mut socket, &rebind_temp_paths(&recorded, cwd)).await?;
        return Ok(());
    }

    if mode == Mode::Replay {
        let message = format!(
            "llm-cassette: no recording for {method} {path} (key {key}). \
             Run the suites with CODEREX_CASSETTE_MODE=record against a real endpoint first."
        );
        eprintln!("{message}");
        // Leave the unmatched request behind for inspection when tracing is
        // explicitly enabled. A normal replay run must not litter the tree
        // with `_missing_*.json`; optional follow-up requests are common.
        if std::env::var_os("CODEREX_CASSETTE_TRACE").is_some() {
            let missing = dir.join(format!("_missing_{key}.json"));
            let _ = std::fs::write(
                missing,
                serde_json::to_string_pretty(&canonical).unwrap_or_default(),
            );
        }
        let body = serde_json::json!({
            "type": "error",
            "error": {"type": "cassette_missing", "message": message},
        })
        .to_string();
        write_response(
            &mut socket,
            &RecordedResponse {
                status: 404,
                content_type: "application/json".into(),
                body,
            },
        )
        .await?;
        return Ok(());
    }

    // Record: forward to the real endpoint, stream the answer back while
    // collecting it for the cassette.
    let url = format!("{upstream}{path}");
    let client = reqwest::Client::new();
    let mut request = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::POST),
        &url,
    );
    for (name, value) in &headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "content-length"
                | "connection"
                | "accept-encoding"
                | "transfer-encoding"
                | "x-api-key"
                | "authorization"
                | "x-cassette-cwd"
        ) {
            continue;
        }
        request = request.header(name.as_str(), value.as_str());
    }
    // Preserve the auth scheme the client used, but with the real key.
    let auth_scheme = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.split_whitespace().next())
        .map_or_else(|| "Bearer".to_string(), str::to_string);
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
    {
        request = request.header("x-api-key", api_key);
    }
    request = request.header("authorization", format!("{auth_scheme} {api_key}"));
    request = request.header("content-type", "application/json");
    // The recording run uses one real model even when a test switches the
    // session model; the canonical key ignores the model as well.
    let forwarded_body = if model.is_empty() || json.get("model").is_none() {
        body
    } else {
        let mut value = json.clone();
        value["model"] = serde_json::Value::String(model.to_string());
        serde_json::to_vec(&value).unwrap_or(body)
    };
    request = request.body(forwarded_body);

    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n",
        reason(status)
    );
    let mut client_gone = socket.write_all(head.as_bytes()).await.is_err();

    let mut body_bytes = Vec::new();
    let mut response = response;
    while let Ok(Some(chunk)) = response.chunk().await {
        body_bytes.extend_from_slice(&chunk);
        if !client_gone && socket.write_all(&chunk).await.is_err() {
            client_gone = true;
        }
    }
    let _ = socket.shutdown().await;
    drop(response);

    let recorded = RecordedResponse {
        status,
        content_type,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    };
    let mut guard = cassettes.lock().await;
    let metadata = recording_metadata(suite, test, upstream, model);
    let entry = guard.entry(key.clone()).or_insert_with(|| CassetteFile {
        key: key.clone(),
        responses: Vec::new(),
        request: canonical,
        metadata: metadata.clone(),
    });
    let filled_metadata = entry.metadata.recorded_at.is_none();
    if filled_metadata {
        entry.metadata = metadata;
    }
    if !entry
        .responses
        .iter()
        .any(|existing| existing.body == recorded.body && existing.status == recorded.status)
    {
        entry.responses.push(recorded.clone());
        persist(dir, entry)?;
    } else if filled_metadata {
        persist(dir, entry)?;
    }
    drop(guard);
    let _ = client_gone;
    Ok(())
}

fn recording_metadata(
    suite: &str,
    test: Option<&str>,
    upstream: &str,
    model: &str,
) -> CassetteMetadata {
    CassetteMetadata {
        recorded_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        suite: (!suite.trim().is_empty()).then(|| suite.trim().to_string()),
        test: test.map(str::to_string),
        endpoint_host: reqwest::Url::parse(upstream)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string)),
        model: (!model.trim().is_empty()).then(|| model.trim().to_string()),
        backfilled: false,
    }
}

fn persist(dir: &Path, cassette: &CassetteFile) -> Result<(), String> {
    let path = dir.join(format!("{}.json", cassette.key));
    let json = serde_json::to_string_pretty(cassette).map_err(|error| error.to_string())?;
    std::fs::write(path, json).map_err(|error| error.to_string())
}

async fn read_request(
    socket: &mut TcpStream,
) -> Result<Option<(String, String, Vec<(String, String)>, Vec<u8>)>, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = socket
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 32 * 1024 * 1024 {
            return Err("request headers too large".into());
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("POST").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name.to_string(), value.to_string()));
    }

    while buffer.len() < header_end + content_length {
        let read = socket
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = buffer[header_end..(header_end + content_length).min(buffer.len())].to_vec();
    Ok(Some((method, path, headers, body)))
}

/// Point absolute temporary paths in a recorded answer at this run's fixture.
///
/// A recorded tool call can carry the recording run's working directory, most
/// commonly a Bash command such as `cd /var/folders/…/.tmpXXXX && cargo test`.
/// That directory is gone by the time the cassette is replayed, so the command
/// would fail and the recorded follow-up turns would no longer make sense. The
/// test harness sends the child's working directory in `X-Cassette-Cwd`; every
/// recorded temp directory in the answer is rewritten to it.
/// Append a line to the file named by `CODEREX_CASSETTE_TRACE`, when set.
///
/// The test harness captures the server's stderr, so this is the way to see
/// what a suite actually asked for while debugging a replay mismatch.
fn trace(line: &str) {
    use std::io::Write as _;
    let Ok(path) = std::env::var("CODEREX_CASSETTE_TRACE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn rebind_temp_paths(recorded: &RecordedResponse, cwd: Option<&str>) -> RecordedResponse {
    let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return recorded.clone();
    };
    // SSE splits a tool input's JSON across `input_json_delta` fragments, and
    // the split can land in the middle of a path; only the assembled input is
    // searchable, so the stream is decoded first.
    let body = if recorded
        .content_type
        .to_ascii_lowercase()
        .contains("event-stream")
    {
        rebind_sse(&recorded.body, cwd)
    } else {
        serde_json::from_str::<serde_json::Value>(&recorded.body).map_or_else(
            |_| recorded.body.clone(),
            |mut value| {
                rewrite_strings(&mut value, cwd);
                serde_json::to_string(&value).unwrap_or_else(|_| recorded.body.clone())
            },
        )
    };
    if body == recorded.body {
        return recorded.clone();
    }
    RecordedResponse {
        body,
        ..recorded.clone()
    }
}

/// Rebinding state for one streamed tool call.
struct ToolBuffer {
    partial: String,
    saw_delta: bool,
}

#[derive(Default)]
struct SseEvent {
    name: Option<String>,
    data: Option<String>,
}

fn parse_sse(body: &str) -> Vec<SseEvent> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut event = SseEvent::default();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event.name = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    let value = rest.strip_prefix(' ').unwrap_or(rest);
                    match &mut event.data {
                        Some(existing) => {
                            existing.push('\n');
                            existing.push_str(value);
                        }
                        None => event.data = Some(value.to_string()),
                    }
                }
            }
            (event.data.is_some() || event.name.is_some()).then_some(event)
        })
        .collect()
}

fn push_sse(out: &mut String, name: Option<&str>, json: &serde_json::Value) {
    push_sse_raw(out, name, &serde_json::to_string(json).unwrap_or_default());
}

fn push_sse_raw(out: &mut String, name: Option<&str>, data: &str) {
    if let Some(name) = name {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    if !data.is_empty() {
        out.push_str("data: ");
        out.push_str(data);
        out.push('\n');
    }
    out.push('\n');
}

fn rebind_sse(body: &str, cwd: &str) -> String {
    use std::collections::BTreeMap;

    let mut out = String::with_capacity(body.len());
    let mut tools: BTreeMap<u64, ToolBuffer> = BTreeMap::new();
    for event in parse_sse(body) {
        let Some(data) = event.data.as_deref() else {
            push_sse_raw(&mut out, event.name.as_deref(), "");
            continue;
        };
        let Ok(mut json) = serde_json::from_str::<serde_json::Value>(data) else {
            push_sse_raw(&mut out, event.name.as_deref(), data);
            continue;
        };
        let kind = json
            .get("type")
            .and_then(|kind| kind.as_str())
            .unwrap_or_default()
            .to_string();
        let index = json.get("index").and_then(serde_json::Value::as_u64);
        match (kind.as_str(), index) {
            ("content_block_start", Some(index))
                if json.pointer("/content_block/type").and_then(|v| v.as_str())
                    == Some("tool_use") =>
            {
                if let Some(input) = json.pointer_mut("/content_block/input") {
                    rewrite_strings(input, cwd);
                }
                push_sse(&mut out, event.name.as_deref(), &json);
                tools.insert(
                    index,
                    ToolBuffer {
                        partial: String::new(),
                        saw_delta: false,
                    },
                );
            }
            ("content_block_delta", Some(index))
                if json.pointer("/delta/type").and_then(|v| v.as_str())
                    == Some("input_json_delta") =>
            {
                if let Some(buffer) = tools.get_mut(&index) {
                    if let Some(partial) = json
                        .pointer("/delta/partial_json")
                        .and_then(|value| value.as_str())
                    {
                        buffer.partial.push_str(partial);
                    }
                    buffer.saw_delta = true;
                } else {
                    push_sse(&mut out, event.name.as_deref(), &json);
                }
            }
            ("content_block_stop", Some(index)) => {
                if let Some(buffer) = tools.remove(&index) {
                    // Re-emit the tool input as one delta, with paths rebound.
                    let partial = buffer.partial.clone();
                    let full = serde_json::from_str::<serde_json::Value>(&partial).map_or_else(
                        |_| partial.clone(),
                        |mut input| {
                            rewrite_strings(&mut input, cwd);
                            serde_json::to_string(&input).unwrap_or_else(|_| partial.clone())
                        },
                    );
                    if buffer.saw_delta {
                        let delta = serde_json::json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "input_json_delta", "partial_json": full},
                        });
                        push_sse(&mut out, Some("content_block_delta"), &delta);
                    }
                }
                push_sse(&mut out, event.name.as_deref(), &json);
            }
            _ => push_sse(&mut out, event.name.as_deref(), &json),
        }
    }
    out
}

/// Replace every recorded test temp directory in the strings of `value`.
fn rewrite_strings(value: &mut serde_json::Value, cwd: &str) {
    match value {
        serde_json::Value::String(text) => {
            let roots = temp_roots(text);
            if !roots.is_empty() {
                trace(&format!("rebind roots={roots:?} cwd={cwd:?}"));
            }
            for root in roots {
                if root != cwd && text.contains(&root) {
                    *text = text.replace(&root, cwd);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_strings(item, cwd);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                rewrite_strings(item, cwd);
            }
        }
        _ => {}
    }
}

/// Every distinct test temp directory mentioned in `text`, in order.
fn temp_roots(text: &str) -> Vec<String> {
    const PREFIXES: [&str; 4] = [
        "/private/var/folders/",
        "/private/tmp/",
        "/var/folders/",
        "/tmp/",
    ];
    let mut out: Vec<String> = Vec::new();
    for prefix in PREFIXES {
        let mut search = 0;
        while let Some(relative) = text[search..].find(prefix) {
            let index = search + relative;
            // `/private/tmp/` also contains `/tmp/`; do not report the inner
            // match as a separate directory.
            if !prefix.starts_with("/private") && text[..index].ends_with("/private") {
                search = index + prefix.len();
                continue;
            }
            let tail = &text[index..];
            let end = tail
                .find(|character: char| {
                    character.is_whitespace() || "\"'`,)]}\\:;".contains(character)
                })
                .unwrap_or(tail.len());
            if let Some(root) = root_of(&tail[..end])
                && !out.contains(&root)
            {
                out.push(root);
            }
            search = index + prefix.len();
        }
    }
    out
}

/// The temp directory a path belongs to, e.g. `/tmp/.tmpAbC12` for
/// `/tmp/.tmpAbC12/src/lib.rs`. Paths that are not test temp directories
/// yield `None`.
fn root_of(path: &str) -> Option<String> {
    let components: Vec<&str> = path.split('/').collect();
    let marker = components
        .iter()
        .position(|component| component.starts_with(".tmp") && component.len() > 4)?;
    Some(components[..=marker].join("/"))
}

async fn write_response(socket: &mut TcpStream, recorded: &RecordedResponse) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        recorded.status,
        reason(recorded.status),
        recorded.content_type,
        recorded.body.len()
    );
    socket
        .write_all(head.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    socket
        .write_all(recorded.body.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let _ = socket.shutdown().await;
    Ok(())
}

const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Response",
    }
}

/// FNV-1a over the path and canonical JSON.
fn fingerprint(path: &str, canonical: &serde_json::Value) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.as_bytes().iter().chain(b"\0").chain(
        serde_json::to_string(canonical)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Everything that can differ between two runs of the same test.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        // The system prompt embeds the temporary working directory and the
        // date; the conversation below is what identifies the request.
        if object.contains_key("system") {
            object.insert(
                "system".into(),
                serde_json::Value::String("<system>".into()),
            );
        }
        for key in ["metadata", "request_context", "request_id"] {
            object.remove(key);
        }
        // Tests switch models, but the cassette records one real model; the
        // model is not part of what the test is asserting.
        if object.contains_key("model") {
            object.insert("model".into(), serde_json::Value::String("<model>".into()));
        }
        // Tool registration goes through a hash map, so the array order is
        // not stable between processes; the set is.
        if let Some(tools) = object.get_mut("tools").and_then(|v| v.as_array_mut()) {
            tools.sort_by(|left, right| {
                let name = |value: &serde_json::Value| {
                    value
                        .get("name")
                        .and_then(|name| name.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                name(left).cmp(&name(right))
            });
        }
    }
    normalize_value(&value, None)
}

fn normalize_value(value: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(normalize_text(text, key)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize_message_item).collect())
        }
        serde_json::Value::Object(object) => {
            let mut out = serde_json::Map::new();
            for (name, item) in object {
                if name == "thinking" || name == "signature" || name == "cache_control" {
                    continue;
                }
                out.insert(name.clone(), normalize_value(item, Some(name)));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

fn normalize_message_item(item: &serde_json::Value) -> serde_json::Value {
    let Some(object) = item.as_object() else {
        return normalize_value(item, None);
    };
    let kind = object.get("type").and_then(|value| value.as_str());
    match kind {
        Some("tool_result") => {
            let mut out = serde_json::Map::new();
            out.insert(
                "type".into(),
                serde_json::Value::String("tool_result".into()),
            );
            if let Some(id) = object.get("tool_use_id") {
                out.insert("tool_use_id".into(), id.clone());
            }
            // Tool output is environment-dependent (paths, timestamps, hashes)
            // and is dropped from the key on purpose.
            out.insert(
                "content".into(),
                serde_json::Value::String("<tool-result>".into()),
            );
            serde_json::Value::Object(out)
        }
        Some("image" | "image_url") => serde_json::json!({"type": "image"}),
        Some("thinking" | "redacted_thinking") => serde_json::json!({"type": "thinking"}),
        _ => normalize_value(item, None),
    }
}

fn normalize_text(text: &str, key: Option<&str>) -> String {
    // Paths: every test runs in a fresh temp directory.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('/') {
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        let is_temp = tail.starts_with("/tmp/")
            || tail.starts_with("/private/tmp/")
            || tail.starts_with("/var/folders/")
            || tail.starts_with("/private/var/folders/")
            || tail.starts_with("/private/var/")
            || tail.starts_with("/var/");
        if is_temp {
            let end = tail
                .find(|character: char| character.is_whitespace() || "\"'`,)]}".contains(character))
                .unwrap_or(tail.len());
            out.push_str("<tmp>");
            rest = &tail[end..];
        } else {
            out.push('/');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);

    // Timestamps and UUIDs change between runs.
    let mut out = replace_pattern(&out, is_timestamp, "<time>");
    out = replace_pattern(&out, is_uuid, "<uuid>");
    if key == Some("input") && out.len() > 2000 {
        out.truncate(2000);
    }
    out
}

fn replace_pattern(text: &str, predicate: fn(&[u8]) -> Option<usize>, replacement: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(length) = predicate(&bytes[index..]) {
            out.push_str(replacement);
            index += length;
        } else {
            let character = text[index..].chars().next().unwrap_or(' ');
            out.push(character);
            index += character.len_utf8();
        }
    }
    out
}

fn is_timestamp(bytes: &[u8]) -> Option<usize> {
    // YYYY-MM-DDTHH:MM:SS
    if bytes.len() >= 19
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b':'
    {
        return Some(19);
    }
    None
}

fn is_uuid(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 36 {
        return None;
    }
    for (index, byte) in bytes[..36].iter().enumerate() {
        let expected_dash = matches!(index, 8 | 13 | 18 | 23);
        if expected_dash {
            if *byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(36)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_roots_find_test_directories_only() {
        let text = "cd /private/var/folders/41/xx/T/.tmpAbC && cargo test";
        assert_eq!(
            temp_roots(text),
            vec!["/private/var/folders/41/xx/T/.tmpAbC"]
        );
        assert!(temp_roots("/usr/local/bin/cargo").is_empty());
    }

    #[test]
    fn sse_tool_input_is_rebound_after_reassembly() {
        // The path is split across two `input_json_delta` fragments, which is
        // exactly why the raw body cannot be searched.
        let body = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"Bash","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cd /tmp/.tmpAb"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"C && ls\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

"#;
        let recorded = RecordedResponse {
            status: 200,
            content_type: "text/event-stream".into(),
            body: body.into(),
        };
        let rebound = rebind_temp_paths(&recorded, Some("/tmp/.tmpNew"));
        assert!(
            rebound.body.contains("/tmp/.tmpNew && ls"),
            "{}",
            rebound.body
        );
        assert!(!rebound.body.contains(".tmpAbC"));
    }

    #[test]
    fn an_old_cassette_without_metadata_still_loads() {
        let cassette: CassetteFile =
            serde_json::from_str(r#"{"key":"abc","responses":[],"request":{"model":"<model>"}}"#)
                .expect("old cassette");
        assert!(cassette.metadata.recorded_at.is_none());
        assert!(!cassette.metadata.backfilled);
    }

    #[test]
    fn recording_metadata_keeps_only_the_upstream_host() {
        let metadata = recording_metadata(
            "live",
            Some("basic_chat_responds"),
            "https://user:secret@example.test:8443/v1?token=hidden",
            "test-model",
        );
        assert_eq!(metadata.endpoint_host.as_deref(), Some("example.test"));
        assert_eq!(metadata.model.as_deref(), Some("test-model"));
        assert_eq!(metadata.test.as_deref(), Some("basic_chat_responds"));
        let serialized = serde_json::to_string(&metadata).expect("serialize");
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("hidden"));
    }
}
