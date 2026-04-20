//! Loop detection for tool call sequences.
//!
//! Detects when the model enters a repetitive pattern of tool calls across
//! turns, which indicates a runaway agent stuck in a loop.
//!
//! ## Detection patterns
//!
//! - **Exact repeat**: The same sequence of tool calls repeats `N` times.
//! - **Alternation**: An A-B-A-B pattern repeats `N` times.
//!
//! Each tool call is fingerprinted by its name + SHA256 of the canonical
//! JSON input, so semantically identical calls with different field ordering
//! produce the same fingerprint.

use crate::types::ToolCall;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

/// Maximum number of turn fingerprints to retain in history.
const DEFAULT_MAX_HISTORY: usize = 32;

/// Default threshold: how many repetitions before triggering loop detection.
const DEFAULT_REPEAT_THRESHOLD: usize = 3;

/// A fingerprint of a single tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallFingerprint {
    /// Name of the tool.
    pub tool_name: String,
    /// SHA256 hex digest of the canonical JSON input.
    pub input_hash: String,
}

impl ToolCallFingerprint {
    /// Create a fingerprint from a tool call.
    #[must_use]
    pub fn from_tool_call(call: &ToolCall) -> Self {
        let canonical = canonicalize_json(&call.input);
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        Self {
            tool_name: call.name.clone(),
            input_hash: hash,
        }
    }
}

/// A sequence of tool call fingerprints from a single turn.
pub type TurnFingerprint = Vec<ToolCallFingerprint>;

/// Detects repetitive tool call patterns across conversation turns.
#[derive(Debug, Clone)]
pub struct LoopDetector {
    /// History of tool call fingerprints per turn (oldest first).
    history: VecDeque<TurnFingerprint>,
    /// Maximum number of turns to retain.
    max_history: usize,
    /// How many repetitions before a loop is declared.
    repeat_threshold: usize,
}

/// Result of checking for loops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopDetection {
    /// No loop detected.
    Ok,
    /// A loop was detected.
    LoopDetected {
        /// Period of the repeating pattern (1 = exact repeat, 2 = A-B, etc.).
        period: usize,
        /// How many times the pattern repeated.
        repetitions: usize,
        /// Human-readable description.
        description: String,
    },
}

impl LoopDetector {
    /// Create a new loop detector with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(DEFAULT_MAX_HISTORY),
            max_history: DEFAULT_MAX_HISTORY,
            repeat_threshold: DEFAULT_REPEAT_THRESHOLD,
        }
    }

    /// Create with custom thresholds.
    #[must_use]
    pub const fn with_thresholds(max_history: usize, repeat_threshold: usize) -> Self {
        Self {
            history: VecDeque::new(),
            max_history,
            repeat_threshold,
        }
    }

    /// Record the tool calls from a completed turn.
    pub fn record_turn(&mut self, calls: &[ToolCall]) {
        let fingerprint: TurnFingerprint = calls.iter().map(ToolCallFingerprint::from_tool_call).collect();
        self.history.push_back(fingerprint);
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
    }

    /// Record an empty turn (no tool calls).
    pub fn record_empty_turn(&mut self) {
        self.history.push_back(Vec::new());
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
    }

    /// Check whether the history contains a loop pattern.
    ///
    /// Checks for exact repeats (period 1) up to alternating patterns
    /// (period up to `max_period`).
    #[must_use]
    pub fn check(&self) -> LoopDetection {
        if self.history.len() < self.repeat_threshold {
            return LoopDetection::Ok;
        }

        let max_period = (self.history.len() / self.repeat_threshold).min(4);

        for period in 1..=max_period {
            if let Some(detection) = self.check_period(period) {
                return detection;
            }
        }

        LoopDetection::Ok
    }

    fn check_period(&self, period: usize) -> Option<LoopDetection> {
        let history: Vec<&TurnFingerprint> = self.history.iter().collect();
        if history.len() < period * self.repeat_threshold {
            return None;
        }

        // Check the most recent `period * repeat_threshold` turns
        let start = history.len() - period * self.repeat_threshold;
        let window = &history[start..];

        // Verify that each chunk of `period` turns matches the first chunk
        let first_chunk = &window[0..period];
        for i in 1..self.repeat_threshold {
            let chunk = &window[i * period..(i + 1) * period];
            if !chunks_equal(first_chunk, chunk) {
                return None;
            }
        }

        let description = if period == 1 {
            format!(
                "Exact repeat detected: same tool sequence repeated {} times",
                self.repeat_threshold
            )
        } else {
            format!(
                "Alternating loop detected: pattern of period {} repeated {} times",
                period, self.repeat_threshold
            )
        };

        Some(LoopDetection::LoopDetected {
            period,
            repetitions: self.repeat_threshold,
            description,
        })
    }

    /// Number of turns recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Whether no turns have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Clear the history.
    pub fn clear(&mut self) {
        self.history.clear();
    }
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

