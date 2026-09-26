//! CodeRex core types, QueryEngine, state machines, and shared traits.
//!
//! This crate contains the foundational types used throughout the CodeRex
//! system, including message types, tool definitions, session state, and
//! the QueryEngine state machine.

#![warn(missing_docs)]

pub mod api_client;
pub mod budget;
pub mod compact;
pub mod compaction_backend;
pub mod confirmation_bus;
pub mod coordinator;
pub mod cost;
pub mod elicitation;
pub mod engine;
pub mod file_traversal;
pub mod fuzzy_matcher;
pub mod hook_executor;
pub mod hook_security;
pub mod loop_detector;
pub mod markdown;
pub mod model_router;
pub mod orchestrator;
pub mod permission_engine;
pub mod permission_manager;
pub mod permissions;
pub mod prompt;
pub mod prompts;
pub mod provider;
pub mod provider_config;
pub mod provider_router;
pub mod providers;
pub mod query_profiles;
pub mod runtime;
pub mod sandbox;
pub mod scheduler;
pub mod session_memory;
pub mod shell_security;
pub mod state;
pub mod stop_hook;
pub mod stream_utils;
pub mod syntax;
pub mod task;
pub mod team;
pub mod team_context;
pub mod tool_executor;
pub mod tool_scheduler;
pub mod types;
pub mod wire_adapter;
pub mod wire_protocol;
pub mod wire_transport;
pub mod worktree;

pub use compact::{
    CompactResult, CompactStrategy, MicroCompactParams, auto_compact, compact_boundary_message,
    insert_compact_boundary, micro_compact, reactive_compact, select_compact_strategy,
    should_auto_compact, should_reactive_compact, snip_compact, truncate_tool_output,
};
pub use compaction_backend::{CompactionBackend, CompactionKind, CompactionRequest};
pub use file_traversal::{
    FileIterator, TraversalConfig, TraversalError, collect_files, count_files, traverse,
};
pub use fuzzy_matcher::{FuzzyMatch, FuzzyMatcher, fuzzy_search};
pub use markdown::{
    MarkdownConfig, MarkdownError, extract_frontmatter, markdown_to_commonmark, markdown_to_html,
    strip_markdown,
};
pub use prompt::{AssembledContext, CompactConfig, PromptBlock, PromptCompiler, PromptContext};
pub use scheduler::{CronError, CronJob, CronRunner, CronSchedule, next_cron_time, validate_cron};
pub use session_memory::SessionMemoryBackend;
pub use syntax::{SyntaxError, SyntaxHighlighter, Token};

pub use engine::{
    DeniedToolCall, PendingElevation, PendingToolCall, QueryEngine, StreamingToolCallAssembler,
    StructuredOutputValidator, SubmitOptions, TerminalReason, TurnProgress, TurnRuntime, TurnState,
    ValidationResult,
};
pub use runtime::{Runtime, RuntimeHandle, UiEvent};

pub use elicitation::{
    ElicitationHandler, ElicitationOption, ElicitationQuestion, ElicitationResponse,
    ElicitationResult, ReplElicitationHandler, SdkElicitationHandler,
};

pub use task::{TaskError, TaskHandle, TaskKind, TaskManager, TaskState};

pub use worktree::{
    WorktreeConfig, WorktreeError, WorktreeManager, WorktreeResult, enter_worktree, exit_worktree,
};

pub use team::{MemberStatus, SharedTask, Team, TeamError, TeamManager, TeamMember};

pub use coordinator::{
    CoordinatorError, CoordinatorPhase, CoordinatorState, coordinator_tool_whitelist,
    is_coordinator_tool,
};

pub use team_context::{
    ActivityEntry, Note, SessionPointer, SessionPointerStore, TeamContext, WorkItem,
};

pub use orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, Orchestrator, OrchestratorError, QueryResult,
    run_background_feature, run_forked_agent, run_lightweight_query, run_side_query,
};
pub use query_profiles::{ModelProfile, PromptProfile, QueryKind, QueryRequest};

pub use confirmation_bus::{
    AutoApproveBus, AutoDenyBus, ConfirmationBus, ConfirmationRequest, ConfirmationResponse,
};

pub use permission_manager::{
    PermissionManager, PermissionManagerConfig, PermissionManagerError, RuleGroup,
};

pub use permission_engine::{
    AutoApproveHook, AutoClassifierHook, BackgroundAgentDenyHook, ExplainPermissionHook,
    NoOpPermissionHook, PendingPermissionStore, PermissionEngine, PermissionHook,
    PermissionRequest, PermissionRequestError, PermissionResponse,
};

pub use hook_security::{
    EnvInterpolator, InterpolationError, SsrfError, SsrfGuard, UrlValidationError, UrlValidator,
};

pub use shell_security::{BashAst, BashCommand, DestructiveLevel, Redirect, RedirectKind};

pub use wire_adapter::{
    DualEventSink, bridge_engine_to_wire, engine_event_to_wire, wire_event_to_engine,
};
pub use wire_protocol::{
    WireEvent, WireRequest, WireToolResult, decode_event, decode_request, encode_event,
    encode_request,
};
pub use wire_transport::{
    InMemoryTransport, InMemoryTransportServer, SinkTransport, StdioTransport, WireTransport,
};

pub use model_router::{ModelRouter, RouteEntry};
pub use provider::{
    Provider, ProviderApiStyle, ProviderCapabilities, ProviderError, ProviderErrorKind,
    ResolvedProviderProfile, classify_api_error, provider_error_to_api,
};
pub use provider_config::{NamedProviderEndpoint, ProviderConfig, ProviderEndpoint};
pub use provider_router::ProviderRouter;

pub mod test_utils;
