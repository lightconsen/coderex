//! Wire-protocol test client.
//!
//! Provides [`WireTestClient`] for interacting with a CodeRex engine
//! over the wire protocol in tests — useful for subprocess-based E2E
//! tests and integration tests.

use coderex_core::wire_protocol::{WireEvent, WireRequest, decode_event, encode_request};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A test client that communicates with a CodeRex subprocess over stdio.
///
/// # Example
///
/// ```rust,ignore
/// use coderex_test_utils::WireTestClient;
///
/// # async fn example() {
/// let mut client = WireTestClient::spawn("cargo", &["run", "--bin", "coderex"])
///     .await
///     .expect("spawn failed");
///
/// client.submit("Hello, world!").await.unwrap();
///
/// let events = client.collect_events().await;
/// assert!(events.iter().any(|e| matches!(e, WireEvent::TextDelta { .. })));
/// # }
/// ```
#[allow(missing_debug_implementations)]
pub struct WireTestClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    buffer: String,
}

impl WireTestClient {
    /// Spawn a new CodeRex subprocess and connect to it over stdio.
    ///
    /// # Errors
    /// Returns an error if the subprocess cannot be spawned or if
    /// stdin/stdout pipes are not available.
    pub async fn spawn(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout"))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            buffer: String::with_capacity(4096),
        })
    }

    /// Send a submit request.
    ///
    /// # Errors
    /// Returns an I/O error if writing to stdin fails.
    pub async fn submit(&mut self, prompt: impl Into<String>) -> std::io::Result<()> {
        let req = WireRequest::Submit {
            prompt: prompt.into(),
            system_prompt: None,
            max_turns: None,
        };
        self.send_request(req).await
    }

    /// Send a raw wire request.
    ///
    /// # Errors
    /// Returns an I/O error if writing to stdin fails.
    pub async fn send_request(&mut self, req: WireRequest) -> std::io::Result<()> {
        let line = encode_request(&req);
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await
    }

    /// Read the next event from stdout.
    ///
    /// Returns `None` if the stdout pipe is closed.
    ///
    /// # Errors
    /// Returns an error if the JSON is malformed.
    pub async fn next_event(&mut self) -> std::io::Result<Option<WireEvent>> {
        self.buffer.clear();
        let bytes = self.stdout.read_line(&mut self.buffer).await?;
        if bytes == 0 {
            return Ok(None);
        }
        match decode_event(&self.buffer) {
            None => Ok(None),
            Some(Ok(event)) => Ok(Some(event)),
            Some(Err(e)) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid event JSON: {e}"),
            )),
        }
    }

    /// Collect all events until session end or timeout.
    ///
    /// Collects events for up to 30 seconds, or until `SessionEnd` is
    /// received, whichever comes first.
    ///
    /// # Errors
    /// Returns an error if reading from stdout fails.
    pub async fn collect_events(&mut self) -> std::io::Result<Vec<WireEvent>> {
        let timeout = tokio::time::Duration::from_secs(30);
        let deadline = tokio::time::Instant::now() + timeout;

        let mut events = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                self.next_event(),
            )
            .await
            {
                Ok(Ok(Some(event))) => {
                    let is_end = matches!(event, WireEvent::SessionEnd);
                    events.push(event);
                    if is_end {
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // Timeout — check if process is still alive
                    match self.child.try_wait()? {
                        Some(_) => break,
                        None => continue,
                    }
                }
            }
        }
        Ok(events)
    }

    /// Kill the subprocess.
    ///
    /// # Errors
    /// Returns an error if the subprocess cannot be killed.
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
    }

    /// Wait for the subprocess to exit.
    ///
    /// # Errors
    /// Returns an error if waiting fails.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

/// Builder for [`WireTestClient`].
#[derive(Debug, Default)]
pub struct WireTestClientBuilder {
    program: String,
    args: Vec<String>,
}

impl WireTestClientBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Add an argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add multiple arguments.
    #[must_use]
    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args.extend(args);
        self
    }

    /// Build and spawn the client.
    ///
    /// # Errors
    /// Returns an error if the subprocess cannot be spawned.
    pub async fn spawn(self) -> std::io::Result<WireTestClient> {
        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        WireTestClient::spawn(&self.program, &args).await
    }
}
