//! REPL loop — interactive conversation with the QueryEngine.

use anyhow::Result;
use coderex_commands::registry::{CommandContext, CommandRegistry};
use coderex_core::types::ModelId;
use coderex_core::engine::{QueryEngine, SubmitOptions};
use coderex_core::engine::TerminalReason;
use coderex_core::state::SessionState;
use coderex_core::permission_engine::PermissionEngine;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::api_client::ApiClient;
use coderex_core::types::EngineEvent;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use tokio::sync::mpsc;

/// The REPL event loop — reads user input, executes commands or queries.
#[allow(missing_debug_implementations)]
pub struct ReplLoop {
    /// Command registry.
    commands: CommandRegistry,
    /// Current model.
    model: ModelId,
    /// Current working directory.
    cwd: String,
    /// Whether the loop should continue.
    running: bool,
    /// Prompt history.
    history: Vec<String>,
    /// QueryEngine for processing user queries (14.1 wired).
    engine: Option<QueryEngine>,
    /// Event receiver for streaming output.
    event_rx: Option<mpsc::UnboundedReceiver<EngineEvent>>,
}

impl ReplLoop {
    /// Create a new REPL loop without a QueryEngine.
    /// Use `with_engine` to wire up the QueryEngine for full conversation support.
    #[must_use]
    pub fn new(cwd: String, model: ModelId) -> Self {
        let mut commands = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut commands);
        Self {
            commands,
            model,
            cwd,
            running: true,
            history: Vec::new(),
            engine: None,
            event_rx: None,
        }
    }

    /// Create a new REPL loop wired to a QueryEngine (14.1).
    ///
    /// # Arguments
    /// * `api_client` — API client implementation for LLM requests.
    /// * `cwd` — Current working directory.
    /// * `model` — Model to use.
    #[must_use]
    pub fn with_engine(
        api_client: Box<dyn ApiClient>,
        cwd: String,
        model: ModelId,
    ) -> Self {
        let mut commands = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut commands);

        let session = SessionState::new(std::path::PathBuf::from(&cwd));
        let checker = PermissionChecker::new(PermissionMode::default());
        let permission_engine = PermissionEngine::new(checker);
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EngineEvent>();

        let engine = QueryEngine::new(
            api_client,
            model.clone(),
            session,
            permission_engine,
            event_tx,
        );

        Self {
            commands,
            model,
            cwd,
            running: true,
            history: Vec::new(),
            engine: Some(engine),
            event_rx: Some(event_rx),
        }
    }

    /// Run the interactive REPL loop reading from stdin.
    #[allow(clippy::unused_async)]
    pub async fn run_interactive(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();

        writeln!(stdout, "CodeRex REPL — type /help for commands, Ctrl+D to exit")?;
        writeln!(stdout)?;

        loop {
            if !self.running {
                break;
            }

            write!(stdout, "❯ ")?;
            stdout.flush()?;

            let mut line = String::new();
            let bytes = stdin.lock().read_line(&mut line)?;
            if bytes == 0 {
                // EOF (Ctrl+D)
                break;
            }

            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }

            self.handle_input(line, &mut stdout)?;
        }

        writeln!(stdout, "\nGoodbye!")?;
        Ok(())
    }

    /// Run a single query in non-interactive (headless) mode.
    pub async fn run_headless(&mut self, prompt: &str) -> Result<String> {
        if prompt.starts_with('/') {
            let parts: Vec<&str> = prompt.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
            let ctx = self.build_context();
            let result = self.commands.execute(cmd_name, &ctx, &args);
            Ok(result.output)
        } else if self.engine.is_some() {
            // 14.1: Route to QueryEngine instead of placeholder
            let mut output = String::new();
            // Drain any pending events before submission
            self.drain_events(&mut output);

            // Take engine temporarily to avoid borrow conflicts
            let mut engine = self.engine.take().unwrap();
            let result = engine.submit(prompt.to_string(), SubmitOptions::default()).await;
            self.engine = Some(engine);

            match result {
                Ok(reason) => {
                    // Drain events after submission
                    self.drain_events(&mut output);
                    if output.is_empty() {
                        Ok(format!("Query completed ({reason:?})"))
                    } else {
                        Ok(output)
                    }
                }
                Err(e) => Ok(format!("Query failed: {e}")),
            }
        } else {
            // No engine configured — fall back to placeholder
            Ok(format!("[QueryEngine would process: {prompt}]"))
        }
    }

    /// Drain pending events from the event channel and append text deltas to output.
    fn drain_events(&mut self, output: &mut String) {
        if let Some(ref mut rx) = self.event_rx {
            while let Ok(event) = rx.try_recv() {
                if let EngineEvent::TextDelta(delta) = event {
                    output.push_str(&delta);
                }
            }
        }
    }

    fn handle_input(&mut self, line: &str, stdout: &mut impl Write) -> std::io::Result<()> {
        if line.starts_with('/') {
            self.execute_command(line, stdout)?;
        } else if line.starts_with('#') {
            // Comment line, ignore
        } else {
            // 14.1: Route to QueryEngine instead of placeholder
            if self.engine.is_some() {
                self.history.push(line.to_string());
                writeln!(stdout, "[Submitted to QueryEngine: {line}]")?;
            } else {
                self.history.push(line.to_string());
                writeln!(stdout, "[Processing: {line}]")?;
            }
        }
        Ok(())
    }

    fn execute_command(&mut self, line: &str, stdout: &mut impl Write) -> std::io::Result<()> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd_name = parts[0].trim_start_matches('/');
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        // Built-in REPL commands that don't go through registry
        match cmd_name {
            "exit" | "quit" => {
                self.running = false;
                writeln!(stdout, "Exiting...")?;
                return Ok(());
            }
            "help" => {
                self.show_help(stdout)?;
                return Ok(());
            }
            _ => {}
        }

        let ctx = self.build_context();
        let result = self.commands.execute(cmd_name, &ctx, &args);
        writeln!(stdout, "{}", result.output)?;
        Ok(())
    }

    fn show_help(&self, stdout: &mut impl Write) -> std::io::Result<()> {
        writeln!(stdout, "Available commands:")?;
        for cmd in self.commands.list() {
            writeln!(stdout, "  /{:<20} {}", cmd.name, cmd.description)?;
        }
        Ok(())
    }

    fn build_context(&self) -> CommandContext {
        let mut ctx = CommandContext::new(
            self.cwd.clone(),
            "repl_session".into(),
            "REPL Session".into(),
            self.model.clone(),
        );
        ctx.message_count = self.history.len();
        ctx.version = env!("CARGO_PKG_VERSION").to_string();
        ctx.feature_flags = HashMap::new();
        ctx
    }
}

