//! Runtime kernel — channels, event loop, UI state, and bootstrap.
//!
//! Defines `UiEvent`, `UiState`, `Runtime::bootstrap()`, and the main
//! event loop that drives rendering state from UI input and engine
//! events. The actual terminal drawing (ratatui) is left to the CLI
//! binary; this module owns the portable state machine that renderers
//! sit on top of.

use crate::types::{EngineEvent, ToolResult};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Events consumed by the UI rendering loop.
///
/// All I/O, ticks, engine updates, and resize events flow through
/// this single channel to drive the UI state machine.
#[derive(Debug)]
pub enum UiEvent {
    /// A key was pressed.
    #[cfg(feature = "crossterm")]
    Input(crossterm::event::KeyEvent),
    /// A key was pressed (portable representation for tests without crossterm).
    KeyPress {
        /// Key code string (e.g. "Enter", "a").
        code: String,
        /// Active modifier keys (e.g. "Control", "Shift").
        modifiers: Vec<String>,
    },
    /// Periodic tick for animations, spinners, idle presence.
    Tick,
    /// An event from the QueryEngine (text delta, tool call, etc.).
    EngineUpdate(EngineEvent),
    /// Terminal was resized to (cols, rows).
    Resize {
        /// Number of columns.
        cols: u16,
        /// Number of rows.
        rows: u16,
    },
}

/// Intent produced by the input handler, consumed by the CLI wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiIntent {
    /// No visible change.
    None,
    /// The user submitted the current input buffer as a prompt.
    Submit(String),
    /// The user requested to clear the input buffer.
    Cleared,
    /// The user requested to interrupt the current turn.
    Interrupt,
}

/// An active tool call the UI is currently tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToolCall {
    /// Correlation id.
    pub id: String,
    /// Tool name.
    pub name: String,
}

/// A pending permission prompt the UI must surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    /// Request id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Serialized tool arguments.
    pub args: String,
}

/// A pending elevation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingElevation {
    /// Request id.
    pub id: String,
    /// Tool name that requires elevated privileges.
    pub tool: String,
    /// Reason provided by the permission engine.
    pub reason: String,
}

/// A pending elicitation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingElicitation {
    /// Request id.
    pub id: String,
    /// Prompt message.
    pub message: String,
}

/// A visible completed tool call entry in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// Correlation id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Whether the call ended in error.
    pub is_error: bool,
    /// Output text (possibly truncated for display).
    pub output: String,
}

/// Visible transcript row types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    /// Streaming assistant text (may be appended to).
    AssistantText(String),
    /// Completed tool call.
    ToolCall(ToolCallRecord),
    /// Visual compact boundary separator.
    CompactBoundary,
    /// Session-end marker.
    SessionEnd,
}

/// Portable UI state driven by `handle_ui_event` / `handle_engine_event`.
///
/// This is intentionally independent of ratatui so it can be unit-tested
/// and reused from headless frontends. Renderers read from this struct.
#[derive(Debug, Default)]
pub struct UiState {
    /// Current editable input buffer (bytes the user typed).
    pub input_buffer: String,
    /// Layout size in (cols, rows) — updated on resize.
    pub size: (u16, u16),
    /// Animation tick counter, advanced on each `Tick` event.
    pub tick_count: u64,
    /// Currently in-flight tool calls (ToolCallStart with no matching End).
    pub active_tool_calls: Vec<ActiveToolCall>,
    /// Pending permission prompts awaiting user response.
    pub pending_permissions: VecDeque<PendingPermission>,
    /// Pending elevation prompts awaiting user response.
    pub pending_elevations: VecDeque<PendingElevation>,
    /// Pending elicitation prompts awaiting user response.
    pub pending_elicitations: VecDeque<PendingElicitation>,
    /// Scrollable transcript entries (assistant text, tool calls, boundaries).
    pub transcript: Vec<TranscriptEntry>,
    /// Whether the session has ended (engine signalled SessionEnd).
    pub session_ended: bool,
}

