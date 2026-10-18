//! CodeRex sub-agent system — types, spawning, and management.

#![warn(missing_docs)]

pub mod custom_agents;
pub mod manager;
pub mod types;

pub use custom_agents::{load_custom_agents, load_custom_agents_from_dir, CustomAgentDef};
pub use manager::{
    execute_via_agent, spawn_dream_agent, AgentError, AgentInstance, AgentManager, AgentMessage,
    AgentObservableEvent, AgentResult, AgentStatus, AgentSubscription, CacheSafeParams,
    FailureAction, ForkedContext, ParentContextSnapshot, RetryPolicy, SpawnOptions, TeammateEvent,
    TeammateEventCallback,
};
pub use types::{AgentType, Thoroughness};
