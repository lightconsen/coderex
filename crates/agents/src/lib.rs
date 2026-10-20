//! CodeRex sub-agent system — types, spawning, and management.

#![warn(missing_docs)]

pub mod custom_agents;
pub mod manager;
pub mod types;

pub use custom_agents::{CustomAgentDef, load_custom_agents, load_custom_agents_from_dir};
pub use manager::{
    AgentError, AgentInstance, AgentManager, AgentMessage, AgentObservableEvent, AgentResult,
    AgentStatus, AgentSubscription, CacheSafeParams, FailureAction, ForkedContext,
    ParentContextSnapshot, RetryPolicy, SpawnOptions, TeammateEvent, TeammateEventCallback,
    agent_log_dir, agent_log_path, read_agent_log, read_agent_log_at,
};
pub use types::{AgentType, Thoroughness};
