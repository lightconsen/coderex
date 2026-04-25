use assert_cmd::cargo::cargo_bin;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child as ProcessChild, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ROWS: u16 = 40;
const COLS: u16 = 120;
const SCREEN_TIMEOUT: Duration = Duration::from_secs(15);
const LIVE_TIMEOUT: Duration = Duration::from_secs(90);

fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
    listener.local_addr().expect("local addr").port()
}

fn wait_for_server(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("static test server did not start on port {port}");
}

struct StaticServer {
    child: ProcessChild,
    port: u16,
}

impl StaticServer {
    fn start() -> Self {
        let port = find_free_port();
        let responses_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/responses.json");
        assert!(
            responses_path.exists(),
            "missing {}",
            responses_path.display()
        );

        let child = Command::new(cargo_bin("static-anthropic-server"))
            .args([
                "--port",
                &port.to_string(),
                "--responses",
                responses_path.to_str().expect("utf8 responses path"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn static-anthropic-server");

        wait_for_server(port);

        Self { child, port }
    }

    fn envs(&self) -> Vec<(String, String)> {
        vec![
            ("ANTHROPIC_API_KEY".into(), "mock".into()),
            (
                "ANTHROPIC_BASE_URL".into(),
                format!("http://127.0.0.1:{}", self.port),
            ),
            ("ANTHROPIC_MODEL".into(), "K2.6".into()),
        ]
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct PtySession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
    reader_thread: Option<thread::JoinHandle<()>>,
}

impl PtySession {
    fn launch(cwd: &Path, extra_envs: &[(String, String)], model: &str) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let mut cmd = CommandBuilder::new(cargo_bin("coderex"));
        cmd.arg("--no-session-persistence");
        cmd.arg("--model");
        cmd.arg(model);
        cmd.cwd(cwd);
        cmd.env("CODEREX_LOG", "off");
        for (key, value) in extra_envs {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn coderex");
        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let writer = pair.master.take_writer().expect("take writer");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            ROWS,
            COLS,
            usize::from(ROWS),
        )));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let parser_bg = Arc::clone(&parser);
        let raw_bg = Arc::clone(&raw);
        let reader_thread = thread::spawn(move || {
            let mut buf = [0_u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut parser) = parser_bg.lock() {
                            parser.process(&buf[..n]);
                        }
                        if let Ok(mut raw) = raw_bg.lock() {
                            raw.extend_from_slice(&buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            writer,
            parser,
            raw,
            reader_thread: Some(reader_thread),
        }
    }

    fn screen_text(&self) -> String {
        self.parser.lock().expect("parser lock").screen().contents()
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().expect("raw lock")).into_owned()
    }

    fn send_text(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    fn send_line(&mut self, text: &str) {
        self.send_text(text);
        self.enter();
    }

    fn enter(&mut self) {
        self.send_bytes(b"\r");
    }

    fn ctrl_c(&mut self) {
        self.send_bytes(&[0x03]);
    }

    fn ctrl_d(&mut self) {
        self.send_bytes(&[0x04]);
    }
    fn send_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write bytes to pty");
        self.writer.flush().expect("flush bytes to pty");
    }

    fn wait_for_text(&self, needle: &str, timeout: Duration) -> String {
        self.wait_for_any_text(&[needle], timeout)
    }

    fn wait_for_any_text(&self, needles: &[&str], timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen_text();
            if needles.iter().any(|needle| screen.contains(needle)) {
                return screen;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for any of {:?}\n--- screen ---\n{}\n--- raw ---\n{}",
                    needles,
                    screen,
                    self.raw_text()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_text_absent(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen_text();
            if !screen.contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for absence of {:?}\n--- screen ---\n{}\n--- raw ---\n{}",
                    needle,
                    screen,
                    self.raw_text()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(err) => panic!("failed to poll child exit: {err}"),
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for coderex to exit\n--- screen ---\n{}\n--- raw ---\n{}",
                    self.screen_text(),
                    self.raw_text()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

fn session_without_engine(cwd: &Path) -> PtySession {
    PtySession::launch(cwd, &[], "claude-sonnet-4-6")
}

fn session_with_static_server(cwd: &Path) -> (StaticServer, PtySession) {
    let server = StaticServer::start();
    let session = PtySession::launch(cwd, &server.envs(), "K2.6");
    (server, session)
}

fn setup_project_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("create src");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .expect("write src/lib.rs");
    dir
}

fn have_live_env() -> bool {
    (std::env::var("ANTHROPIC_API_KEY").is_ok() && std::env::var("ANTHROPIC_BASE_URL").is_ok())
        || (std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("OPENAI_BASE_URL").is_ok())
}

fn live_model() -> Option<String> {
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
}

fn provider_env_matches(expected: &str) -> bool {
    live_model()
        .map(|model| model.to_lowercase().contains(expected))
        .unwrap_or(false)
}

#[test]
fn startup_renders_prompt_and_default_regions() {
    let dir = setup_project_dir();
    let session = session_without_engine(dir.path());
    let screen = session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    assert!(screen.contains("Type /help for commands"));
    assert!(screen.contains("claude-sonnet-4-6"));
    assert!(screen.contains("Permissions: default"));
    assert!(session.raw_text().contains("❯"));
}

#[test]
fn help_command_renders_in_tui() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/help");
    let screen = session.wait_for_any_text(&["/clear", "Available"], SCREEN_TIMEOUT);
    assert!(screen.contains("/clear"));
}

#[test]
fn status_command_renders_in_tui() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn clear_command_clears_transcript_state() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/help");
    session.wait_for_text("/clear", SCREEN_TIMEOUT);
    session.send_line("/clear");
    session.wait_for_text("Are you sure you want to run /clear?", SCREEN_TIMEOUT);
    session.send_line("y");
    let screen = session.wait_for_text("cleared", SCREEN_TIMEOUT);
    assert!(screen.contains("cleared"));
    assert!(!screen.contains("Available commands"));
}

#[test]
fn ctrl_c_clears_current_input() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_text("temporary input");
    session.wait_for_text("temporary input", SCREEN_TIMEOUT);
    session.ctrl_c();
    session.wait_for_text_absent("temporary input", SCREEN_TIMEOUT);
    let screen = session.screen_text();
    assert!(screen.contains("❯ "));
}

#[test]
fn ctrl_d_exits_cleanly() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.ctrl_d();
    session.wait_for_exit(Duration::from_secs(5));
}

#[test]
fn single_prompt_gets_response() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("hello");
    let screen = session.wait_for_any_text(&["Hello!", "happy to help"], SCREEN_TIMEOUT);
    assert!(screen.contains("Hello!"));
}

#[test]
fn two_turn_transcript_is_preserved() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("hello");
    session.wait_for_any_text(&["Hello!", "happy to help"], SCREEN_TIMEOUT);
    session.send_line("cargo test results");
    let screen =
        session.wait_for_any_text(&["I don't have the ability", "cargo test"], SCREEN_TIMEOUT);
    assert!(screen.contains("hello"));
    assert!(screen.contains("cargo test") || session.raw_text().contains("cargo test results"));
}

