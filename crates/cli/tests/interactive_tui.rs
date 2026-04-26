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

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                while let Some(next) = chars.next() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

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
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
    reader_thread: Option<thread::JoinHandle<()>>,
}

impl PtySession {
    fn launch(cwd: &Path, extra_envs: &[(String, String)], model: &str) -> Self {
        Self::launch_with_args(cwd, extra_envs, model, false, &[])
    }

    fn launch_with_persistence(
        cwd: &Path,
        extra_envs: &[(String, String)],
        model: &str,
        persist_session: bool,
    ) -> Self {
        Self::launch_with_args(cwd, extra_envs, model, persist_session, &[])
    }

    fn launch_with_args(
        cwd: &Path,
        extra_envs: &[(String, String)],
        model: &str,
        persist_session: bool,
        args: &[&str],
    ) -> Self {
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
        if !persist_session {
            cmd.arg("--no-session-persistence");
        }
        cmd.arg("--model");
        cmd.arg(model);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        cmd.env("CODEREX_LOG", "off");
        for (key, value) in extra_envs {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn coderex");
        let master = pair.master;
        let mut reader = master.try_clone_reader().expect("clone reader");
        let writer = master.take_writer().expect("take writer");
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
            master,
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

    fn esc(&mut self) {
        self.send_bytes(&[0x1b]);
    }

    fn tab(&mut self) {
        self.send_bytes(b"\t");
    }

    fn backspace(&mut self) {
        self.send_bytes(&[0x7f]);
    }

    fn up(&mut self) {
        self.send_bytes(b"\x1b[A");
    }

    fn down(&mut self) {
        self.send_bytes(b"\x1b[B");
    }

    fn page_up(&mut self) {
        self.send_bytes(b"\x1b[5~");
    }

    fn page_down(&mut self) {
        self.send_bytes(b"\x1b[6~");
    }

    fn left(&mut self) {
        self.send_bytes(b"\x1b[D");
    }

    fn right(&mut self) {
        self.send_bytes(b"\x1b[C");
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write bytes to pty");
        self.writer.flush().expect("flush bytes to pty");
    }

    fn wait_for_text(&self, needle: &str, timeout: Duration) -> String {
        self.wait_for_any_text(&[needle], timeout)
    }

    fn wait_for_raw_text(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let raw = strip_ansi(&self.raw_text());
            if raw.contains(needle) {
                return raw;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for raw text {:?}\n--- screen ---\n{}\n--- raw ---\n{}",
                    needle,
                    self.screen_text(),
                    raw
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
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

fn session_with_static_server_args(cwd: &Path, args: &[&str]) -> (StaticServer, PtySession) {
    let server = StaticServer::start();
    let session = PtySession::launch_with_args(cwd, &server.envs(), "K2.6", false, args);
    (server, session)
}

fn session_with_static_server_persistent(cwd: &Path) -> (StaticServer, PtySession) {
    let server = StaticServer::start();
    let session = PtySession::launch_with_persistence(cwd, &server.envs(), "K2.6", true);
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

fn setup_git_project_dir() -> tempfile::TempDir {
    let dir = setup_project_dir();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"sequence-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");

    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "sequence@example.invalid"][..],
        &["config", "user.name", "Sequence Test"][..],
        &["add", "Cargo.toml", "src/lib.rs"][..],
        &["commit", "-q", "-m", "initial fixture"][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("run git setup command");
        assert!(status.success(), "git setup command failed: {args:?}");
    }

    dir
}

fn approve_permission(session: &mut PtySession, raw_needle: &str) {
    session.wait_for_raw_text(raw_needle, SCREEN_TIMEOUT);
    session.send_text("a");
}

fn approve_permission_for_command(session: &mut PtySession, screen_needle: &str) {
    session.wait_for_text("Permission Request", SCREEN_TIMEOUT);
    session.wait_for_text(screen_needle, SCREEN_TIMEOUT);
    session.send_text("a");
}

fn deny_permission(session: &mut PtySession, raw_needle: &str) {
    session.wait_for_raw_text(raw_needle, SCREEN_TIMEOUT);
    session.send_text("d");
}

fn deny_permission_for_tool(session: &mut PtySession, tool_needle: &str) {
    session.wait_for_text("Permission Request", SCREEN_TIMEOUT);
    session.wait_for_text(tool_needle, SCREEN_TIMEOUT);
    session.send_text("d");
}

struct InteractionStep<'a> {
    input: &'a str,
    expect_any: &'a [&'a str],
    wait_for_completion: bool,
}

impl<'a> InteractionStep<'a> {
    fn prompt(input: &'a str, expect_any: &'a [&'a str]) -> Self {
        Self {
            input,
            expect_any,
            wait_for_completion: true,
        }
    }

    fn slash(input: &'a str, expect_any: &'a [&'a str]) -> Self {
        Self {
            input,
            expect_any,
            wait_for_completion: false,
        }
    }
}

fn run_interaction_sequence(
    session: &mut PtySession,
    steps: &[InteractionStep<'_>],
    forbidden_raw: &[&str],
) {
    for step in steps {
        assert!(
            !step.expect_any.contains(&"Completed in"),
            "`Completed in` is only a lifecycle check; each step must assert a semantic result"
        );
        session.send_line(step.input);
        session.wait_for_any_text(step.expect_any, SCREEN_TIMEOUT);
        if step.wait_for_completion {
            session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        }
    }

    let raw = strip_ansi(&session.raw_text());
    for forbidden in forbidden_raw {
        assert!(
            !raw.contains(forbidden),
            "interactive sequence emitted forbidden raw text {forbidden:?}"
        );
    }
    assert!(raw.contains("❯"));
}

fn assert_raw_contains_any(session: &PtySession, needles: &[&str], context: &str) {
    let raw = strip_ansi(&session.raw_text());
    assert!(
        needles.iter().any(|needle| raw.contains(needle)),
        "expected raw output for {context} to contain one of {needles:?}\n--- raw ---\n{raw}"
    );
}

fn assert_raw_absent(session: &PtySession, needles: &[&str]) {
    let raw = strip_ansi(&session.raw_text());
    for needle in needles {
        assert!(
            !raw.contains(needle),
            "raw output unexpectedly contained {needle:?}\n--- raw ---\n{raw}"
        );
    }
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
    assert!(!session.raw_text().contains("[DEBUG] args.print"));
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
fn tokens_command_renders_in_tui() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/tokens");
    let screen = session.wait_for_text("Token Usage", SCREEN_TIMEOUT);
    assert!(screen.contains("Token Usage"));
}

#[test]
fn model_command_updates_interactive_state() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/model haiku");
    let screen = session.wait_for_any_text(
        &["Model switched to:", "claude-haiku-4-5-20251001"],
        SCREEN_TIMEOUT,
    );
    assert!(screen.contains("haiku") || screen.contains("claude-haiku-4-5-20251001"));
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
fn unknown_command_keeps_repl_usable() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/does-not-exist");
    let screen = session.wait_for_text("Unknown command", SCREEN_TIMEOUT);
    assert!(screen.contains("Unknown command"));
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
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
    assert!(screen.contains("Planning next step"));
}

#[test]
fn question_response_stays_in_transcript_without_raw_elicitation_prompt() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("ask a question");
    let screen = session.wait_for_text("Which approach would you prefer?", SCREEN_TIMEOUT);
    let raw = strip_ansi(&session.raw_text());
    assert!(screen.contains("Which approach would you prefer?"));
    assert!(!screen.contains("[Query]"));
    assert!(!screen.contains("Type your answer and press Enter."));
    assert!(!screen.contains("Continue — Proceed with the current approach"));
    assert!(!raw.contains("[Query]"));
    assert!(!raw.contains("Enter a number or type a custom response"));
}

#[test]
fn long_response_keeps_tail_visible_in_tui() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("long response");
    let screen = session.wait_for_text("TAIL: final sentence visible.", SCREEN_TIMEOUT);
    assert!(screen.contains("TAIL: final sentence visible."));
}

#[test]
fn long_response_does_not_hide_following_turn() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("very long response");
    session.wait_for_text("VERY_LONG_TAIL", SCREEN_TIMEOUT);
    session.send_line("long response");
    let screen = session.wait_for_text("TAIL: final sentence visible.", SCREEN_TIMEOUT);
    assert!(screen.contains("TAIL: final sentence visible."));
}

#[test]
fn message_area_scrolls_while_footer_stays_fixed() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("very long response");
    session.wait_for_text("VERY_LONG_TAIL", SCREEN_TIMEOUT);

