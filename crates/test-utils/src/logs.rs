//! Capture `tracing` events produced during a test.
//!
//! Some behavior is only observable in a log line: "this hook will never run",
//! "the response named a request nothing is waiting on", "the path entry was
//! dropped because it was relative". Without a way to assert on the message, a
//! mutation that deletes the log call survives the test suite, which is what
//! `docs/mutation.md` records.
//!
//! The capture is installed with [`tracing::subscriber::set_default`], so it
//! is scoped to the current thread and does not race other tests in the same
//! process. Spawned threads need their own capture; [`capture_logs_async`]
//! covers the common case of a future polled on a current-thread runtime.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

/// One captured event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    /// The event level.
    pub level: Level,
    /// The module that logged it.
    pub target: String,
    /// The `message` field, when the event has one.
    pub message: String,
    /// Every other field, rendered as `key=value`, separated by spaces.
    pub fields: String,
}

impl LogEvent {
    /// The message and the other fields, for substring assertions.
    #[must_use]
    pub fn text(&self) -> String {
        if self.fields.is_empty() {
            self.message.clone()
        } else {
            format!("{} {}", self.message, self.fields)
        }
    }
}

/// Collects events into a shared buffer.
#[derive(Default)]
struct Collector {
    events: Arc<Mutex<Vec<LogEvent>>>,
}

impl<S: Subscriber> Layer<S> for Collector {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let recorded = LogEvent {
            level: *metadata.level(),
            target: metadata.target().to_string(),
            message: visitor.message,
            fields: visitor.fields.join(" "),
        };
        if let Ok(mut events) = self.events.lock() {
            events.push(recorded);
        }
    }

    fn on_new_span(&self, _attrs: &Attributes<'_>, _id: &Id, _context: Context<'_, S>) {}
    fn on_record(&self, _span: &Id, _values: &Record<'_>, _context: Context<'_, S>) {}
    fn enabled(&self, _metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        true
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Vec<String>,
}

impl FieldVisitor {
    /// Record one field. An event may carry an explicit field literally named
    /// `message` as well as the implicit format message; keep both rather than
    /// letting whichever was recorded last win.
    fn record(&mut self, field: &Field, debug: String, display: String) {
        if field.name() == "message" && self.message.is_empty() {
            self.message = display;
        } else {
            self.fields.push(format!("{}={debug}", field.name()));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        self.record(field, rendered.clone(), rendered);
    }
}

/// A live capture. Drop the guard (or let it fall out of scope) to stop
/// capturing on this thread.
pub struct LogCapture {
    events: Arc<Mutex<Vec<LogEvent>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl std::fmt::Debug for LogCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `DefaultGuard` has no `Debug`; the captured events are what a
        // failing assertion wants to print anyway.
        formatter
            .debug_struct("LogCapture")
            .field("events", &self.events())
            .finish()
    }
}

impl LogCapture {
    /// Everything captured so far.
    #[must_use]
    pub fn events(&self) -> Vec<LogEvent> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }

    /// The messages captured so far, in order.
    #[must_use]
    pub fn messages(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .map(|event| event.text())
            .collect()
    }

    /// Whether any captured message (with its other fields) contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.messages()
            .iter()
            .any(|message| message.contains(needle))
    }

    /// Whether any captured event is at `level` and contains `needle`.
    #[must_use]
    pub fn contains_at(&self, level: Level, needle: &str) -> bool {
        self.events()
            .iter()
            .any(|event| event.level == level && event.text().contains(needle))
    }

    /// Forget everything captured so far.
    pub fn clear(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
    }
}

/// Start capturing `tracing` events on the current thread.
///
/// The default dispatcher is replaced for the lifetime of the returned guard,
/// so this must not be combined with another [`tracing::subscriber::set_default`]
/// in the same scope.
#[must_use]
pub fn capture_logs() -> LogCapture {
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector {
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry().with(collector);
    let guard = tracing::subscriber::set_default(subscriber);
    LogCapture {
        events,
        _guard: guard,
    }
}

/// Run `future` to completion on a current-thread runtime while capturing
/// logs, returning its output and the captured events.
///
/// A current-thread runtime keeps the future on this thread, so the scoped
/// dispatcher applies to it. Code that spawns work onto a multi-thread
/// runtime needs its own capture around the worker.
pub fn capture_logs_async<F: std::future::Future>(future: F) -> (F::Output, Vec<LogEvent>) {
    let capture = capture_logs();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    let output = runtime.block_on(future);
    let events = capture.events();
    (output, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logged_message_is_captured_with_its_fields() {
        let capture = capture_logs();
        tracing::warn!(tool = "read_file", "the tool name is not unique");
        assert!(capture.contains("the tool name is not unique"));
        assert!(capture.contains("tool=read_file") || capture.contains("tool=\"read_file\""));
        let events = capture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::WARN);
    }

    #[test]
    fn the_level_filter_matches_only_that_level() {
        let capture = capture_logs();
        tracing::info!("an informational line");
        assert!(!capture.contains_at(Level::ERROR, "informational"));
        assert!(capture.contains_at(Level::INFO, "informational"));
    }

    #[test]
    fn clear_drops_what_was_captured() {
        let capture = capture_logs();
        tracing::info!("first");
        capture.clear();
        assert!(!capture.contains("first"));
    }

    #[test]
    fn the_async_helper_captures_events_from_the_future() {
        let (value, events) = capture_logs_async(async {
            tracing::warn!("from the future");
            7
        });
        assert_eq!(value, 7);
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("from the future")),
            "{events:?}"
        );
    }
}
