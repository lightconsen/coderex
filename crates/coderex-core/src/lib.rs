//! CodeRex core types, QueryEngine, state machines, and shared traits.
//!
//! This crate contains the foundational types used throughout the CodeRex
//! system, including message types, tool definitions, session state, and
//! the QueryEngine state machine.

#![warn(missing_docs)]

pub mod types;
pub mod state;
pub mod runtime;
pub mod engine;
pub mod api_client;
pub mod budget;
pub mod cost;
pub mod permissions;
pub mod permission_engine;
pub mod sandbox;
pub mod file_traversal;
pub mod fuzzy_matcher;
pub mod markdown;
pub mod syntax;
pub mod scheduler;
pub mod prompt;
pub mod compact;
pub mod stop_hook;
pub mod hook_executor;
pub mod elicitation;
pub mod task;
pub mod worktree;
pub mod team;
pub mod coordinator;
pub mod team_context;
pub mod orchestrator;
pub mod query_profiles;
pub mod prompts;

pub use file_traversal::{FileIterator, TraversalConfig, TraversalError, collect_files, count_files, traverse};
pub use fuzzy_matcher::{FuzzyMatch, FuzzyMatcher, fuzzy_search};
pub use markdown::{MarkdownConfig, MarkdownError, extract_frontmatter, markdown_to_commonmark, markdown_to_html, strip_markdown};
pub use syntax::{SyntaxError, SyntaxHighlighter, Token};
pub use scheduler::{CronError, CronSchedule, next_cron_time, validate_cron};
pub use prompt::{AssembledContext, CompactConfig, PromptBlock, PromptCompiler, PromptContext};
pub use compact::{
    CompactResult, CompactStrategy, auto_compact, compact_boundary_message, insert_compact_boundary,
    micro_compact, reactive_compact, select_compact_strategy, should_auto_compact,
    should_reactive_compact, snip_compact,
};

pub use runtime::{Runtime, RuntimeHandle, UiEvent};
pub use engine::{
    DeniedToolCall, PendingElevation, PendingToolCall, QueryEngine, StreamingToolCallAssembler,
    StructuredOutputValidator, SubmitOptions, TerminalReason, TurnProgress, TurnRuntime, TurnState,
    ValidationResult,
};

pub use elicitation::{
    ElicitationHandler, ElicitationOption, ElicitationQuestion, ElicitationResponse,
    ElicitationResult, ReplElicitationHandler, SdkElicitationHandler,
};

pub use task::{
    TaskError, TaskHandle, TaskKind, TaskManager, TaskState,
};

pub use worktree::{
    WorktreeConfig, WorktreeError, WorktreeManager, WorktreeResult, enter_worktree, exit_worktree,
};

pub use team::{
    MemberStatus, SharedTask, Team, TeamError, TeamManager, TeamMember,
};

pub use coordinator::{
    CoordinatorError, CoordinatorPhase, CoordinatorState, coordinator_tool_whitelist,
    is_coordinator_tool,
};

pub use team_context::{
    ActivityEntry, Note, SessionPointer, SessionPointerStore, TeamContext, WorkItem,
};

pub use query_profiles::{
    ModelProfile, PromptProfile, QueryKind, QueryRequest,
};
pub use orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, Orchestrator, OrchestratorError, QueryResult,
    run_background_feature, run_lightweight_query, run_side_query, run_forked_agent,
};

pub mod test_utils;