    for _ in 0..12 {
        session.page_up();
    }
    let screen = session.wait_for_text("very long response line 0", SCREEN_TIMEOUT);
    assert!(screen.contains("❯"));
    assert!(screen.contains("Permissions:"));
    assert!(!screen.contains("VERY_LONG_TAIL"));

    for _ in 0..12 {
        session.page_down();
    }
    let screen = session.wait_for_text("VERY_LONG_TAIL", SCREEN_TIMEOUT);
    assert!(screen.contains("❯"));
    assert!(screen.contains("Permissions:"));
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
    let raw = session.wait_for_raw_text("Waitingforpermission:Writingsrc/lib.rs", SCREEN_TIMEOUT);
    assert!(
        raw.contains("Waitingforpermission:Writingsrc/lib.rs")
            || raw.contains("Waiting for permission: Writing src/lib.rs")
    );
    assert!(raw.contains("Tool:Write"));
    assert!(raw.contains("[a]Allowonce") || raw.contains("[a] Allow once"));
    session.send_text("d");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
}

#[test]
fn write_action_denial_keeps_session_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("add a recursive fibonacci function to src/lib.rs with unit tests");
    session.wait_for_raw_text("Waitingforpermission:Writingsrc/lib.rs", SCREEN_TIMEOUT);
    session.send_text("d");
    session.wait_for_raw_text("Error:tool'Write'denied", SCREEN_TIMEOUT);
    let screen = session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert!(screen.contains("Completed in"));
    assert!(session.raw_text().contains("❯"));
}

