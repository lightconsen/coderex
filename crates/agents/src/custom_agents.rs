//! Custom agent definitions loaded from user configuration files.
//!
//! Scans `~/.coderex/agents/*.json` for custom agent definitions that
//! extend the built-in agent types with user-defined prompts, tool
//! whitelists, and resource limits.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// A user-defined custom agent loaded from a JSON config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAgentDef {
    /// Unique name for the custom agent.
    pub name: String,
    /// System prompt for the agent.
    pub system_prompt: String,
    /// Tool whitelist — only these tools are available.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Default model override (optional).
    pub model: Option<String>,
    /// Maximum tokens (optional).
    pub max_tokens: Option<u32>,
    /// JSON schema for validating agent output (optional).
    pub output_schema: Option<serde_json::Value>,
}

/// Load all custom agent definitions from `~/.coderex/agents/*.json`.
///
/// Returns a map of agent name to definition. An unreadable or malformed
/// file is skipped with a warning naming its path and the error, so a typo
/// in a definition does not make the agent vanish without explanation.
#[must_use]
pub fn load_custom_agents() -> HashMap<String, CustomAgentDef> {
    match home::home_dir() {
        Some(home) => load_custom_agents_from_dir(&home.join(".coderex").join("agents")),
        None => HashMap::new(),
    }
}

/// Load custom agent definitions from an explicit directory.
///
/// This is the testable form of [`load_custom_agents`]: callers that manage
/// their own configuration directory (tests, embedders, project-local agent
/// sets) do not have to pretend to be `$HOME`.
///
/// Files that cannot be read, do not parse, or fail validation are skipped
/// with a warning naming the path; the remaining files still load.
#[must_use]
pub fn load_custom_agents_from_dir(agents_dir: &Path) -> HashMap<String, CustomAgentDef> {
    let mut agents = HashMap::new();

    let entries = match std::fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(_) => return agents,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "skipping custom agent file: cannot read it"
                );
                continue;
            }
        };

        let def: CustomAgentDef = match serde_json::from_str(&contents) {
            Ok(d) => d,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "skipping custom agent file: not a valid custom agent definition"
                );
                continue;
            }
        };

        if let Err(error) = validate_custom_agent(&def) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "skipping custom agent file: invalid definition"
            );
            continue;
        }

        agents.insert(def.name.clone(), def);
    }

    agents
}

/// Reject a definition that parses but cannot be used.
///
/// A definition with no name cannot be selected, and one with no system
/// prompt silently behaves like a generic agent. Both are reported rather
/// than dropped so the author can fix the file.
fn validate_custom_agent(def: &CustomAgentDef) -> Result<(), String> {
    if def.name.trim().is_empty() {
        return Err("the definition has an empty `name`".into());
    }
    if def.system_prompt.trim().is_empty() {
        return Err(format!("agent '{}' has an empty `system_prompt`", def.name));
    }
    Ok(())
}

/// Resolve a custom agent definition by name.
#[must_use]
pub fn get_custom_agent<'a>(
    name: &str,
    agents: &'a HashMap<String, CustomAgentDef>,
) -> Option<&'a CustomAgentDef> {
    agents.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_custom_agents_reads_json_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        let def = CustomAgentDef {
            name: "test_agent".into(),
            system_prompt: "You are a test agent.".into(),
            allowed_tools: vec!["Read".into()],
            model: Some("claude-sonnet".into()),
            max_tokens: Some(1024),
            output_schema: None,
        };

        let mut file = std::fs::File::create(agents_dir.join("test.json")).unwrap();
        file.write_all(serde_json::to_string(&def).unwrap().as_bytes())
            .unwrap();

        let loaded = load_custom_agents_from_dir(&agents_dir);
        let loaded_def = loaded.get("test_agent").expect("agent was not loaded");
        assert_eq!(loaded_def.system_prompt, "You are a test agent.");
        assert_eq!(loaded_def.allowed_tools, vec!["Read"]);
        assert_eq!(loaded_def.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(loaded_def.max_tokens, Some(1024));
    }

    #[test]
    fn missing_directory_loads_nothing() {
        let loaded =
            load_custom_agents_from_dir(std::path::Path::new("/definitely/not/a/real/dir"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn malformed_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{ not json").unwrap();
        std::fs::write(tmp.path().join("ignored.txt"), "not an agent").unwrap();
        assert!(load_custom_agents_from_dir(tmp.path()).is_empty());
    }

    /// Test subscriber that records warning events on the current thread.
    /// Mirrors the pattern the plugins crate uses for the same purpose.
    #[derive(Clone, Default)]
    struct WarningLog(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl WarningLog {
        fn warnings(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl tracing::Subscriber for WarningLog {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor<'a>(&'a mut Vec<String>);

            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push(format!("{}={value:?}", field.name()));
                }
            }

            let mut fields = Vec::new();
            event.record(&mut Visitor(&mut fields));
            self.0.lock().unwrap().push(fields.join(" "));
        }
    }

    /// A file that cannot be read or parsed, and a definition that parses but
    /// is invalid, are each reported by path; the valid file still loads.
    #[test]
    fn bad_files_warn_and_valid_definitions_still_load() {
        let tmp = tempfile::tempdir().unwrap();

        let valid = CustomAgentDef {
            name: "valid_agent".into(),
            system_prompt: "prompt".into(),
            allowed_tools: vec![],
            model: None,
            max_tokens: None,
            output_schema: None,
        };
        std::fs::write(
            tmp.path().join("valid.json"),
            serde_json::to_string(&valid).unwrap(),
        )
        .unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{ not json").unwrap();
        std::fs::write(
            tmp.path().join("empty-name.json"),
            serde_json::json!({"name": "  ", "system_prompt": "prompt"}).to_string(),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("empty-prompt.json"),
            serde_json::json!({"name": "no_prompt", "system_prompt": ""}).to_string(),
        )
        .unwrap();
        // Reading a directory as a file fails, which exercises the read path.
        std::fs::create_dir(tmp.path().join("unreadable.json")).unwrap();

        let log = WarningLog::default();
        let loaded = tracing::subscriber::with_default(log.clone(), || {
            load_custom_agents_from_dir(tmp.path())
        });

        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("valid_agent"));

        let warnings = log.warnings();
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        for file in [
            "broken.json",
            "empty-name.json",
            "empty-prompt.json",
            "unreadable.json",
        ] {
            assert!(
                warnings.iter().any(|warning| warning.contains(file)),
                "no warning named {file}: {warnings:?}"
            );
        }
    }

    #[test]
    fn get_custom_agent_finds_existing() {
        let mut agents = HashMap::new();
        agents.insert(
            "my_agent".into(),
            CustomAgentDef {
                name: "my_agent".into(),
                system_prompt: "prompt".into(),
                allowed_tools: vec![],
                model: None,
                max_tokens: None,
                output_schema: None,
            },
        );

        assert!(get_custom_agent("my_agent", &agents).is_some());
        assert!(get_custom_agent("missing", &agents).is_none());
    }
}
