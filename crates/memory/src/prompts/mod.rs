//! Memory-specific prompts and prompt helpers.
//!
//! This module hosts prompt text and assembly helpers that are specific to
//! the memory subsystem (session memory updates, long-term extraction,
//! memory compaction, etc.).

/// Re-export core memory prompt constants for convenience.
pub use coderex_core::prompts::{EXTRACT_MEMORIES_SYSTEM, SESSION_MEMORY_UPDATE_INSTRUCTION};

/// Assemble a session memory update user prompt from current memory and
/// recent conversation excerpt.
pub fn session_memory_update_user_prompt(current_memory: &str, conversation: &str) -> String {
    let header = if current_memory.is_empty() {
        "Create a new session memory file based on this conversation.".to_string()
    } else {
        format!(
            "Update the session memory below based on the recent conversation.\n\n\
             Current memory:\n```\n{current_memory}\n```"
        )
    };
    format!(
        "{header}\n\n\
         Recent conversation:\n```\n{conversation}\n```\n\n\
         Produce the updated memory content."
    )
}

/// Assemble a long-term memory extraction user prompt from conversation text.
pub fn extract_memories_user_prompt(conversation: &str, existing_topics: &str) -> String {
    let context = if existing_topics.is_empty() {
        String::new()
    } else {
        format!("Existing memory topics (update if needed):\n{existing_topics}\n\n")
    };
    format!(
        "{context}\
         Analyze the following conversation and extract information worth \
         preserving long-term. Return a JSON array of objects with \
         'topic' and 'content' keys.\n\n\
         Conversation:\n```\n{conversation}\n```"
    )
}