#[test]
fn write_action_approval_updates_file_and_keeps_session_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("add a recursive fibonacci function to src/lib.rs with unit tests");
    session.wait_for_raw_text("Waitingforpermission:Writingsrc/lib.rs", SCREEN_TIMEOUT);
    session.send_text("a");
    let screen = session.wait_for_any_text(
        &[
            "Done! I've added the recursive fibonacci function.",
            "Completed in",
        ],
        SCREEN_TIMEOUT,
    );
    assert!(
        screen.contains("Done! I've added the recursive fibonacci function.")
            || screen.contains("Completed in")
    );
    let updated =
        std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read updated file");
    assert!(updated.contains("pub fn fibonacci"));
    assert!(session.raw_text().contains("❯"));
}

#[test]
fn bash_action_denial_keeps_session_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("list files in this directory");
    let raw = session.wait_for_raw_text(
        "Waitingforpermission:Runningfind.-name'*.rs'-typef",
        SCREEN_TIMEOUT,
    );
    assert!(
        raw.contains("Waitingforpermission:Runningfind.-name'*.rs'-typef")
            || raw.contains("Waiting for permission: Running find . -name '*.rs' -type f")
    );
    session.send_text("d");
    session.wait_for_raw_text("Error:tool'Bash'denied", SCREEN_TIMEOUT);
    let screen = session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert!(screen.contains("Completed in"));
    assert!(session.raw_text().contains("❯"));
}

