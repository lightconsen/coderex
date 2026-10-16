//! PII scrubbing for telemetry events.
//!
//! Sanitizes sensitive data from event payloads before storage.
//! Covers: email addresses, IP addresses, file paths containing usernames,
//! API keys and other credential shapes, and an exclusion set of field names
//! that are dropped from payloads entirely.

use regex::Regex;

/// JSON object keys that must never appear in an uploaded payload,
/// regardless of which event carried them (case-insensitive).
pub const EXCLUDED_FIELDS: [&str; 8] = [
    "api_key",
    "apikey",
    "authorization",
    "password",
    "secret",
    "token",
    "cookie",
    "set-cookie",
];

/// Whether a JSON object key is on the never-send list.
fn is_excluded_field(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    EXCLUDED_FIELDS.contains(&lower.as_str())
}

/// Scrubber that regex-replaces sensitive patterns.
#[derive(Debug, Clone)]
pub struct Scrubber {
    patterns: Vec<(Regex, String)>,
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrubber {
    /// Create a new scrubber with the default pattern set.
    pub fn new() -> Self {
        let mut patterns = Vec::new();

        // Email addresses
        if let Ok(re) = Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}") {
            patterns.push((re, "<EMAIL>".to_string()));
        }

        // IPv4 addresses
        if let Ok(re) = Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b") {
            patterns.push((re, "<IP>".to_string()));
        }

        // IPv6 addresses (simplified — matches common forms)
        if let Ok(re) = Regex::new(r"\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b") {
            patterns.push((re, "<IP>".to_string()));
        }

        // PEM private key blocks
        if let Ok(re) = Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----") {
            patterns.push((re, "<PRIVATE_KEY>".to_string()));
        }

        // Bearer / authorization header values
        if let Ok(re) = Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}") {
            patterns.push((re, "<AUTH_TOKEN>".to_string()));
        }

        // JSON Web Tokens (header.payload.signature)
        if let Ok(re) =
            Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
        {
            patterns.push((re, "<JWT>".to_string()));
        }

        // GitHub tokens (ghp_, gho_, ghu_, ghs_, ghr_)
        if let Ok(re) = Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // AWS access key IDs
        if let Ok(re) = Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Slack tokens
        if let Ok(re) = Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b") {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Generic API keys / tokens (labelled, base64-like)
        if let Ok(re) = Regex::new(
            r#"\b(?:sk-|api[_-]?key|token|secret)[_-]?[=:]?\s*["']?[a-zA-Z0-9_\-]{20,}["']?"#,
        ) {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Home directory paths — attempt to catch /home/username or /Users/username
        if let Ok(re) = Regex::new(r"(/(?:home|Users)/)[^/\s]+") {
            patterns.push((re, "${1}<USER>".to_string()));
        }

        Self { patterns }
    }

    /// Scrub sensitive data from a string, returning the sanitized version.
    pub fn scrub(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (re, replacement) in &self.patterns {
            result = re.replace_all(&result, replacement.as_str()).to_string();
        }
        result
    }

    /// Recursively scrub a JSON value in place.
    ///
    /// Strings go through the same pattern set as [`Self::scrub`]. Object keys
    /// on the [`EXCLUDED_FIELDS`] list are *removed* (not blanked), so the field
    /// never reaches the wire even if it slipped through event-level scrubbing.
    pub fn scrub_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => *s = self.scrub(s),
            serde_json::Value::Array(items) => {
                for item in items.iter_mut() {
                    self.scrub_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                map.retain(|key, _| !is_excluded_field(key));
                for (_, child) in map.iter_mut() {
                    self.scrub_json(child);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    /// Scrub a [`TelemetryEvent`] in place.
    pub fn scrub_event(&self, event: &mut crate::TelemetryEvent) {
        use crate::TelemetryEvent;
        match event {
            TelemetryEvent::Error { message, .. } => {
                *message = self.scrub(message);
            }
            TelemetryEvent::ApiCall {
                provider, model, ..
            } => {
                *provider = self.scrub(provider);
                *model = self.scrub(model);
            }
            TelemetryEvent::ToolUsed { tool_name, .. } => {
                *tool_name = self.scrub(tool_name);
            }
            TelemetryEvent::SessionStart { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_email() {
        let scrubber = Scrubber::new();
        let text = "Contact alice@123.com for help";
        assert_eq!(scrubber.scrub(text), "Contact <EMAIL> for help");
    }

    #[test]
    fn scrub_ipv4() {
        let scrubber = Scrubber::new();
        let text = "Server at 192.168.1.1 responded";
        assert_eq!(scrubber.scrub(text), "Server at <IP> responded");
    }

    #[test]
    fn scrub_home_path() {
        let scrubber = Scrubber::new();
        let text = "File at /home/alice/project/src/main.rs";
        assert_eq!(
            scrubber.scrub(text),
            "File at /home/<USER>/project/src/main.rs"
        );
    }

    #[test]
    fn scrub_api_key() {
        let scrubber = Scrubber::new();
        let text = "api_key=sk-abc123def456ghi789jkl012mno345";
        assert_eq!(scrubber.scrub(text), "<API_KEY>");
    }

    #[test]
    fn scrub_event_error_message() {
        let scrubber = Scrubber::new();
        let mut event = crate::TelemetryEvent::Error {
            error_type: "io".into(),
            message: "failed to read /home/bob/.ssh/id_rsa for bob@123.com".into(),
            timestamp: chrono::Utc::now(),
        };
        scrubber.scrub_event(&mut event);
        if let crate::TelemetryEvent::Error { message, .. } = event {
            assert!(!message.contains("bob@123.com"));
            assert!(message.contains("<EMAIL>"));
            assert!(message.contains("<USER>"));
        }
    }

    #[test]
    fn scrub_github_token() {
        let scrubber = Scrubber::new();
        let text = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 used";
        let scrubbed = scrubber.scrub(text);
        assert!(!scrubbed.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"));
        assert!(scrubbed.contains("<API_KEY>"));
    }

    #[test]
    fn scrub_aws_access_key() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("key AKIAIOSFODNN7EXAMPLE here");
        assert!(!scrubbed.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(scrubbed.contains("<API_KEY>"));
    }

    #[test]
    fn scrub_bearer_token() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("Authorization: Bearer abc123DEF456ghi789");
        assert!(!scrubbed.contains("abc123DEF456ghi789"));
        assert!(scrubbed.contains("<AUTH_TOKEN>"));
    }

    #[test]
    fn scrub_private_key_header() {
        let scrubber = Scrubber::new();
        let scrubbed = scrubber.scrub("-----BEGIN RSA PRIVATE KEY-----\nMIIE...");
        assert!(scrubbed.contains("<PRIVATE_KEY>"));
    }

    #[test]
    fn scrub_jwt() {
        let scrubber = Scrubber::new();
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let scrubbed = scrubber.scrub(&format!("token={token}"));
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("<JWT>"));
    }

    #[test]
    fn excluded_field_match_is_case_insensitive() {
        assert!(is_excluded_field("api_key"));
        assert!(is_excluded_field("API_KEY"));
        assert!(is_excluded_field("Authorization"));
        assert!(!is_excluded_field("model"));
    }

    /// The exclusion set drops sensitive *keys* outright — the field must not
    /// appear on the wire even as an empty string.
    #[test]
    fn scrub_json_drops_excluded_fields() {
        let scrubber = Scrubber::new();
        let mut value = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "api_key": "sk-abcdefghijklmnopqrstuvwxyz",
            "nested": { "Authorization": "Bearer xyz", "keep": 1 }
        });
        scrubber.scrub_json(&mut value);

        assert!(value.get("api_key").is_none());
        assert!(value["nested"].get("Authorization").is_none());
        assert_eq!(value["model"], serde_json::json!("claude-sonnet-4-6"));
        assert_eq!(value["nested"]["keep"], serde_json::json!(1));
    }

    #[test]
    fn scrub_json_scrubs_nested_strings() {
        let scrubber = Scrubber::new();
        let mut value = serde_json::json!({
            "messages": ["mail alice@example.com", "at /Users/alice/project"],
        });
        scrubber.scrub_json(&mut value);
        let rendered = value.to_string();
        assert!(!rendered.contains("alice@example.com"));
        assert!(!rendered.contains("/Users/alice"));
        assert!(rendered.contains("<EMAIL>"));
        assert!(rendered.contains("<USER>"));
    }
}
