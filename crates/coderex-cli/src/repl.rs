//! REPL loop — interactive conversation with the QueryEngine.

use anyhow::Result;
use coderex_commands::registry::{CommandContext, CommandRegistry, CommandResult};
use coderex_core::types::ModelId;
use coderex_core::engine::{QueryEngine, SubmitOptions};
use coderex_core::permission_engine::{ExplainPermissionHook, PermissionEngine};
use coderex_core::prompts;
use coderex_core::query_profiles::PromptProfile;
use coderex_core::state::SessionState;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::api_client::ApiClient;
use coderex_core::types::{EngineEvent, Message};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Session-level state that persists across turns.
#[derive(Debug, Default)]
struct SessionEffects {
    /// Active prompt profiles from mode commands.
    prompt_profiles: Vec<PromptProfile>,
    /// Additional system prompt text appended by commands.
    append_system_prompt: Option<String>,
    /// Allowed tools override (None = use defaults).
    allowed_tools_override: Option<Vec<String>>,
    /// Additional user-context fragments accumulated from commands.
    extra_context: Vec<String>,
    /// Whether a title has been generated for this session.
    title_generated: bool,
}

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
    /// Session-level effects from commands.
    session_effects: SessionEffects,
    /// API client for orchestrator side-queries (title, permission explain, etc.).
    api_client: Option<Box<dyn ApiClient>>,
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
            session_effects: SessionEffects::default(),
            api_client: None,
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
        let mut permission_engine = PermissionEngine::new(checker);
        permission_engine.add_hook(Arc::new(ExplainPermissionHook::new(api_client.clone_box())));
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EngineEvent>();

        let engine = QueryEngine::new(
            api_client.clone_box(),
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
            session_effects: SessionEffects::default(),
            api_client: Some(api_client),
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

            self.handle_input(line, &mut stdout).await?;
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
            self.apply_command_effects(&result);
            Ok(result.output)
        } else if self.engine.is_some() {
            // 14.1: Route to QueryEngine instead of placeholder
            Ok(self.submit_prompt(prompt).await)
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

    /// Build SubmitOptions incorporating active command effects.
    fn build_submit_options(&self) -> SubmitOptions {
        let mut options = SubmitOptions::default();

        if !self.session_effects.prompt_profiles.is_empty() {
            options.custom_system_prompt = Some(
                prompts::compose_system_prompt_for_profiles(&self.session_effects.prompt_profiles),
            );
        }

        if let Some(ref prompt) = self.session_effects.append_system_prompt {
            options.append_system_prompt = Some(prompt.clone());
        }

        if let Some(ref tools) = self.session_effects.allowed_tools_override {
            options.allowed_tools_override = Some(tools.clone());
        } else if let Some(default_tools) =
            prompts::default_allowed_tools_for_profiles(&self.session_effects.prompt_profiles)
        {
            options.allowed_tools_override = Some(default_tools);
        }

        if !self.session_effects.extra_context.is_empty() {
            options.replay_user_messages = Some(
                self.session_effects
                    .extra_context
                    .iter()
                    .cloned()
                    .map(|ctx| Message::user(format!("Additional context: {ctx}")))
                    .collect(),
            );
        }

        options
    }

    /// Apply effects from a CommandResult to session state.
    fn apply_command_effects(&mut self, result: &CommandResult) {
        if let Some(profile) = result.prompt_profile {
            if !self.session_effects.prompt_profiles.contains(&profile) {
                self.session_effects.prompt_profiles.push(profile);
            }
        }

        if let Some(ref prompt) = result.append_system_prompt {
            if let Some(ref existing) = self.session_effects.append_system_prompt {
                self.session_effects.append_system_prompt =
                    Some(format!("{existing}\n{prompt}"));
            } else {
                self.session_effects.append_system_prompt = Some(prompt.clone());
            }
        }

        if let Some(ref ctx) = result.extra_context {
            self.session_effects.extra_context.push(ctx.clone());
        }

        if let Some(ref tools) = result.allowed_tools_override {
            self.session_effects.allowed_tools_override = Some(tools.clone());
        }
    }

    async fn submit_prompt(&mut self, prompt: &str) -> String {
        let mut output = String::new();
        self.drain_events(&mut output);

        let options = self.build_submit_options();
        let mut engine = self.engine.take().unwrap();
        let result = engine.submit_streaming(prompt.to_string(), options).await;
        self.engine = Some(engine);

        self.drain_events(&mut output);

        if !self.session_effects.title_generated {
            if let Some(ref client) = self.api_client {
                if let Ok(title) = prompts::generate_session_title(client.as_ref(), prompt).await {
                    self.session_effects.title_generated = true;
                    if !output.is_empty() {
                        output.insert_str(0, &format!("[Session title: {title}]\n"));
                    } else {
                        output = format!("[Session title: {title}]");
                    }
                }
            }
        }

        match result {
            Ok(reason) => {
                if output.is_empty() {
                    format!("Query completed ({reason:?})")
                } else {
                    output
                }
            }
            Err(e) => format!("Query failed: {e}"),
        }
    }

    async fn handle_input(&mut self, line: &str, stdout: &mut impl Write) -> std::io::Result<()> {
        if line.starts_with('/') {
            self.execute_command(line, stdout).await?;
        } else if line.starts_with('#') {
            // Comment line, ignore
        } else {
            // 14.1: Route to QueryEngine instead of placeholder
            if self.engine.is_some() {
                self.history.push(line.to_string());
                let output = self.submit_prompt(line).await;
                writeln!(stdout, "{output}")?;
            } else {
                self.history.push(line.to_string());
                writeln!(stdout, "[Processing: {line}]")?;
            }
        }
        Ok(())
    }

    async fn execute_command(&mut self, line: &str, stdout: &mut impl Write) -> std::io::Result<()> {
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
        self.apply_command_effects(&result);
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
    async fn headless_help() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/help").await.unwrap();
        assert!(result.contains("System Status") || result.contains("Available"));
    }

    #[test]
    fn session_effects_default() {
        let effects = SessionEffects::default();
        assert!(effects.prompt_profiles.is_empty());
        assert!(effects.append_system_prompt.is_none());
        assert!(effects.allowed_tools_override.is_none());
        assert!(effects.extra_context.is_empty());
        assert!(!effects.title_generated);
    }

    #[test]
    fn apply_command_effects_from_profile() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = CommandResult::success("advisor mode")
            .with_prompt_profile(coderex_core::query_profiles::PromptProfile::Advisor);
        loop_obj.apply_command_effects(&result);
        assert_eq!(loop_obj.session_effects.prompt_profiles.len(), 1);
        assert_eq!(loop_obj.session_effects.prompt_profiles[0], coderex_core::query_profiles::PromptProfile::Advisor);
    }

    #[test]
    fn build_submit_options_includes_effects() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        loop_obj.session_effects.prompt_profiles = vec![
            coderex_core::query_profiles::PromptProfile::Advisor,
            coderex_core::query_profiles::PromptProfile::Reviewer,
        ];
        let options = loop_obj.build_submit_options();
        assert!(options.custom_system_prompt.is_some());
        let prompt = options.custom_system_prompt.unwrap();
        assert!(prompt.contains("senior technical advisor"));
        assert!(prompt.contains("code review specialist"));
        assert_eq!(options.allowed_tools_override.unwrap(), vec!["Read", "Grep"]);
    }
}
