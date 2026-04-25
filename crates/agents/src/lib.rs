//! CodeRex sub-agent system — types, spawning, and management.

#![warn(missing_docs)]

pub mod custom_agents;
pub mod manager;
pub mod types;

pub use custom_agents::{CustomAgentDef, load_custom_agents};
pub use manager::{
    AgentError, AgentInstance, AgentManager, AgentMessage, AgentResult, AgentStatus, ForkedContext,
    SpawnOptions, TeammateEvent, execute_via_agent, spawn_dream_agent,
};
pub use types::{AgentType, Thoroughness};
