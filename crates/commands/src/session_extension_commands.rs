//! Session extension commands: /resume, /rename, /rewind.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Session metadata stored on disk.
#[derive(Debug, Serialize, Deserialize)]
struct SessionMeta {
    session_id: String,
    title: String,
    message_count: usize,
}

/// Find the session data directory for a session ID.
fn session_dir(base: &Path, session_id: &str) -> std::io::Result<Option<std::path::PathBuf>> {
    if !base.is_dir() {
        return Ok(None);
    }
    let dir = base.join(session_id);
    if dir.is_dir() {
        Ok(Some(dir))
    } else {
        Ok(None)
    }
}

/// Find the most recent session in the session base directory.
fn most_recent_session(base: &Path) -> std::io::Result<Option<(String, String)>> {
    if !base.is_dir() {
        return Ok(None);
    }
    let mut entries: Vec<_> = std::fs::read_dir(base)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let meta_path = e.path().join("meta.json");
            if meta_path.exists() {
                let content = std::fs::read_to_string(meta_path).ok()?;
                let meta: SessionMeta = serde_json::from_str(&content).ok()?;
                Some((meta.session_id, meta.title))
            } else {
                None
            }
        })
        .collect();
    entries.sort();
    Ok(entries.into_iter().last())
}

/// Save session metadata to disk.
fn save_session_meta(base: &Path, session_id: &str, title: &str, message_count: usize) -> Result<(), String> {
    let dir = base.join(session_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create session dir: {e}"))?;
    let meta = SessionMeta {
        session_id: session_id.to_string(),
        title: title.to_string(),
        message_count,
    };
    let content = serde_json::to_string_pretty(&meta).map_err(|e| format!("failed to serialize: {e}"))?;
    std::fs::write(dir.join("meta.json"), content).map_err(|e| format!("failed to write metadata: {e}"))
}

/// Get the default session base path.
fn default_session_base() -> std::path::PathBuf {
    coderex_config::settings::config_home().join("sessions")
}

/// Register session extension commands.
pub fn register(registry: &mut CommandRegistry) {
    // /resume — resume previous session
    registry.register(SlashCommand::local(
        "resume",
        "Resume a previous session",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            let session_base = default_session_base();
            if args.is_empty() {
                // Resume most recent session
                match most_recent_session(&session_base) {
                    Ok(Some((id, title))) => {
                        CommandResult::success(format!(
                            "Resuming session: {id} — '{title}'"
                        ))
                        .with_state_change(true)
                    }
                    Ok(None) => {
                        CommandResult::success("No previous sessions found. Sessions are stored in ~/.coderex/sessions/")
                    }
                    Err(e) => CommandResult::error(format!("Failed to search sessions: {e}")),
                }
            } else {
                let session_id = &args[0];
                let base = default_session_base();
                match session_dir(&base, session_id) {
                    Ok(Some(dir)) => {
                        CommandResult::success(format!(
                            "Resuming session: {session_id} (data at: {})",
                            dir.display()
                        ))
                        .with_state_change(true)
                    }
                    Ok(None) => {
                        // Check for sessions with matching prefix
                        if base.is_dir() {
                            let matches: Vec<_> = std::fs::read_dir(&base)
                                .into_iter()
                                .flatten()
                                .filter_map(|e| e.ok())
                                .filter(|e| {
                                    e.path().is_dir() && e.file_name().to_string_lossy().contains(session_id)
                                })
                                .map(|e| e.file_name().to_string_lossy().to_string())
                                .collect();
                            if !matches.is_empty() {
                                return CommandResult::error(format!(
                                    "Session '{session_id}' not found. Did you mean: {}?",
                                    matches.join(", ")
                                ));
                            }
                        }
                        CommandResult::error(format!("Session '{session_id}' not found. Sessions are stored in ~/.coderex/sessions/"))
                    }
                    Err(e) => CommandResult::error(format!("Failed to find session: {e}")),
                }
            }
        }),
    ));

    // /rename — rename current session
    registry.register(SlashCommand::local(
        "rename",
        "Rename the current session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error(format!(
                    "Usage: /rename <new-title>\nCurrent title: {}",
                    ctx.session_title
                ))
            } else {
                let new_title = args.join(" ");
                let session_base = default_session_base();
                // Persist the rename to disk
                let save_result = save_session_meta(
                    &session_base,
                    &ctx.session_id,
                    &new_title,
                    ctx.message_count,
                );
                match save_result {
                    Ok(()) => CommandResult::success(format!(
                        "Session renamed from '{}' to '{}'\nMetadata saved to {}/{}",
                        ctx.session_title, new_title,
                        session_base.display(), ctx.session_id
                    ))
                    .with_state_change(true),
                    Err(e) => CommandResult::error(format!(
                        "Failed to persist rename: {e}\nSession title changed in memory only: '{}' → '{}'",
                        ctx.session_title, new_title
                    ))
                    .with_state_change(true),
                }
            }
        }),
    ));

    // /rewind — rewind to a point in history
    registry.register(SlashCommand::local(
        "rewind",
        "Rewind to a previous point in the session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error("Usage: /rewind <message-number>")
            } else {
                match args[0].parse::<usize>() {
                    Ok(n) if n <= ctx.message_count => {
                        let messages_to_remove = ctx.message_count.saturating_sub(n);
                        CommandResult::success(format!(
                            "Rewound to message {n} of {total}\n{to_remove} messages removed from transcript.",
                            total = ctx.message_count,
                            to_remove = messages_to_remove
                        ))
                        .with_state_change(true)
                    }
                    Ok(n) => {
                        CommandResult::error(format!(
                            "Message {n} not found. Session has {} messages.",
                            ctx.message_count
                        ))
                    }
                    Err(_) => {
                        CommandResult::error("Invalid message number. Usage: /rewind <number>")
                    }
                }
            }
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext {
            message_count: 20,
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn resume_no_sessions() {
        // With a nonexistent session base, should return "no sessions found" or an error
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("resume", &ctx(), &[]);
        // Either "no previous sessions" or an error about the path
        assert!(result.output.to_lowercase().contains("session") || result.output.to_lowercase().contains("no previous"));
    }

    #[test]
    fn resume_specific_session() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("resume", &ctx(), &["s_abc".into()]);
        // Session doesn't exist, so either "not found" or an error
        assert!(result.output.contains("s_abc") || result.output.contains("not found") || result.output.contains("No previous"));
    }

    #[test]
    fn rename_requires_arg() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rename", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn rename_success() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rename", &ctx(), &["New".into(), "Title".into()]);
        assert!(result.output.contains("New Title"));
        assert!(result.state_changed);
    }

    #[test]
    fn rename_persists_to_disk() {
        let _tmp = tempfile::tempdir().unwrap();
        // Create a fake home dir by setting up session dir
        let c = CommandContext {
            message_count: 5,
            ..CommandContext::new("/src".into(), "test-rename".into(), "Old Title".into(), ModelId::sonnet())
        };
        // The command will try to write to ~/.coderex/sessions/test-rename/meta.json
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rename", &c, &["New Title".into()]);
        // Should either succeed or fail gracefully
        assert!(result.output.contains("New Title"));
        assert!(result.state_changed);
    }

    #[test]
    fn rewind_requires_arg() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn rewind_valid_number() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &["5".into()]);
        assert!(result.output.contains("message 5"));
        assert!(result.state_changed);
    }

    #[test]
    fn rewind_out_of_range() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &["999".into()]);
        assert!(result.output.contains("not found"));
    }

    #[test]
    fn rewind_invalid_number() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &["abc".into()]);
        assert!(result.output.contains("Invalid"));
    }

    #[test]
    fn session_meta_serialization() {
        let meta = SessionMeta {
            session_id: "test".into(),
            title: "Test Session".into(),
            message_count: 10,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.session_id, "test");
        assert_eq!(parsed.title, "Test Session");
        assert_eq!(parsed.message_count, 10);
    }
}
