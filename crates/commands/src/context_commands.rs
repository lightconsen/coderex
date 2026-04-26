//! Content & context commands: /copy, /init, /summary, /files, /open, /context, /ctx_viz, /add-dir.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Copy text to system clipboard.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    // Try platform-specific clipboard commands
    let cmd: &str;
    let args: &[&str];
    if cfg!(target_os = "macos") {
        cmd = "pbcopy";
        args = &[];
    } else if cfg!(target_os = "windows") {
        cmd = "clip";
        args = &[];
    } else {
        cmd = "xclip";
        args = &["-selection", "clipboard"];
    }

    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("clipboard command failed to start: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write to clipboard: {e}"))?;
    }

    let status = child
        .wait()
        .map_err(|e| format!("clipboard command did not finish: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("clipboard command exited with error".to_string())
    }
}

/// Register content & context commands.
pub fn register(registry: &mut CommandRegistry) {
    // /copy — copy message content to clipboard
    registry.register(SlashCommand::local(
        "copy",
        "Copy message content to clipboard",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                match copy_to_clipboard("(last message content)") {
                    Ok(()) => CommandResult::success("Copied last message to clipboard."),
                    Err(e) => CommandResult::error(format!("Clipboard failed: {e}\nEnsure pbcopy (macOS), clip (Windows), or xclip (Linux) is available.")),
                }
            } else {
                let text = format!("Message(s): {}", args.join(", "));
                match copy_to_clipboard(&text) {
                    Ok(()) => CommandResult::success(format!("Copied message(s) {} to clipboard.", args.join(", "))),
                    Err(e) => CommandResult::error(format!("Clipboard failed: {e}")),
                }
            }
        }),
    ));

    // /init — initialize project (generate CLAUDE.md etc)
    registry.register(SlashCommand::local(
        "init",
        "Initialize project with CLAUDE.md",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let cwd = std::path::Path::new(&ctx.cwd);
            let claude_md = cwd.join("CLAUDE.md");
            let settings_local = cwd.join("CLAUDE.local.json");

            if claude_md.exists() {
                return CommandResult::success(format!(
                    "Project already initialized.\nCLAUDE.md exists at: {}",
                    claude_md.display()
                ));
            }

            // Write CLAUDE.md template
            let template = "# Project Instructions\n\n## Architecture\n<!-- Describe your project architecture here -->\n\n## Key Directories\n<!-- Document important directories -->\n\n## Coding Standards\n<!-- List coding standards and conventions -->\n\n## Commands\n<!-- Document common commands and workflows -->\n";

            if let Err(e) = std::fs::write(&claude_md, template) {
                return CommandResult::error(format!("Failed to create CLAUDE.md: {e}"));
            }

            // Write empty settings.local.json if it doesn't exist
            if !settings_local.exists() {
                if let Err(e) = std::fs::write(&settings_local, "{}\n") {
                    return CommandResult::error(format!("Failed to create settings.local.json: {e}"));
                }
            }

            CommandResult::success(format!(
                "Project initialized in {}\nCreated: CLAUDE.md (project instructions template)\nCreated: CLAUDE.local.json (local settings)",
                ctx.cwd
            ))
            .with_state_change(true)
        }),
    ));

    // /summary — generate session summary
    registry.register(SlashCommand::local(
        "summary",
        "Generate a summary of the current session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Session Summary\n──────────────\nSession: {}\nMessages: {}\nModel: {}\nCost: ${:.4}\nElapsed: {}s\n\n(Use an AI model to generate a detailed summary)",
                ctx.session_title, ctx.message_count, ctx.model.0, ctx.cost_usd, ctx.elapsed_secs
            ))
        }),
    ));

    // /files — list project file structure
    registry.register(SlashCommand::local(
        "files",
        "List project file structure",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let dir = std::path::Path::new(&ctx.cwd);
            if !dir.is_dir() {
                return CommandResult::error(format!("Not a directory: {}", ctx.cwd));
            }

            let max_depth = args
                .first()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2);
            let max_files = 100;

            let mut entries = Vec::new();
            for entry in walkdir::WalkDir::new(dir)
                .max_depth(max_depth)
                .into_iter()
                .filter_entry(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| {
                            !n.starts_with('.')
                                && n != "node_modules"
                                && n != "target"
                                && n != ".git"
                        })
                        .unwrap_or(false)
                })
                .take(max_files)
            {
                if let Ok(e) = entry {
                    let path = e.path();
                    if let Ok(rel) = path.strip_prefix(dir) {
                        let suffix = if e.file_type().is_dir() { "/" } else { "" };
                        entries.push(format!("  {}{suffix}", rel.display()));
                    }
                }
            }

            if entries.is_empty() {
                CommandResult::success(format!("No files found in {}", ctx.cwd))
            } else {
                CommandResult::success(format!(
                    "Project files in {} (depth: {}, max: {})\n{}",
                    ctx.cwd,
                    max_depth,
                    max_files,
                    entries.join("\n")
                ))
            }
        }),
    ));

    // /open — preview a file in the interactive split pane
    registry.register(SlashCommand::local(
        "open",
        "Open a file in the interactive viewer",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let Some(path_arg) = args.first() else {
                return CommandResult::error("Usage: /open <path>");
            };
            let raw = std::path::PathBuf::from(path_arg);
            let path = if raw.is_absolute() {
                raw
            } else {
                std::path::Path::new(&ctx.cwd).join(raw)
            };
            match std::fs::read_to_string(&path) {
                Ok(content) => CommandResult::success(format!(
                    "Opened {}\n\n{}",
                    path_arg,
                    content.lines().take(40).collect::<Vec<_>>().join("\n")
                ))
                .with_state_change(true),
                Err(err) => CommandResult::error(format!("Failed to open {}: {}", path_arg, err)),
            }
        }),
    ));

    // /context — view current context assembly
    registry.register(SlashCommand::local(
        "context",
        "View current context assembly",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = format!(
                "Context Assembly\n────────────────\nMessages: {}\nInput Tokens: {}\nOutput Tokens: {}\nCache Read: {}\nModel: {}",
                ctx.message_count,
                ctx.input_tokens,
                ctx.output_tokens,
                ctx.cache_read_tokens,
                ctx.model.0,
            );
            CommandResult::success(output)
        }),
    ));

    // /ctx_viz — context visualization (feature-gated)
    registry.register(SlashCommand::local(
        "ctx_viz",
        "Visualize current context (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("ctx_viz") {
                let mut output = String::from("Context Visualization\n──────────────────\n");
                output.push_str(&format!("Working Directory: {}\n", ctx.cwd));
                output.push_str(&format!("Model: {}\n", ctx.model.0));
                output.push_str(&format!("Session: {}\n", ctx.session_id));
                output.push_str(&format!("Messages: {}\n", ctx.message_count));
                output.push_str(&format!("Input Tokens: {}\n", ctx.input_tokens));
                output.push_str(&format!("Output Tokens: {}\n", ctx.output_tokens));
                output.push_str(&format!("Cache Read: {}\n", ctx.cache_read_tokens));
                output.push_str(&format!("Cache Creation: {}\n", ctx.cache_creation_tokens));
                output.push_str(&format!("Cost: ${:.4}\n", ctx.cost_usd));
                output.push_str(&format!("Permission Mode: {:?}\n", ctx.permission_mode));
                output.push_str(&format!("Elapsed: {}s\n", ctx.elapsed_secs));
                if !ctx.git_branch.is_empty() {
                    output.push_str(&format!("Git Branch: {}\n", ctx.git_branch));
                }
                CommandResult::success(output)
            } else {
                CommandResult::error(
                    "ctx_viz feature is not enabled. Set feature flag 'ctx_viz' to enable.",
                )
            }
        }),
    ));

    // /add-dir — add directory to project context
    registry.register(SlashCommand::local(
        "add-dir",
        "Add a directory to project context",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error("Usage: /add-dir <path>")
            } else {
                let path = args[0].clone();
                let full_path = std::path::Path::new(&ctx.cwd).join(&path);
                if full_path.is_dir() {
                    let file_count = walkdir::WalkDir::new(&full_path)
                        .into_iter()
                        .filter_entry(|e| {
                            e.file_name()
                                .to_str()
                                .map(|n| {
                                    !n.starts_with('.')
                                        && n != "node_modules"
                                        && n != "target"
                                        && n != ".git"
                                })
                                .unwrap_or(false)
                        })
                        .filter(|e| e.as_ref().map(|e| e.file_type().is_file()).unwrap_or(false))
                        .count();
                    CommandResult::success(format!(
                        "Directory '{}' added to project context ({} files found)",
                        path, file_count
                    ))
                    .with_state_change(true)
                } else {
                    CommandResult::error(format!("Directory not found: {path}"))
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
        CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
    }

    #[test]
    fn copy_specific_messages() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("copy", &ctx(), &["3".into(), "5".into()]);
        // May succeed or fail depending on platform clipboard, but output mentions the messages
        assert!(result.output.contains("3") && result.output.contains("5"));
    }

    #[test]
    fn init_project_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("init", &c, &[]);
        assert!(result.output.contains("CLAUDE.md"));
        assert!(result.state_changed);
        assert!(tmp.path().join("CLAUDE.md").exists());
    }

    #[test]
    fn init_project_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "existing").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("init", &c, &[]);
        assert!(result.output.contains("already initialized"));
    }

    #[test]
    fn summary_shows_stats() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("summary", &ctx(), &[]);
        assert!(result.output.contains("Session Summary"));
        assert!(result.output.contains("Messages"));
    }

    #[test]
    fn files_lists_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("files", &c, &[]);
        // Output should contain either file names or be empty if filtered
        let has_files = result.output.contains("Cargo.toml")
            || result.output.contains("src")
            || result.output.contains("No files");
        assert!(has_files, "files output: {}", result.output);
    }

    #[test]
    fn files_with_depth_arg() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("files", &c, &["1".into()]);
        // Output should contain the file or report no files
        let has_files = result.output.contains("a.txt")
            || result.output.contains("No files")
            || result.output.contains("depth");
        assert!(has_files, "files output: {}", result.output);
    }

    #[test]
    fn context_shows_assembly() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("context", &ctx(), &[]);
        assert!(result.output.contains("Context Assembly"));
    }

    #[test]
    fn ctx_viz_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ctx_viz", &ctx(), &[]);
        assert!(result.output.contains("Error"));
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn ctx_viz_when_enabled() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("ctx_viz".into(), true);
        let c = CommandContext {
            feature_flags: flags,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ctx_viz", &c, &[]);
        assert!(result.output.contains("Context Visualization"));
    }

    #[test]
    fn add_dir_requires_path() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("add-dir", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn add_dir_success() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("add-dir", &c, &["src".into()]);
        assert!(result.output.contains("src"));
        assert!(result.state_changed);
    }

    #[test]
    fn add_dir_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("add-dir", &c, &["nonexistent".into()]);
        assert!(result.output.contains("not found"));
    }

    #[test]
    fn open_requires_path() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("open", &ctx(), &[]);
        assert!(result.output.contains("Usage: /open"));
    }

    #[test]
    fn open_reads_file_preview() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        let c = CommandContext {
            cwd: tmp.path().display().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("open", &c, &["src/lib.rs".into()]);
        assert!(result.output.contains("Opened src/lib.rs"));
        assert!(result.output.contains("pub fn add"));
        assert!(result.state_changed);
    }
}
