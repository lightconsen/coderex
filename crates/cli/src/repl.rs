//! REPL loop — interactive conversation with the QueryEngine.

use anyhow::Result;
use coderex_commands::registry::{CommandContext, CommandRegistry, CommandResult};
use coderex_core::types::ModelId;
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::permission_engine::{ExplainPermissionHook, PermissionEngine};
use coderex_core::prompts;
use coderex_core::orchestrator::{background_feature_label, BackgroundFeatureKind, BackgroundFeatureRequest};
use coderex_core::query_profiles::{ModelProfile, PromptProfile, QueryKind, QueryRequest};
use coderex_core::state::SessionState;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::api_client::ApiClient;
use coderex_core::types::{EngineEvent, Message};
use coderex_memory::{
    ai_workflow, auto_dream::{AutoDream, DreamConfig}, DreamCheckResult,
    long_term::LongTermStore, session_memory::SessionMemoryStore,
    team_memory::TeamMemorySyncService,
};
use coderex_tools::{
    register_builtin_tools, ExecutionConfig, ExecutionContext, RegistryToolDispatcher,
    ToolExecutor as ToolsExecutor, ToolRegistry,
};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Session-level state that persists across turns.
#[derive(Debug, Default)]
struct SessionEffects {
    /// Active prompt profiles from mode commands.
    prompt_profiles: Vec<PromptProfile>,
    /// Additional system prompt text appended by commands.
    append_system_prompt: Option<String>,
    /// Full system prompt override (replaces base prompt).
    override_system_prompt: Option<String>,
    /// Allowed tools override (None = use defaults).
    allowed_tools_override: Option<Vec<String>>,
    /// Additional user-context fragments accumulated from commands.
    extra_context: Vec<String>,
    /// Whether a title has been generated for this session.
    title_generated: bool,
}

/// Result from a completed background task, delivered to the UI.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BackgroundTaskResult {
    /// The task ID.
    id: String,
    /// Which background feature ran.
    kind: BackgroundFeatureKind,
    /// Whether it succeeded.
    success: bool,
    /// Output content (result text or error message).
    content: String,
}

/// Handle for an active background task.
#[derive(Debug)]
struct BackgroundTask {
    /// Unique task ID.
    id: String,
    /// Feature kind.
    kind: BackgroundFeatureKind,
    /// Cancellation token — signal to abort the task.
    cancel_token: CancellationToken,
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
    /// Orchestrator for routing AI calls through product-level entry points.
    orchestrator: Option<coderex_core::orchestrator::Orchestrator>,
    /// Active background tasks (observable + cancelable).
    background_tasks: Vec<BackgroundTask>,
    /// Sender for background task results back to the UI thread.
    background_results_tx: Option<mpsc::UnboundedSender<BackgroundTaskResult>>,
    /// Receiver for background task results.
    background_results_rx: Option<mpsc::UnboundedReceiver<BackgroundTaskResult>>,
    /// Session memory store for persisting session facts.
    session_memory_store: Option<Arc<SessionMemoryStore>>,
    /// Long-term memory store for extracted memories.
    long_term_store: Option<Arc<LongTermStore>>,
    /// Session ID for memory operations.
    session_id: Option<uuid::Uuid>,
    /// Auto-dream manager for background session consolidation.
    auto_dream: Option<Arc<AutoDream>>,
    /// Shutdown signal for the auto-dream watcher.
    dream_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Team memory sync service for cross-repo memory sharing.
    team_memory_service: Option<Arc<TeamMemorySyncService>>,
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
            orchestrator: None,
            background_tasks: Vec::new(),
            background_results_tx: None,
            background_results_rx: None,
            session_memory_store: None,
            long_term_store: None,
            session_id: None,
            auto_dream: None,
            dream_shutdown_tx: None,
            team_memory_service: None,
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
        let session_id = session.session_id;
        let checker = PermissionChecker::new(PermissionMode::default());
        let mut permission_engine = PermissionEngine::new(checker);
        let perm_path = std::path::PathBuf::from(&cwd)
            .join(".coderex")
            .join("permissions.json");
        permission_engine.set_persist_path(perm_path);
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EngineEvent>();
        permission_engine.add_hook(Arc::new(ExplainPermissionHook::new_with_event_tx(
            api_client.clone_box(),
            event_tx.clone(),
        )));

        let memory_base = std::path::PathBuf::from(&cwd).join(".coderex").join("memory");
        let session_memory_store = Arc::new(SessionMemoryStore::new(memory_base.join("session")));
        let long_term_store = Arc::new(LongTermStore::new(memory_base.join("long_term")));

        let auto_dream = Arc::new(AutoDream::new(memory_base.clone(), DreamConfig::default()));
        let team_memory_service = Arc::new(TeamMemorySyncService::new(
            "default".to_string(),
            memory_base.join("team"),
        ));

        let engine = QueryEngine::new(
            api_client.clone_box(),
            model.clone(),
            session,
            permission_engine,
            event_tx,
        );

        let mut engine = engine;
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let registry = Arc::new(registry);
        let inner_executor = Arc::new(ToolsExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            inner_executor,
            ExecutionContext::Normal,
            std::path::PathBuf::from(&cwd),
        );
        engine.set_tool_executor(Box::new(dispatcher));
        engine.recover_orphaned_permissions();