#[test]
fn slash_palette_filters_selects_and_dismisses() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_text("/tok");
    let screen = session.wait_for_text("Commands [/tok]", SCREEN_TIMEOUT);
    assert!(screen.contains("/tokens - Show token usage"));

    session.down();
    session.tab();
    let raw = session.wait_for_raw_text("/tokens ", SCREEN_TIMEOUT);
    assert!(raw.contains("/tokens "));

    session.ctrl_c();
    session.send_text("/mod");
    let screen = session.wait_for_text("Commands [/mod]", SCREEN_TIMEOUT);
    assert!(screen.contains("/model - Switch AI model"));
    session.esc();
    session.wait_for_text_absent("Commands [/mod]", SCREEN_TIMEOUT);
}

#[test]
fn history_recall_restores_previous_input() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/status");
    session.wait_for_text("System Status", SCREEN_TIMEOUT);
    session.up();
    let raw = session.wait_for_raw_text("/status", SCREEN_TIMEOUT);
    assert!(raw.contains("/status"));
}

#[test]
fn cursor_left_and_backspace_edit_input_before_submit() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_text("/hlp");
    session.left();
    session.left();
    session.right();
    session.left();
    session.send_text("e");
    session.enter();
    let screen = session.wait_for_text("Available Commands", SCREEN_TIMEOUT);
    assert!(screen.contains("Available Commands"));

    session.send_text("/helpp");
    session.backspace();
    session.enter();
    let screen = session.wait_for_text("Available Commands", SCREEN_TIMEOUT);
    assert!(screen.contains("Available Commands"));
}

#[test]
fn open_command_shows_split_pane_file_viewer() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/open src/lib.rs");
    let screen =
        session.wait_for_any_text(&["File Viewer", "src/lib.rs", "pub fn add"], SCREEN_TIMEOUT);
    assert!(screen.contains("src/lib.rs") || screen.contains("pub fn add"));
}

#[test]
fn plan_and_activity_overlays_render_details() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("hello");
    session.wait_for_text("Hello!", SCREEN_TIMEOUT);

    session.send_line("/plan");
    let screen = session.wait_for_any_text(&["Current Plan", "Planning next step"], SCREEN_TIMEOUT);
    assert!(screen.contains("Current Plan") || screen.contains("Planning next step"));
    session.esc();

    session.send_line("/activity");
    let screen =
        session.wait_for_any_text(&["Recent Activity", "Planning next step"], SCREEN_TIMEOUT);
    assert!(screen.contains("Recent Activity") || screen.contains("Planning next step"));
}

#[test]
fn provider_failure_recovers_to_usable_prompt() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("simulate error");
    let screen =
        session.wait_for_any_text(&["temporarily unavailable", "Query failed"], SCREEN_TIMEOUT);
    assert!(screen.contains("temporarily unavailable") || screen.contains("Query failed"));
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn resize_keeps_tui_alive() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.resize(16, 70);
    let screen = session.wait_for_any_text(&["CodeRex v", "❯"], SCREEN_TIMEOUT);
    assert!(screen.contains("CodeRex v") || session.raw_text().contains("❯"));
}

#[test]
fn vim_command_enables_normal_mode_editing() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("/vim");
    session.wait_for_text("Vim mode: enabled", SCREEN_TIMEOUT);
    session.send_text("abc");
    session.esc();
    thread::sleep(Duration::from_millis(150));
    session.send_text("A");
    session.send_text("!");
    session.enter();
    let raw = session.wait_for_raw_text("abc!", SCREEN_TIMEOUT);
    assert!(raw.contains("abc!"));
}

#[test]
fn unicode_input_is_preserved_in_interactive_session() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("hello 你好，世界");
    session.wait_for_any_text(&["Hello!", "happy to help"], SCREEN_TIMEOUT);
    let raw = session.wait_for_raw_text("你好，世界", SCREEN_TIMEOUT);
    assert!(raw.contains("你好，世界"));
}