fn chunks_equal(a: &[&TurnFingerprint], b: &[&TurnFingerprint]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// Canonicalize a JSON value by sorting object keys recursively.
///
/// This ensures that `{"b":1,"a":2}` and `{"a":2,"b":1}` produce
/// the same fingerprint.
fn canonicalize_json(value: &serde_json::Value) -> String {
    let canonical = canonicalize_value(value.clone());
    canonical.to_string()
}

fn canonicalize_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let canonicalized: serde_json::Map<String, serde_json::Value> = entries
                .into_iter()
                .map(|(k, v)| (k, canonicalize_value(v)))
                .collect();
            serde_json::Value::Object(canonicalized)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(canonicalize_value).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("tc_{name}"),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn fingerprint_same_for_reordered_json() {
        let call1 = make_call("read", serde_json::json!({"path": "/tmp", "limit": 10}));
        let call2 = make_call("read", serde_json::json!({"limit": 10, "path": "/tmp"}));
        let fp1 = ToolCallFingerprint::from_tool_call(&call1);
        let fp2 = ToolCallFingerprint::from_tool_call(&call2);
        assert_eq!(fp1.input_hash, fp2.input_hash);
    }

    #[test]
    fn fingerprint_differs_for_different_inputs() {
        let call1 = make_call("read", serde_json::json!({"path": "/tmp"}));
        let call2 = make_call("read", serde_json::json!({"path": "/etc"}));
        let fp1 = ToolCallFingerprint::from_tool_call(&call1);
        let fp2 = ToolCallFingerprint::from_tool_call(&call2);
        assert_ne!(fp1.input_hash, fp2.input_hash);
    }

    #[test]
    fn detects_exact_repeat() {
        let mut detector = LoopDetector::with_thresholds(10, 3);
        let calls = vec![make_call("read", serde_json::json!({"path": "/tmp"}))];

        detector.record_turn(&calls);
        detector.record_turn(&calls);
        assert!(matches!(detector.check(), LoopDetection::Ok));

        detector.record_turn(&calls);
        assert!(
            matches!(detector.check(), LoopDetection::LoopDetected { period: 1, repetitions: 3, .. }),
            "expected loop detected, got {:?}",
            detector.check()
        );
    }

    #[test]
    fn detects_alternation() {
        let mut detector = LoopDetector::with_thresholds(10, 3);
        let calls_a = vec![make_call("read", serde_json::json!({"path": "/tmp"}))];
        let calls_b = vec![make_call("write", serde_json::json!({"path": "/tmp", "content": "x"}))];

        detector.record_turn(&calls_a);
        detector.record_turn(&calls_b);
        detector.record_turn(&calls_a);
        detector.record_turn(&calls_b);
        assert!(matches!(detector.check(), LoopDetection::Ok));

        detector.record_turn(&calls_a);
        detector.record_turn(&calls_b);
        assert!(
            matches!(detector.check(), LoopDetection::LoopDetected { period: 2, repetitions: 3, .. }),
            "expected alternation loop, got {:?}",
            detector.check()
        );
    }

    #[test]
    fn no_loop_for_diverse_calls() {
        let mut detector = LoopDetector::with_thresholds(10, 3);
        detector.record_turn(&[make_call("read", serde_json::json!({"path": "/a"}))]);
        detector.record_turn(&[make_call("read", serde_json::json!({"path": "/b"}))]);
        detector.record_turn(&[make_call("read", serde_json::json!({"path": "/c"}))]);
        assert!(matches!(detector.check(), LoopDetection::Ok));
    }

    #[test]
    fn empty_turns_reset_pattern() {
        let mut detector = LoopDetector::with_thresholds(10, 3);
        let calls = vec![make_call("read", serde_json::json!({"path": "/tmp"}))];

        detector.record_turn(&calls);
        detector.record_turn(&calls);
        detector.record_empty_turn(); // breaks the pattern
        detector.record_turn(&calls);
        assert!(matches!(detector.check(), LoopDetection::Ok));
    }

    #[test]
    fn respects_max_history() {
        let mut detector = LoopDetector::with_thresholds(4, 3);
        let calls = vec![make_call("read", serde_json::json!({"path": "/tmp"}))];

        detector.record_turn(&calls);
        detector.record_turn(&calls);
        detector.record_turn(&calls);
        assert!(matches!(detector.check(), LoopDetection::LoopDetected { .. }));

        // Add enough diverse turns to push the loop out of history
        detector.record_turn(&[make_call("write", serde_json::json!({"path": "/tmp"}))]);
        detector.record_turn(&[make_call("write", serde_json::json!({"path": "/tmp"}))]);
        assert!(matches!(detector.check(), LoopDetection::Ok));
    }
}
