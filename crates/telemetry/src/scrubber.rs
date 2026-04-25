//! PII scrubbing for telemetry events.
//!
//! Sanitizes sensitive data from event payloads before storage.
//! Covers: email addresses, IP addresses, file paths containing usernames,
//! and API keys.

use regex::Regex;

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
        if let Ok(re) = Regex::new(r#"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"#) {
            patterns.push((re, "<EMAIL>".to_string()));
        }

        // IPv4 addresses
        if let Ok(re) = Regex::new(r#"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b"#) {
            patterns.push((re, "<IP>".to_string()));
        }

        // IPv6 addresses (simplified — matches common forms)
        if let Ok(re) = Regex::new(r#"\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b"#) {
            patterns.push((re, "<IP>".to_string()));
        }

        // API keys — common patterns (hex strings, base64-like)
        if let Ok(re) = Regex::new(
            r#"\b(?:sk-|api[_-]?key|token|secret)[_-]?[=:]?\s*["']?[a-zA-Z0-9_\-]{20,}["']?"#,
        ) {
            patterns.push((re, "<API_KEY>".to_string()));
        }

        // Home directory paths — attempt to catch /home/username or /Users/username
        if let Ok(re) = Regex::new(r#"(/(?:home|Users)/)[^/\s]+"#) {
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
            _ => {}
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
}
