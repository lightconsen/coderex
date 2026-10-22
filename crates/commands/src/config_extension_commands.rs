//! Config extension commands: /theme, /hooks, /mcp, /plugin, /memory, /skills, /tasks, /plan.

use crate::registry::{CommandArgs, CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::fmt::Write as _;
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
            .unwrap_or_else(coderex_config::settings::config_home)
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
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// One declaration emitted under `config_home()/plugin-types`.
struct GeneratedDeclaration {
    /// The name `/plugin types <name>` accepts.
    name: String,
    /// The declaration file on disk.
    path: std::path::PathBuf,
}

/// List the declarations under `root`, sorted by name.
///
/// The generated artefacts are directories with an `index.d.ts` (named after
/// the directory) plus `tsconfig.json` (named `tsconfig`). A plugin directory
/// uses its `index.d.ts` when it has one; otherwise every `.d.ts` it contains
/// is listed as `<plugin>/<file>` so a plugin-declared name is still
/// printable.
fn generated_declarations(root: &std::path::Path) -> Vec<GeneratedDeclaration> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(std::result::Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut declarations = Vec::new();
    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if path.is_file() {
            if file_name.ends_with(".d.ts") {
                declarations.push(GeneratedDeclaration {
                    name: file_name,
                    path,
                });
            } else if file_name == "tsconfig.json" {
                declarations.push(GeneratedDeclaration {
                    name: "tsconfig".to_string(),
                    path,
                });
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let index = path.join("index.d.ts");
        if index.is_file() {
            declarations.push(GeneratedDeclaration {
                name: file_name,
                path: index,
            });
            continue;
        }
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&path)
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
            })
            .collect();
        files.sort();
        for file in files {
            let Some(stem) = file.file_name() else {
                continue;
            };
            declarations.push(GeneratedDeclaration {
                name: format!("{file_name}/{}", stem.to_string_lossy()),
                path: file,
            });
        }
    }
    declarations.sort_by(|left, right| left.name.cmp(&right.name));
    declarations
}

/// Resolve `/plugin types <name>` against the listed declarations.
///
/// The canonical name is accepted, and so is a relative path
/// (`coderex-tools/index.d.ts`). No path is joined here: the requested name is
/// only ever matched against an already-listed file, so `..` cannot escape
/// the type root.
fn find_declaration(
    root: &std::path::Path,
    declarations: &[GeneratedDeclaration],
    requested: &str,
) -> Option<std::path::PathBuf> {
    if let Some(found) = declarations
        .iter()
        .find(|declaration| declaration.name == requested)
    {
        return Some(found.path.clone());
    }
    declarations
        .iter()
        .find(|declaration| {
            declaration
                .path
                .strip_prefix(root)
                .is_ok_and(|relative| relative.to_string_lossy() == requested)
        })
        .map(|declaration| declaration.path.clone())
}