#[test]
fn five_turn_interaction_stays_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_line("hello");
    session.wait_for_text("Hello!", SCREEN_TIMEOUT);
    session.send_line("long response");
    session.wait_for_text("TAIL: final sentence visible.", SCREEN_TIMEOUT);
    session.send_line("ask a question");
    session.wait_for_text("Which approach would you prefer?", SCREEN_TIMEOUT);
    session.send_line("show content of file");
    session.wait_for_any_text(&["pub fn add", "a + b"], SCREEN_TIMEOUT);
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn reported_skills_tools_sequence_stays_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    run_interaction_sequence(
        &mut session,
        &[
            InteractionStep::prompt("hi", &["Hello!"]),
            InteractionStep::prompt("show your skills", &["Available skills:"]),
            InteractionStep::prompt("show your tools", &["Available tools:"]),
            InteractionStep::prompt("show your skills", &["Available skills:"]),
            InteractionStep::prompt("show your tools", &["Available tools:"]),
            InteractionStep::prompt("hi", &["Hello!"]),
        ],
        &[
            "Plugin tool 'Skill' not found",
            "provider returned no visible response",
        ],
    );
}

#[test]
fn mixed_prompt_tool_sequence_matrix_stays_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    run_interaction_sequence(
        &mut session,
        &[
            InteractionStep::prompt("hi", &["Hello!"]),
            InteractionStep::prompt("search for available tools", &["Tool search flow complete"]),
            InteractionStep::prompt("show content of file", &["pub fn add", "a + b"]),
            InteractionStep::prompt(
                "create a test task",
                &["[TaskCreate]", "Running TaskCreate"],
            ),
            InteractionStep::prompt(
                "list tracked tasks",
                &["smoke task", "[TaskList]", "Task list flow complete"],
            ),
            InteractionStep::prompt(
                "schedule a test cron",
                &["[CronCreate]", "Running CronCreate"],
            ),
            InteractionStep::prompt(
                "list scheduled cron",
                &[
                    "say hi from scheduled smoke",
                    "[CronList]",
                    "Cron list flow complete",
                ],
            ),
            InteractionStep::prompt(
                "delegate a simple agent",
                &["agent-smoke-agent", "[Agent]", "Agent flow complete"],
            ),
            InteractionStep::prompt("show your tools", &["Available tools:"]),
            InteractionStep::prompt("hi", &["Hello!"]),
        ],
        &[
            "provider returned no visible response",
            "Plugin tool 'Skill' not found",
            "MCP resource handle not available",
        ],
    );

    assert_raw_contains_any(&session, &["ToolSearch"], "tool search path");
    assert_raw_contains_any(&session, &["smoketask"], "task create/list result");
    assert_raw_contains_any(
        &session,
        &["sayhifromscheduledsmoke"],
        "cron create/list result",
    );
    assert_raw_contains_any(&session, &["agent-smoke-agent"], "agent result");
}

