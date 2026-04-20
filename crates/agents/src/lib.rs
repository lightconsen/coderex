//! CodeRex sub-agent system — types, spawning, and management.

#![warn(missing_docs)]

pub mod custom_agents;
pub mod types;
pub mod manager;

pub use types::{AgentType, Thoroughness};
pub use manager::{
    AgentError, AgentInstance, AgentManager, AgentMessage, ForkedContext,
    SpawnOptions, TeammateEvent, execute_via_agent, spawn_dream_agent,
};
pub use custom_agents::{CustomAgentDef, load_custom_agents};
