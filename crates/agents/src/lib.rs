//! CodeRex sub-agent system — types, spawning, and management.

#![warn(missing_docs)]

pub mod custom_agents;
pub mod manager;
pub mod types;

pub use custom_agents::{load_custom_agents, CustomAgentDef};
pub use manager::{
    execute_via_agent, spawn_dream_agent, AgentError, AgentInstance, AgentManager, AgentMessage,
    AgentResult, AgentStatus, CacheSafeParams, ForkedContext, SpawnOptions, TeammateEvent,
};
pub use types::{AgentType, Thoroughness};