impl UiState {
    /// Create an empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a key press against the input buffer.
    ///
    /// Returns a `UiIntent` describing whether the input produced a
    /// user-visible action (submit, clear, interrupt).
    pub fn handle_key(&mut self, code: &str, modifiers: &[String]) -> UiIntent {
        let ctrl = modifiers.iter().any(|m| m.eq_ignore_ascii_case("Control"));
        match code {
            "Enter" => {
                if self.input_buffer.is_empty() {
                    return UiIntent::None;
                }
                let prompt = std::mem::take(&mut self.input_buffer);
                UiIntent::Submit(prompt)
            }
            "Backspace" => {
                self.input_buffer.pop();
                UiIntent::None
            }
            "Esc" => {
                if self.input_buffer.is_empty() {
                    UiIntent::Interrupt
                } else {
                    self.input_buffer.clear();
                    UiIntent::Cleared
                }
            }
            code if ctrl && code.eq_ignore_ascii_case("c") => UiIntent::Interrupt,
            code if ctrl && code.eq_ignore_ascii_case("u") => {
                self.input_buffer.clear();
                UiIntent::Cleared
            }
            code if code.chars().count() == 1 => {
                self.input_buffer.push_str(code);
                UiIntent::None
            }
            _ => UiIntent::None,
        }
    }

    /// Apply an engine event to the state.
    pub fn apply_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::TextDelta(text) => self.push_text_delta(&text),
            EngineEvent::ToolCallStart { id, name } => {
                self.active_tool_calls.push(ActiveToolCall { id, name });
            }
            EngineEvent::ToolCallEnd { id, result } => {
                self.complete_tool_call(&id, &result);
            }
            EngineEvent::CompactBoundary => {
                self.transcript.push(TranscriptEntry::CompactBoundary);
            }
            EngineEvent::PermissionRequest { id, tool, args } => {
                self.pending_permissions.push_back(PendingPermission {
                    id,
                    tool,
                    args: args.to_string(),
                });
            }
            EngineEvent::ElicitationRequest { id, message } => {
                self.pending_elicitations
                    .push_back(PendingElicitation { id, message });
            }
            EngineEvent::ElevationRequest { id, tool, reason } => {
                self.pending_elevations
                    .push_back(PendingElevation { id, tool, reason });
            }
            EngineEvent::SessionEnd => {
                self.session_ended = true;
                self.active_tool_calls.clear();
                self.transcript.push(TranscriptEntry::SessionEnd);
            }
        }
    }

    fn push_text_delta(&mut self, delta: &str) {
        if let Some(TranscriptEntry::AssistantText(buf)) = self.transcript.last_mut() {
            buf.push_str(delta);
        } else {
            self.transcript
                .push(TranscriptEntry::AssistantText(delta.to_string()));
        }
    }

    fn complete_tool_call(&mut self, id: &str, result: &ToolResult) {
        if let Some(pos) = self
            .active_tool_calls
            .iter()
            .position(|c| c.id == id)
        {
            let active = self.active_tool_calls.remove(pos);
            self.transcript
                .push(TranscriptEntry::ToolCall(ToolCallRecord {
                    id: active.id,
                    name: active.name,
                    is_error: result.is_error,
                    output: result.output.clone(),
                }));
        } else {
            // Orphan end event — still record the result with a placeholder name.
            self.transcript
                .push(TranscriptEntry::ToolCall(ToolCallRecord {
                    id: id.to_string(),
                    name: "<unknown>".into(),
                    is_error: result.is_error,
                    output: result.output.clone(),
                }));
        }
    }

    /// Consume and return the oldest pending permission, if any.
    pub fn pop_permission(&mut self) -> Option<PendingPermission> {
        self.pending_permissions.pop_front()
    }

    /// Consume and return the oldest pending elevation, if any.
    pub fn pop_elevation(&mut self) -> Option<PendingElevation> {
        self.pending_elevations.pop_front()
    }

    /// Consume and return the oldest pending elicitation, if any.
    pub fn pop_elicitation(&mut self) -> Option<PendingElicitation> {
        self.pending_elicitations.pop_front()
    }
}