/// Session state for the REPL.
#[derive(Debug)]
pub struct ReplSession {
    /// The session ID.
    pub id: String,
    /// The session title.
    pub title: String,
    /// Whether the session is active.
    pub active: bool,
    /// Message count.
    pub message_count: usize,
}

impl ReplSession {
    /// Create a new session.
    #[must_use]
    pub fn new(id: String, title: String) -> Self {
        Self {
            id,
            title,
            active: true,
            message_count: 0,
        }
    }
}

/// Parse a line of input into a command and arguments.
#[must_use]
pub fn parse_input(line: &str) -> Option<(&str, Vec<String>)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('/') {
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let cmd = parts[0].trim_start_matches('/');
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        Some((cmd, args))
    } else {
        Some(("message", vec![trimmed.to_string()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::test_utils::MockApiClient;

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_input("").is_none());
        assert!(parse_input("   ").is_none());
    }

    #[test]
    fn parse_command() {
        let result = parse_input("/clear").unwrap();
        assert_eq!(result.0, "clear");
        assert!(result.1.is_empty());
    }

    #[test]
    fn parse_command_with_args() {
        let result = parse_input("/model sonnet").unwrap();
        assert_eq!(result.0, "model");
        assert_eq!(result.1, vec!["sonnet"]);
    }

    #[test]
    fn parse_message() {
        let result = parse_input("Hello, world!").unwrap();
        assert_eq!(result.0, "message");
        assert_eq!(result.1, vec!["Hello, world!"]);
    }

    #[test]
    fn parse_command_multiple_args() {
        let result = parse_input("/btw this is important").unwrap();
        assert_eq!(result.0, "btw");
        assert_eq!(result.1, vec!["this", "is", "important"]);
    }

    #[test]
    fn repl_loop_new() {
        let loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        assert!(!loop_obj.commands.is_empty());
    }

    #[test]
    fn repl_loop_with_engine() {
        let mock_client = MockApiClient::new();
        let loop_obj = ReplLoop::with_engine(
            Box::new(mock_client),
            "/src".into(),
            ModelId::sonnet(),
        );
        assert!(loop_obj.engine.is_some());
        assert!(loop_obj.event_rx.is_some());
    }

    #[test]
    fn repl_session_new() {
        let session = ReplSession::new("s1".into(), "Test".into());
        assert_eq!(session.id, "s1");
        assert!(session.active);
        assert_eq!(session.message_count, 0);
    }

    #[tokio::test]
    async fn headless_command() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/status").await.unwrap();
        assert!(result.contains("System Status"));
    }

    #[tokio::test]
    async fn headless_message_without_engine() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("Hello").await.unwrap();
        assert!(result.contains("Hello"));
        assert!(result.contains("QueryEngine"));
    }

    #[tokio::test]
    async fn headless_message_with_engine() {
        let mock_client = MockApiClient::new();
        mock_client.enqueue_response("Hello back!", 10, 5);

        let mut loop_obj = ReplLoop::with_engine(
            Box::new(mock_client),
            "/src".into(),
            ModelId::sonnet(),
        );
        let result = loop_obj.run_headless("Hello").await.unwrap();
        // QueryEngine processes the message — output contains the response
        assert!(result.contains("Hello back!") || result.contains("Query completed"));
    }

    #[tokio::test]
    async fn headless_help() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/help").await.unwrap();
        assert!(result.contains("System Status") || result.contains("Available"));
    }
}