#[test]
fn read_search_list_sequence_matrix_stays_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_line("show content of file");
    session.wait_for_any_text(&["pub fn add", "a + b"], SCREEN_TIMEOUT);
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(&session, &["pubfnadd"], "read file content");

    session.send_line("search for add function");
    approve_permission(&mut session, r#"Args:{"command":"grep-rn'fn'src/"}"#);
    session.wait_for_any_text(
        &["src/lib.rs", "Found the matching functions"],
        SCREEN_TIMEOUT,
    );
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(&session, &["grep-rn'fn'src/", "src/lib.rs"], "grep output");

    session.send_line("list files in this directory");
    approve_permission(&mut session, r#"Args:{"command":"find.-name'*.rs'-typef"}"#);
    session.wait_for_any_text(&["src/lib.rs", "Here are the files"], SCREEN_TIMEOUT);
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["find.-name'*.rs'-typef", "src/lib.rs"],
        "list output",
    );

    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn code_write_and_git_sequence_matrix_stays_usable() {
    let dir = setup_git_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_line("add multiply function to src/lib.rs");
    approve_permission(&mut session, "Waitingforpermission:Writingsrc/lib.rs");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    let updated = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read src/lib.rs");
    assert!(updated.contains("pub fn multiply"));

    session.send_line("run project tests");
    approve_permission_for_command(&mut session, "Running cargo test --quiet");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["testresult:ok", "Projecttestspassed"],
        "project test output",
    );

    session.send_line("show git status");
    approve_permission_for_command(&mut session, "Running git status --short");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["Msrc/lib.rs", "Gitstatusdisplayed"],
        "git status output",
    );

    session.send_line("show git diff");
    approve_permission_for_command(&mut session, "Running git diff -- src/lib.rs");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(&session, &["multiply", "+pubfnmultiply"], "git diff output");

    session.send_line("stage src/lib.rs");
    approve_permission_for_command(&mut session, "Running git add src/lib.rs");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["Msrc/lib.rs", "Stagedsrc/lib.rs"],
        "git add output",
    );

    session.send_line("commit staged changes");
    approve_permission_for_command(&mut session, "Running git commit -m");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["test:addmultiply", "Committedstagedchanges"],
        "git commit output",
    );

    session.send_line("show latest git commit");
    approve_permission_for_command(&mut session, "Running git log --oneline -1");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["test:addmultiply", "Latestgitcommitdisplayed"],
        "git log output",
    );

    session.send_line("/activity");
    let screen = session.wait_for_any_text(
        &["Recent Activity", "Running git log", "Running git commit"],
        SCREEN_TIMEOUT,
    );
    assert!(
        screen.contains("Recent Activity")
            || screen.contains("Running git log")
            || screen.contains("Running git commit")
    );
}

#[test]
fn dangerous_git_deny_sequence_matrix_stays_usable() {
    let dir = setup_git_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_line("run git reset --hard");
    deny_permission(&mut session, r#"Args:{"command":"gitreset--hard"}"#);
    session.wait_for_raw_text("Error:tool'Bash'denied", SCREEN_TIMEOUT);
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);

    session.send_line("show git status");
    approve_permission_for_command(&mut session, "Running git status --short");
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    assert_raw_contains_any(
        &session,
        &["Msrc/lib.rs", "Gitstatusdisplayed"],
        "post-denial git status output",
    );

    session.send_line("hi");
    session.wait_for_text("Hello!", SCREEN_TIMEOUT);
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);

    let raw = strip_ansi(&session.raw_text());
    assert!(!raw.contains("provider returned no visible response"));
    assert!(!raw.contains("Plugin tool 'Skill' not found"));
}