/// Handle to the running runtime, allowing shutdown.
#[derive(Debug)]
pub struct RuntimeHandle {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl RuntimeHandle {
    /// Signal the runtime to shut down gracefully.
    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// The runtime kernel.
///
/// Manages the tokio task that runs the main event loop,
/// consuming events from both the UI and engine channels.
pub struct Runtime {
    /// Tokio handle for the runtime task.
    #[allow(dead_code)]
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Bootstrap the runtime, returning communication channels and a handle.
    ///
    /// # Returns
    ///
    /// A tuple of:
    /// - `ui_tx`: sender for UI events
    /// - `ui_rx`: receiver for UI events (for `Runtime::run`)
    /// - `engine_tx`: sender for engine events (from QueryEngine → UI)
    /// - `engine_rx`: receiver for engine events (UI side)
    /// - `handle`: handle to shut down the runtime
    pub fn bootstrap() -> (
        mpsc::UnboundedSender<UiEvent>,
        mpsc::UnboundedReceiver<UiEvent>,
        mpsc::UnboundedSender<EngineEvent>,
        mpsc::UnboundedReceiver<EngineEvent>,
        RuntimeHandle,
    ) {
        let (ui_tx, ui_rx): (mpsc::UnboundedSender<UiEvent>, mpsc::UnboundedReceiver<UiEvent>) =
            mpsc::unbounded_channel();
        let (engine_tx, engine_rx): (
            mpsc::UnboundedSender<EngineEvent>,
            mpsc::UnboundedReceiver<EngineEvent>,
        ) = mpsc::unbounded_channel();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = RuntimeHandle { shutdown_tx };

        let task = tokio::spawn(async move {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    tracing::info!("runtime: received shutdown signal");
                }
            }
            tracing::info!("runtime: shutting down");
        });

        let _runtime = Self { task };
        (ui_tx, ui_rx, engine_tx, engine_rx, handle)
    }

    /// Run the main event loop, accumulating state into the supplied
    /// `UiState`. A frame should be rendered on each loop iteration
    /// from the shared state. Intents produced by the user (submits,
    /// interrupts) are pushed onto `intent_tx`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        mut engine_rx: mpsc::UnboundedReceiver<EngineEvent>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        state: Arc<Mutex<UiState>>,
        intent_tx: mpsc::UnboundedSender<UiIntent>,
    ) {
        use tokio::time::{interval, Duration};
        let mut tick = interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    tracing::info!("event loop: shutdown");
                    break;
                }

                Some(event) = ui_rx.recv() => {
                    let intent = Self::handle_ui_event(event, &state);
                    if !matches!(intent, UiIntent::None) {
                        let _ = intent_tx.send(intent);
                    }
                }

                Some(event) = engine_rx.recv() => {
                    Self::handle_engine_event(event, &state);
                }

                _ = tick.tick() => {
                    let _ = ui_tx.send(UiEvent::Tick);
                }
            }
        }
    }

    /// Apply a UI event to the shared state and return any intent
    /// produced (e.g. submit, interrupt).
    pub fn handle_ui_event(
        event: UiEvent,
        state: &Arc<Mutex<UiState>>,
    ) -> UiIntent {
        let mut state = state.lock().expect("ui state poisoned");
        match event {
            UiEvent::KeyPress { code, modifiers } => {
                tracing::debug!("ui: key press: {code}");
                state.handle_key(&code, &modifiers)
            }
            UiEvent::Tick => {
                state.tick_count = state.tick_count.wrapping_add(1);
                UiIntent::None
            }
            UiEvent::Resize { cols, rows } => {
                tracing::debug!("ui: resize to {cols}x{rows}");
                state.size = (cols, rows);
                UiIntent::None
            }
            #[cfg(feature = "crossterm")]
            UiEvent::Input(key_event) => {
                let (code, modifiers) = crossterm_key_to_portable(&key_event);
                state.handle_key(&code, &modifiers)
            }
            UiEvent::EngineUpdate(engine_event) => {
                state.apply_engine_event(engine_event);
                UiIntent::None
            }
        }
    }

    /// Apply an engine event directly (fast path used by `Runtime::run`).
    pub fn handle_engine_event(event: EngineEvent, state: &Arc<Mutex<UiState>>) {
        let mut state = state.lock().expect("ui state poisoned");
        state.apply_engine_event(event);
    }
}

