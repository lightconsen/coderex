//! Feature-specific prompt templates.
//!
//! This module contains dedicated prompt templates for single-purpose AI tasks
//! like session title generation, permission explanation, compact summary, etc.
//!
//! These prompts are independent of the base system prompt and are used by
//! the orchestrator for side queries and lightweight queries.

pub mod session_title;
pub mod permission_explainer;
pub mod compact_summary;
pub mod session_memory;
pub mod memory_extraction;
pub mod agent_summary;
pub mod prompt_suggestion;
pub mod agent_generation;
pub mod magic_docs;

pub use session_title::{generate_session_title, generate_session_title_from_messages, parse_title};
pub use permission_explainer::{explain_permission, explain_permission_from_value};
pub use compact_summary::{generate_compact_summary, generate_compact_summary_from_messages};
pub use session_memory::{update_session_memory, create_session_memory};
pub use memory_extraction::{extract_memories, extract_single_memory};
pub use agent_summary::{summarize_agent_activity, generate_away_summary, generate_team_status};
pub use prompt_suggestion::{suggest_prompts, suggest_next_action};
pub use agent_generation::{generate_agent, generate_agent_prompt, AgentSpec};
pub use magic_docs::{generate_docs_for_code, generate_codebase_overview, generate_readme};

// ─── Session Title ──────────────────────────────────────────────────────────

/// System prompt for generating a session title.
pub const SESSION_TITLE_SYSTEM: &str =
    "Generate a concise 3-7 word sentence-case title that captures the main topic \
     of this conversation. Return ONLY a JSON object: {\"title\": \"...\"}";

/// User prompt template for session title generation.
pub fn session_title_user_prompt(conversation: &str) -> String {
    format!(
        "Here is the conversation so far:\n{conversation}\n\n\
         Generate a 3-7 word sentence-case title that identifies this session."
    )
}

// ─── Permission Explanation ─────────────────────────────────────────────────

/// System prompt for explaining tool call risks.
pub const PERMISSION_EXPLAIN_SYSTEM: &str =
    "You are a security advisor. Explain the potential risks of a tool call \
     in 1-2 clear sentences. Focus on concrete risks, not generic warnings. \
     If the tool call appears safe, say so briefly.";

/// User prompt template for permission explanation.
pub fn permission_explain_user_prompt(tool_name: &str, tool_input: &str) -> String {
    format!(
        "Explain the potential risks of this tool call:\n\n\
         **Tool:** {tool_name}\n\
         **Input:**\n```\n{tool_input}\n```\n\n\
         What could go wrong if this executes?"
    )
}

// ─── Compact Summary ────────────────────────────────────────────────────────

/// System prompt for compact summary generation.
pub const COMPACT_SUMMARY_SYSTEM: &str =
    "You are a helpful AI assistant tasked with summarizing conversations.";

/// User prompt template for compact summary.
pub fn compact_summary_user_prompt(summary_request: &str) -> String {
    format!(
        "Please summarize the conversation above for future reference.\n\
         Include key decisions, file paths, and current state of work.\n\
         {summary_request}"
    )
}

// ─── Session Memory Update ──────────────────────────────────────────────────

/// System prompt for session memory updates (reuses main system prompt for cache).
/// The user prompt constrains behavior.
pub const SESSION_MEMORY_UPDATE_INSTRUCTION: &str =
    "Update the session memory file based on the conversation above. \
     This is NOT part of the user conversation — do not add the note-taking process to notes. \
     Use the Edit tool to update the memory file. \
     Keep information density high and actionable. \
     Always update the Current State section. \
     After editing, stop.";

// ─── Extract Memories ───────────────────────────────────────────────────────

/// System prompt for extracting long-term memories.
pub const EXTRACT_MEMORIES_SYSTEM: &str =
    "You are a memory extraction subagent. Analyze the recent conversation \
     and extract information worth preserving long-term. Do NOT grep code or \
     run git commands to verify content — only analyze the messages. \
     Write memory files to the memory directory following the established format.";

