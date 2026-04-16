//! Runtime kernel — channels, event loop, and bootstrap.
//!
//! Phase 0.4: Defines `UiEvent`, `Runtime::bootstrap()`, and the main event loop skeleton.

use crate::types::EngineEvent;
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

        // Spawn the main event loop in the background.
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

    /// Run the main event loop.
    ///
    /// Consumes events from both the UI and engine channels,
    /// driving the UI rendering loop.
    ///
    /// ```text
    /// loop {
    ///     tokio::select! {
    ///         Some(event) = ui_rx.recv() => handle_ui_event(event),
    ///         Some(event) = engine_rx.recv() => handle_engine_event(event),
    ///         _ = tick_interval.tick() => emit UiEvent::Tick,
    ///         _ = shutdown => break,
    ///     }
    ///     render_frame();
    /// }
    /// ```
    pub async fn run(
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        mut ui_rx: mpsc::UnboundedReceiver<UiEvent>,
        mut engine_rx: mpsc::UnboundedReceiver<EngineEvent>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        use tokio::time::{interval, Duration};
        let mut tick = interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                biased;

                // Shutdown takes priority
                _ = &mut shutdown_rx => {
                    tracing::info!("event loop: shutdown");
                    break;
                }

                // UI events
                Some(event) = ui_rx.recv() => {
                    Self::handle_ui_event(event, &ui_tx);
                }

                // Engine events
                Some(event) = engine_rx.recv() => {
                    Self::handle_engine_event(event);
                }

                // Periodic tick
                _ = tick.tick() => {
                    let _ = ui_tx.send(UiEvent::Tick);
                }
            }

            // After every event, a frame would be rendered.
            // In a real implementation: terminal.draw(|frame| ...)?;
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_ui_event(
        event: UiEvent,
        _ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ) {
        match event {
            UiEvent::KeyPress { ref code, .. } => {
                tracing::debug!("ui: key press: {code}");
                // TODO: update input buffer, handle shortcuts, submit to QueryEngine
            }
            UiEvent::Tick => {
                // TODO: drive animations, spinner, buddy idle presence
            }
            UiEvent::Resize { cols, rows } => {
                tracing::debug!("ui: resize to {cols}x{rows}");
                // TODO: recalculate layout
            }
            #[cfg(feature = "crossterm")]
            UiEvent::Input(_) => {
                // TODO: crossterm input handling
            }
            UiEvent::EngineUpdate(_) => {
                // EngineUpdate is handled at the top level of the event loop,
                // not via handle_ui_event. This branch exists for exhaustiveness.
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_engine_event(event: EngineEvent) {
        match event {
            EngineEvent::TextDelta(ref text) => {
                tracing::debug!("engine: text delta: {} chars", text.len());
                // TODO: stream text to UI
            }
            EngineEvent::ToolCallStart { ref id, ref name } => {
                tracing::debug!("engine: tool call start: {name} ({id})");
                // TODO: show tool call in UI
            }
            EngineEvent::ToolCallEnd { ref id, .. } => {
                tracing::debug!("engine: tool call end: {id}");
                // TODO: show tool result in UI
            }
            EngineEvent::CompactBoundary => {
                tracing::debug!("engine: compact boundary");
                // TODO: insert visual separator
            }
            EngineEvent::PermissionRequest { ref tool, .. } => {
                tracing::debug!("engine: permission request: {tool}");
                // TODO: show permission dialog
            }
            EngineEvent::ElicitationRequest { ref message, .. } => {
                tracing::debug!("engine: elicitation: {message}");
                // TODO: show elicitation prompt
            }
            EngineEvent::SessionEnd => {
                tracing::info!("engine: session ended");
                // TODO: finalize UI state
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_returns_valid_channels() {
        let (ui_tx, _ui_rx, engine_tx, mut engine_rx, handle) = Runtime::bootstrap();
        assert!(ui_tx.send(UiEvent::Tick).is_ok());
        assert!(engine_tx.send(EngineEvent::SessionEnd).is_ok());
        // Verify engine_rx can receive
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
}