/// Translate a crossterm key event into the portable (code, modifiers) pair
/// used by `UiState::handle_key`.
#[cfg(feature = "crossterm")]
fn crossterm_key_to_portable(key: &crossterm::event::KeyEvent) -> (String, Vec<String>) {
    use crossterm::event::{KeyCode, KeyModifiers};
    let code = match key.code {
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    };
    let mut modifiers = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers.push("Control".into());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers.push("Shift".into());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        modifiers.push("Alt".into());
    }
    (code, modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolResult;

    #[tokio::test]
    async fn bootstrap_returns_valid_channels() {
        let (ui_tx, _ui_rx, engine_tx, mut engine_rx, handle) = Runtime::bootstrap();
        assert!(ui_tx.send(UiEvent::Tick).is_ok());
        assert!(engine_tx.send(EngineEvent::SessionEnd).is_ok());
        let event = engine_rx.try_recv().unwrap();
        assert!(matches!(event, EngineEvent::SessionEnd));
        handle.shutdown();
    }

    #[tokio::test]
    async fn engine_event_received() {
        let (_ui_tx, _ui_rx, engine_tx, mut engine_rx, handle) = Runtime::bootstrap();
        let event = EngineEvent::TextDelta("hello".into());
        engine_tx.send(event).unwrap();
        let received = engine_rx.try_recv().unwrap();
        if let EngineEvent::TextDelta(text) = received {
            assert_eq!(text, "hello");
        } else {
            panic!("expected TextDelta");
        }
        handle.shutdown();
    }

    #[test]
    fn ui_event_key_press_debug() {
        let event = UiEvent::KeyPress {
            code: "Enter".into(),
            modifiers: vec![],
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("Enter"));
    }

    #[test]
    fn ui_event_resize() {
        let event = UiEvent::Resize { cols: 80, rows: 24 };
        assert!(matches!(event, UiEvent::Resize { cols: 80, rows: 24 }));
    }

    #[test]
    fn ui_event_tick() {
        let event = UiEvent::Tick;
        assert!(matches!(event, UiEvent::Tick));
    }

    #[test]
    fn handle_key_accumulates_input() {
        let mut state = UiState::new();
        assert_eq!(
            state.handle_key("h", &[]),
            UiIntent::None
        );
        state.handle_key("i", &[]);
        assert_eq!(state.input_buffer, "hi");
    }

    #[test]
    fn handle_key_backspace_removes_last_char() {
        let mut state = UiState::new();
        state.handle_key("h", &[]);
        state.handle_key("i", &[]);
        state.handle_key("Backspace", &[]);
        assert_eq!(state.input_buffer, "h");
    }

    #[test]
    fn handle_key_enter_submits_and_clears_buffer() {
        let mut state = UiState::new();
        state.handle_key("h", &[]);
        state.handle_key("i", &[]);
        let intent = state.handle_key("Enter", &[]);
        assert_eq!(intent, UiIntent::Submit("hi".into()));
        assert!(state.input_buffer.is_empty());
    }

    #[test]
    fn handle_key_ctrl_c_is_interrupt() {
        let mut state = UiState::new();
        state.handle_key("h", &[]);
        let intent = state.handle_key("c", &["Control".into()]);
        assert_eq!(intent, UiIntent::Interrupt);
    }

    #[test]
    fn handle_key_ctrl_u_clears_input() {
        let mut state = UiState::new();
        state.handle_key("h", &[]);
        state.handle_key("i", &[]);
        let intent = state.handle_key("u", &["Control".into()]);
        assert_eq!(intent, UiIntent::Cleared);
        assert!(state.input_buffer.is_empty());
    }

    #[test]
    fn apply_text_delta_streams_into_transcript() {
        let mut state = UiState::new();
        state.apply_engine_event(EngineEvent::TextDelta("Hello ".into()));
        state.apply_engine_event(EngineEvent::TextDelta("world".into()));
        assert_eq!(state.transcript.len(), 1);
        if let TranscriptEntry::AssistantText(text) = &state.transcript[0] {
            assert_eq!(text, "Hello world");
        } else {
            panic!("expected AssistantText");
        }
    }

    #[test]
    fn tool_call_round_trip_moves_to_transcript() {
        let mut state = UiState::new();
        state.apply_engine_event(EngineEvent::ToolCallStart {
            id: "tc1".into(),
            name: "read".into(),
        });
        assert_eq!(state.active_tool_calls.len(), 1);

        state.apply_engine_event(EngineEvent::ToolCallEnd {
            id: "tc1".into(),
            result: ToolResult {
                tool_call_id: "tc1".into(),
                output: "ok".into(),
                is_error: false,
            },
        });
        assert!(state.active_tool_calls.is_empty());
        match state.transcript.last().unwrap() {
            TranscriptEntry::ToolCall(record) => {
                assert_eq!(record.name, "read");
                assert_eq!(record.output, "ok");
                assert!(!record.is_error);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn permission_request_queues_prompt() {
        let mut state = UiState::new();
        state.apply_engine_event(EngineEvent::PermissionRequest {
            id: "req1".into(),
            tool: "bash".into(),
            args: serde_json::json!({"cmd": "ls"}),
        });
        let prompt = state.pop_permission().expect("permission queued");
        assert_eq!(prompt.tool, "bash");
        assert_eq!(prompt.id, "req1");
    }

    #[test]
    fn compact_boundary_becomes_separator() {
        let mut state = UiState::new();
        state.apply_engine_event(EngineEvent::CompactBoundary);
        assert!(matches!(
            state.transcript.last(),
            Some(TranscriptEntry::CompactBoundary)
        ));
    }

    #[test]
    fn session_end_clears_active_and_marks_state() {
        let mut state = UiState::new();
        state.apply_engine_event(EngineEvent::ToolCallStart {
            id: "tc1".into(),
            name: "read".into(),
        });
        state.apply_engine_event(EngineEvent::SessionEnd);
        assert!(state.session_ended);
        assert!(state.active_tool_calls.is_empty());
        assert!(matches!(
            state.transcript.last(),
            Some(TranscriptEntry::SessionEnd)
        ));
    }

    #[test]
    fn resize_updates_size_in_state() {
        let state = Arc::new(Mutex::new(UiState::new()));
        let intent = Runtime::handle_ui_event(
            UiEvent::Resize { cols: 120, rows: 40 },
            &state,
        );
        assert_eq!(intent, UiIntent::None);
        assert_eq!(state.lock().unwrap().size, (120, 40));
    }

    #[test]
    fn tick_increments_counter() {
        let state = Arc::new(Mutex::new(UiState::new()));
        let _ = Runtime::handle_ui_event(UiEvent::Tick, &state);
        let _ = Runtime::handle_ui_event(UiEvent::Tick, &state);
        assert_eq!(state.lock().unwrap().tick_count, 2);
    }

    #[test]
    fn engine_update_routes_through_state() {
        let state = Arc::new(Mutex::new(UiState::new()));
        let _ = Runtime::handle_ui_event(
            UiEvent::EngineUpdate(EngineEvent::TextDelta("hi".into())),
            &state,
        );
        let guarded = state.lock().unwrap();
        assert_eq!(guarded.transcript.len(), 1);
    }
}
