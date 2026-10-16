//! Interactive PTY E2E runner for `coderex`.

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child as ProcessChild, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// JSON scenario consumed by the interactive E2E runner.
#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    /// Working directory used for the spawned `coderex` process.
    pub cwd: PathBuf,
    /// Model passed to `coderex --model`.
    #[serde(default = "default_model")]
    pub model: String,
    /// Terminal row count.
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Terminal column count.
    #[serde(default = "default_cols")]
    pub cols: u16,
    /// Whether session persistence should stay enabled.
    #[serde(default)]
    pub persist_session: bool,
    /// Extra environment variables for the spawned session.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Extra CLI args passed through to `coderex`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional embedded test server.
    #[serde(default)]
    pub server: Option<ServerConfig>,
    /// Ordered interaction sequence.
    pub steps: Vec<Step>,
}

/// Optional local helper server configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerConfig {
    /// Launch the static Anthropic-compatible fixture server.
    Static {
        /// Path to the JSON response map.
        responses_file: PathBuf,
        /// Model exposed to `coderex`.
        model: Option<String>,
    },
}

/// A single scenario step.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Step {
    SendText {
        text: String,
    },
    SendLine {
        text: String,
    },
    Enter,
    CtrlC,
    CtrlD,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    PageUp,
    PageDown,
    Left,
    Right,
    Resize {
        rows: u16,
        cols: u16,
    },
    SleepMs {
        ms: u64,
    },
    WaitForText {
        text: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    WaitForAnyText {
        texts: Vec<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    WaitForRawText {
        text: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    WaitForTextAbsent {
        text: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    AssertRegionContains {
        region: Region,
        text: String,
    },
    AssertRegionNotContains {
        region: Region,
        text: String,
    },
    AssertRegionClean {
        region: Region,
    },
    AssertCoreRegionsClean,
    DumpRegions,
}

/// A logical screen region to validate.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    MessageArea,
    Statusline,
    InputLine,
    PermissionBar,
    FullScreen,
    RawOutput,
}

/// Execute an interactive E2E scenario from a JSON file.
pub fn run_scenario_file(path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read scenario file {}", path.display()))?;
    let scenario: Scenario = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse scenario file {}", path.display()))?;
    run_scenario(&scenario)
}

/// Execute an interactive E2E scenario.
pub fn run_scenario(scenario: &Scenario) -> Result<()> {
    let mut extra_envs = scenario.env.clone();
    let mut server = None;
    let model = if let Some(server_cfg) = &scenario.server {
        match server_cfg {
            ServerConfig::Static {
                responses_file,
                model,
            } => {
                let static_server = StaticServer::start(responses_file, model.as_deref())?;
                for (key, value) in static_server.envs(model.as_deref()) {
                    extra_envs.insert(key, value);
                }
                let selected = model.clone().unwrap_or_else(|| scenario.model.clone());
                server = Some(static_server);
                selected
            }
        }
    } else {
        scenario.model.clone()
    };

    let env_pairs = extra_envs.into_iter().collect::<Vec<_>>();
    let mut session = PtySession::launch(
        &scenario.cwd,
        &env_pairs,
        &model,
        scenario.persist_session,
        scenario.rows,
        scenario.cols,
        &scenario.args,
    )?;

    for (index, step) in scenario.steps.iter().enumerate() {
        println!("[step {}/{}] {:?}", index + 1, scenario.steps.len(), step);
        execute_step(&mut session, step)?;
    }

    drop(server);
    Ok(())
}

fn execute_step(session: &mut PtySession, step: &Step) -> Result<()> {
    match step {
        Step::SendText { text } => session.send_text(text)?,
        Step::SendLine { text } => session.send_line(text)?,
        Step::Enter => session.enter()?,
        Step::CtrlC => session.ctrl_c()?,
        Step::CtrlD => session.ctrl_d()?,
        Step::Esc => session.esc()?,
        Step::Tab => session.tab()?,
        Step::Backspace => session.backspace()?,
        Step::Up => session.up()?,
        Step::Down => session.down()?,
        Step::PageUp => session.page_up()?,
        Step::PageDown => session.page_down()?,
        Step::Left => session.left()?,
        Step::Right => session.right()?,
        Step::Resize { rows, cols } => session.resize(*rows, *cols)?,
        Step::SleepMs { ms } => thread::sleep(Duration::from_millis(*ms)),
        Step::WaitForText { text, timeout_ms } => {
            let _ = session.wait_for_text(text, Duration::from_millis(*timeout_ms))?;
        }
        Step::WaitForAnyText { texts, timeout_ms } => {
            let needles = texts.iter().map(String::as_str).collect::<Vec<_>>();
            let _ = session.wait_for_any_text(&needles, Duration::from_millis(*timeout_ms))?;
        }
        Step::WaitForRawText { text, timeout_ms } => {
            let _ = session.wait_for_raw_text(text, Duration::from_millis(*timeout_ms))?;
        }
        Step::WaitForTextAbsent { text, timeout_ms } => {
            session.wait_for_text_absent(text, Duration::from_millis(*timeout_ms))?;
        }
        Step::AssertRegionContains { region, text } => {
            let snapshot = session.region_text(*region);
            if !snapshot.contains(text) {
                bail!(
                    "region {:?} does not contain {:?}\n--- region ---\n{}\n--- screen ---\n{}",
                    region,
                    text,
                    snapshot,
                    session.screen_text()
                );
            }
        }
        Step::AssertRegionNotContains { region, text } => {
            let snapshot = session.region_text(*region);
            if snapshot.contains(text) {
                bail!(
                    "region {:?} unexpectedly contains {:?}\n--- region ---\n{}\n--- screen ---\n{}",
                    region,
                    text,
                    snapshot,
                    session.screen_text()
                );
            }
        }
        Step::AssertRegionClean { region } => {
            assert_region_clean(session, *region)?;
        }
        Step::AssertCoreRegionsClean => {
            for region in [
                Region::MessageArea,
                Region::Statusline,
                Region::InputLine,
                Region::PermissionBar,
                Region::RawOutput,
            ] {
                assert_region_clean(session, region)?;
            }
        }
        Step::DumpRegions => {
            println!(
                "--- message_area ---\n{}",
                session.region_text(Region::MessageArea)
            );
            println!(
                "--- statusline ---\n{}",
                session.region_text(Region::Statusline)
            );
            println!(
                "--- input_line ---\n{}",
                session.region_text(Region::InputLine)
            );
            println!(
                "--- permission_bar ---\n{}",
                session.region_text(Region::PermissionBar)
            );
        }
    }
    Ok(())
}

fn assert_region_clean(session: &PtySession, region: Region) -> Result<()> {
    let snapshot = session.region_text(region);
    if snapshot.contains('\u{fffd}') {
        bail!(
            "region {:?} contains replacement characters\n--- region ---\n{}",
            region,
            snapshot
        );
    }
    if let Some(ch) = snapshot
        .chars()
        .find(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        bail!(
            "region {:?} contains control character {:?}\n--- region ---\n{}",
            region,
            ch,
            snapshot
        );
    }
    Ok(())
}

fn default_rows() -> u16 {
    40
}

fn default_cols() -> u16 {
    120
}

fn default_timeout_ms() -> u64 {
    15_000
}

fn default_model() -> String {
    "claude-sonnet-4-6".to_string()
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for next in chars.by_ref() {
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

/// Drop every whitespace character, so a needle can be compared against a
/// rendering whose spaces are not all there.
///
/// Ratatui repaints only the cells that changed, and the cells it skips reach
/// the byte stream as cursor moves rather than as spaces. `strip_ansi` then
/// removes those moves, so whether a given space survives into the raw stream
/// depends on the previous frame, not on what was displayed — a needle written
/// without spaces matched or missed on the luck of the diff. Squeezing both
/// sides makes the wait say what it means: these words were written to the
/// terminal at some point.
fn squeeze_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn sibling_binary(name: &str) -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to locate current executable")?;
    let dir = current
        .parent()
        .context("current executable has no parent directory")?;
    let mut candidate = dir.join(name);
    if cfg!(windows) {
        candidate.set_extension("exe");
    }
    if candidate.exists() {
        Ok(candidate)
    } else {
        bail!("unable to locate sibling binary {}", candidate.display())
    }
}

fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind free port")?;
    Ok(listener
        .local_addr()
        .context("failed to read bound port")?
        .port())
}

fn wait_for_server(port: u16) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("static test server did not start on port {port}")
}

struct StaticServer {
    child: ProcessChild,
    port: u16,
}

impl StaticServer {
    fn start(responses_file: &Path, model: Option<&str>) -> Result<Self> {
        let port = find_free_port()?;
        let binary = sibling_binary("static-anthropic-server")?;
        let child = Command::new(binary)
            .args([
                "--port",
                &port.to_string(),
                "--responses",
                responses_file
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("responses file path is not UTF-8"))?,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn static-anthropic-server")?;
        wait_for_server(port)?;
        let _ = model;
        Ok(Self { child, port })
    }

    fn envs(&self, model: Option<&str>) -> Vec<(String, String)> {
        vec![
            ("ANTHROPIC_API_KEY".into(), "mock".into()),
            (
                "ANTHROPIC_BASE_URL".into(),
                format!("http://127.0.0.1:{}", self.port),
            ),
            (
                "ANTHROPIC_MODEL".into(),
                model.unwrap_or("K2.6").to_string(),
            ),
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
    /// The config home this session was given, held so it outlives it.
    ///
    /// Otherwise the CLI reads the developer's own settings — and the first-run
    /// consent gate *writes* them — so a test would inherit whatever permission
    /// mode the person running it happens to have configured.
    config_home: PathBuf,
}

impl PtySession {
    #[allow(clippy::too_many_arguments)]
    fn launch(
        cwd: &Path,
        extra_envs: &[(String, String)],
        model: &str,
        persist_session: bool,
        rows: u16,
        cols: u16,
        args: &[String],
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let coderex = sibling_binary("coderex")?;
        let mut cmd = CommandBuilder::new(
            coderex
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("coderex binary path is not UTF-8"))?,
        );
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
        // A config home of its own, seeded with a decision already recorded:
        // an untouched one makes the CLI ask for telemetry consent on stdin
        // before it draws anything, and there is no one to answer.
        let config_home = unique_config_home()?;
        std::fs::write(
            config_home.join("settings.json"),
            r#"{"config_version": 3, "telemetry_enabled": false}"#,
        )
        .context("seed config home")?;
        cmd.env("CLAUDE_CONFIG_DIR", &config_home);
        for (key, value) in extra_envs {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn coderex in PTY")?;
        let master = pair.master;
        let mut reader = master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = master.take_writer().context("failed to take PTY writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows,
            cols,
            usize::from(rows),
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

        Ok(Self {
            child,
            master,
            writer,
            parser,
            raw,
            reader_thread: Some(reader_thread),
            config_home,
        })
    }

    fn screen_text(&self) -> String {
        self.parser.lock().expect("parser lock").screen().contents()
    }

    fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw.lock().expect("raw lock")).into_owned()
    }

    fn region_text(&self, region: Region) -> String {
        match region {
            Region::RawOutput => strip_ansi(&self.raw_text()),
            Region::FullScreen => self.screen_text(),
            Region::MessageArea
            | Region::Statusline
            | Region::InputLine
            | Region::PermissionBar => {
                let screen = self.screen_text();
                let regions = ScreenRegions::from_screen(&screen);
                match region {
                    Region::MessageArea => regions.message_area,
                    Region::Statusline => regions.statusline,
                    Region::InputLine => regions.input_line,
                    Region::PermissionBar => regions.permission_bar,
                    Region::FullScreen | Region::RawOutput => unreachable!(),
                }
            }
        }
    }

    fn send_text(&mut self, text: &str) -> Result<()> {
        self.writer
            .write_all(text.as_bytes())
            .context("failed to write text to PTY")?;
        self.writer.flush().context("failed to flush PTY writer")?;
        Ok(())
    }

    fn send_line(&mut self, text: &str) -> Result<()> {
        self.send_text(text)?;
        self.enter()
    }

    fn enter(&mut self) -> Result<()> {
        self.send_bytes(b"\r")
    }

    fn ctrl_c(&mut self) -> Result<()> {
        self.send_bytes(&[0x03])
    }

    fn ctrl_d(&mut self) -> Result<()> {
        self.send_bytes(&[0x04])
    }

    fn esc(&mut self) -> Result<()> {
        self.send_bytes(&[0x1b])
    }

    fn tab(&mut self) -> Result<()> {
        self.send_bytes(b"\t")
    }

    fn backspace(&mut self) -> Result<()> {
        self.send_bytes(&[0x7f])
    }

    fn up(&mut self) -> Result<()> {
        self.send_bytes(b"\x1b[A")
    }

    fn down(&mut self) -> Result<()> {
        self.send_bytes(b"\x1b[B")
    }

    fn page_up(&mut self) -> Result<()> {
        self.send_bytes(b"\x1b[5~")
    }

    fn page_down(&mut self) -> Result<()> {
        self.send_bytes(b"\x1b[6~")
    }

    fn left(&mut self) -> Result<()> {
        self.send_bytes(b"\x1b[D")
    }

    fn right(&mut self) -> Result<()> {
        self.send_bytes(b"\x1b[C")
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")
    }

    fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .context("failed to write bytes to PTY")?;
        self.writer.flush().context("failed to flush PTY bytes")?;
        Ok(())
    }

    fn wait_for_text(&self, needle: &str, timeout: Duration) -> Result<String> {
        self.wait_for_any_text(&[needle], timeout)
    }

    fn wait_for_raw_text(&self, needle: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let needle = squeeze_whitespace(needle);
        loop {
            let raw = strip_ansi(&self.raw_text());
            if squeeze_whitespace(&raw).contains(&needle) {
                return Ok(raw);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for raw text {:?}\n--- screen ---\n{}\n--- raw ---\n{}",
                    needle,
                    self.screen_text(),
                    raw
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_any_text(&self, needles: &[&str], timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen_text();
            if needles.iter().any(|needle| screen.contains(needle)) {
                return Ok(screen);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for any of {:?}\n--- screen ---\n{}\n--- raw ---\n{}",
                    needles,
                    screen,
                    self.raw_text()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_text_absent(&self, needle: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen_text();
            if !screen.contains(needle) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for absence of {:?}\n--- screen ---\n{}\n--- raw ---\n{}",
                    needle,
                    screen,
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
        // The session's own config home, not the developer's.
        let _ = std::fs::remove_dir_all(&self.config_home);
    }
}

/// A fresh directory to use as `CLAUDE_CONFIG_DIR` for one session.
///
/// Built from `std` rather than `tempfile`, which is a dev-dependency this
/// module (part of the library) cannot use.
fn unique_config_home() -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let path = std::env::temp_dir().join(format!(
        "coderex-e2e-config-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).context("create config home")?;
    Ok(path)
}

struct ScreenRegions {
    message_area: String,
    statusline: String,
    input_line: String,
    permission_bar: String,
}

impl ScreenRegions {
    fn from_screen(screen: &str) -> Self {
        let mut lines = screen.split('\n').collect::<Vec<_>>();
        if lines.last().is_some_and(|line| line.is_empty()) {
            let _ = lines.pop();
        }
        let trimmed = lines
            .into_iter()
            .map(|line| line.trim_end().to_string())
            .collect::<Vec<_>>();
        let len = trimmed.len();
        let permission_bar = trimmed
            .get(len.saturating_sub(1))
            .cloned()
            .unwrap_or_default();
        let input_line = trimmed
            .get(len.saturating_sub(2))
            .cloned()
            .unwrap_or_default();
        let statusline = trimmed
            .get(len.saturating_sub(3))
            .cloned()
            .unwrap_or_default();
        let message_cutoff = len.saturating_sub(3);
        let message_area = trimmed[..message_cutoff].join("\n");
        Self {
            message_area,
            statusline,
            input_line,
            permission_bar,
        }
    }
}
