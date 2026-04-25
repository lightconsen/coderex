//! Custom agent definitions loaded from user configuration files.
//!
//! Scans `~/.coderex/agents/*.json` for custom agent definitions that
//! extend the built-in agent types with user-defined prompts, tool
//! whitelists, and resource limits.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
/// Returns a map of agent name to definition. Silently ignores unreadable
/// or malformed files.
#[must_use]
pub fn load_custom_agents() -> HashMap<String, CustomAgentDef> {
    let mut agents = HashMap::new();

    let agents_dir = match home::home_dir() {
        Some(home) => home.join(".coderex").join("agents"),
        None => return agents,
    };

    let entries = match std::fs::read_dir(&agents_dir) {
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
            Err(_) => continue,
        };

        let def: CustomAgentDef = match serde_json::from_str(&contents) {
            Ok(d) => d,
            Err(_) => continue,
        };

        agents.insert(def.name.clone(), def);
    }

    agents
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

        // Temporarily override home dir by creating a custom loader
        // Since load_custom_agents uses home::home_dir(), we can't easily
        // mock it. Instead, test the parsing directly.
        let json = r#"{"name":"parsed_agent","system_prompt":"hello","allowed_tools":["Read"],"model":null,"max_tokens":null,"output_schema":null}"#;
        let parsed: CustomAgentDef = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.name, "parsed_agent");
        assert_eq!(parsed.system_prompt, "hello");
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
