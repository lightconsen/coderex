//! Resolution of the `--agent <name>` flag against custom agent definitions.
//!
//! A named agent is a JSON file under `~/.coderex/agents/`. Selecting one for
//! the session applies its system prompt and, when the definition provides
//! one, its tool whitelist. Names are resolved before the session starts so a
//! typo fails with the list of configured names instead of silently running
//! the default agent.

use coderex_agents::custom_agents::{CustomAgentDef, load_custom_agents};
use std::collections::HashMap;
use std::fmt;

/// A custom agent resolved from its definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    /// The definition's name.
    pub name: String,
    /// The system prompt the session should use.
    pub system_prompt: String,
    /// Tools the definition restricts the session to. Empty means no
    /// restriction beyond the session default.
    pub allowed_tools: Vec<String>,
    /// Optional model override from the definition.
    pub model: Option<String>,
    /// Optional token limit from the definition.
    pub max_tokens: Option<u32>,
}

impl ResolvedAgent {
    fn from_definition(def: &CustomAgentDef) -> Self {
        Self {
            name: def.name.clone(),
            system_prompt: def.system_prompt.clone(),
            allowed_tools: def.allowed_tools.clone(),
            model: def.model.clone(),
            max_tokens: def.max_tokens,
        }
    }
}

/// Why a name could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgent {
    /// The requested name.
    pub name: String,
    /// Configured custom agent names, sorted.
    pub available: Vec<String>,
}

impl fmt::Display for UnknownAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.available.is_empty() {
            write!(
                f,
                "unknown agent {:?}: no custom agents are configured; add a JSON definition \
                 under ~/.coderex/agents/ (for example ~/.coderex/agents/{}.json)",
                self.name, self.name
            )
        } else {
            write!(
                f,
                "unknown agent {:?}; available custom agents: {}",
                self.name,
                self.available.join(", ")
            )
        }
    }
}

impl std::error::Error for UnknownAgent {}

/// Resolve a custom agent name from `~/.coderex/agents/`.
///
/// # Errors
/// Returns [`UnknownAgent`] when no definition matches. The error lists the
/// configured names so the user can correct the flag without guessing.
pub fn resolve_agent(name: &str) -> Result<ResolvedAgent, UnknownAgent> {
    resolve_agent_in(name, &load_custom_agents())
}

/// Resolve a name against an explicit definition map.
///
/// Used by tests and embedders that supply their own agent set.
pub fn resolve_agent_in(
    name: &str,
    agents: &HashMap<String, CustomAgentDef>,
) -> Result<ResolvedAgent, UnknownAgent> {
    agents
        .get(name)
        .map(ResolvedAgent::from_definition)
        .ok_or_else(|| {
            let mut available: Vec<String> = agents.keys().cloned().collect();
            available.sort();
            UnknownAgent {
                name: name.to_string(),
                available,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str) -> CustomAgentDef {
        CustomAgentDef {
            name: name.into(),
            system_prompt: format!("You are {name}."),
            allowed_tools: vec!["Read".into()],
            model: Some("claude-test".into()),
            max_tokens: Some(1000),
            output_schema: None,
        }
    }

    #[test]
    fn resolves_definition_fields() {
        let mut agents = HashMap::new();
        agents.insert("reviewer".into(), def("reviewer"));

        let resolved = resolve_agent_in("reviewer", &agents).unwrap();
        assert_eq!(resolved.name, "reviewer");
        assert_eq!(resolved.system_prompt, "You are reviewer.");
        assert_eq!(resolved.allowed_tools, vec!["Read"]);
        assert_eq!(resolved.model.as_deref(), Some("claude-test"));
        assert_eq!(resolved.max_tokens, Some(1000));
    }

    #[test]
    fn unknown_name_lists_available_names() {
        let mut agents = HashMap::new();
        agents.insert("reviewer".into(), def("reviewer"));
        agents.insert("advisor".into(), def("advisor"));

        let error = resolve_agent_in("typo", &agents).unwrap_err();
        // Sorted, deterministic output.
        assert_eq!(error.available, vec!["advisor", "reviewer"]);
        let message = error.to_string();
        assert!(message.contains("unknown agent"), "{message}");
        assert!(message.contains("advisor"), "{message}");
        assert!(message.contains("reviewer"), "{message}");
    }

    #[test]
    fn no_configured_agents_explains_where_to_add_them() {
        let error = resolve_agent_in("missing", &HashMap::new()).unwrap_err();
        assert!(error.available.is_empty());
        let message = error.to_string();
        assert!(message.contains("~/.coderex/agents/"), "{message}");
    }
}