#[test]
fn permission_mode_sequence_matrix_enforces_expected_policy() {
    // Default: read-only tools run without prompting.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--permissions", "default"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("show content of file");
        session.wait_for_any_text(&["pub fn add", "a + b"], SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        assert_raw_absent(&session, &["PermissionRequest", "Waitingforpermission"]);
    }

    // Default: dangerous file edits ask and can be denied without side effects.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--permissions", "default"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("add a recursive fibonacci function to src/lib.rs with unit tests");
        deny_permission(&mut session, "Waitingforpermission:Writingsrc/lib.rs");
        session.wait_for_raw_text("Error:tool'Write'denied", SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap_or_default();
        assert!(!content.contains("fibonacci"));
    }

    // Accept-edits: file edits auto-run, but shell commands still ask.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--permissions", "accept-edits"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("add multiply function to src/lib.rs");
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap_or_default();
        assert!(content.contains("pub fn multiply"));
        assert_raw_absent(&session, &["Waitingforpermission:Writingsrc/lib.rs"]);

        session.send_line("list files in this directory");
        deny_permission_for_tool(&mut session, "Tool: Bash");
        session.wait_for_raw_text("Error:tool'Bash'denied", SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
    }

    // Plan: reads are allowed, mutations are denied without prompting.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--permissions", "plan"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("show content of file");
        session.wait_for_any_text(&["pub fn add", "a + b"], SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);

        session.send_line("add multiply function to src/lib.rs");
        session.wait_for_raw_text("Error:tool'Write'denied", SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap_or_default();
        assert!(!content.contains("pub fn multiply"));
        assert_raw_absent(&session, &["PermissionRequest"]);
    }

    // Dont-ask: dangerous operations are denied without prompting.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--permissions", "dont-ask"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("add multiply function to src/lib.rs");
        session.wait_for_raw_text("Error:tool'Write'denied", SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap_or_default();
        assert!(!content.contains("pub fn multiply"));
        assert_raw_absent(&session, &["PermissionRequest"]);
    }

    // Bypass-permissions: dangerous file edits and shell commands auto-run.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--permissions", "bypass-permissions"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("add multiply function to src/lib.rs");
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap_or_default();
        assert!(content.contains("pub fn multiply"));

        session.send_line("list files in this directory");
        session.wait_for_any_text(&["src/lib.rs", "Here are the files"], SCREEN_TIMEOUT);
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        assert_raw_absent(&session, &["PermissionRequest", "Waitingforpermission"]);
    }

    // --dangerously-skip-permissions is an alias for full bypass.
    {
        let dir = setup_project_dir();
        let (_server, mut session) =
            session_with_static_server_args(dir.path(), &["--dangerously-skip-permissions"]);
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("add multiply function to src/lib.rs");
        session.wait_for_text("Completed in", SCREEN_TIMEOUT);
        let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap_or_default();
        assert!(content.contains("pub fn multiply"));
        assert_raw_absent(&session, &["PermissionRequest", "Waitingforpermission"]);
    }
}

#[test]
fn extension_slash_sequence_matrix_stays_usable() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    run_interaction_sequence(
        &mut session,
        &[
            InteractionStep::slash("/tasks", &["Tasks:", "Task files"]),
            InteractionStep::slash("/mcp", &["MCP:"]),
            InteractionStep::slash("/plugin", &["plugins", "Installed plugins"]),
            InteractionStep::slash("/skills", &["Skills:", "skills loaded"]),
            InteractionStep::slash("/hooks", &["Active hooks", "No hooks"]),
            InteractionStep::slash("/memory", &["Memory:"]),
            InteractionStep::slash("/status", &["System Status"]),
        ],
        &[
            "provider returned no visible response",
            "Plugin tool 'Skill' not found",
        ],
    );
}

#[test]
fn ui_recovery_sequence_matrix_stays_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_line("/theme light");
    session.wait_for_text("Theme changed to: light", SCREEN_TIMEOUT);
    session.send_line("/theme dark");
    session.wait_for_text("Theme changed to: dark", SCREEN_TIMEOUT);

    session.send_line("very long response");
    session.wait_for_text("VERY_LONG_TAIL", SCREEN_TIMEOUT);
    for _ in 0..4 {
        session.page_up();
    }
    session.wait_for_text("very long response line", SCREEN_TIMEOUT);
    for _ in 0..4 {
        session.page_down();
    }
    session.wait_for_text("VERY_LONG_TAIL", SCREEN_TIMEOUT);

    session.send_line("simulate error");
    session.wait_for_any_text(&["temporarily unavailable", "Query failed"], SCREEN_TIMEOUT);
    session.send_line("/status");
    session.wait_for_text("System Status", SCREEN_TIMEOUT);
    session.send_line("hi");
    session.wait_for_text("Hello!", SCREEN_TIMEOUT);
    session.wait_for_text("Completed in", SCREEN_TIMEOUT);
}