#[test]
fn safe_read_action_completes_without_broken_state() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("show content of file");
    session.wait_for_any_text(&["Reading src/lib.rs...", "[Read]"], SCREEN_TIMEOUT);
    session.wait_for_any_text(&["pub fn add", "a + b"], SCREEN_TIMEOUT);
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn write_action_shows_permission_wait() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("add a recursive fibonacci function to src/lib.rs with unit tests");
    let screen = session.wait_for_text(
        "Waiting for permission: Writing src/lib.rs...",
        SCREEN_TIMEOUT,
    );
    assert!(screen.contains("Writing src/lib.rs..."));
}

#[test]
fn write_action_denial_keeps_session_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("add a recursive fibonacci function to src/lib.rs with unit tests");
    session.wait_for_text(
        "Waiting for permission: Writing src/lib.rs...",
        SCREEN_TIMEOUT,
    );
    session.ctrl_c();
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
#[ignore = "requires live Kimi environment"]
fn interactive_kimi_live_smoke() {
    if !have_live_env() || !provider_env_matches("k2") {
        return;
    }
    let dir = setup_project_dir();
    let mut session = PtySession::launch(
        dir.path(),
        &[],
        &live_model().unwrap_or_else(|| "K2.6".into()),
    );
    session.wait_for_text("CodeRex v", LIVE_TIMEOUT);
    session.send_line("Use the Read tool to show content of src/lib.rs and summarize it");
    let screen = session.wait_for_any_text(
        &["Completed in", "Reading src/lib.rs...", "src/lib.rs"],
        LIVE_TIMEOUT,
    );
    assert!(screen.contains("src/lib.rs") || screen.contains("Completed in"));
}

#[test]
#[ignore = "requires live Qwen environment"]
fn interactive_qwen_live_smoke() {
    if !have_live_env() || !provider_env_matches("qwen") {
        return;
    }
    let dir = setup_project_dir();
    let mut session = PtySession::launch(
        dir.path(),
        &[],
        &live_model().unwrap_or_else(|| "qwen3.6-plus".into()),
    );
    session.wait_for_text("CodeRex v", LIVE_TIMEOUT);
    session.send_line("Use the Read tool to show content of src/lib.rs and summarize it");
    let screen = session.wait_for_any_text(
        &["Completed in", "Reading src/lib.rs...", "src/lib.rs"],
        LIVE_TIMEOUT,
    );
    assert!(screen.contains("src/lib.rs") || screen.contains("Completed in"));
}
