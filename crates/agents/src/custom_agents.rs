//! Custom agent definitions loaded from user configuration files.
//!
//! Scans `~/.coderex/agents/*.json` for custom agent definitions that
//! extend the built-in agent types with user-defined prompts, tool
//! whitelists, and resource limits.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// Why one custom agent file was skipped.
///
/// Returned by [`load_custom_agents_from_dir_with_warnings`] so callers (and
/// tests) can see the decision without depending on tracing's per-callsite
/// interest cache, which makes a warning disappear when another test in the
/// same binary evaluates the callsite first with no subscriber installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLoadWarning {
    /// The file that was skipped.
    pub path: PathBuf,
    /// Why it was skipped.
    pub reason: String,
}

impl std::fmt::Display for AgentLoadWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "skipping custom agent file: {} path={}",
            self.reason,
            self.path.display()
        )
    }
}

/// Load all custom agent definitions from `~/.coderex/agents/*.json`.
///
/// Returns a map of agent name to definition. An unreadable or malformed
/// file is skipped with a warning naming its path and the error, so a typo
/// in a definition does not make the agent vanish without explanation.
#[must_use]
pub fn load_custom_agents() -> HashMap<String, CustomAgentDef> {
    home::home_dir().map_or_else(HashMap::new, |home| {
        load_custom_agents_from_dir(&home.join(".coderex").join("agents"))
    })
}

/// Load custom agent definitions from an explicit directory.
///
/// This is the testable form of [`load_custom_agents`]: callers that manage
/// their own configuration directory (tests, embedders, project-local agent
/// sets) do not have to pretend to be `$HOME`.
///
/// Files that cannot be read, do not parse, or fail validation are skipped
/// with a warning naming the path; the remaining files still load.
pub fn load_custom_agents_from_dir(agents_dir: &Path) -> HashMap<String, CustomAgentDef> {
    let (agents, warnings) = load_custom_agents_from_dir_with_warnings(agents_dir);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    agents
}

/// Load custom agent definitions and return why any file was skipped.
///
/// This is the testable form: see [`AgentLoadWarning`]. It is also the form
/// an embedding caller should use when it wants to show the skips itself.
#[must_use]
pub fn load_custom_agents_from_dir_with_warnings(
    agents_dir: &Path,
) -> (HashMap<String, CustomAgentDef>, Vec<AgentLoadWarning>) {
    let mut agents = HashMap::new();
    let mut warnings = Vec::new();

    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return (agents, warnings);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(error) => {
                warnings.push(AgentLoadWarning {
                    path,
                    reason: format!("cannot read it: {error}"),
                });
                continue;
            }
        };

        let def: CustomAgentDef = match serde_json::from_str(&contents) {
            Ok(d) => d,
            Err(error) => {
                warnings.push(AgentLoadWarning {
                    path,
                    reason: format!("not a valid custom agent definition: {error}"),
                });
                continue;
            }
        };

        if let Err(error) = validate_custom_agent(&def) {
            warnings.push(AgentLoadWarning {
                path,
                reason: format!("invalid definition: {error}"),
            });
            continue;
        }

        agents.insert(def.name.clone(), def);
    }

    (agents, warnings)
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
pub fn get_custom_agent<'a, S: std::hash::BuildHasher>(
    name: &str,
    agents: &'a HashMap<String, CustomAgentDef, S>,
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

        let (loaded, warnings) = load_custom_agents_from_dir_with_warnings(tmp.path());

        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("valid_agent"));

        assert_eq!(warnings.len(), 4, "{warnings:?}");
        for file in [
            "broken.json",
            "empty-name.json",
            "empty-prompt.json",
            "unreadable.json",
        ] {
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.path.to_string_lossy().ends_with(file)),
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