#[test]
fn theme_command_switches_interactive_theme_and_stays_usable() {
    let dir = setup_project_dir();
    let mut session = session_without_engine(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);

    session.send_line("/theme light");
    session.wait_for_text("Theme changed to: light", SCREEN_TIMEOUT);
    session.send_line("/theme dark");
    session.wait_for_text("Theme changed to: dark", SCREEN_TIMEOUT);
    session.send_line("/theme tokyo-night");
    session.wait_for_text("Theme changed to: tokyo-night", SCREEN_TIMEOUT);
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn compact_after_history_keeps_session_usable() {
    let dir = setup_project_dir();
    let (_server, mut session) = session_with_static_server(dir.path());
    session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
    session.send_line("hello");
    session.wait_for_any_text(&["Hello!", "happy to help"], SCREEN_TIMEOUT);
    session.send_line("long response");
    session.wait_for_text("TAIL: final sentence visible.", SCREEN_TIMEOUT);
    session.send_line("/compact");
    session.wait_for_any_text(&["Context compacted", "already compact"], SCREEN_TIMEOUT);
    session.send_line("/status");
    let screen = session.wait_for_text("System Status", SCREEN_TIMEOUT);
    assert!(screen.contains("System Status"));
}

#[test]
fn resume_restores_persisted_plan_and_activity() {
    let dir = setup_project_dir();

    {
        let (_server, mut session) = session_with_static_server_persistent(dir.path());
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("show content of file");
        session.wait_for_any_text(&["pub fn add", "a + b"], SCREEN_TIMEOUT);
        session.ctrl_d();
        session.wait_for_exit(SCREEN_TIMEOUT);
    }

    {
        let (_server, mut session) = session_with_static_server_persistent(dir.path());
        session.wait_for_text("CodeRex v", SCREEN_TIMEOUT);
        session.send_line("/resume");
        let resumed = session.wait_for_text("Resumed session:", SCREEN_TIMEOUT);
        assert!(resumed.contains("Loaded"));

        session.send_line("/activity");
        let activity = session.wait_for_any_text(
            &["Planning next step", "Reading src/lib.rs", "Reading files"],
            SCREEN_TIMEOUT,
        );
        assert!(
            activity.contains("Planning next step")
                || activity.contains("Reading src/lib.rs")
                || activity.contains("Reading files")
        );

        session.send_line("/plan");
        let plan = session.wait_for_any_text(
            &[
                "Current step:",
                "Thinking through response",
                "Reading src/lib.rs",
            ],
            SCREEN_TIMEOUT,
        );
        assert!(
            plan.contains("Current step:")
                || plan.contains("Thinking through response")
                || plan.contains("Reading src/lib.rs")
        );
    }
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
    session.wait_for_any_text(
        &["Reading src/lib.rs...", "pub fn add", "a + b"],
        LIVE_TIMEOUT,
    );
    session.wait_for_text("Completed in", LIVE_TIMEOUT);
    assert_raw_contains_any(&session, &["pubfnadd", "a+b"], "live Kimi read tool output");
    assert_raw_absent(
        &session,
        &["provider returned no visible response", "Query failed"],
    );
}

#[test]
#[ignore = "requires live Kimi environment"]
fn interactive_kimi_directory_listing_smoke() {
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
    session.send_line("show current dir");
    let screen = session.wait_for_any_text(&["coderex", "/tmp", "tmp"], LIVE_TIMEOUT);
    assert!(screen.contains("coderex") || screen.contains("/tmp") || screen.contains("tmp"));

    session.send_line("list the files in this dir");
    let screen = session.wait_for_any_text(&["Cargo.toml", "src", ".coderex"], LIVE_TIMEOUT);
    assert!(screen.contains("Cargo.toml") || screen.contains("src") || screen.contains(".coderex"));
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
    session.wait_for_any_text(
        &["Reading src/lib.rs...", "pub fn add", "a + b"],
        LIVE_TIMEOUT,
    );
    session.wait_for_text("Completed in", LIVE_TIMEOUT);
    assert_raw_contains_any(&session, &["pubfnadd", "a+b"], "live Qwen read tool output");
    assert_raw_absent(
        &session,
        &["provider returned no visible response", "Query failed"],
    );
}