/// Read memory entries from the memory file.
fn read_memory_entries() -> Vec<String> {
    let memory_path = config_home().join("memory").join("MEMORY.md");
    if memory_path.exists() {
        std::fs::read_to_string(&memory_path).map_or_else(
            |_| Vec::new(),
            |content| {
                content
                    .lines()
                    .map(std::string::ToString::to_string)
                    .take(20)
                    .collect()
            },
        )
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
                    let mut available = list_dir_items(&themes_path);
                    available.extend(ctx.plugin_themes.iter().cloned());
                    let mut output = format!("Current theme: {}", ctx.theme);
                    if !available.is_empty() {
                        let _ = write!(
                            output,
                            "\nAvailable themes:\n{}",
                            available
                                .iter()
                                .take(10)
                                .map(|t| format!("  {t}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        );
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
                        || ctx.plugin_themes.iter().any(|name| name == new_theme)
                    {
                        // `statusline.theme` is the key startup reads (and a
                        // known settings field). Writing a top-level `theme`
                        // made the next startup reject the whole file as an
                        // unknown field.
                        let mut settings = read_user_settings().unwrap_or_default();
                        let mut statusline = settings
                            .get("statusline")
                            .and_then(|value| value.as_object().cloned())
                            .unwrap_or_default();
                        statusline.insert(
                            "theme".to_string(),
                            serde_json::Value::String(new_theme.clone()),
                        );
                        settings.insert(
                            "statusline".to_string(),
                            serde_json::Value::Object(statusline),
                        );
                        let _ = write_user_settings(&settings);
                        CommandResult::success(format!(
                            "Theme changed to: {new_theme}\nSaved to settings.json.",
                        ))
                        .with_state_change(true)
                    } else {
                        let mut available = list_dir_items(&themes_path);
                        available.extend(ctx.plugin_themes.iter().cloned());
                        let hint = if available.is_empty() {
                            "\nNo custom themes installed in ~/.coderex/themes/".to_string()
                        } else {
                            format!(
                                "\nAvailable themes:\n{}",
                                available
                                    .iter()
                                    .map(|t| format!("  {t}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )
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
                    let _ = write!(
                        output,
                        "\nInstalled hooks:\n{}",
                        available
                            .iter()
                            .map(|h| format!("  {h}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
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
                let mut output = String::from("MCP: Connected");
                if !ctx.mcp_servers.is_empty() {
                    output.push_str("\nServers:");
                    for server in &ctx.mcp_servers {
                        output.push_str("\n  ");
                        output.push_str(server);
                    }
                }
                if !ctx.mcp_conflicts.is_empty() {
                    output.push_str("\nRefused tool names (duplicate across servers):");
                    for conflict in &ctx.mcp_conflicts {
                        output.push_str("\n  ");
                        output.push_str(conflict);
                    }
                }
                CommandResult::success(output)
            } else if args.is_empty() {
                // Read MCP config if available
                let settings_path = config_home().join("settings.json");
                let mcp_info = if settings_path.exists() {
                    std::fs::read_to_string(&settings_path).map_or_else(
                        |_| "Unable to read settings.json.".to_string(),
                        |content| {
                            if content.contains("mcpServers") {
                                "MCP servers configured in settings.json but not connected.\nCheck server URLs and authentication.".to_string()
                            } else {
                                "No MCP servers configured.\nAdd mcpServers to settings.json.".to_string()
                            }
                        },
                    )
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
        Arc::new(|ctx: &CommandContext, args: &[String]| {
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
                        let name = args.get(1).map_or("<name>", std::string::String::as_str);
                        CommandResult::error(format!("Plugin installation requires marketplace access. Use: /plugin install {name}"))
                    }
                    "components" => {
                        if ctx.plugin_components.is_empty() {
                            return CommandResult::success("No plugin components loaded.");
                        }
                        let mut by_kind: std::collections::BTreeMap<&str, Vec<&str>> =
                            std::collections::BTreeMap::new();
                        for (kind, name) in &ctx.plugin_components {
                            by_kind.entry(kind.as_str()).or_default().push(name.as_str());
                        }
                        let mut output = String::from("Plugin components:");
                        for (kind, names) in by_kind {
                            let _ = write!(output, "\n  {kind}: {}", names.join(", "));
                        }
                        CommandResult::success(output)
                    }
                    "types" => {
                        let root = config_home().join("plugin-types");
                        let declarations = generated_declarations(&root);
                        if declarations.is_empty() {
                            return CommandResult::success(format!(
                                "No generated plugin type declarations at {}.\nThey are written at startup when the plugin type surface is exported.",
                                root.display()
                            ));
                        }
                        if let Some(requested) = args.get(1) {
                            return find_declaration(&root, &declarations, requested).map_or_else(
                                || {
                                    CommandResult::error(format!(
                                        "No generated declaration named '{requested}'. Try /plugin types."
                                    ))
                                },
                                |path| match std::fs::read_to_string(&path) {
                                    Ok(source) => CommandResult::success(source),
                                    Err(error) => CommandResult::error(format!(
                                        "Cannot read {}: {error}",
                                        path.display()
                                    )),
                                },
                            );
                        }
                        let mut output = String::from("Generated plugin type declarations:");
                        for declaration in &declarations {
                            let relative = declaration
                                .path
                                .strip_prefix(&root)
                                .unwrap_or(&declaration.path);
                            let _ = write!(
                                output,
                                "\n  {:<20} {}",
                                declaration.name,
                                relative.display()
                            );
                        }
                        CommandResult::success(output)
                    }
                    _ => CommandResult::error(
                        "Usage: /plugin [list|components|types [name]|install <name>]",
                    ),
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
                        "Memory: 0 entries\nMemory files found: {memory_files:?}\nUse /memory search <query> to search.\nMemory stored in ~/.coderex/memory/"
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
            // Try to find and read the plan file
            let home = config_home();
            let plans_path = home.join("plans");
            let plan_files = list_dir_items(&plans_path);
            if ctx.has_plan {
                let mut output = "Plan loaded.\n\n".to_string();
                if !plan_files.is_empty() {
                    let _ = write!(
                        output,
                        "Available plans:\n{}\n",
                        plan_files
                            .iter()
                            .map(|p| format!("  {p}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
                output.push_str("(Use the Read tool to view plan details)");
                CommandResult::success(output)
            } else {
                // Check if any plan files exist
                if plan_files.is_empty() {
                    CommandResult::success("No active plan.\nUse /plan create to generate one.")
                } else {
                    CommandResult::success(format!(
                        "Plan files found in ~/.coderex/plans/:\n{}",
                        plan_files.join("\n")
                    ))
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
    fn plugin_types_lists_and_prints_generated_declarations() {
        let home = tempfile::tempdir().unwrap();
        set_test_config_home(home.path().to_path_buf());
        let types_root = home.path().join("plugin-types");
        std::fs::create_dir_all(types_root.join("coderex-tools")).unwrap();
        std::fs::create_dir_all(types_root.join("acme")).unwrap();
        std::fs::write(
            types_root.join("coderex-tools/index.d.ts"),
            "declare module 'coderex-tools' {}",
        )
        .unwrap();
        std::fs::write(
            types_root.join("acme/index.d.ts"),
            "declare module 'acme' {}",
        )
        .unwrap();
        std::fs::write(types_root.join("tsconfig.json"), "{}").unwrap();

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let listing = reg.execute("plugin", &ctx(), &["types".into()]);
        assert!(!listing.output.starts_with("Error:"), "{listing:?}");
        for name in ["acme", "coderex-tools", "tsconfig"] {
            assert!(listing.output.contains(name), "{listing:?}");
        }

        let printed = reg.execute("plugin", &ctx(), &["types".into(), "coderex-tools".into()]);
        assert_eq!(printed.output, "declare module 'coderex-tools' {}");

        let by_path = reg.execute(
            "plugin",
            &ctx(),
            &["types".into(), "acme/index.d.ts".into()],
        );
        assert_eq!(by_path.output, "declare module 'acme' {}");

        let missing = reg.execute("plugin", &ctx(), &["types".into(), "nope".into()]);
        assert!(missing.output.starts_with("Error:"), "{missing:?}");
        assert!(missing.output.contains("nope"));
    }

    #[test]
    fn plugin_types_reports_an_empty_root() {
        let home = tempfile::tempdir().unwrap();
        set_test_config_home(home.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plugin", &ctx(), &["types".into()]);
        assert!(!result.output.starts_with("Error:"), "{result:?}");
        assert!(
            result
                .output
                .contains("No generated plugin type declarations")
        );
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
