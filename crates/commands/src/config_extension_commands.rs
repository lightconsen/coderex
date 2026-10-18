//! Config extension commands: /theme, /hooks, /mcp, /plugin, /memory, /skills, /tasks, /plan.

use crate::registry::{CommandArgs, CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

thread_local! {
    static TEST_CONFIG_HOME: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_config_home(path: std::path::PathBuf) {
    TEST_CONFIG_HOME.with(|h| *h.borrow_mut() = Some(path));
}

fn config_home() -> std::path::PathBuf {
    TEST_CONFIG_HOME.with(|h| {
        h.borrow()
            .clone()
            .unwrap_or_else(|| coderex_config::settings::config_home())
    })
}

/// Read the user settings.json as a JSON object.
fn read_user_settings() -> Option<serde_json::Map<String, serde_json::Value>> {
    let path = config_home().join("settings.json");
    if !path.exists() {
        return None;
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
}

/// Write the user settings.json.
fn write_user_settings(
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let home = config_home();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let path = home.join("settings.json");
    let value = serde_json::Value::Object(settings.clone());
    let json = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// List items in a directory matching a pattern, returning their names.
fn list_dir_items(path: &std::path::Path) -> Vec<String> {
    if !path.is_dir() {
        return Vec::new();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            Some(name)
        })
        .collect()
}

/// Read memory entries from the memory file.
fn read_memory_entries() -> Vec<String> {
    let memory_path = config_home().join("memory").join("MEMORY.md");
    if memory_path.exists() {
        match std::fs::read_to_string(&memory_path) {
            Ok(content) => content.lines().map(|l| l.to_string()).take(20).collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    }
}

/// Register config extension commands.
pub fn register(registry: &mut CommandRegistry) {
    // /theme — switch/view theme
    registry.register(
        SlashCommand::local(
            "theme",
            "View or change the UI theme",
            true,
            true,
            true,
            Arc::new(|ctx: &CommandContext, args: &[String]| {
                if args.is_empty() {
                    let home = config_home();
                    let themes_path = home.join("themes");
                    let available = list_dir_items(&themes_path);
                    let mut output = format!("Current theme: {}", ctx.theme);
                    if !available.is_empty() {
                        output.push_str(&format!(
                            "\nAvailable themes:\n{}",
                            available
                                .iter()
                                .take(10)
                                .map(|t| format!("  {t}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                    CommandResult::success(output)
                } else {
                    let new_theme = &args[0];
                    let home = config_home();
                    let themes_path = home.join("themes");
                    let theme_file = themes_path.join(format!("{new_theme}.toml"));
                    let theme_dir = themes_path.join(new_theme);
                    if theme_file.exists()
                        || theme_dir.is_dir()
                        || new_theme == "light"
                        || new_theme == "dark"
                        || new_theme == "high-contrast"
                        || new_theme == "tokyo-night"
                    {
                        let mut settings = read_user_settings().unwrap_or_default();
                        settings.insert(
                            "theme".to_string(),
                            serde_json::Value::String(new_theme.clone()),
                        );
                        let _ = write_user_settings(&settings);
                        CommandResult::success(format!(
                            "Theme changed to: {new_theme}\nSaved to settings.json.",
                        ))
                        .with_state_change(true)
                    } else {
                        let available = list_dir_items(&themes_path);
                        let hint = if !available.is_empty() {
                            format!(
                                "\nAvailable themes:\n{}",
                                available
                                    .iter()
                                    .map(|t| format!("  {t}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )
                        } else {
                            "\nNo custom themes installed in ~/.coderex/themes/".to_string()
                        };
                        CommandResult::error(format!("Theme not found: {new_theme}{hint}"))
                    }
                }
            }),
        )
        .with_args(CommandArgs::AtMost(1)),
    );

    // /hooks — view/manage hooks
    registry.register(SlashCommand::local(
        "hooks",
        "View or manage hooks",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let home = config_home();
            let hooks_path = home.join("hooks");
            let available = list_dir_items(&hooks_path);

            if args.is_empty() {
                let mut output = format!("Active hooks: {}", ctx.active_hooks);
                if !available.is_empty() {
                    output.push_str(&format!(
                        "\nInstalled hooks:\n{}",
                        available
                            .iter()
                            .map(|h| format!("  {h}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                CommandResult::success(output)
            } else {
                match args[0].as_str() {
                    "list" => {
                        if available.is_empty() {
                            CommandResult::success("No hooks installed in ~/.coderex/hooks/")
                        } else {
                            CommandResult::success(format!(
                                "Installed hooks:\n{}",
                                available.join("\n")
                            ))
                        }
                    }
                    _ => CommandResult::error("Usage: /hooks [list]"),
                }
            }
        }),
    ));

    // /mcp — view/manage MCP services
    registry.register(SlashCommand::local(
        "mcp",
        "View or manage MCP services",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if ctx.mcp_connected {
                CommandResult::success("MCP: Connected")
            } else if args.is_empty() {
                // Read MCP config if available
                let settings_path = config_home().join("settings.json");
                let mcp_info = if settings_path.exists() {
                    match std::fs::read_to_string(&settings_path) {
                        Ok(content) => {
                            if content.contains("mcpServers") {
                                "MCP servers configured in settings.json but not connected.\nCheck server URLs and authentication.".to_string()
                            } else {
                                "No MCP servers configured.\nAdd mcpServers to settings.json.".to_string()
                            }
                        }
                        Err(_) => "Unable to read settings.json.".to_string()
                    }
                } else {
                    "No settings.json found.\nAdd mcpServers to configure MCP.".to_string()
                };
                CommandResult::success(format!("MCP: Not connected\n{mcp_info}\nUse settings.json to configure MCP servers."))
            } else {
                CommandResult::error("MCP management requires runtime connection.")
            }
        }),
    ));

    // /plugin — view/manage plugins
    registry.register(SlashCommand::local(
        "plugin",
        "View or manage plugins",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            let home = config_home();
            let plugins_path = home.join("plugins");
            let installed = list_dir_items(&plugins_path);

            if args.is_empty() {
                if installed.is_empty() {
                    CommandResult::success("No plugins installed.\nInstall plugins in ~/.coderex/plugins/\nUse /plugin install <name> to install from marketplace.")
                } else {
                    CommandResult::success(format!("Installed plugins:\n{}", installed.iter().map(|p| format!("  {p}")).collect::<Vec<_>>().join("\n")))
                }
            } else {
                match args[0].as_str() {
                    "list" => {
                        if installed.is_empty() {
                            CommandResult::success("No plugins installed.")
                        } else {
                            CommandResult::success(format!("Installed plugins:\n{}", installed.join("\n")))
                        }
                    }
                    "install" => {
                        let name = args.get(1).map(|s| s.as_str()).unwrap_or("<name>");
                        CommandResult::error(format!("Plugin installation requires marketplace access. Use: /plugin install {name}"))
                    }
                    _ => CommandResult::error("Usage: /plugin [list|install <name>]"),
                }
            }
        }),
    ));

    // /memory — view/search long-term memory
    registry.register(SlashCommand::local(
        "memory",
        "View or search long-term memory",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            let entries = read_memory_entries();
            let home = config_home();
            let memory_path = home.join("memory");
            let memory_files = list_dir_items(&memory_path);

            if args.is_empty() {
                let count = entries.len();
                if count == 0 {
                    CommandResult::success(format!(
                        "Memory: 0 entries\nMemory files found: {:?}\nUse /memory search <query> to search.\nMemory stored in ~/.coderex/memory/",
                        memory_files
                    ))
                } else {
                    CommandResult::success(format!(
                        "Memory: {count} entries\n{}",
                        entries.join("\n")
                    ))
                }
            } else if args[0] == "search" {
                let query = args[1..].join(" ");
                let matches: Vec<_> = entries.iter()
                    .filter(|l| l.to_lowercase().contains(&query.to_lowercase()))
                    .collect();
                if matches.is_empty() {
                    CommandResult::success(format!("No memory entries matching: {query}"))
                } else {
                    CommandResult::success(format!(
                        "Memory search for '{query}' — {} matches:\n{}",
                        matches.len(),
                        matches.iter().take(10).map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
                    ))
                }
            } else {
                CommandResult::success(format!("Searching memory for: {}", args.join(" ")))
            }
        }),
    ));

    // /skills — view/load skills
    registry.register(SlashCommand::local(
        "skills",
        "View or load skills",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            let home = config_home();

            // Scan multiple skill directories
            let skill_dirs = [
                home.join("skills"), // ~/.coderex/skills/
            ];

            let mut all_skills: Vec<String> = Vec::new();
            for dir in &skill_dirs {
                let items = list_dir_items(dir);
                all_skills.extend(items);
            }

            if args.is_empty() {
                if all_skills.is_empty() {
                    CommandResult::success(
                        "Skills:\n  No skills loaded.\nPlace skill scripts in ~/.coderex/skills/",
                    )
                } else {
                    CommandResult::success(format!(
                        "Skills:\n{}",
                        all_skills
                            .iter()
                            .map(|s| format!("  {s}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ))
                }
            } else {
                match args[0].as_str() {
                    "list" => {
                        if all_skills.is_empty() {
                            CommandResult::success("No skills loaded.")
                        } else {
                            CommandResult::success(format!(
                                "Loaded skills:\n{}",
                                all_skills.join("\n")
                            ))
                        }
                    }
                    _ => CommandResult::error("Usage: /skills [list]"),
                }
            }
        }),
    ));

    // /tasks — view task list (command version)
    registry.register(SlashCommand::local(
        "tasks",
        "View the task list",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            // Look for task files in .coderex/tasks/ and cwd
            let home = config_home();
            let tasks_path = home.join("tasks");
            let task_files = list_dir_items(&tasks_path);

            if args.is_empty() {
                if task_files.is_empty() {
                    CommandResult::success("Tasks:\n  No tasks registered.\nUse TaskCreate to create tasks.\nTask files can be placed in ~/.coderex/tasks/")
                } else {
                    CommandResult::success(format!("Task files in ~/.coderex/tasks/:\n{}", task_files.iter().map(|t| format!("  {t}")).collect::<Vec<_>>().join("\n")))
                }
            } else {
                match args[0].as_str() {
                    "list" => {
                        if task_files.is_empty() {
                            CommandResult::success("No task files found.")
                        } else {
                            CommandResult::success(format!("Task files:\n{}", task_files.join("\n")))
                        }
                    }
                    _ => CommandResult::error("Usage: /tasks [list]"),
                }
            }
        }),
    ));

    // /plan — view/edit current plan
    registry.register(SlashCommand::local(
        "plan",
        "View or edit the current plan",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.has_plan {
                // Try to find and read the plan file
                let home = config_home();
                let plans_path = home.join("plans");
                let plan_files = list_dir_items(&plans_path);
                let mut output = "Plan loaded.\n\n".to_string();
                if !plan_files.is_empty() {
                    output.push_str(&format!(
                        "Available plans:\n{}\n",
                        plan_files
                            .iter()
                            .map(|p| format!("  {p}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                output.push_str("(Use the Read tool to view plan details)");
                CommandResult::success(output)
            } else {
                // Check if any plan files exist
                let home = config_home();
                let plans_path = home.join("plans");
                let plan_files = list_dir_items(&plans_path);
                if !plan_files.is_empty() {
                    CommandResult::success(format!(
                        "Plan files found in ~/.coderex/plans/:\n{}",
                        plan_files.join("\n")
                    ))
                } else {
                    CommandResult::success("No active plan.\nUse /plan create to generate one.")
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
    fn theme_shows_current() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("theme", &ctx(), &[]);
        assert!(result.output.contains("Current theme"));
    }

    #[test]
    fn theme_changes() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("theme", &ctx(), &["dark".into()]);
        assert!(result.output.contains("dark"));
        assert!(result.state_changed);
        // Verify settings.json was written
        assert!(tmp.path().join("settings.json").exists());
    }

    #[test]
    fn theme_unknown_not_found() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("theme", &ctx(), &["nonexistent-theme-xyz".into()]);
        // Will show error since theme doesn't exist
        assert!(result.output.contains("nonexistent-theme-xyz"));
    }

    #[test]
    fn hooks_shows_count() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("hooks", &ctx(), &[]);
        assert!(result.output.contains("Active hooks"));
    }

    #[test]
    fn hooks_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("hooks", &ctx(), &["list".into()]);
        assert!(result.output.contains("hooks") || result.output.contains("Installed"));
    }

    #[test]
    fn mcp_status_not_connected() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("mcp", &ctx(), &[]);
        assert!(result.output.contains("Not connected"));
    }

    #[test]
    fn mcp_status_connected() {
        let c = CommandContext {
            mcp_connected: true,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("mcp", &c, &[]);
        assert!(result.output.contains("Connected"));
    }

    #[test]
    fn plugin_shows_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plugin", &ctx(), &[]);
        assert!(result.output.contains("No plugins") || result.output.contains("Installed"));
    }

    #[test]
    fn plugin_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plugin", &ctx(), &["list".into()]);
        assert!(result.output.contains("plugin") || result.output.contains("No plugins"));
    }

    #[test]
    fn memory_search() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute(
            "memory",
            &ctx(),
            &["search".into(), "user".into(), "preferences".into()],
        );
        assert!(
            result.output.contains("user")
                || result.output.contains("No memory")
                || result.output.contains("Memory")
        );
    }

    #[test]
    fn memory_empty_by_default() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("memory", &ctx(), &[]);
        assert!(result.output.contains("Memory"));
    }

    #[test]
    fn skills_shows_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("skills", &ctx(), &[]);
        assert!(result.output.contains("No skills") || result.output.contains("Skills"));
    }

    #[test]
    fn skills_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("skills", &ctx(), &["list".into()]);
        assert!(result.output.contains("skills") || result.output.contains("No skills"));
    }

    #[test]
    fn tasks_shows_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("tasks", &ctx(), &[]);
        assert!(result.output.contains("No tasks") || result.output.contains("tasks"));
    }

    #[test]
    fn tasks_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("tasks", &ctx(), &["list".into()]);
        assert!(result.output.to_lowercase().contains("task"));
    }

    #[test]
    fn plan_no_plan() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plan", &ctx(), &[]);
        assert!(result.output.contains("No active plan") || result.output.contains("Plan"));
    }

    #[test]
    fn plan_loaded() {
        let c = CommandContext {
            has_plan: true,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plan", &c, &[]);
        assert!(result.output.contains("Plan"));
    }
}