        let orchestrator = coderex_core::orchestrator::Orchestrator::new(api_client.clone_box());
        let (bg_tx, bg_rx) = mpsc::unbounded_channel::<BackgroundTaskResult>();

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
            orchestrator: Some(orchestrator),
            background_tasks: Vec::new(),
            background_results_tx: Some(bg_tx),
            background_results_rx: Some(bg_rx),
            session_memory_store: Some(session_memory_store),
            long_term_store: Some(long_term_store),
            session_id: Some(session_id),
            auto_dream: Some(auto_dream),
            dream_shutdown_tx: None,
            team_memory_service: Some(team_memory_service),
        }
    }

    /// Run the interactive REPL loop reading from stdin.
    #[allow(clippy::unused_async)]
    pub async fn run_interactive(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();

        crate::welcome::print_welcome_screen(
            &mut stdout,
            env!("CARGO_PKG_VERSION"),
            &self.model.0,
            &self.cwd,
            self.engine.is_some(),
        )?;

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

    /// Run the interactive REPL with full ratatui TUI.
    pub async fn run_tui(&mut self) -> Result<()> {
        use crossterm::event::{Event, KeyCode, KeyModifiers};
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use coderex_ui::input::InputAction;
        use coderex_ui::message_list::{Message, MessageRole};
        use coderex_ui::overlay::Overlay;
        use coderex_ui::state::{UiState, ViewMode};
        use coderex_ui::terminal::TerminalUi;
        use coderex_ui::vim::{VimAction, VimMode, VimState};
        use std::time::Duration;

        enable_raw_mode()?;
        let mut terminal_ui = TerminalUi::new()?;
        terminal_ui.enter()?;

        let mut ui_state = UiState::new();

        // Welcome message
        ui_state.messages.push(Message {
            role: MessageRole::System,
            content: format!(
                "CodeRex v{} | model: {} | cwd: {} | Type /help for commands, Ctrl+C to exit",
                env!("CARGO_PKG_VERSION"),
                self.model.0,
                self.cwd
            ).into(),
            tool_name: None,
            streaming: false,
                            folded: false,
        });

        // Crossterm event channel — read in a blocking thread
        let (ct_tx, mut ct_rx) = tokio::sync::mpsc::unbounded_channel::<crossterm::event::Event>();
        std::thread::spawn(move || {
            loop {
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        if let Ok(event) = crossterm::event::read() {
                            if ct_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(false) => continue,
                    Err(_) => break,
                }
            }
        });

        let mut event_rx = self.event_rx.take();
        let mut vim = VimState::new_insert();
        let mut thinking = false;
        let mut query_done_rx: Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        > = None;

        let result = loop {
            if !self.running {
                break Ok(());
            }

            // Sync statusline from REPL state
            self.sync_statusline(&mut ui_state.statusline);
            ui_state.statusline.thinking = thinking;
            if thinking {
                ui_state.statusline.spinner_frame = ui_state.statusline.spinner_frame.wrapping_add(1);
                if ui_state.statusline.thinking_label.is_none() {
                    ui_state.statusline.thinking_label = Some("Thinking (Esc to cancel)".into());
                }
            } else {
                ui_state.statusline.thinking_label = None;
                ui_state.statusline.spinner_frame = 0;
            }

            // Poll for completed background tasks and route by kind
            for result in self.poll_background_results() {
                match result.kind {
                    BackgroundFeatureKind::PromptSuggestion => {
                        if result.success {
                            ui_state.input.suggestion = Some(result.content.trim().to_string());
                        }
                    }
                    BackgroundFeatureKind::SessionMemoryUpdate => {
                        self.handle_memory_update_result(&result).await;
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: if result.success {
                                "Session memory updated.".into()
                            } else {
                                format!("Session memory update failed: {}", result.content).into()
                            },
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    BackgroundFeatureKind::MemoryExtraction => {
                        self.handle_memory_extraction_result(&result).await;
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: if result.success {
                                "Long-term memories extracted.".into()
                            } else {
                                format!("Memory extraction failed: {}", result.content).into()
                            },
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    _ => {
                        let label = format!("[Background {:?}] {}", result.kind,
                            if result.success { "Done" } else { "Failed" });
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: format!("{label}\n{}", result.content).into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                }
            }

            // Draw frame
            if let Err(e) = terminal_ui.draw(&mut ui_state) {
                tracing::warn!("TUI draw error: {e}");
            }

            tokio::select! {
                Some(event) = ct_rx.recv() => {
                    match event {
                        Event::Key(key) => {
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                if thinking {
                                    thinking = false;
                                    query_done_rx = None;
                                    if let Some(last) = ui_state.messages.messages.last_mut() {
                                        if last.role == MessageRole::Assistant {
                                            last.streaming = false;
                                            let mut msg = last.content.to_string();
                                            msg.push_str(" (interrupted)");
                                            last.content = msg.into();
                                        }
                                    }
                                    continue;
                                } else {
                                    break Ok(());
                                }
                            }

                            // Ctrl+Shift+S: toggle split-pane view
                            if key.code == KeyCode::Char('S')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                use coderex_ui::state::LayoutMode;
                                ui_state.layout_mode = match ui_state.layout_mode {
                                    LayoutMode::SinglePane => LayoutMode::HorizontalSplit { left_percent: 50 },
                                    LayoutMode::HorizontalSplit { .. } => LayoutMode::SinglePane,
                                };
                                continue;
                            }

                            let action = vim.handle_key(key);
                            match action {
                                VimAction::Submit => {
                                    if let Some(text) = ui_state.input.take() {
                                        self.process_tui_input(
                                            &text,
                                            &mut ui_state,
                                            &mut thinking,
                                            &mut query_done_rx,
                                        ).await;
                                    }
                                }
                                VimAction::Escape => {
                                    if vim.mode == VimMode::Normal {
                                        ui_state.overlays.clear();
                                        ui_state.active_view = ViewMode::MainRepl;
                                    }
                                }
                                VimAction::Insert => {
                                    if vim.is_insert() {
                                        match key.code {
                                            KeyCode::Char(c) => {
                                                ui_state.input.apply(&InputAction::InsertChar(c));
                                            }
                                            KeyCode::Backspace => {
                                                ui_state.input.apply(&InputAction::Backspace);
                                            }
                                            KeyCode::Delete => {
                                                ui_state.input.apply(&InputAction::Delete);
                                            }
                                            KeyCode::Left => {
                                                ui_state.input.apply(&InputAction::CursorLeft);
                                            }
                                            KeyCode::Right => {
                                                ui_state.input.apply(&InputAction::CursorRight);
                                            }
                                            KeyCode::Up => {
                                                ui_state.input.apply(&InputAction::HistoryUp);
                                            }
                                            KeyCode::Down => {
                                                ui_state.input.apply(&InputAction::HistoryDown);
                                            }
                                            KeyCode::Home => {
                                                ui_state.input.apply(&InputAction::MoveToStart);
                                            }
                                            KeyCode::End => {
                                                ui_state.input.apply(&InputAction::MoveToEnd);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                VimAction::Left => {
                                    ui_state.input.apply(&InputAction::CursorLeft);
                                }
                                VimAction::Right => {
                                    ui_state.input.apply(&InputAction::CursorRight);
                                }
                                VimAction::Up => {
                                    ui_state.input.apply(&InputAction::HistoryUp);
                                }
                                VimAction::Down => {
                                    ui_state.input.apply(&InputAction::HistoryDown);
                                }
                                VimAction::JumpToStart => {
                                    ui_state.input.apply(&InputAction::MoveToStart);
                                }
                                VimAction::JumpToEnd => {
                                    ui_state.input.apply(&InputAction::MoveToEnd);
                                }
                                VimAction::DeleteToEnd => {
                                    ui_state.input.apply(&InputAction::KillToEnd);
                                }
                                VimAction::DeleteLine => {
                                    ui_state.input.text.clear();
                                    ui_state.input.cursor = 0;
                                }
                                VimAction::DeleteWord => {
                                    ui_state.input.apply(&InputAction::DeleteWord);
                                }
                                VimAction::DeleteChar => {
                                    ui_state.input.apply(&InputAction::Delete);
                                }
                                VimAction::YankLine => {
                                    vim.yank(&ui_state.input.text);
                                }
                                VimAction::Paste => {
                                    let yanked = vim.yanked().to_string();
                                    for c in yanked.chars() {
                                        ui_state.input.apply(&InputAction::InsertChar(c));
                                    }
                                }
                                VimAction::Undo => {
                                    if let Some(prev) = vim.pop_undo() {
                                        vim.push_undo(&ui_state.input.text);
                                        ui_state.input.text = prev;
                                        ui_state.input.cursor = ui_state.input.text.len();
                                    }
                                }
                                VimAction::PageUp => {
                                    ui_state.messages.scroll_up(5);
                                }
                                VimAction::PageDown => {
                                    ui_state.messages.scroll_down();
                                }
                                VimAction::HalfPageUp => {
                                    ui_state.messages.scroll_up(10);
                                }
                                VimAction::HalfPageDown => {
                                    ui_state.messages.scroll_down();
                                }
                                VimAction::JumpTop => {
                                    ui_state.messages.start = 0;
                                }
                                VimAction::JumpBottom => {
                                    ui_state.messages.scroll_to_bottom();
                                }
                                VimAction::Command => {
                                    let cmds: Vec<String> = self
                                        .commands
                                        .list()
                                        .iter()
                                        .map(|c| format!("/{} - {}", c.name, c.description))
                                        .collect();
                                    ui_state.overlays.push(Overlay::CommandPalette {
                                        query: ":".into(),
                                        commands: cmds,
                                        selected: 0,
                                    });
                                }
                                VimAction::Search => {
                                    ui_state.active_view = ViewMode::Search;
                                }
                                VimAction::ToggleFold => {
                                    // Toggle fold on the last visible message (or focused if selector active)
                                    if let Some(msg) = ui_state.messages.messages.last_mut() {
                                        msg.folded = !msg.folded;
                                    }
                                }
                                VimAction::ToggleSplit => {
                                    use coderex_ui::state::LayoutMode;
                                    ui_state.layout_mode = match ui_state.layout_mode {
                                        LayoutMode::SinglePane => LayoutMode::HorizontalSplit { left_percent: 50 },
                                        LayoutMode::HorizontalSplit { .. } => LayoutMode::SinglePane,
                                    };
                                }
                                VimAction::SearchNext => {
                                    ui_state.search.next();
                                }
                                VimAction::SearchPrev => {
                                    ui_state.search.prev();
                                }
                                VimAction::ExportCopy => {
                                    use coderex_ui::export::{ExportFormat, export_messages, copy_to_clipboard};
                                    use coderex_ui::selector::MessageSelector;
                                    let core_msgs: Vec<coderex_core::types::Message> = ui_state.messages.messages.iter().map(|m| {
                                        let role = match m.role {
                                            coderex_ui::message_list::MessageRole::User => coderex_core::types::Role::User,
                                            coderex_ui::message_list::MessageRole::Assistant => coderex_core::types::Role::Assistant,
                                            coderex_ui::message_list::MessageRole::System => coderex_core::types::Role::System,
                                            coderex_ui::message_list::MessageRole::ToolCall | coderex_ui::message_list::MessageRole::ToolResult => coderex_core::types::Role::Tool,
                                        };
                                        coderex_core::types::Message {
                                            role,
                                            content: coderex_core::types::MessageContent::Text(m.content.to_string()),
                                        }
                                    }).collect();
                                    let selector = MessageSelector::new(core_msgs.len());
                                    let exported = export_messages(&core_msgs, &selector, ExportFormat::PlainText);
                                    if let Err(e) = copy_to_clipboard(&exported) {
                                        ui_state.notifications.err(format!("Copy failed: {e}"));
                                    } else {
                                        ui_state.notifications.success("Copied to clipboard");
                                    }
                                }
                                VimAction::Repeat
                                | VimAction::Ignore
                                | VimAction::Append
                                | VimAction::InsertAtEnd
                                | VimAction::InsertAtStart => {}
                            }

                            // Sync slash-command suggestion overlay when typing in insert mode.
                            if vim.is_insert() {
                                self.update_slash_suggestions(&mut ui_state);
                            }
                        }
                        Event::Resize(_, _) => {}
                        _ => {}
                    }
                }

                Some(engine_event) = async {
                    event_rx.as_mut().unwrap().recv().await
                }, if event_rx.is_some() => {
                    match engine_event {
                        EngineEvent::TextDelta(delta) => {
                            if let Some(last) = ui_state.messages.messages.last_mut() {
                                if last.role == MessageRole::Assistant && last.streaming {
                                    let mut content = last.content.to_string();
                                    content.push_str(&delta);
                                    last.content = content.into();
                                }
                            }
                        }
                        EngineEvent::ToolCallStart { name, .. } => {
                            ui_state.messages.push(Message {
                                role: MessageRole::ToolCall,
                                content: format!("Calling {name}...").into(),
                                tool_name: Some(name.into()),
                                streaming: false,
                            folded: false,
                            });
                        }
                        EngineEvent::ToolCallEnd { result, .. } => {
                            let content = if result.is_error {
                                format!("Error: {}", result.output)
                            } else {
                                result.output
                            };
                            ui_state.messages.push(Message {
                                role: MessageRole::ToolResult,
                                content: content.into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                        }
                        EngineEvent::PermissionRequest { tool, .. } => {
                            ui_state.overlays.push(Overlay::Notification {
                                message: format!("Permission required: {tool}"),
                                level: coderex_ui::overlay::NotificationLevel::Warning,
                            });
                        }
                        _ => {}
                    }
                }

                Some((engine, result)) = async {
                    query_done_rx.as_mut().unwrap().await.ok()
                }, if query_done_rx.is_some() => {
                    self.engine = Some(engine);
                    query_done_rx = None;
                    thinking = false;

                    if let Some(last) = ui_state.messages.messages.last_mut() {
                        if last.role == MessageRole::Assistant {
                            last.streaming = false;
                        }
                    }

                    match result {
                        Ok(TerminalReason::ApiUnavailable { message }) => {
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: format!(
                                    "The AI service is temporarily unavailable. Your conversation context is preserved. Please try again later.\n({message})"
                                ).into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                        }
                        Ok(_) => {
                            // Trigger background features after successful turn
                            let prompt_text = self.history.last().cloned().unwrap_or_default();
                            self.trigger_background_features(&prompt_text).await;
                        }
                        Err(e) => {
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: format!("Query failed: {e}").into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                        }
                    }
                }

                else => break Ok(()),
            }
        };

        // Cleanup
        let _ = terminal_ui.exit();
        let _ = disable_raw_mode();
        self.event_rx = event_rx;

        result
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

    /// Build a QueryRequest for the main agent from current session effects.
    fn build_query_request_for_main_agent(&self) -> QueryRequest {
        QueryRequest {
            kind: QueryKind::MainAgent,
            user_prompt: String::new(), // filled at submit time
            // If override is set, it replaces the base prompt; otherwise None
            custom_system_prompt: self.session_effects.override_system_prompt.clone(),
            append_system_prompt: self.session_effects.append_system_prompt.clone(),
            prompt_profile: self
                .session_effects
                .prompt_profiles
                .last()
                .copied()
                .unwrap_or_default(),
            allowed_tools: self.session_effects.allowed_tools_override.clone(),
            model_profile: ModelProfile::Main,
            inherit_session: true,
            stream: true,
            max_tokens: 4096,
        }
    }

    /// Build SubmitOptions incorporating active command effects.
    ///
    /// Routes through the Orchestrator so that prompt profiles, model selection,
    /// and tool whitelists are applied consistently with side/forked queries.
    fn build_submit_options(&self) -> SubmitOptions {
        let request = self.build_query_request_for_main_agent();
        let mut options = if let Some(ref orchestrator) = self.orchestrator {
            orchestrator.build_submit_options(&request)
        } else {
            // Fallback when no orchestrator is available
            let mut opts = SubmitOptions::default();
            if let Some(ref override_prompt) = self.session_effects.override_system_prompt {
                // Override takes precedence over profile-based composition
                opts.custom_system_prompt = Some(override_prompt.clone());
            } else if !self.session_effects.prompt_profiles.is_empty() {
                opts.custom_system_prompt = Some(
                    prompts::compose_system_prompt_for_profiles(
                        &self.session_effects.prompt_profiles,
                    ),
                );
            }
            if let Some(ref prompt) = self.session_effects.append_system_prompt {
                opts.append_system_prompt = Some(prompt.clone());
            }
            if let Some(ref tools) = self.session_effects.allowed_tools_override {
                opts.allowed_tools_override = Some(tools.clone());
            } else if let Some(default_tools) =
                prompts::default_allowed_tools_for_profiles(&self.session_effects.prompt_profiles)
            {
                opts.allowed_tools_override = Some(default_tools);
            }
            opts
        };

        // REPL-specific: extra context from commands is not part of QueryRequest
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

    /// Trigger background features after a successful turn.
    ///
    /// Tasks are tracked in `self.background_tasks` with cancellation tokens.
    /// Results are delivered asynchronously via `self.background_results_rx`.
    async fn trigger_background_features(&mut self, _user_prompt: &str) {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return,
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return,
        };

        self.cleanup_finished_background_tasks();

        // Trigger prompt suggestions after the user has sent a few messages
        if self.history.len() >= 3 && self.history.len() % 5 == 0 {
            let conversation_summary = self
                .history
                .iter()
                .rev()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let request = BackgroundFeatureRequest {
                kind: BackgroundFeatureKind::PromptSuggestion,
                system_prompt: "You are a prompt suggestion assistant.".into(),
                user_prompt: format!(
                    "Suggest 1-2 brief follow-up prompts based on this conversation context.\n\
                     Recent messages:\n{conversation_summary}\n\
                     Return only the suggestions, one per line."
                ),
                parent_context: None,
                allowed_tools: None,
                max_tokens: 128,
                writable: false,
            };
            self.spawn_background_task(
                "prompt_suggestion",
                BackgroundFeatureKind::PromptSuggestion,
                client,
                request,
                tx,
            );
        }
    }

    /// Spawn a tracked background task with cancellation support.
    fn spawn_background_task(
        &mut self,
        id: &str,
        kind: BackgroundFeatureKind,
        client: Box<dyn ApiClient>,
        request: BackgroundFeatureRequest,
        result_tx: mpsc::UnboundedSender<BackgroundTaskResult>,
    ) {
        let cancel_token = CancellationToken::new();
        let task_id = id.to_string();
        let task_cancel = cancel_token.clone();

        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind,
            cancel_token: cancel_token.clone(),
        });

        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = coderex_core::orchestrator::run_background_feature(
                    client.as_ref(), request
                ) => res,
            };

            let (success, content) = match result {
                Ok(r) => (true, r.content),
                Err(e) => (false, e.to_string()),
            };

            let _ = result_tx.send(BackgroundTaskResult {
                id: task_id,
                kind,
                success,
                content,
            });
        });
    }

    /// Cancel all running background tasks.
    fn cancel_background_tasks(&mut self) {
        for task in &self.background_tasks {
            task.cancel_token.cancel();
            tracing::info!("Cancelled background task: {} ({:?})", task.id, task.kind);
        }
        self.background_tasks.clear();
    }

    /// Remove finished (cancelled or completed) background tasks from tracking.
    fn cleanup_finished_background_tasks(&mut self) {
        self.background_tasks.retain(|t| !t.cancel_token.is_cancelled());
    }

    /// Poll for completed background task results and return them.
    fn poll_background_results(&mut self) -> Vec<BackgroundTaskResult> {
        let mut results = Vec::new();
        if let Some(ref mut rx) = self.background_results_rx {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        results
    }

    /// Run agent generation synchronously and return the result.
    async fn run_agent_generation(&mut self, description: &str) -> String {
        let Some(ref client) = self.api_client else {
            return "No API client configured.".into();
        };
        match coderex_core::prompts::generate_agent(client.as_ref(), description).await {
            Ok(spec) => format!(
                "Agent Generated\n──────────────\nName: {}\nDescription: {}\nModel: {}\nAllowed Tools: {}\n\nSystem Prompt:\n{}",
                spec.name,
                spec.description,
                spec.model_profile,
                spec.allowed_tools.as_ref().map(|t| t.join(", ")).unwrap_or_else(|| "all".into()),
                spec.system_prompt
            ),
            Err(e) => format!("Agent generation failed: {e}"),
        }
    }

    /// Compact the session context by truncating older messages.
    /// Keeps the first user message and the most recent 3 turns.
    async fn compact_context(
        &mut self,
        ui_state: &mut coderex_ui::state::UiState,
    ) -> String {
        let Some(ref mut engine) = self.engine else {
            return "No engine configured.".into();
        };
        let result = engine.compact_session_messages(3);
        if result.tokens_removed == 0 {
            return "Context is already compact.".into();
        }
        // Sync UI message list: rebuild from compacted messages
        ui_state.messages.messages.clear();
        ui_state.messages.start = 0;
        for msg in &result.messages {
            let (role, content, tool_name) = match (&msg.role, &msg.content) {
                (coderex_core::types::Role::User, coderex_core::types::MessageContent::Text(t)) => {
                    (coderex_ui::message_list::MessageRole::User, t.clone(), None)
                }
                (coderex_core::types::Role::Assistant, coderex_core::types::MessageContent::Text(t)) => {
                    (coderex_ui::message_list::MessageRole::Assistant, t.clone(), None)
                }
                (coderex_core::types::Role::System, coderex_core::types::MessageContent::Text(t)) => {
                    (coderex_ui::message_list::MessageRole::System, t.clone(), None)
                }
                (coderex_core::types::Role::Tool, coderex_core::types::MessageContent::ToolResult(tr)) => {
                    (coderex_ui::message_list::MessageRole::ToolResult, tr.output.clone(), Some(tr.tool_call_id.clone().into()))
                }
                (_, c) => {
                    (coderex_ui::message_list::MessageRole::System, format!("{c:?}"), None)
                }
            };
            ui_state.messages.push(coderex_ui::message_list::Message {
                role,
                content: content.into(),
                tool_name: tool_name.map(|t: String| t.into()),
                streaming: false,
                            folded: false,
            });
        }
        format!(
            "Context compacted: {} tokens removed, {} messages remaining.",
            result.tokens_removed,
            result.messages.len()
        )
    }

    /// Compact context in non-TUI mode.
    async fn compact_context_non_tui(&mut self) -> String {
        let Some(ref mut engine) = self.engine else {
            return "No engine configured.".into();
        };
        let result = engine.compact_session_messages(3);
        if result.tokens_removed == 0 {
            return "Context is already compact.".into();
        }
        format!(
            "Context compacted: {} tokens removed, {} messages remaining.",
            result.tokens_removed,
            result.messages.len()
        )
    }

    /// Spawn a background task that updates session memory via ai_workflow.
    async fn spawn_memory_update_task(&mut self) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };
        let store = match &self.session_memory_store {
            Some(s) => Arc::clone(s),
            None => return "Session memory store not available.".into(),
        };
        let session_id = match self.session_id {
            Some(s) => s,
            None => return "Session ID not available.".into(),
        };
        let conversation = self.history.iter().rev().take(10).cloned().collect::<Vec<_>>().join("\n");
        let current_memory = store.get_facts(session_id)
            .into_iter()
            .map(|f| f.fact)
            .collect::<Vec<_>>()
            .join("\n");

        self.cleanup_finished_background_tasks();
        let task_id = format!("memory_update_{}", self.background_tasks.len());
        let cancel_token = CancellationToken::new();
        let task_cancel = cancel_token.clone();
        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind: BackgroundFeatureKind::SessionMemoryUpdate,
            cancel_token: cancel_token.clone(),
        });

        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = ai_workflow::update_session_memory_ai(
                    client.as_ref(), &store, session_id, &current_memory, &conversation
                ) => res.map_err(|e| coderex_core::orchestrator::OrchestratorError::Config(e.to_string())),
            };
            let (success, content) = match result {
                Ok(text) => (true, text),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(BackgroundTaskResult {
                id: task_id_clone,
                kind: BackgroundFeatureKind::SessionMemoryUpdate,
                success,
                content,
            });
        });
        format!("Background task started: {task_id}")
    }

    /// Spawn a background task that extracts long-term memories via ai_workflow.
    async fn spawn_memory_extraction_task(&mut self) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };
        let store = match &self.long_term_store {
            Some(s) => Arc::clone(s),
            None => return "Long-term memory store not available.".into(),
        };
        let conversation = self.history.iter().rev().take(10).cloned().collect::<Vec<_>>().join("\n");

        self.cleanup_finished_background_tasks();
        let task_id = format!("memory_extraction_{}", self.background_tasks.len());
        let cancel_token = CancellationToken::new();
        let task_cancel = cancel_token.clone();
        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind: BackgroundFeatureKind::MemoryExtraction,
            cancel_token: cancel_token.clone(),
        });

        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = ai_workflow::extract_long_term_memories_ai(
                    client.as_ref(), &store, &conversation
                ) => res.map_err(|e| coderex_core::orchestrator::OrchestratorError::Config(e.to_string())),
            };
            let (success, content) = match result {
                Ok(topics) => (true, topics.join(", ")),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(BackgroundTaskResult {
                id: task_id_clone,
                kind: BackgroundFeatureKind::MemoryExtraction,
                success,
                content,
            });
        });
        format!("Background task started: {task_id}")
    }

    /// Handle a completed session-memory-update result (no-op: already persisted in task).
    async fn handle_memory_update_result(&mut self, _result: &BackgroundTaskResult) {}

    /// Handle a completed memory-extraction result (no-op: already persisted in task).
    async fn handle_memory_extraction_result(&mut self, _result: &BackgroundTaskResult) {}

    /// Spawn a background feature by kind with a sensible default request.
    fn spawn_background_feature_by_kind(&mut self, kind: BackgroundFeatureKind, user_prompt: String) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };

        self.cleanup_finished_background_tasks();

        let (system_prompt, max_tokens) = match kind {
            BackgroundFeatureKind::AgentSummary => (
                "You are a sub-agent summarizer. Produce a concise summary of recent activity.".into(),
                256,
            ),
            BackgroundFeatureKind::AwaySummary => (
                "You are a session summarizer. Summarize what happened while the user was away.".into(),
                256,
            ),
            BackgroundFeatureKind::MagicDocs => (
                "You are a documentation generator. Produce concise code documentation.".into(),
                1024,
            ),
            BackgroundFeatureKind::CodebaseOverview => (
                "You are a codebase analyst. Produce a high-level overview of the project.".into(),
                1024,
            ),
            BackgroundFeatureKind::ReadmeGeneration => (
                "You are a README generator. Produce a project README in Markdown.".into(),
                2048,
            ),
            BackgroundFeatureKind::SessionMemoryUpdate => (
                "You are a memory manager. Update session memory based on recent conversation.".into(),
                512,
            ),
            BackgroundFeatureKind::MemoryExtraction => (
                "You are a memory extractor. Identify long-term memories from this session.".into(),
                512,
            ),
            BackgroundFeatureKind::PromptSuggestion => (
                "You are a prompt suggestion assistant.".into(),
                128,
            ),
            BackgroundFeatureKind::NextActionSuggestion => (
                "You are a task planner. Suggest the next best action.".into(),
                128,
            ),
            BackgroundFeatureKind::TeamStatus => (
                "You are a team status aggregator.".into(),
                256,
            ),
        };

        let parent_context = if self.history.len() >= 3 {
            Some(self.history.iter().rev().take(5).cloned().collect::<Vec<_>>().join("\n"))
        } else {
            None
        };

        let request = BackgroundFeatureRequest {
            kind,
            system_prompt,
            user_prompt,
            parent_context,
            allowed_tools: None,
            max_tokens,
            writable: matches!(kind, BackgroundFeatureKind::SessionMemoryUpdate | BackgroundFeatureKind::MemoryExtraction),
        };

        let id = format!("{}_{}", background_feature_label(kind), self.background_tasks.len());
        self.spawn_background_task(&id, kind, client, request, tx);
        format!("Background task started: {id}")
    }

    /// Apply effects from a CommandResult to session state.
    fn apply_command_effects(&mut self, result: &CommandResult) {
        if let Some(profile) = result.prompt_profile {
            if !self.session_effects.prompt_profiles.contains(&profile) {
                self.session_effects.prompt_profiles.push(profile);
            }
        }

        if let Some(ref prompt) = result.override_system_prompt {
            // Override takes precedence: replace the base prompt entirely
            self.session_effects.override_system_prompt = Some(prompt.clone());
        } else if let Some(ref prompt) = result.append_system_prompt {
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

        // Trigger background features on successful turn
        if result.is_ok() {
            self.trigger_background_features(prompt).await;
        }

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
            Ok(TerminalReason::ApiUnavailable { message }) => {
                format!(
                    "The AI service is temporarily unavailable. Your conversation context is preserved. Please try again later.\n({message})"
                )
            }
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

    /// Sync REPL state into the TUI statusline.
    fn sync_statusline(&self, statusline: &mut coderex_ui::statusline::Statusline) {
        statusline.model = self.model.0.clone();
        statusline.session_title = if self.session_effects.title_generated {
            "Active".into()
        } else {
            "New Session".into()
        };
        statusline.permission_mode = "default".into();
        statusline.git_branch = current_git_branch(&self.cwd);
        statusline.total_tokens = self.history.len() * 100;
        statusline.cost_usd = 0.0;
    }

    /// Update slash-command suggestion overlay based on current input.
    fn update_slash_suggestions(&self, ui_state: &mut coderex_ui::state::UiState) {
        use coderex_ui::overlay::Overlay;

        if ui_state.input.text.starts_with('/') {
            let query = ui_state.input.text.trim_start_matches('/');
            let cmds: Vec<String> = self
                .commands
                .list()
                .iter()
                .filter(|c| query.is_empty() || c.name.contains(query))
                .map(|c| format!("/{} - {}", c.name, c.description))
                .collect();
            // Replace existing palette overlay so suggestions stay in sync.
            if let Some(top) = ui_state.overlays.top() {
                if matches!(top, Overlay::CommandPalette { .. }) {
                    ui_state.overlays.pop();
                }
            }
            ui_state.overlays.push(Overlay::CommandPalette {
                query: ui_state.input.text.clone(),
                commands: cmds,
                selected: 0,
            });
        } else {
            // Remove palette overlay when input no longer starts with '/'.
            if let Some(top) = ui_state.overlays.top() {
                if matches!(top, Overlay::CommandPalette { .. }) {
                    ui_state.overlays.pop();
                }
            }
        }
    }

    /// Process a line of input in the TUI context.
    async fn process_tui_input(
        &mut self,
        text: &str,
        ui_state: &mut coderex_ui::state::UiState,
        thinking: &mut bool,
        query_done_rx: &mut Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        >,
    ) {
        self.history.push(text.to_string());

        // Add user message
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::User,
            content: text.into(),
            tool_name: None,
            streaming: false,
                            folded: false,
        });

        if text.starts_with('/') {
            let parts: Vec<&str> = text.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

            match cmd_name {
                "exit" | "quit" => {
                    self.running = false;
                    return;
                }
                "help" => {
                    let mut output = String::from("Available commands:\n");
                    for cmd in self.commands.list() {
                        output.push_str(&format!("  /{:<20} {}\n", cmd.name, cmd.description));
                    }
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::Assistant,
                        content: output.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "cancel" => {
                    self.cancel_background_tasks();
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: "All background tasks cancelled.".into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "compact" => {
                    let msg = self.compact_context(ui_state).await;
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "agent-summary" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::AgentSummary,
                        "Summarize what the agent has done recently.".into(),
                    );
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "magic-docs" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::MagicDocs,
                        "Generate documentation for the current codebase.".into(),
                    );
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "generate-readme" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::ReadmeGeneration,
                        "Generate a README for the current project.".into(),
                    );
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "away-summary" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::AwaySummary,
                        "Summarize what happened while the user was away.".into(),
                    );
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "codebase-overview" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::CodebaseOverview,
                        "Generate a high-level overview of the codebase.".into(),
                    );
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "update-memory" => {
                    let msg = self.spawn_memory_update_task().await;
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "extract-memories" => {
                    let msg = self.spawn_memory_extraction_task().await;
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "agent-generation" => {
                    let description = args.join(" ");
                    if description.is_empty() {
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: "Usage: /agent-generation <description>".into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    } else {
                        let msg = self.run_agent_generation(&description).await;
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: msg.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    return;
                }
                "auto-dream" => {
                    let msg = self.handle_auto_dream_command(&args);
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                "team-memory" => {
                    let msg = self.handle_team_memory_command(&args).await;
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                            folded: false,
                    });
                    return;
                }
                _ => {}
            }

            let ctx = self.build_context();
            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.apply_command_effects(&result);

            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::Assistant,
                content: result.output.into(),
                tool_name: None,
                streaming: false,
                            folded: false,
            });
        } else if self.engine.is_some() {
            // Start streaming assistant placeholder
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::Assistant,
                content: "".into(),
                tool_name: None,
                streaming: true,
                folded: false,
            });

            let options = self.build_submit_options();
            let mut engine = self.engine.take().unwrap();
            let prompt = text.to_string();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let result = engine.submit_streaming(prompt, options).await;
                let _ = tx.send((engine, result));
            });
            *query_done_rx = Some(rx);
            *thinking = true;
        } else {
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::Assistant,
                content: "[No engine configured]".into(),
                tool_name: None,
                streaming: false,
                            folded: false,
            });
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
            "cancel" => {
                self.cancel_background_tasks();
                writeln!(stdout, "All background tasks cancelled.")?;
                return Ok(());
            }
            "compact" => {
                let msg = self.compact_context_non_tui().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "agent-summary" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::AgentSummary,
                    "Summarize what the agent has done recently.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "magic-docs" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::MagicDocs,
                    "Generate documentation for the current codebase.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "generate-readme" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::ReadmeGeneration,
                    "Generate a README for the current project.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "away-summary" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::AwaySummary,
                    "Summarize what happened while the user was away.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "codebase-overview" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::CodebaseOverview,
                    "Generate a high-level overview of the codebase.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "update-memory" => {
                let msg = self.spawn_memory_update_task().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "extract-memories" => {
                let msg = self.spawn_memory_extraction_task().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "agent-generation" => {
                let description = args.join(" ");
                if description.is_empty() {
                    writeln!(stdout, "Usage: /agent-generation <description>")?;
                } else {
                    let msg = self.run_agent_generation(&description).await;
                    writeln!(stdout, "{msg}")?;
                }
                return Ok(());
            }
            "auto-dream" => {
                let msg = self.handle_auto_dream_command(&args);
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "team-memory" => {
                let msg = self.handle_team_memory_command(&args).await;
                writeln!(stdout, "{msg}")?;
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

    /// Handle /auto-dream command.
    fn handle_auto_dream_command(&mut self, args: &[String]) -> String {
        let dream = match &self.auto_dream {
            Some(d) => d,
            None => return "Auto-dream is not initialized.".into(),
        };

        if args.is_empty() {
            return "Usage: /auto-dream <enable|disable|status>".into();
        }

        match args[0].as_str() {
            "enable" => {
                let mut config = dream.config.clone();
                config.enabled = true;
                if args.len() >= 2 {
                    if let Ok(n) = args[1].parse::<usize>() {
                        config.session_threshold = n;
                    }
                }
                if args.len() >= 3 {
                    if let Ok(n) = args[2].parse::<i64>() {
                        config.time_threshold_hours = n;
                    }
                }
                // Record a session to trigger immediate check if threshold is 1
                let _ = dream.record_session();

                // Spawn the watcher if API client is available
                if let Some(api) = &self.api_client {
                    let dream = dream.clone();
                    let api = api.clone_box();
                    let model = self.model.clone();
                    let _memory_dir = dream.memory_dir.clone();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    self.dream_shutdown_tx = Some(shutdown_tx);

                    tokio::spawn(async move {
                        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
                        let watcher_handle = tokio::spawn(async move {
                            // Collect recent session summaries as context
                            let recent_sessions: Vec<String> = Vec::new();
                            dream.spawn_watcher_with_model(
                                tx,
                                std::time::Duration::from_secs(30),
                                shutdown_rx,
                                api.as_ref(),
                                &model,
                                recent_sessions,
                            ).await;
                        });

                        // Forward dream results to memory dir
                        while let Some(session) = rx.recv().await {
                            tracing::info!(
                                "Auto-dream completed: analyzed {} sessions",
                                session.sessions_analyzed
                            );
                            // Dream artifacts are already written by complete_dream
                            let _ = session;
                        }
                        let _ = watcher_handle.await;
                    });

                    format!("Auto-dream enabled (threshold: {} sessions or {}h)", config.session_threshold, config.time_threshold_hours)
                } else {
                    "Auto-dream requires an API client and memory store.".into()
                }
            }
            "disable" => {
                if let Some(tx) = self.dream_shutdown_tx.take() {
                    let _ = tx.send(());
                    "Auto-dream disabled.".into()
                } else {
                    "Auto-dream is not running.".into()
                }
            }
            "status" => {
                let config = &dream.config;
                let running = self.dream_shutdown_tx.is_some();
                let threshold = dream.check_threshold();
                let threshold_msg = match threshold {
                    DreamCheckResult::ShouldDream { sessions_since_last, hours_since_last } =>
                        format!("thresholds met: {sessions_since_last} sessions, {hours_since_last}h since last dream"),
                    DreamCheckResult::NotReady => "thresholds not yet met".into(),
                    DreamCheckResult::InProgress => "dream in progress".into(),
                };
                format!(
                    "Auto-dream: {} | config: {} sessions or {}h | {threshold_msg}",
                    if running { "running" } else { "stopped" },
                    config.session_threshold,
                    config.time_threshold_hours,
                )
            }
            _ => "Usage: /auto-dream <enable|disable|status>".into(),
        }
    }

    /// Handle /team-memory command.
    async fn handle_team_memory_command(&mut self, args: &[String]) -> String {
        let svc = match &self.team_memory_service {
            Some(s) => s,
            None => return "Team memory sync is not initialized.".into(),
        };

        if args.is_empty() {
            return "Usage: /team-memory <upsert|read|list|delete|sync> [args...]".into();
        }

        match args[0].as_str() {
            "upsert" => {
                if args.len() < 3 {
                    return "Usage: /team-memory upsert <topic> <content>".into();
                }
                let topic = &args[1];
                let content = args[2..].join(" ");
                match svc.upsert(topic, &content) {
                    Ok(entry) => format!("Team memory upserted: '{}' (checksum: {})", entry.topic, entry.checksum.to_hex()),
                    Err(e) => format!("Failed to upsert team memory: {e}"),
                }
            }
            "read" => {
                if args.len() < 2 {
                    return "Usage: /team-memory read <topic>".into();
                }
                match svc.read(&args[1]) {
                    Ok(Some(entry)) => format!("[{}]\n{}", entry.topic, entry.content),
                    Ok(None) => "No entry found.".into(),
                    Err(e) => format!("Failed to read team memory: {e}"),
                }
            }
            "list" => {
                match svc.list() {
                    Ok(entries) if entries.is_empty() => "No team memories.".into(),
                    Ok(entries) => {
                        entries.iter()
                            .map(|e| format!("- {} (updated: {})", e.topic, e.updated_at.format("%Y-%m-%d %H:%M")))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    Err(e) => format!("Failed to list team memories: {e}"),
                }
            }
            "delete" => {
                if args.len() < 2 {
                    return "Usage: /team-memory delete <topic>".into();
                }
                match svc.delete(&args[1]) {
                    Ok(()) => "Team memory deleted.".into(),
                    Err(e) => format!("Failed to delete team memory: {e}"),
                }
            }
            "sync" => {
                // Sync via in-memory relay or HTTP transport
                // For now, show local state info
                match svc.list() {
                    Ok(entries) => {
                        format!(
                            "Local team memories: {} entries. Use /team-memory sync --remote to sync with relay.",
                            entries.len()
                        )
                    }
                    Err(e) => format!("Failed to list for sync: {e}"),
                }
            }
            _ => "Usage: /team-memory <upsert|read|list|delete|sync> [args...]".into(),
        }
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
        if let Some(ref engine) = self.engine {
            ctx.cost_summary = engine.cost_summary();
        }
        ctx
    }
}

/// Get the current git branch for the statusline.
fn current_git_branch(cwd: &str) -> String {
    std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
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

    #[test]
    fn current_git_branch_falls_back() {
        let branch = current_git_branch("/nonexistent");
        assert!(branch.is_empty());
    }
}
