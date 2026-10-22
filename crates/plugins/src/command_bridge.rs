//! Plugin command bridge — registers plugin commands as slash commands.
//!
//! Iterates over loaded plugins' discovered commands and registers each
//! as a `SlashCommand` in the `CommandRegistry`.

use crate::plugins::PluginManager;
use coderex_commands::registry::{CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Bridges plugin-discovered commands into a `CommandRegistry`.
#[derive(Debug)]
pub struct PluginCommandBridge;

impl PluginCommandBridge {
    /// Register all commands from active process and wasm plugins into the registry.
    ///
    /// Command names are namespaced as `{plugin_name}/{command_name}`.
    pub fn register_commands(plugin_manager: &Arc<PluginManager>, registry: &mut CommandRegistry) {
        // Process plugins
        for entry in &plugin_manager.active_processes {
            let plugin_name = entry.key().clone();
            let process = entry.value();

            for cmd_info in process.commands() {
                let name = format!("{}/{}", plugin_name, cmd_info.name);
                let name_static: &'static str = Box::leak(name.into_boxed_str());

                let desc = format!("{} (plugin: {})", cmd_info.description, plugin_name);
                let desc_static: &'static str = Box::leak(desc.into_boxed_str());

                let pm = Arc::clone(plugin_manager);
                let p_name = plugin_name.clone();
                let c_name = cmd_info.name.clone();

                let handler = Arc::new(
                    move |_ctx: &coderex_commands::registry::CommandContext, args: &[String]| {
                        let result = tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(async {
                                pm.execute_command(&p_name, &c_name, args).await
                            })
                        });

                        match result {
                            Ok(output) => CommandResult::success(
                                crate::plugin_ui::render_plugin_response(&output),
                            ),
                            Err(e) => CommandResult::error(e),
                        }
                    },
                );

                registry.register(SlashCommand::local(
                    name_static,
                    desc_static,
                    true, // supports_non_interactive
                    true, // remote_safe
                    true, // bridge_safe
                    handler,
                ));
            }
        }

        // WASM plugins
        for entry in &plugin_manager.active_wasm_instances {
            let plugin_name = entry.key().clone();
            let instance = entry.value();

            for cmd_info in instance.commands() {
                let name = format!("{}/{}", plugin_name, cmd_info.name);
                let name_static: &'static str = Box::leak(name.into_boxed_str());

                let desc = format!("{} (plugin: {})", cmd_info.description, plugin_name);
                let desc_static: &'static str = Box::leak(desc.into_boxed_str());

                let pm = Arc::clone(plugin_manager);
                let p_name = plugin_name.clone();
                let c_name = cmd_info.name.clone();

                let handler = Arc::new(
                    move |_ctx: &coderex_commands::registry::CommandContext, args: &[String]| {
                        let result = tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(async {
                                pm.execute_command(&p_name, &c_name, args).await
                            })
                        });

                        match result {
                            Ok(output) => CommandResult::success(
                                crate::plugin_ui::render_plugin_response(&output),
                            ),
                            Err(e) => CommandResult::error(e),
                        }
                    },
                );

                registry.register(SlashCommand::local(
                    name_static,
                    desc_static,
                    true, // supports_non_interactive
                    true, // remote_safe
                    true, // bridge_safe
                    handler,
                ));
            }
        }
    }
}