// ─── Agent Summary (for coordinator sub-agents) ────────────────────────────

/// System prompt for summarizing a sub-agent's progress.
pub const AGENT_SUMMARY_SYSTEM: &str =
    "Summarize what the agent is currently doing in 3-5 words. \
     Use present continuous tense (e.g., 'Reading runAgent.ts', 'Fixing null check in validate.ts'). \
     Mention file or function names, not branch names.";

// ─── Prompt Profile System Prompt Generators ────────────────────────────────

use crate::query_profiles::PromptProfile;

/// Get the system prompt for a given profile.
#[must_use]
pub fn system_prompt_for_profile(profile: PromptProfile) -> &'static str {
    match profile {
        PromptProfile::DefaultAgent => DEFAULT_AGENT_SYSTEM,
        PromptProfile::Advisor => ADVISOR_SYSTEM,
        PromptProfile::BugHunter => BUG_HUNTER_SYSTEM,
        PromptProfile::Reviewer => REVIEWER_SYSTEM,
        PromptProfile::MemoryUpdater => MEMORY_UPDATER_SYSTEM,
        PromptProfile::TitleGenerator => SESSION_TITLE_SYSTEM,
        PromptProfile::PermissionExplainer => PERMISSION_EXPLAIN_SYSTEM,
    }
}

const DEFAULT_AGENT_SYSTEM: &str =
    "You are an expert software engineer and coding agent. Help the user with \
     coding tasks including reading, writing, editing, and debugging code. \
     Use tools to inspect and modify files. Think through problems step by step.";

const ADVISOR_SYSTEM: &str =
    "You are a senior technical advisor. Focus on guidance, architecture advice, \
     and recommendations rather than directly implementing changes. Explain your \
     reasoning and suggest approaches with trade-offs.";

const BUG_HUNTER_SYSTEM: &str =
    "You are a bug hunting specialist. Systematically review code for bugs, \
     edge cases, security issues, and potential failures. Be thorough and \
     methodical — check for null cases, off-by-one errors, resource leaks, \
     and concurrency issues.";

const REVIEWER_SYSTEM: &str =
    "You are a code review specialist. Review code for correctness, \
     maintainability, performance, security, and adherence to conventions. \
     Provide constructive feedback with specific examples and suggestions.";

const MEMORY_UPDATER_SYSTEM: &str =
    "You are a session memory maintenance agent. Update the session memory \
     file based on the conversation. Write high-density, actionable technical \
     content. Always update the Current State section.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_system_prompts_are_nonempty() {
        for profile in [
            PromptProfile::DefaultAgent,
            PromptProfile::Advisor,
            PromptProfile::BugHunter,
            PromptProfile::Reviewer,
            PromptProfile::MemoryUpdater,
            PromptProfile::TitleGenerator,
            PromptProfile::PermissionExplainer,
        ] {
            let prompt = system_prompt_for_profile(profile);
            assert!(!prompt.is_empty(), "empty prompt for {profile:?}");
        }
    }

    #[test]
    fn session_title_prompt_contains_json_hint() {
        let prompt = session_title_user_prompt("user asked about Rust lifetimes");
        assert!(prompt.contains("title"));
        assert!(prompt.contains("Rust"));
    }

    #[test]
    fn permission_explain_prompt_contains_tool_info() {
        let prompt = permission_explain_user_prompt("Bash", "rm -rf /tmp");
        assert!(prompt.contains("Bash"));
        assert!(prompt.contains("rm -rf /tmp"));
        assert!(prompt.contains("risks"));
    }

    #[test]
    fn compact_summary_prompt_is_well_formed() {
        let prompt = compact_summary_user_prompt("Focus on the auth changes.");
        assert!(prompt.contains("summarize"));
        assert!(prompt.contains("auth changes"));
    }
}
