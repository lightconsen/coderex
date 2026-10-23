//! REPL loop — interactive conversation with the QueryEngine.

use crate::mcp_elicitation::{SessionElicitationHandler, TuiElicitationRequest};
use crate::session_resume;
use anyhow::Result;
use coderex_agents::manager::{
    AgentManager, AgentSubscription, ParentContextSnapshot, TeammateEvent,
};
use coderex_commands::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use coderex_commands::{CommandHistory, MacroRegistry};
use coderex_core::api_client::ApiClient;
use coderex_core::confirmation_bus::{ConfirmationBus, ConfirmationRequest, ConfirmationResponse};
use coderex_core::elicitation::{ElicitationHandler, ElicitationResponse, ElicitationResult};
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, background_feature_label,
};
use coderex_core::permission_engine::{
    AutoClassifierHook, BackgroundAgentDenyHook, ExplainPermissionHook, PermissionEngine,
};
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::prompts;
use coderex_core::query_profiles::{ModelProfile, PromptProfile, QueryKind, QueryRequest};
use coderex_core::state::SessionState;
use coderex_core::tool_executor::ToolExecutor as _;
use coderex_core::types::ModelId;
use coderex_core::types::{AgentLifecycleEvent, EngineEvent, Message, ToolCall};
use coderex_memory::{
    DreamCheckResult, ai_workflow,
    auto_dream::{AutoDream, DreamConfig},
    long_term::LongTermStore,
    session_memory::SessionMemoryStore,
    team_memory::TeamMemorySyncService,
};
use coderex_tools::{
    AgentTool, ChromeClient, CronScheduler, ExecutionConfig, ExecutionContext, RateLimiter,
    RegistryToolDispatcher, TaskStore, ToolExecutor as ToolsExecutor, ToolRegistry,
    register_builtin_tools,
};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Session-level state that persists across turns.
#[derive(Debug, Default)]
struct SessionEffects {
    /// Active prompt profiles from mode commands.
    prompt_profiles: Vec<PromptProfile>,
    /// Additional system prompt text appended by commands.
    append_system_prompt: Option<String>,
    /// Full system prompt override (replaces base prompt).
    override_system_prompt: Option<String>,
    /// Allowed tools override (None = use defaults).
    allowed_tools_override: Option<Vec<String>>,
    /// Additional user-context fragments accumulated from commands.
    extra_context: Vec<String>,
    /// Whether a title has been generated for this session.
    title_generated: bool,
}

// ─── Runtime tool handles ───────────────────────────────────────────────────
//
// `TeamCreate`, `TeamDelete`, `EnterWorktree`, `ExitWorktree`,
// `ListMcpResources`, and `ReadMcpResource` are baseline tools that expect a
// handle in `ToolContext.extra` under `TEAM_MANAGER_KEY`,
// `WORKTREE_HANDLE_KEY`, and `MCP_RESOURCE_HANDLE_KEY`. The tools crate keeps
// those types and keys private (`mod builtins` is not public), and the
// dispatcher has no generic extra-injection setter, so the session cannot put
// the handles into the context from here.
//
// What it can do is register a tool of the same name that delegates its
// metadata to the built-in implementation and performs the work against the
// concrete manager directly. `ToolRegistry::register` replaces by name, so
// registering these after `register_builtin_tools` swaps the unusable built-in
// for the wired one without changing the tool set the model sees.

/// The live MCP manager slot shared with [`ReplLoop`].
pub(crate) type McpManagerSlot =
    Arc<std::sync::RwLock<Option<Arc<coderex_mcp::manager::McpManager>>>>;

/// Shared team state for the `TeamCreate`/`TeamDelete` replacements.
#[derive(Debug, Default)]
pub(crate) struct TeamToolState {
    manager: std::sync::Mutex<coderex_core::TeamManager>,
}

impl TeamToolState {
    fn create(&self, name: &str) -> Result<String, String> {
        let mut manager = self
            .manager
            .lock()
            .map_err(|_| "team manager lock poisoned".to_string())?;
        Ok(manager.create_team(name).id.clone())
    }

    fn delete(&self, team_id: &str) -> Result<(), String> {
        let mut manager = self
            .manager
            .lock()
            .map_err(|_| "team manager lock poisoned".to_string())?;
        manager
            .delete_team(team_id)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Shared worktree state for the `EnterWorktree`/`ExitWorktree` replacements.
///
/// `base` is the checkout the session started in; worktrees are created under
/// `<base>/.coderex/worktrees`, the same layout `--worktree` uses.
pub(crate) struct WorktreeToolState {
    base: std::path::PathBuf,
    manager: std::sync::Mutex<coderex_core::worktree::WorktreeManager>,
    active: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl WorktreeToolState {
    fn new(base: &std::path::Path) -> Self {
        let config = coderex_core::worktree::WorktreeConfig {
            base_dir: base.join(".coderex").join("worktrees"),
            ..coderex_core::worktree::WorktreeConfig::default()
        };
        Self {
            base: base.to_path_buf(),
            manager: std::sync::Mutex::new(coderex_core::worktree::WorktreeManager::new(config)),
            active: std::sync::Mutex::new(None),
        }
    }

    fn enter(&self, branch: Option<&str>) -> Result<String, String> {
        let mut manager = self
            .manager
            .lock()
            .map_err(|_| "worktree manager lock poisoned".to_string())?;
        let created = manager
            .create_worktree(&self.base, branch)
            .map_err(|error| error.to_string())?;
        drop(manager);
        *self
            .active
            .lock()
            .map_err(|_| "worktree state lock poisoned".to_string())? = Some(created.path.clone());
        Ok(created.path.to_string_lossy().to_string())
    }

    fn exit(&self) -> Result<(), String> {
        let Some(path) = self
            .active
            .lock()
            .map_err(|_| "worktree state lock poisoned".to_string())?
            .take()
        else {
            return Err("no active worktree".to_string());
        };
        let mut manager = self
            .manager
            .lock()
            .map_err(|_| "worktree manager lock poisoned".to_string())?;
        manager
            .remove_worktree(&path)
            .map_err(|error| error.to_string())
    }
}

/// MCP resource access backed by the session's live manager.
///
/// Discovery fills the manager's registry and cache; a URI that is not cached
/// is read from its server with `resources/read` and cached for next time.
pub(crate) struct McpResourceToolState {
    slot: McpManagerSlot,
}

impl McpResourceToolState {
    fn list(&self) -> Vec<(String, String, String)> {
        let Some(manager) = self.slot.read().unwrap().as_ref().cloned() else {
            return Vec::new();
        };
        manager
            .list_resource_info()
            .into_iter()
            .map(|resource| (resource.uri, resource.name, resource.mime_type))
            .collect()
    }

    async fn read(&self, uri: &str) -> Result<String, String> {
        let Some(manager) = self.slot.read().unwrap().as_ref().cloned() else {
            return Err("no MCP manager is attached to this session".to_string());
        };
        let content = manager.read_resource_any(uri).await?;
        if let Some(text) = content.text {
            return Ok(text);
        }
        if let Some(blob) = content.blob {
            return Ok(blob);
        }
        Err(format!("resource '{uri}' has no text content"))
    }
}

/// One row from [`McpPromptToolState::list`]:
/// `(server, name, description, arguments)` where each argument is
/// `(name, description, required)`.
type McpPromptRow = (
    String,
    String,
    Option<String>,
    Vec<(String, Option<String>, bool)>,
);

/// MCP prompt access backed by the session's live manager.
///
/// Discovery fills the manager's prompt registry; resolving a prompt issues a
/// live `prompts/get` and renders its messages as text.
pub(crate) struct McpPromptToolState {
    slot: McpManagerSlot,
}

impl McpPromptToolState {
    fn list(&self) -> Vec<McpPromptRow> {
        let Some(manager) = self.slot.read().unwrap().as_ref().cloned() else {
            return Vec::new();
        };
        manager
            .list_prompts()
            .into_iter()
            .map(|entry| {
                let arguments = entry
                    .prompt
                    .arguments
                    .into_iter()
                    .map(|argument| (argument.name, argument.description, argument.required))
                    .collect();
                (
                    entry.server,
                    entry.prompt.name,
                    entry.prompt.description,
                    arguments,
                )
            })
            .collect()
    }

    async fn get(
        &self,
        server: &str,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, String> {
        let Some(manager) = self.slot.read().unwrap().as_ref().cloned() else {
            return Err("no MCP manager is attached to this session".to_string());
        };
        let result = manager.get_prompt(server, name, arguments).await?;
        Ok(result.render_text())
    }
}

/// Concrete handles for the baseline tools that need runtime state.
#[derive(Clone)]
pub(crate) struct RuntimeToolHandles {
    pub(crate) team: Arc<TeamToolState>,
    pub(crate) worktree: Arc<WorktreeToolState>,
    pub(crate) mcp: Arc<McpResourceToolState>,
    pub(crate) mcp_prompts: Arc<McpPromptToolState>,
}

impl RuntimeToolHandles {
    pub(crate) fn new(cwd: &str, mcp_slot: &McpManagerSlot) -> Self {
        Self {
            team: Arc::new(TeamToolState::default()),
            worktree: Arc::new(WorktreeToolState::new(std::path::Path::new(cwd))),
            mcp: Arc::new(McpResourceToolState {
                slot: Arc::clone(mcp_slot),
            }),
            mcp_prompts: Arc::new(McpPromptToolState {
                slot: Arc::clone(mcp_slot),
            }),
        }
    }

    /// Install the wired replacements into `registry`.
    ///
    /// Call after `register_builtin_tools`: registration is by name and the
    /// later registration wins.
    pub(crate) fn install(&self, registry: &mut ToolRegistry) {
        Self::replace(registry, "TeamCreate", |original| {
            Arc::new(TeamCreateWithState {
                original,
                state: Arc::clone(&self.team),
            })
        });
        Self::replace(registry, "TeamDelete", |original| {
            Arc::new(TeamDeleteWithState {
                original,
                state: Arc::clone(&self.team),
            })
        });
        Self::replace(registry, "EnterWorktree", |original| {
            Arc::new(EnterWorktreeWithState {
                original,
                state: Arc::clone(&self.worktree),
            })
        });
        Self::replace(registry, "ExitWorktree", |original| {
            Arc::new(ExitWorktreeWithState {
                original,
                state: Arc::clone(&self.worktree),
            })
        });
        Self::replace(registry, "ListMcpResources", |original| {
            Arc::new(ListMcpResourcesWithState {
                original,
                state: Arc::clone(&self.mcp),
            })
        });
        Self::replace(registry, "ReadMcpResource", |original| {
            Arc::new(ReadMcpResourceWithState {
                original,
                state: Arc::clone(&self.mcp),
            })
        });
        Self::replace(registry, "ListMcpPrompts", |original| {
            Arc::new(ListMcpPromptsWithState {
                original,
                state: Arc::clone(&self.mcp_prompts),
            })
        });
        Self::replace(registry, "GetMcpPrompt", |original| {
            Arc::new(GetMcpPromptWithState {
                original,
                state: Arc::clone(&self.mcp_prompts),
            })
        });
    }

    fn replace(
        registry: &mut ToolRegistry,
        name: &str,
        make: impl FnOnce(Arc<dyn coderex_tools::Tool>) -> Arc<dyn coderex_tools::Tool>,
    ) {
        if let Some(original) = registry.get_tool_arc(name) {
            registry.register(make(original));
        }
    }
}

struct TeamCreateWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<TeamToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for TeamCreateWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let name = input
            .arguments
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| coderex_tools::ToolError::InvalidInput {
                message: "missing required field: name".into(),
            })?;
        let team_id = self
            .state
            .create(name)
            .map_err(|message| coderex_tools::ToolError::InvalidInput { message })?;
        Ok(coderex_tools::ToolOutput {
            output: team_id,
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct TeamDeleteWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<TeamToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for TeamDeleteWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let team_id = input
            .arguments
            .get("team_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| coderex_tools::ToolError::InvalidInput {
                message: "missing required field: team_id".into(),
            })?;
        self.state
            .delete(team_id)
            .map_err(|message| coderex_tools::ToolError::InvalidInput { message })?;
        Ok(coderex_tools::ToolOutput {
            output: format!("deleted {team_id}"),
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct EnterWorktreeWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<WorktreeToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for EnterWorktreeWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let branch = input
            .arguments
            .get("branch")
            .and_then(serde_json::Value::as_str);
        let path = self
            .state
            .enter(branch)
            .map_err(|message| coderex_tools::ToolError::InvalidInput { message })?;
        Ok(coderex_tools::ToolOutput {
            output: path,
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct ExitWorktreeWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<WorktreeToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for ExitWorktreeWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        _input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        self.state
            .exit()
            .map_err(|message| coderex_tools::ToolError::InvalidInput { message })?;
        Ok(coderex_tools::ToolOutput {
            output: "exited".into(),
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct ListMcpResourcesWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<McpResourceToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for ListMcpResourcesWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        _input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let resources: Vec<_> = self
            .state
            .list()
            .into_iter()
            .map(|(uri, name, mime_type)| {
                serde_json::json!({ "uri": uri, "name": name, "mime_type": mime_type })
            })
            .collect();
        Ok(coderex_tools::ToolOutput {
            output: serde_json::to_string_pretty(&resources).unwrap_or_else(|_| "[]".into()),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Defaults the `LSP` tool uses when a call does not name a server.
///
/// Plugin manifests can declare `lspServers`; the first configured server
/// becomes the default so the model does not have to know a command line.
#[derive(Debug, Clone)]
struct PluginLspDefaults {
    command: String,
    args: Vec<String>,
    root_uri: Option<String>,
}

struct LspWithPluginDefaults {
    original: Arc<dyn coderex_tools::Tool>,
    defaults: Arc<std::sync::RwLock<Option<PluginLspDefaults>>>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for LspWithPluginDefaults {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let mut arguments = input.arguments.clone();
        let defaults = self
            .defaults
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let (Some(defaults), Some(object)) = (defaults, arguments.as_object_mut()) {
            object
                .entry("server_command")
                .or_insert_with(|| serde_json::Value::String(defaults.command.clone()));
            if !defaults.args.is_empty() {
                object.entry("server_args").or_insert_with(|| {
                    serde_json::Value::Array(
                        defaults
                            .args
                            .iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    )
                });
            }
            if let Some(root) = defaults.root_uri {
                object
                    .entry("root_uri")
                    .or_insert_with(|| serde_json::Value::String(root));
            }
        }
        self.original
            .execute(
                coderex_tools::ToolInput {
                    call_id: input.call_id,
                    name: input.name,
                    arguments,
                },
                ctx,
            )
            .await
    }
}

struct ReadMcpResourceWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<McpResourceToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for ReadMcpResourceWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let uri = input
            .arguments
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| coderex_tools::ToolError::InvalidInput {
                message: "missing required field: uri".into(),
            })?;
        let content = self
            .state
            .read(uri)
            .await
            .map_err(|message| coderex_tools::ToolError::InvalidInput { message })?;
        Ok(coderex_tools::ToolOutput {
            output: content,
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct ListMcpPromptsWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<McpPromptToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for ListMcpPromptsWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        _input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let prompts: Vec<_> = self
            .state
            .list()
            .into_iter()
            .map(|(server, name, description, arguments)| {
                let arguments: Vec<_> = arguments
                    .into_iter()
                    .map(|(name, description, required)| {
                        serde_json::json!({
                            "name": name,
                            "description": description,
                            "required": required,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "server": server,
                    "name": name,
                    "description": description,
                    "arguments": arguments,
                })
            })
            .collect();
        Ok(coderex_tools::ToolOutput {
            output: serde_json::to_string_pretty(&prompts).unwrap_or_else(|_| "[]".into()),
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct GetMcpPromptWithState {
    original: Arc<dyn coderex_tools::Tool>,
    state: Arc<McpPromptToolState>,
}

#[async_trait::async_trait]
impl coderex_tools::Tool for GetMcpPromptWithState {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        self.original.descriptor()
    }

    fn capabilities(&self) -> coderex_tools::ToolCapabilities {
        self.original.capabilities()
    }

    fn rule_exemptions_for(&self, input: &serde_json::Value) -> coderex_tools::ToolRuleExemptions {
        self.original.rule_exemptions_for(input)
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        let server = input
            .arguments
            .get("server")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| coderex_tools::ToolError::InvalidInput {
                message: "missing required field: server".into(),
            })?;
        let name = input
            .arguments
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| coderex_tools::ToolError::InvalidInput {
                message: "missing required field: name".into(),
            })?;
        let arguments = match input.arguments.get("arguments") {
            None | Some(serde_json::Value::Null) => serde_json::Map::new(),
            Some(serde_json::Value::Object(arguments)) => arguments.clone(),
            Some(_) => {
                return Err(coderex_tools::ToolError::InvalidInput {
                    message: "'arguments' must be an object".into(),
                });
            }
        };
        let content = self
            .state
            .get(server, name, arguments)
            .await
            .map_err(|message| coderex_tools::ToolError::InvalidInput { message })?;
        Ok(coderex_tools::ToolOutput {
            output: content,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// The slash-command name for a prompt: `mcp__<server>__<prompt>`.
///
/// The server and prompt components are sanitised; the raw names stay with the
/// handler and are what `prompts/get` sends.
fn prompt_command_name(server: &str, prompt: &str) -> String {
    format!(
        "mcp__{}__{}",
        sanitize_prompt_component(server),
        sanitize_prompt_component(prompt)
    )
}

/// Replace every character that is not an ASCII letter, digit, or `_`.
fn sanitize_prompt_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Run an `mcp__…` prompt command.
///
/// The registry's handler interface is synchronous, while `prompts/get` is a
/// request over the live channel. The CLI runs on a multi-threaded Tokio
/// runtime, so `block_in_place` keeps the worker's other tasks running while
/// this one waits for the server.
fn execute_mcp_prompt_command(
    slot: &McpManagerSlot,
    server: &str,
    prompt: &str,
    declared: &[coderex_mcp::prompts::McpPromptArgument],
    args: &[String],
) -> CommandResult {
    let arguments = match coderex_mcp::prompts::parse_prompt_arguments(args, declared) {
        Ok(arguments) => arguments,
        Err(error) => return CommandResult::error(error),
    };

    let Some(manager) = slot
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .cloned()
    else {
        return CommandResult::error("no MCP manager is attached to this session");
    };

    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(manager.get_prompt(server, prompt, arguments))
    });

    match result {
        Ok(result) => prompt_result_command(&result),
        Err(error) => CommandResult::error(error),
    }
}

/// The command result for a resolved prompt.
fn prompt_result_command(result: &coderex_mcp::prompts::McpPromptResult) -> CommandResult {
    CommandResult::success(result.render_text())
}

/// Translate a teammate callback event into the parent engine's lifecycle
/// event, so the UI and headless event stream consume the existing
/// `EngineEvent::AgentEvent` path instead of a second event system.
fn teammate_event_to_lifecycle(event: &TeammateEvent) -> (String, AgentLifecycleEvent) {
    match event {
        TeammateEvent::Idle { agent_id, reason } => (
            agent_id.clone(),
            AgentLifecycleEvent::Idle {
                reason: reason.clone(),
            },
        ),
        TeammateEvent::TaskCompleted {
            agent_id,
            subject,
            summary,
        } => {
            let summary = if subject.is_empty() {
                summary.clone()
            } else {
                format!("{subject}: {summary}")
            };
            (agent_id.clone(), AgentLifecycleEvent::Completed { summary })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractivePhase {
    Idle,
    Planning,
    Thinking,
    RunningTool(String),
    WaitingForPermission(String),
}

impl InteractivePhase {
    fn reset(&mut self) {
        *self = Self::Idle;
    }

    fn note_text_delta(&mut self) {
        if !matches!(self, Self::RunningTool(_)) {
            *self = Self::Thinking;
        }
    }

    fn note_tool_start(&mut self, summary: &str) {
        *self = Self::RunningTool(summary.to_string());
    }

    fn note_tool_end(&mut self) {
        *self = Self::Thinking;
    }

    fn note_permission_request(&mut self, tool: &str, args: &serde_json::Value) {
        *self = Self::WaitingForPermission(permission_activity_label(
            &ToolCall::summarize_activity(tool, args),
        ));
    }

    fn status_label(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Planning => Some("Planning...".into()),
            Self::Thinking => Some("Thinking...".into()),
            Self::RunningTool(summary) | Self::WaitingForPermission(summary) => {
                Some(summary.clone())
            }
        }
    }
}

fn permission_activity_label(summary: &str) -> String {
    if summary.is_empty() {
        "Waiting for permission...".into()
    } else {
        format!("Waiting for permission: {summary}")
    }
}

fn normalize_plan_label(label: &str) -> String {
    let trimmed = label.trim();
    let trimmed = trimmed.trim_end_matches('.');
    let trimmed = trimmed.trim_end_matches("...");
    if trimmed.is_empty() {
        "Working".into()
    } else {
        trimmed.into()
    }
}

fn push_activity_row(ui_state: &mut coderex_ui::state::UiState, label: &str) {
    let concise = normalize_plan_label(label);
    let duplicate = ui_state
        .activity_log
        .last()
        .is_some_and(|last| last == &concise);
    if duplicate {
        return;
    }

    ui_state.activity_log.push(concise.clone());
    if ui_state.activity_log.len() > 25 {
        let excess = ui_state.activity_log.len() - 25;
        ui_state.activity_log.drain(..excess);
    }

    ui_state.messages.push(coderex_ui::message_list::Message {
        role: coderex_ui::message_list::MessageRole::System,
        content: concise.into(),
        tool_name: None,
        streaming: false,
        folded: false,
    });
}

/// How a sub-agent's lifecycle reads in the UI.
///
/// The engine emits these, and nothing consumed them — the TUI's event match
/// ended at `_ => {}`, so delegating to a sub-agent looked like a silent pause
/// in the transcript. The agent's own text is deliberately not shown: the Agent
/// tool's result already carries it, and replaying a sub-agent's stream under
/// the main conversation would duplicate it with the wrong author.
fn agent_event_label(agent_id: &str, event: &AgentLifecycleEvent) -> Option<String> {
    let short: String = agent_id.chars().take(8).collect();
    Some(match event {
        AgentLifecycleEvent::Started => format!("Sub-agent {short} started"),
        AgentLifecycleEvent::ToolCall { name, .. } => format!("Sub-agent {short}: {name}"),
        AgentLifecycleEvent::Completed { .. } => format!("Sub-agent {short} finished"),
        AgentLifecycleEvent::Idle { reason } => format!("Sub-agent {short} idle ({reason})"),
        AgentLifecycleEvent::Failed { .. } => format!("Sub-agent {short} failed"),
        AgentLifecycleEvent::TextDelta(_) => return None,
    })
}

/// Put a sub-agent's lifecycle on screen: an activity row, and a notification
/// for the transitions a person would want to notice.
fn apply_agent_event(
    ui_state: &mut coderex_ui::state::UiState,
    agent_id: &str,
    event: &AgentLifecycleEvent,
) {
    let Some(label) = agent_event_label(agent_id, event) else {
        return;
    };
    push_activity_row(ui_state, &label);
    match event {
        AgentLifecycleEvent::Started => ui_state.notifications.info(label),
        AgentLifecycleEvent::Completed { summary } if !summary.trim().is_empty() => {
            ui_state
                .notifications
                .success(format!("{label}: {}", summary.trim()));
        }
        AgentLifecycleEvent::Completed { .. } => ui_state.notifications.success(label),
        AgentLifecycleEvent::Failed { error } => {
            ui_state.notifications.err(format!("{label}: {error}"));
        }
        // A tool call, idle note, or text delta is activity, not something to interrupt for.
        AgentLifecycleEvent::ToolCall { .. }
        | AgentLifecycleEvent::Idle { .. }
        | AgentLifecycleEvent::TextDelta(_) => {}
    }
}

fn set_plan_step(ui_state: &mut coderex_ui::state::UiState, label: &str) {
    let normalized = normalize_plan_label(label);
    if let Some(current) = ui_state.plan.steps.get_mut(ui_state.plan.current_step) {
        if current.label == normalized && !current.complete {
            ui_state.plan.current_label = Some(normalized);
            return;
        }
        current.complete = true;
    }

    ui_state.plan.steps.push(coderex_ui::state::PlanStep {
        label: normalized.clone(),
        complete: false,
    });
    ui_state.plan.current_step = ui_state.plan.steps.len().saturating_sub(1);
    ui_state.plan.current_label = Some(normalized);
}

fn complete_plan(ui_state: &mut coderex_ui::state::UiState) {
    if let Some(current) = ui_state.plan.steps.get_mut(ui_state.plan.current_step) {
        current.complete = true;
    }
}

fn selected_palette_command(ui_state: &coderex_ui::state::UiState) -> Option<String> {
    let coderex_ui::overlay::Overlay::CommandPalette {
        commands, selected, ..
    } = ui_state.overlays.top()?
    else {
        return None;
    };

    commands
        .get(*selected)
        .and_then(|entry| entry.split_whitespace().next())
        .map(ToString::to_string)
}

fn show_detail_overlay(
    ui_state: &mut coderex_ui::state::UiState,
    title: impl Into<String>,
    body: impl Into<String>,
) {
    ui_state
        .overlays
        .push(coderex_ui::overlay::Overlay::Detail {
            title: title.into(),
            body: body.into(),
        });
}

fn permission_request_args(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::Value::String(arguments.into()))
}

/// Translate one engine event into a frame of the line-delimited protocol.
///
/// `None` for events that are this process's own business — a permission dialog
/// the host cannot answer, an elevation request — rather than something a
/// consumer outside the process acts on. The protocol is what a host reads to
/// follow the conversation, not a mirror of everything the UI does.
fn protocol_frame(
    event: &EngineEvent,
    session_id: &str,
) -> Option<coderex_events::ProtocolMessage> {
    use coderex_events::{
        AssistantFrame, MessageBody, ProtocolMessage, SystemFrame, UserFrame, protocol,
    };

    Some(match event {
        // One frame per delta, as the reference streams them.
        EngineEvent::TextDelta(text) => ProtocolMessage::StreamEvent(protocol::StreamEventFrame {
            event: serde_json::json!({
                "type": "content_block_delta",
                "delta": { "type": "text_delta", "text": text },
            }),
            parent_tool_use_id: None,
            session_id: session_id.to_string(),
        }),
        EngineEvent::ToolCallStart { id, name, summary } => {
            ProtocolMessage::Assistant(AssistantFrame {
                message: MessageBody::assistant(vec![protocol::tool_use_block(
                    id,
                    name,
                    serde_json::json!({ "summary": summary }),
                )]),
                parent_tool_use_id: None,
                session_id: session_id.to_string(),
            })
        }
        EngineEvent::ToolCallEnd { id, result } => ProtocolMessage::User(UserFrame {
            // The tool result is a user-side message in the protocol: it is
            // what the model is sent back, and the host needs to see it there.
            // Images travel with it, or a stream-json consumer would see less
            // than the model did.
            message: MessageBody::user(vec![protocol::tool_result_block_with_images(
                id,
                &result.output,
                &result
                    .images
                    .iter()
                    .map(|image| (image.media_type.clone(), image.data.clone()))
                    .collect::<Vec<_>>(),
            )]),
            parent_tool_use_id: None,
            session_id: session_id.to_string(),
        }),
        EngineEvent::CompactBoundary => ProtocolMessage::System(SystemFrame {
            subtype: "compact_boundary".to_string(),
            session_id: session_id.to_string(),
            data: serde_json::Map::new(),
        }),
        EngineEvent::Notification { message } => ProtocolMessage::System(SystemFrame {
            subtype: "informational".to_string(),
            session_id: session_id.to_string(),
            data: serde_json::Map::from_iter([(
                "message".to_string(),
                serde_json::Value::String(message.clone()),
            )]),
        }),
        _ => return None,
    })
}

/// Requests whose `allow_session` bit is true, keyed by request id.
///
/// Shared between the turn loop (which emits the request) and the stdin
/// reader (which decides whether an `allow_session` answer may become
/// [`ConfirmationResponse::ApproveSession`]).
type PermissionSessionApprovals = std::sync::Arc<std::sync::Mutex<HashMap<String, bool>>>;

/// Turn one stdin line into a bus response.
///
/// `None` for anything that is not a usable `control_response`; the reader
/// ignores those lines rather than treating a chatty host as an error.
///
/// An `error` answer denies the tool: failing closed is the only safe reading
/// of "the host could not answer". A session-wide approval is honoured only
/// when the original request offered it — otherwise it degrades to a
/// single-call approval instead of widening silently.
fn control_response_from_line(
    line: &str,
    request_allows_session: bool,
) -> Option<(String, ConfirmationResponse)> {
    use coderex_events::protocol::{ControlDecision, ControlResponseFrame};

    let frame = ControlResponseFrame::parse_line(line).ok()?;
    let request_id = frame.response.request_id.clone();
    let response = match frame.decision().ok()? {
        ControlDecision::AllowSession if request_allows_session => {
            ConfirmationResponse::ApproveSession
        }
        ControlDecision::Allow | ControlDecision::AllowSession => ConfirmationResponse::Approved,
        ControlDecision::Deny | ControlDecision::Error(_) => ConfirmationResponse::Denied,
    };
    Some((request_id, response))
}

/// Read `control_response` lines from stdin for one turn.
///
/// Unknown and malformed lines are ignored: stdin is shared with whatever
/// else the host sends, and one bad line must not strand the turn. EOF ends
/// the reader silently. The task is aborted when the turn ends.
fn spawn_headless_control_reader(
    bus: ConfirmationBus,
    approvals: PermissionSessionApprovals,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::AsyncBufReadExt as _;

    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Parse the id once to look up whether this request permitted a
            // session-wide answer; the helper re-parses the same line to
            // apply the mapping.
            let Ok(frame) = coderex_events::protocol::ControlResponseFrame::parse_line(&line)
            else {
                continue;
            };
            let request_id = &frame.response.request_id;
            let allowed = approvals
                .lock()
                .is_ok_and(|approvals| approvals.get(request_id).copied().unwrap_or(false));
            if let Some((request_id, response)) = control_response_from_line(&line, allowed) {
                bus.respond(&request_id, response).await;
            }
        }
    })
}

fn sync_permission_overlay(
    ui_state: &mut coderex_ui::state::UiState,
    tool: &str,
    args: &str,
    risk_summary: &str,
    always_option: bool,
) {
    ui_state.permission_detail = Some(coderex_ui::state::PermissionDetail {
        tool: tool.to_string(),
        args: args.to_string(),
        risk_summary: risk_summary.to_string(),
    });

    if matches!(
        ui_state.overlays.top(),
        Some(coderex_ui::overlay::Overlay::PermissionDialog { .. })
    ) {
        ui_state.overlays.pop();
    }

    ui_state
        .overlays
        .push(coderex_ui::overlay::Overlay::PermissionDialog {
            tool: tool.to_string(),
            args: args.to_string(),
            risk_summary: risk_summary.to_string(),
            always_option,
        });
}

fn clear_permission_overlay(ui_state: &mut coderex_ui::state::UiState) {
    ui_state.permission_detail = None;
    ui_state.permission_bar.clear_pending();
    if matches!(
        ui_state.overlays.top(),
        Some(coderex_ui::overlay::Overlay::PermissionDialog { .. })
    ) {
        ui_state.overlays.pop();
    }
}

fn core_message_to_ui_message(
    message: &coderex_core::types::Message,
) -> coderex_ui::message_list::Message {
    let (role, content, tool_name) = match (&message.role, &message.content) {
        (coderex_core::types::Role::User, coderex_core::types::MessageContent::Text(text)) => (
            coderex_ui::message_list::MessageRole::User,
            text.clone(),
            None,
        ),
        (coderex_core::types::Role::Assistant, coderex_core::types::MessageContent::Text(text)) => {
            (
                coderex_ui::message_list::MessageRole::Assistant,
                text.clone(),
                None,
            )
        }
        (coderex_core::types::Role::System, coderex_core::types::MessageContent::Text(text)) => (
            coderex_ui::message_list::MessageRole::System,
            text.clone(),
            None,
        ),
        (coderex_core::types::Role::Tool, coderex_core::types::MessageContent::ToolCall(call)) => (
            coderex_ui::message_list::MessageRole::ToolCall,
            call.activity_summary(),
            Some(call.name.clone()),
        ),
        (
            coderex_core::types::Role::Tool,
            coderex_core::types::MessageContent::ToolResult(result),
        ) => (
            coderex_ui::message_list::MessageRole::ToolResult,
            result.output.clone(),
            Some(result.tool_call_id.clone()),
        ),
        (_, content) => (
            coderex_ui::message_list::MessageRole::System,
            format!("{content:?}"),
            None,
        ),
    };

    coderex_ui::message_list::Message {
        role,
        content: content.into(),
        tool_name: tool_name.map(Into::into),
        streaming: false,
        folded: false,
    }
}

fn restore_transcript_messages(
    ui_state: &mut coderex_ui::state::UiState,
    messages: &[coderex_core::types::Message],
) {
    ui_state.messages = coderex_ui::message_list::MessageView::new(1000);
    ui_state.messages.start = 0;
    for message in messages {
        ui_state.messages.push(core_message_to_ui_message(message));
    }
}

fn apply_interactive_snapshot(
    ui_state: &mut coderex_ui::state::UiState,
    snapshot: &session_resume::InteractiveResumeSnapshot,
) {
    ui_state.plan.steps = snapshot
        .plan
        .steps
        .iter()
        .map(|step| coderex_ui::state::PlanStep {
            label: step.label.clone(),
            complete: step.complete,
        })
        .collect();
    ui_state.plan.current_step = snapshot.plan.current_step;
    ui_state
        .plan
        .current_label
        .clone_from(&snapshot.plan.current_label);
    ui_state.activity_log.clone_from(&snapshot.activity_log);
}

fn build_interactive_snapshot(
    ui_state: &coderex_ui::state::UiState,
    session_title: Option<String>,
) -> session_resume::InteractiveResumeSnapshot {
    session_resume::InteractiveResumeSnapshot {
        session_title,
        plan: session_resume::PersistedInteractivePlan {
            steps: ui_state
                .plan
                .steps
                .iter()
                .map(|step| session_resume::PersistedPlanStep {
                    label: step.label.clone(),
                    complete: step.complete,
                })
                .collect(),
            current_step: ui_state.plan.current_step,
            current_label: ui_state.plan.current_label.clone(),
        },
        activity_log: ui_state.activity_log.clone(),
    }
}

/// Result from a completed background task, delivered to the UI.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BackgroundTaskResult {
    /// The task ID.
    id: String,
    /// Which background feature ran.
    kind: BackgroundFeatureKind,
    /// Whether it succeeded.
    success: bool,
    /// Output content (result text or error message).
    content: String,
}

/// Elicitation handler for the full-screen TUI.
///
/// This keeps clarification prompts inside the TUI state instead of writing
/// directly to stderr and corrupting the screen.
#[derive(Debug, Clone)]
struct TuiElicitationHandler {
    tx: tokio::sync::mpsc::Sender<TuiElicitationRequest>,
}

impl TuiElicitationHandler {
    const fn new(tx: tokio::sync::mpsc::Sender<TuiElicitationRequest>) -> Self {
        Self { tx }
    }
}

/// How a form field's type reads in the transcript.
const fn field_type_label(field: &coderex_mcp::elicitation::FormField) -> &'static str {
    match field.field_type {
        coderex_mcp::elicitation::FormFieldType::String => "text",
        coderex_mcp::elicitation::FormFieldType::Integer => "integer",
        coderex_mcp::elicitation::FormFieldType::Number => "number",
        coderex_mcp::elicitation::FormFieldType::Boolean => "true/false",
    }
}

#[async_trait::async_trait]
impl ElicitationHandler for TuiElicitationHandler {
    async fn request(
        &self,
        questions: Vec<coderex_core::elicitation::ElicitationQuestion>,
    ) -> ElicitationResult {
        let Some(question) = questions.first() else {
            return ElicitationResult::Cancelled;
        };

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = TuiElicitationRequest {
            prompt: question.question.clone(),
            form: None,
            response_tx,
        };

        if self.tx.send(request).await.is_err() {
            return ElicitationResult::Cancelled;
        }

        response_rx
            .await
            .map_or(ElicitationResult::Cancelled, |value| {
                let answer = value
                    .get("answer")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(ToString::to_string);

                answer.map_or(ElicitationResult::Cancelled, |other_text| {
                    ElicitationResult::Responded(ElicitationResponse {
                        request_id: question.header.clone(),
                        selected: Vec::new(),
                        other_text: Some(other_text),
                    })
                })
            })
    }
}

/// Handle for an active background task.
#[derive(Debug)]
struct BackgroundTask {
    /// Unique task ID.
    id: String,
    /// Feature kind.
    kind: BackgroundFeatureKind,
    /// Cancellation token — signal to abort the task.
    cancel_token: CancellationToken,
}

/// The REPL event loop — reads user input, executes commands or queries.
#[allow(missing_debug_implementations)]
/// Runs a workflow's agents through the session's agent manager.
///
/// A workflow spawns agents the same way the Agent tool does; what it adds is
/// being asked for them by a script, one at a time and in parallel.
struct WorkflowAgentRunner {
    manager: Arc<tokio::sync::Mutex<AgentManager>>,
    session_id: String,
    cwd: String,
}

#[async_trait::async_trait]
impl coderex_workflow::AgentRunner for WorkflowAgentRunner {
    async fn run(
        &self,
        request: coderex_workflow::AgentRequest,
    ) -> Result<coderex_workflow::AgentOutput, String> {
        use coderex_agents::manager::SpawnOptions;
        use coderex_agents::types::{AgentType, Thoroughness};

        // A tool the agent was told not to use cannot be spelled here, so the
        // spawn is refused rather than run un-narrowed — the script asked for
        // something narrower than this can provide.
        if !request.disallowed_tools.is_empty() {
            return Err(format!(
                "this workflow asked to withhold {:?} from an agent, which this session cannot                  express; refusing the spawn rather than running it un-narrowed",
                request.disallowed_tools
            ));
        }

        let agent_type = match request.agent_type.as_deref() {
            Some("explore") => AgentType::Explore {
                thoroughness: Thoroughness::Medium,
            },
            Some("plan") => AgentType::Plan,
            Some("verification" | "verify") => AgentType::Verification,
            Some("claude_code_guide") => AgentType::ClaudeCodeGuide,
            _ => AgentType::GeneralPurpose,
        };

        let options = SpawnOptions {
            model_override: request.model.clone(),
            isolation: request.isolation.clone(),
            output_schema: request.schema.clone(),
            ..Default::default()
        };

        let mut manager = self.manager.lock().await;
        let instance = manager
            .spawn_agent(
                agent_type,
                &request.prompt,
                options,
                &self.session_id,
                &self.cwd,
                0,
            )
            .await
            .map_err(|error| format!("could not start the agent: {error}"))?;
        let agent_id = instance.task_handle.id.clone();

        let result = manager
            .wait_for_completion(&agent_id)
            .await
            .map_err(|error| format!("the agent did not finish: {error}"))?;
        drop(manager);

        Ok(coderex_workflow::AgentOutput {
            text: Some(result.output),
            tokens: result.token_usage.unwrap_or(0),
        })
    }
}

/// The Workflow tool, with its enablement decided per call.
///
/// `--betas` is applied in [`ReplLoop::apply_cli_config`], after
/// [`ReplLoop::with_engine`] has built and shared the tool registry. The
/// registry is immutable once shared, so a plain `WorkflowTool` frozen its
/// `enabled` bit at construction and `--betas +workflow_scripts` could not
/// reach it. This wrapper keeps the bit shared: the descriptor (what the model
/// is offered) and the execute gate both read it, so the later overlay turns
/// the already-registered tool on without rebuilding the registry.
struct BetaGatedWorkflowTool {
    inner: coderex_workflow::WorkflowTool,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl BetaGatedWorkflowTool {
    fn new(inner: coderex_workflow::WorkflowTool) -> Self {
        Self {
            // The inner tool always accepts calls; the wrapper is the gate.
            inner: inner.with_enabled(true),
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The shared named-script map, so plugin workflows can be added after
    /// the registry is built.
    fn named_script_map(
        &self,
    ) -> Arc<std::sync::RwLock<std::collections::HashMap<String, String>>> {
        self.inner.named_scripts()
    }

    fn enabled_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.enabled)
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl coderex_tools::Tool for BetaGatedWorkflowTool {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        let mut descriptor = self.inner.descriptor();
        descriptor.is_enabled = self.enabled.load(std::sync::atomic::Ordering::Relaxed);
        descriptor
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        if !self.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(coderex_tools::ToolError::InvalidInput {
                message: "workflow scripts are not enabled in this session — set \
                          --betas +workflow_scripts (or \
                          CODEREX_FEATURE_FLAGS=+workflow_scripts) to turn them on"
                    .to_string(),
            });
        }
        self.inner.execute(input, ctx).await
    }
}

/// Resolve the organization scope for org memory.
///
/// `CODEREX_ORG_ID` wins; otherwise the org id from the remote-managed
/// settings cache (written by the orchestrator's startup phase) is used. An
/// absent or blank value leaves org memory unconfigured rather than inventing
/// a scope, which would silently mix organizations.
fn configured_org_id(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The org id from a remote-settings cache document.
///
/// Kept as a pure parse so the file layout is tested without touching the
/// developer's real config home.
fn org_id_from_remote_settings_json(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    configured_org_id(
        value
            .get("settings")?
            .get("org_id")?
            .as_str()
            .map(str::to_string),
    )
}

/// Read the org id the orchestrator cached, when remote settings configured one.
fn cached_org_id() -> Option<String> {
    let path = coderex_config::config_home().join("remote_settings_cache.json");
    let document = std::fs::read_to_string(path).ok()?;
    org_id_from_remote_settings_json(&document)
}

/// Build the org-scoped store, when an organization is configured.
fn org_memory_service(
    memory_base: &std::path::Path,
    org_id: Option<String>,
) -> Option<Arc<TeamMemorySyncService>> {
    let org_id = org_id?;
    Some(Arc::new(TeamMemorySyncService::new_with_org(
        "default".to_string(),
        memory_base.join("org"),
        org_id,
    )))
}

/// How many hook entries a settings document declares.
///
/// The loader accepts both a bare array and `{"hooks":[...]}`. This counts
/// top-level entries for the status display; it deliberately does not try to
/// validate them.
fn count_configured_hooks(value: &serde_json::Value) -> usize {
    value
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())
        .map_or(0, Vec::len)
}

/// Drop `type: "agent"` hook entries from a settings hooks document.
///
/// Returns the filtered document and how many entries were removed. Both shapes
/// [`coderex_hooks`] accepts are handled — a bare hooks array and an object
/// with a `hooks` array — so the caller can gate the agent-kind path without
/// knowing which shape each source used. A document of any other shape is
/// returned unchanged and reported as zero skipped, so the hooks crate still
/// produces the structural error itself.
fn strip_agent_hooks(value: &serde_json::Value) -> (serde_json::Value, usize) {
    fn is_agent(entry: &serde_json::Value) -> bool {
        entry.get("type").and_then(serde_json::Value::as_str) == Some("agent")
    }
    fn keep(entries: &[serde_json::Value]) -> (Vec<serde_json::Value>, usize) {
        let kept: Vec<serde_json::Value> = entries
            .iter()
            .filter(|entry| !is_agent(entry))
            .cloned()
            .collect();
        let skipped = entries.len() - kept.len();
        (kept, skipped)
    }

    match value {
        serde_json::Value::Array(entries) => {
            let (kept, skipped) = keep(entries);
            (serde_json::Value::Array(kept), skipped)
        }
        serde_json::Value::Object(map) => {
            let Some(serde_json::Value::Array(entries)) = map.get("hooks") else {
                return (value.clone(), 0);
            };
            let (kept, skipped) = keep(entries);
            let mut filtered = map.clone();
            filtered.insert("hooks".into(), serde_json::Value::Array(kept));
            (serde_json::Value::Object(filtered), skipped)
        }
        _ => (value.clone(), 0),
    }
}

/// The result of `/macro`: either text to show, or commands to execute.
enum MacroCommandOutcome {
    Message(String),
    Run(Vec<String>),
}

/// One `notify` change, normalized to the reference's three event kinds.
#[derive(Debug, Clone)]
struct FileChangeBatch {
    /// Every path the underlying event named (a rename names two).
    paths: Vec<String>,
    /// `"change"`, `"add"`, or `"unlink"` — the `event` value a hook sees.
    kind: &'static str,
}

/// A file watcher whose path set can grow after it is constructed.
///
/// The settings files are supplied at construction. A `SessionStart` hook can
/// return `watchPaths`, and a `FileChanged` hook may carry a static `watch`
/// list; both are added with [`add_paths`](Self::add_paths) once known. Unlike
/// [`coderex_config::ConfigWatcher`], which publishes only path lists, this
/// watcher keeps the kind of each change so the hook input can name it.
#[cfg(feature = "hot-reload")]
struct HookFileWatcher {
    watcher: notify::RecommendedWatcher,
    /// Paths already handed to `notify`, so a repeated sync is a no-op.
    watched: std::collections::HashSet<std::path::PathBuf>,
}

#[cfg(feature = "hot-reload")]
impl HookFileWatcher {
    /// Watch `paths` and return the event receiver.
    ///
    /// A path that does not exist yet is skipped, matching the old settings
    /// watcher: `notify` cannot watch a file it cannot see. A later
    /// [`add_paths`](Self::add_paths) call retries it.
    fn new(
        paths: &[std::path::PathBuf],
    ) -> Result<(Self, mpsc::UnboundedReceiver<FileChangeBatch>), notify::Error> {
        use notify::EventKind;

        let (tx, rx) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            let Ok(event) = result else {
                return;
            };
            let kind = match event.kind {
                EventKind::Create(_) => "add",
                EventKind::Remove(_) => "unlink",
                EventKind::Modify(_) => "change",
                // Access events are reads, not changes, and the rest carry no
                // meaning the hook contract can express.
                _ => return,
            };
            if event.paths.is_empty() {
                return;
            }
            let paths = event
                .paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            let _ = tx.send(FileChangeBatch { paths, kind });
        })?;

        let mut watcher = Self {
            watcher,
            watched: std::collections::HashSet::new(),
        };
        watcher.add_paths(paths.iter().cloned());
        Ok((watcher, rx))
    }

    /// Start watching any path not already watched.
    ///
    /// Relative paths are refused here as well: the engine validates them at
    /// parse time, but a programmatic caller must not be able to slip one in.
    fn add_paths<I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = std::path::PathBuf>,
    {
        use notify::{RecursiveMode, Watcher};

        for path in paths {
            if !path.is_absolute() {
                tracing::warn!(path = %path.display(), "refusing to watch a relative path");
                continue;
            }
            if !self.watched.insert(path.clone()) {
                continue;
            }
            if !path.exists() {
                self.watched.remove(&path);
                continue;
            }
            if let Err(error) = self.watcher.watch(&path, RecursiveMode::NonRecursive) {
                self.watched.remove(&path);
                tracing::warn!(path = %path.display(), %error, "could not watch path");
            }
        }
    }
}

/// The settings files watched by default, when they exist.
#[cfg(feature = "hot-reload")]
fn default_settings_paths(cwd: &str) -> Vec<std::path::PathBuf> {
    [
        coderex_config::config_home().join("settings.json"),
        std::path::Path::new(cwd).join(".coderex/settings.json"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

/// Build the JSON input a `FileChanged` hook reads.
///
/// The reference shape is `{"file_path": "...", "event": "change|add|unlink"}`.
/// Each batch path gets its own event.
#[cfg(any(feature = "hot-reload", test))]
fn file_changed_input(path: &str, kind: &str) -> String {
    serde_json::json!({
        "file_path": path,
        "event": kind,
    })
    .to_string()
}

/// Fire `FileChanged` for every path in a batch through `hooks`.
///
/// The per-event input names one file and its kind; the `paths` context entry
/// keeps the whole batch so a hook can see a rename as one change rather than
/// two unrelated ones.
#[cfg(feature = "hot-reload")]
async fn fire_file_changed_batch(
    hooks: &dyn coderex_core::hook_executor::HookExecutor,
    session_id: &str,
    batch: &FileChangeBatch,
) {
    let paths = batch.paths.join("\n");
    for path in &batch.paths {
        let mut context = HashMap::new();
        context.insert("paths".to_string(), paths.clone());
        context.insert("session_id".to_string(), session_id.to_string());
        context.insert("source".to_string(), "file_watcher".to_string());
        let result = hooks
            .fire_hook(
                coderex_hooks::HookEvent::FileChanged.as_str(),
                &file_changed_input(path, batch.kind),
                &context,
            )
            .await;
        if !result.errors.is_empty() {
            tracing::warn!(errors = ?result.errors, "FileChanged hook errors");
        }
    }
}

/// Keeps a headless run's watcher and firing task alive, then joins both.
#[cfg(feature = "hot-reload")]
struct HeadlessHookWatcher {
    /// Dropping the watcher closes the channel, which ends the task.
    watcher: Option<HookFileWatcher>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "hot-reload")]
impl HeadlessHookWatcher {
    /// Stop watching and wait for the firing task to finish.
    async fn stop(mut self) {
        self.watcher.take();
        let _ = self.task.await;
    }
}

pub struct ReplLoop {
    /// Command registry.
    commands: CommandRegistry,
    /// Current model.
    model: ModelId,
    /// Telemetry handle when the session opted in.
    ///
    /// Used to record `tool_used`, `api_call` and `error` events; session
    /// start/end are owned by the orchestrator.
    telemetry: Option<coderex_telemetry::TelemetryHandle>,
    /// Agent definitions contributed by active plugins.
    plugin_agents: Vec<(String, serde_json::Value)>,
    /// Named workflow scripts contributed by plugins (shared with the tool).
    plugin_workflows: Option<Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>>,
    /// Plugin themes, as `(name, theme JSON)`.
    plugin_themes: Vec<(String, serde_json::Value)>,
    /// Plugin output styles, as `(name, style document)`.
    plugin_output_styles: Vec<(String, serde_json::Value)>,
    /// Plugin monitor declarations, started with the session.
    plugin_monitors: Option<serde_json::Value>,
    /// The running monitor supervisor, when any monitor was armed.
    monitor_supervisor: Option<crate::plugin_monitors::PluginMonitorSupervisor>,
    /// Plugin LSP server configurations, as `(name, config)`.
    plugin_lsp_servers: Vec<(String, serde_json::Value)>,
    /// Default the `LSP` tool applies when a call names no server.
    lsp_defaults: Arc<std::sync::RwLock<Option<PluginLspDefaults>>>,
    /// Plugin channels, types, binaries and experimental data, for `/plugin`.
    plugin_other_components: Vec<(String, Vec<String>)>,
    /// In-flight tool calls, keyed by call id, for duration/success events.
    tool_starts: std::collections::HashMap<String, (String, std::time::Instant)>,
    /// Model-written labels from `ToolUseSummary`, keyed by tool-call id.
    ///
    /// `ToolCallStart` consumes the entry so the activity row shows the
    /// summary the engine produced under `tool_use_summaries`; when the flag
    /// is off the map stays empty and the start event's own label is used.
    tool_use_summaries: std::collections::HashMap<String, String>,
    /// Session usage at the end of the previous turn, for per-turn deltas.
    last_usage: coderex_core::types::UsageSummary,
    /// Model to retry with once when the primary model is unavailable
    /// (`--fallback-model`).
    fallback_model: Option<ModelId>,
    /// Maximum output tokens per provider request.
    ///
    /// A named custom agent may set this; the CLI has no override flag, so an
    /// explicit programmatic value is the only thing that wins over the file.
    max_tokens: u32,
    /// True when `--model` was given. A named custom agent's `model` must not
    /// override an explicitly chosen session model.
    explicit_model_override: bool,
    /// Current working directory.
    cwd: String,
    /// Whether the loop should continue.
    running: bool,
    /// A setup failure that must stop the session before it runs.
    ///
    /// `--worktree` is the current producer: if the isolated checkout cannot
    /// be created, running the prompt in the original checkout is exactly the
    /// silent fallback the flag exists to prevent. Headless mode consults this
    /// instead of relying on `running`, which only the interactive loop reads.
    setup_error: Option<String>,
    /// Prompt history.
    history: Vec<String>,
    /// QueryEngine for processing user queries (14.1 wired).
    engine: Option<QueryEngine>,
    /// Event receiver for streaming output.
    event_rx: Option<mpsc::UnboundedReceiver<EngineEvent>>,
    /// Session-level effects from commands.
    session_effects: SessionEffects,
    /// API client for orchestrator side-queries (title, permission explain, etc.).
    api_client: Option<Box<dyn ApiClient>>,
    /// Orchestrator for routing AI calls through product-level entry points.
    orchestrator: Option<coderex_core::orchestrator::Orchestrator>,
    /// Active background tasks (observable + cancelable).
    background_tasks: Vec<BackgroundTask>,
    /// Sender for background task results back to the UI thread.
    background_results_tx: Option<mpsc::UnboundedSender<BackgroundTaskResult>>,
    /// Receiver for background task results.
    background_results_rx: Option<mpsc::UnboundedReceiver<BackgroundTaskResult>>,
    /// Session memory store for persisting session facts.
    session_memory_store: Option<Arc<SessionMemoryStore>>,
    /// Long-term memory store for extracted memories.
    long_term_store: Option<Arc<LongTermStore>>,
    /// Session ID for memory operations.
    session_id: Option<uuid::Uuid>,
    /// Auto-dream manager for background session consolidation.
    auto_dream: Option<Arc<AutoDream>>,
    /// Shutdown signal for the auto-dream watcher.
    dream_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Team memory sync service for cross-repo memory sharing.
    team_memory_service: Option<Arc<TeamMemorySyncService>>,
    /// Whether the memory tools may write. Cleared by
    /// `--no-session-persistence`, which arrives after the tools are
    /// registered, so the tools read it at call time.
    memory_writable: Arc<std::sync::atomic::AtomicBool>,
    /// The live file watcher behind `FileChanged`.
    ///
    /// The receiver alone is not enough: `notify` stops watching when its
    /// watcher is dropped, so the session has to keep the watcher alive for
    /// as long as it wants change events. Its path set grows after
    /// `SessionStart` reports `watchPaths`.
    #[cfg(feature = "hot-reload")]
    config_watcher: Option<HookFileWatcher>,
    /// Change batches for the TUI's `select!`.
    ///
    /// Drained by `run_tui`'s event loop, which fires `FileChanged` with the
    /// path and kind the watcher reported. A headless run builds its own
    /// watcher (see [`Self::spawn_headless_file_watcher`]) because it has no
    /// event loop to select on it.
    #[cfg(feature = "hot-reload")]
    config_changed_rx: Option<mpsc::UnboundedReceiver<FileChangeBatch>>,
    /// A lifecycle hook queued by a synchronous command path.
    ///
    /// `/init` runs synchronously inside the registry, while firing a hook is
    /// async. `sync_command_state` records a successful `/init` here and the
    /// async entry points (`execute_command`, and `run_tui` after
    /// `process_tui_input`) take and fire it exactly once.
    pending_setup: Option<String>,
    /// The session's file checkpoint store.
    ///
    /// Edit tools snapshot through it before they write; `/rewind` restores
    /// the files a truncated turn touched, not just the conversation.
    checkpoint_store: Option<Arc<coderex_core::checkpoints::CheckpointStore>>,
    /// Command history with persistence.
    command_history: CommandHistory,
    /// Macro registry for user-defined command sequences.
    macro_registry: MacroRegistry,
    /// Lifecycle hooks from settings, installed on the engine by
    /// [`apply_cli_config`](Self::apply_cli_config).
    hooks_settings: Option<serde_json::Value>,
    /// The same hooks split by source tier, when the settings service resolved
    /// them.
    ///
    /// [`hooks_settings`](Self::hooks_settings) is the merged view kept for
    /// compatibility. When this is present it is authoritative for
    /// installation: managed hooks are installed as
    /// [`HookTier::Managed`](coderex_hooks::HookTier::Managed) and the core
    /// document is installed as `Core`, so a managed hook neither loses its
    /// position nor runs a second time through the merged document.
    hook_layers: Option<coderex_config::settings::HookLayers>,
    /// How many hooks the installed executor is actually running.
    ///
    /// `/hooks` and `/status` used to read `CommandContext.active_hooks`,
    /// which nothing ever assigned, so they always reported zero.
    active_hooks: usize,
    /// Handle for delivering mod UI button activations to the mod worker.
    ///
    /// Kept after the executor is installed so the terminal can press a
    /// button without reaching into the engine.
    mod_ui_handle: Option<coderex_mods::ModUiHandle>,
    /// Where the line-delimited protocol goes, when `--output-format
    /// stream-json` was asked for.
    ///
    /// One writer, one consumer outside the process — not a bus. The writer is
    /// boxed rather than pinned to `Stdout` so a test can read back what a turn
    /// emitted.
    protocol_sink: Option<coderex_events::MessageSink<Box<dyn std::io::Write + Send + Sync>>>,
    /// Whether a headless turn negotiates tool approvals over stdin.
    ///
    /// Enabled only when the host can answer (see
    /// [`with_headless_permission_control`](Self::with_headless_permission_control)):
    /// a `can_use_tool` request goes out on the protocol sink and a
    /// `control_response` line is read from stdin. When this is off, an `Ask`
    /// keeps the pre-existing headless behaviour (the tool does not run).
    headless_permission_control: bool,
    /// Pending destructive command awaiting confirmation.
    pending_destructive_cmd: Option<(String, Vec<String>)>,
    /// Plugin manager for dynamic plugin commands and tools.
    plugin_manager: Option<Arc<coderex_plugins::plugins::PluginManager>>,
    /// Primary tool executor (stored so plugin fallback can be layered on top).
    _primary_tool_executor:
        Option<Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync>>,
    /// Dispatcher reference for setting per-turn state (e.g., elicitation channel).
    dispatcher: Option<Arc<RegistryToolDispatcher>>,
    /// Sub-agent lifecycle manager shared with the registered Agent tool.
    agent_manager: Option<Arc<tokio::sync::Mutex<AgentManager>>>,
    /// Parent conversation snapshot read by the Agent tool when it forks a
    /// sub-agent. Updated immediately before each turn so a spawn reuses the
    /// conversation as it was when the turn started.
    parent_context_snapshot: Arc<std::sync::Mutex<Option<ParentContextSnapshot>>>,
    /// Filtered broadcast subscription used to observe sub-agent events
    /// directly (as opposed to lifecycle events forwarded through the parent
    /// engine bus).
    agent_observable_rx: Option<AgentSubscription>,
    /// The live MCP servers, filled in by `with_mcp_manager`.
    ///
    /// A slot rather than a plain field because the sub-agent tool factory is
    /// built before the MCP servers are attached, and reads this when it later
    /// builds a child registry.
    mcp_manager: Arc<std::sync::RwLock<Option<Arc<coderex_mcp::manager::McpManager>>>>,
    /// Slash-command names already registered for discovered MCP prompts.
    ///
    /// The command registry is append-only, so a refresh records what it has
    /// already added instead of registering the same prompt twice. A prompt
    /// that disappears from a server keeps its command; invoking it fails with
    /// that server's error.
    mcp_prompt_commands: std::collections::HashSet<String>,
    /// Session handler for questions MCP servers ask, when one was installed.
    ///
    /// The manager already holds the inbound half; the TUI needs the same
    /// object to register its question channel, so the session keeps it too.
    mcp_elicitation: Option<Arc<SessionElicitationHandler>>,
    /// The server artifacts are published to, kept alive for the session.
    ///
    /// Dropping it would leave the model publishing pages nobody can open.
    artifact_server: Option<coderex_artifacts::ArtifactServer>,
    /// Where artifacts are stored, when it is not the default location.
    artifact_store_root: Option<std::path::PathBuf>,
    /// The turn currently running in a task, so a key handler can stop it.
    running_query: Option<coderex_core::engine::InterruptHandle>,
    /// Pending elicitation response sender — when set, user input is treated as an answer.
    pending_elicitation: Option<tokio::sync::oneshot::Sender<serde_json::Value>>,
    /// The multi-field form being filled, when the pending elicitation is one.
    ///
    /// Kept separately from `pending_elicitation` because the TUI edits it
    /// with navigation keys rather than reading one line of free text.
    pending_mcp_form: Option<crate::mcp_elicitation::McpFormState>,
    /// Confirmation bus for interactive permission requests.
    confirmation_bus: Option<ConfirmationBus>,
    /// Receiver for cron job fire events.
    cron_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>>,
    /// Teleport manager for session sharing and directory switching.
    teleport_manager: Option<coderex_bridge::teleport::TeleportManager>,
    /// Skip permission prompts (--dangerously-skip-permissions).
    skip_permissions: bool,
    /// Permission mode override (--permissions).
    permission_mode: Option<PermissionMode>,
    /// Disable session persistence (--no-session-persistence).
    no_persistence: bool,
    /// Disable slash commands (--disable-slash-commands).
    no_slash_commands: bool,
    /// JSON schema for structured output (--json-schema).
    json_schema: Option<String>,
    /// Maximum budget in USD for this session (--max-budget-usd).
    max_budget_usd: Option<f64>,
    /// Maximum number of turns (--max-turns).
    max_turns: Option<u32>,
    /// Replay user messages from previous session (--replay-user-messages).
    replay_user_messages: bool,
    /// Include partial messages in context (--include-partial-messages).
    include_partial_messages: bool,
    /// Beta features enabled (--betas).
    betas: Vec<String>,
    /// Feature flags resolved for this session: the environment plus
    /// `--betas` overrides.
    feature_flags: coderex_config::flags::FeatureFlags,
    /// Shared enable bit of the already-registered Workflow tool.
    ///
    /// The registry is built before `--betas` is applied and cannot be
    /// mutated afterwards, so the tool reads this bit on every call.
    workflow_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Capabilities derived from [`Self::feature_flags`].
    capabilities: coderex_config::flags::CapabilitySet,
    /// Settings-file context-window overrides, layered into the engine's
    /// resolver together with the environment, client data, and experiments.
    context_window_settings: coderex_core::types::ContextWindowOverrides,
    /// Pull-request context fetched for `--from-pr`.
    pr_context: Option<String>,
    /// Live IDE MCP connection for `--ide`, kept for the session.
    ide_manager: Option<Arc<coderex_mcp::manager::McpManager>>,
    /// Human-readable IDE connection status.
    ide_status: Option<String>,
    /// Chrome integration enabled (--chrome / --no-chrome).
    chrome_enabled: bool,
    /// Agent to use (--agent).
    agent: Option<String>,
    /// IDE mode (--ide).
    ide: Option<String>,
    /// Worktree mode (--worktree).
    worktree: bool,
    /// Fork from session ID (--fork-session).
    fork_session: Option<String>,
    /// Load context from PR URL (--from-pr).
    from_pr: Option<String>,
    /// Session name for transcript naming (--name).
    session_name: Option<String>,
    /// Additional directories to include in context (--add-dir).
    add_dir: Vec<String>,
    /// Input format for non-interactive mode (--input-format).
    input_format: String,
    /// Tmux integration (--tmux).
    tmux_enabled: bool,
    /// Debug file path (--debug-file).
    debug_file: Option<std::path::PathBuf>,
    /// When the current query started (for elapsed-time display).
    query_start_time: Option<std::time::Instant>,
    /// Whether Vim keybindings are enabled for interactive input.
    vim_mode_enabled: bool,
    /// Active TUI theme name.
    current_theme: String,
    /// `vim_mode` from settings, applied at startup.
    settings_vim_mode: Option<bool>,
    /// `statusline.theme` from settings, applied at startup.
    settings_statusline_theme: Option<String>,
    /// Session id to resume into the current interactive loop.
    resume_session: Option<String>,
}

// Manual impl: many fields (trait objects, channels, locks) are not `Debug`,
// so we print only the type name rather than deriving.
impl std::fmt::Debug for ReplLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplLoop").finish_non_exhaustive()
    }
}

/// Wrapper that implements `ToolExecutor` for `Arc<dyn ToolExecutor>`.
///
/// Needed because `Arc<dyn Trait>` does not automatically implement `Trait`.
#[derive(Clone)]
struct ArcToolExecutor(Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync>);

#[async_trait::async_trait]
impl coderex_core::tool_executor::ToolExecutor for ArcToolExecutor {
    fn list_tools(&self) -> Vec<coderex_core::tool_executor::ToolInfo> {
        self.0.list_tools()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> coderex_core::tool_executor::ToolDispatchOutcome {
        self.0.execute(tool_call_id, tool_name, input).await
    }
}

/// Tool executor that falls back to plugin tools when the primary reports "not found".
#[derive(Clone)]
struct PluginToolFallbackExecutor {
    primary: ArcToolExecutor,
    plugin: Arc<coderex_plugins::tool_executor::PluginToolExecutor>,
}

#[async_trait::async_trait]
impl coderex_core::tool_executor::ToolExecutor for PluginToolFallbackExecutor {
    fn list_tools(&self) -> Vec<coderex_core::tool_executor::ToolInfo> {
        self.primary.list_tools()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> coderex_core::tool_executor::ToolDispatchOutcome {
        let primary = self.primary.execute(tool_call_id, tool_name, input).await;

        if primary.result.is_error && primary_tool_was_missing(&primary.result.output, tool_name) {
            self.plugin.execute(tool_call_id, tool_name, input).await
        } else {
            primary
        }
    }
}

fn primary_tool_was_missing(output: &str, tool_name: &str) -> bool {
    output == format!("tool '{tool_name}' not found in registry")
        || output.starts_with(&format!(
            "tool '{tool_name}' is not registered in this engine;"
        ))
}

/// Install the engine behaviours gated by a session's feature flags.
///
/// `coordinator_mode` installs a started [`CoordinatorState`] so the engine's
/// existing per-turn phase advance runs; `history_snip` makes compaction prefer
/// the snip strategy as soon as it is warranted; `streaming_tool_execution`
/// and `tool_use_summaries` reach the engine's own setters; and
/// `team_collaboration` installs a [`TeamContext`] built from the stored team
/// memory entries. All are read from the final flag set, so `--betas`
/// participates. Shared with the `serve` engine factory so both production
/// engine-construction paths honour the flags.
///
/// [`TeamContext`]: coderex_core::TeamContext
///
/// [`CoordinatorState`]: coderex_core::coordinator::CoordinatorState
pub(crate) fn apply_engine_flag_behaviours(
    engine: &mut QueryEngine,
    flags: &coderex_config::flags::FeatureFlags,
    memory_base: Option<&std::path::Path>,
) {
    if flags.coordinator_mode {
        let mut coordinator = coderex_core::coordinator::CoordinatorState::new();
        coordinator.start();
        engine.set_coordinator(coordinator);
    }
    engine.set_history_snip(flags.history_snip);
    // Both are engine behaviours with setters: the streaming loop consults
    // the first, and the tool path consults the second.
    engine.set_streaming_tool_execution(flags.streaming_tool_execution);
    engine.set_tool_use_summaries(flags.tool_use_summaries);

    if flags.team_collaboration {
        // The team id names the collaboration; shared notes come from the
        // same stored team-memory entries `/team-memory` reads. An empty
        // context is still installed — the engine injects nothing from it,
        // and a later write is visible without rebuilding the engine.
        let team_id = std::env::var("CODEREX_TEAM_ID").unwrap_or_else(|_| "local".to_string());
        let mut context = coderex_core::TeamContext::new(team_id);
        if let Some(memory_base) = memory_base {
            let service =
                TeamMemorySyncService::new("default".to_string(), memory_base.join("team"));
            match service.list() {
                Ok(entries) => {
                    for entry in entries {
                        context.add_note(entry.topic, entry.content, "team-memory".to_string());
                    }
                }
                Err(error) => {
                    // Collaboration is additive: an unreadable store leaves
                    // the context empty rather than failing startup.
                    tracing::warn!(%error, "could not load team memory for team context");
                }
            }
        }
        engine.set_team_context(std::sync::Arc::new(std::sync::Mutex::new(context)));
    }
}

impl ReplLoop {
    /// Create a new REPL loop without a QueryEngine.
    /// Use `with_engine` to wire up the QueryEngine for full conversation support.
    #[must_use]
    pub fn new(cwd: String, model: ModelId) -> Self {
        let mut commands = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut commands);
        let mut this = Self {
            commands,
            model,
            fallback_model: None,
            telemetry: None,
            plugin_agents: Vec::new(),
            plugin_workflows: None,
            plugin_themes: Vec::new(),
            plugin_output_styles: Vec::new(),
            plugin_monitors: None,
            monitor_supervisor: None,
            plugin_lsp_servers: Vec::new(),
            lsp_defaults: Arc::new(std::sync::RwLock::new(None)),
            plugin_other_components: Vec::new(),
            tool_starts: std::collections::HashMap::new(),
            tool_use_summaries: std::collections::HashMap::new(),
            last_usage: coderex_core::types::UsageSummary::default(),
            max_tokens: QueryEngine::DEFAULT_MAX_TOKENS,
            explicit_model_override: false,
            cwd,
            running: true,
            setup_error: None,
            history: Vec::new(),
            engine: None,
            event_rx: None,
            session_effects: SessionEffects::default(),
            api_client: None,
            orchestrator: None,
            background_tasks: Vec::new(),
            background_results_tx: None,
            background_results_rx: None,
            session_memory_store: None,
            long_term_store: None,
            session_id: None,
            auto_dream: None,
            dream_shutdown_tx: None,
            team_memory_service: None,
            memory_writable: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            workflow_enabled: Arc::new(std::sync::atomic::AtomicBool::new(
                coderex_config::flags::FeatureFlags::from_env().workflow_scripts,
            )),
            command_history: CommandHistory::new(1000),
            macro_registry: MacroRegistry::new(),
            hooks_settings: None,
            hook_layers: None,
            active_hooks: 0,
            mod_ui_handle: None,
            checkpoint_store: None,
            protocol_sink: None,
            headless_permission_control: false,
            pending_destructive_cmd: None,
            plugin_manager: None,
            _primary_tool_executor: None,
            dispatcher: None,
            agent_manager: None,
            parent_context_snapshot: Arc::new(std::sync::Mutex::new(None)),
            agent_observable_rx: None,
            mcp_manager: Arc::new(std::sync::RwLock::new(None)),
            mcp_prompt_commands: std::collections::HashSet::new(),
            mcp_elicitation: None,
            artifact_server: None,
            artifact_store_root: None,
            running_query: None,
            pending_elicitation: None,
            pending_mcp_form: None,
            confirmation_bus: None,
            cron_rx: None,
            teleport_manager: Some(coderex_bridge::teleport::TeleportManager::new()),
            #[cfg(feature = "hot-reload")]
            config_watcher: None,
            #[cfg(feature = "hot-reload")]
            config_changed_rx: None,
            pending_setup: None,
            skip_permissions: false,
            permission_mode: None,
            no_persistence: false,
            no_slash_commands: false,
            json_schema: None,
            max_budget_usd: None,
            max_turns: None,
            replay_user_messages: false,
            include_partial_messages: false,
            betas: Vec::new(),
            feature_flags: coderex_config::flags::FeatureFlags::from_env(),
            capabilities: coderex_config::flags::FeatureFlags::from_env().capabilities(),
            context_window_settings: coderex_core::types::ContextWindowOverrides::default(),
            pr_context: None,
            ide_manager: None,
            ide_status: None,
            chrome_enabled: true,
            agent: None,
            ide: None,
            worktree: false,
            fork_session: None,
            from_pr: None,
            session_name: None,
            add_dir: Vec::new(),
            input_format: "text".into(),
            tmux_enabled: false,
            debug_file: None,
            query_start_time: None,
            vim_mode_enabled: false,
            current_theme: "dark".into(),
            settings_vim_mode: None,
            settings_statusline_theme: None,
            resume_session: None,
        };
        this.init_command_persistence();
        this
    }

    /// Supply settings-file context-window overrides for this session.
    pub fn with_context_window_settings(
        &mut self,
        overrides: coderex_core::types::ContextWindowOverrides,
    ) {
        self.context_window_settings = overrides;
    }

    /// Create a new REPL loop wired to a QueryEngine (14.1).
    ///
    /// # Arguments
    /// * `api_client` — API client implementation for LLM requests.
    /// * `cwd` — Current working directory.
    /// * `model` — Model to use.
    #[must_use]
    pub fn with_engine(api_client: Box<dyn ApiClient>, cwd: String, model: ModelId) -> Self {
        let mut commands = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut commands);

        let session = SessionState::new(std::path::PathBuf::from(&cwd));
        let session_id = session.session_id;
        let checker = PermissionChecker::new(PermissionMode::default());
        let mut permission_engine = PermissionEngine::new(checker);
        let confirmation_bus = ConfirmationBus::new(10);
        let perm_path = std::path::PathBuf::from(&cwd)
            .join(".coderex")
            .join("permissions.json");
        permission_engine.set_persist_path(perm_path);
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EngineEvent>();
        permission_engine.add_hook(Arc::new(ExplainPermissionHook::new_with_event_tx(
            api_client.clone_box(),
            event_tx.clone(),
        )));
        permission_engine.add_hook(Arc::new(AutoClassifierHook::new(api_client.clone_box())));
        // A background run is one nobody is watching, so the tools that would
        // need watching are refused there. The hook existed and was tested;
        // nothing registered it, so it never ran.
        permission_engine.add_hook(Arc::new(BackgroundAgentDenyHook));

        let memory_base = std::path::PathBuf::from(&cwd)
            .join(".coderex")
            .join("memory");
        let session_memory_store = Arc::new(SessionMemoryStore::new(memory_base.join("session")));
        let long_term_store = Arc::new(LongTermStore::new(memory_base.join("long_term")));

        let auto_dream = Arc::new(AutoDream::new(memory_base.clone(), DreamConfig::default()));
        let team_memory_service = Arc::new(TeamMemorySyncService::new(
            "default".to_string(),
            memory_base.join("team"),
        ));
        // The model-facing memory family. A write to a shared store is decided
        // by the model rather than assumed safe, and the service refuses such a
        // write outright when no classifier is installed.
        //
        // `org` is part of the advertised store set, so it has to be wired, not
        // merely named. The explicit environment override wins; otherwise the
        // org id from the orchestrator's remote-settings cache is used. When
        // neither exists the org store stays unconfigured and the tools report
        // that honestly.
        let org_id = configured_org_id(std::env::var("CODEREX_ORG_ID").ok()).or_else(cached_org_id);
        if org_id.is_none() {
            tracing::debug!("no organization id configured; org memory is not configured");
        }
        let org_memory_service = org_memory_service(&memory_base, org_id);
        let mut memory_service = coderex_memory::service::MemoryService::new(
            Arc::clone(&long_term_store),
            Arc::clone(&team_memory_service),
        )
        .with_classifier(Arc::new(crate::memory_tools::ModelMemoryClassifier::new(
            api_client.clone_box(),
        )));
        if let Some(org) = org_memory_service {
            memory_service = memory_service.with_org(org);
        }
        let memory_service = Arc::new(memory_service);
        let memory_writable = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let engine = QueryEngine::new(
            api_client.clone_box(),
            model.clone(),
            session,
            permission_engine,
            event_tx.clone(),
        );

        let mut engine = engine;
        // Persistent session memory that survives compaction.
        engine.set_session_memory(Arc::new(
            coderex_memory::session_memory_backend::StoreSessionMemoryBackend::new(Arc::clone(
                &session_memory_store,
            )),
        ));

        let mut agent_manager = AgentManager::standalone();
        agent_manager.set_api_client(api_client.clone_box());
        agent_manager.set_model(model.clone());
        // Teammate events are a callback surface on the manager; forward them
        // into the session's existing engine event channel so the UI renders
        // them and headless mode logs them, rather than leaving the callback
        // without a production subscriber.
        let teammate_event_tx = event_tx.clone();
        agent_manager.set_event_callback(move |event| {
            let (agent_id, lifecycle) = teammate_event_to_lifecycle(&event);
            tracing::info!(agent_id = %agent_id, event = ?event, "teammate event");
            let _ = teammate_event_tx.send(EngineEvent::AgentEvent {
                agent_id,
                event: lifecycle,
            });
        });
        agent_manager.set_event_sender(event_tx);
        agent_manager.set_permission_checker(engine.permission_checker());
        let mcp_slot: Arc<std::sync::RwLock<Option<Arc<coderex_mcp::manager::McpManager>>>> =
            Arc::new(std::sync::RwLock::new(None));
        let mcp_for_subagents = Arc::clone(&mcp_slot);
        // Team and MCP-resource state is session-wide; the worktree state is
        // per checkout, so the sub-agent factory builds its own from the
        // child's cwd.
        let runtime_handles = RuntimeToolHandles::new(&cwd, &mcp_slot);
        let subagent_team = Arc::clone(&runtime_handles.team);
        let subagent_mcp = Arc::clone(&runtime_handles.mcp);
        let subagent_mcp_prompts = Arc::clone(&runtime_handles.mcp_prompts);
        // Background tools run inside a sub-agent's turn too, and a job record
        // written there must still name the session that owns it.
        let subagent_session_id = session_id.to_string();
        agent_manager.set_tool_executor_factory(move |cwd, sandbox| {
            let mut registry = ToolRegistry::new();
            register_builtin_tools(&mut registry);
            if let Some(mcp) = mcp_for_subagents.read().unwrap().as_ref() {
                mcp.register_tools_into(&registry);
            }
            RuntimeToolHandles {
                team: Arc::clone(&subagent_team),
                worktree: Arc::new(WorktreeToolState::new(&cwd)),
                mcp: Arc::clone(&subagent_mcp),
                mcp_prompts: Arc::clone(&subagent_mcp_prompts),
            }
            .install(&mut registry);
            let executor = Arc::new(ToolsExecutor::new(ExecutionConfig::default()));
            let mut dispatcher = RegistryToolDispatcher::new(
                Arc::new(registry),
                executor,
                ExecutionContext::Normal,
                cwd,
            );
            dispatcher.set_sandbox_override(sandbox);
            dispatcher.set_rate_limiter(Arc::new(RateLimiter::new()));
            dispatcher.set_chrome_client(ChromeClient::new());
            dispatcher.set_session_id(subagent_session_id.clone());
            // The child registry intentionally has no nested AgentManager. A
            // synchronous parent holds its manager while waiting, so nesting
            // through that same manager could deadlock; the built-in Agent
            // tool instead reports that nested delegation is unavailable.
            Box::new(dispatcher)
        });
        // The agent tool cannot reach the engine's live session, so it reads
        // a snapshot the REPL refreshes before each turn.
        let parent_context_snapshot =
            Arc::new(std::sync::Mutex::new(None::<ParentContextSnapshot>));
        let snapshot_for_agent_tool = Arc::clone(&parent_context_snapshot);
        // Production subscriber for the manager's streaming broadcast. The
        // parent engine bus carries lifecycle transitions; this receiver
        // carries the child's raw events for headless logging and future
        // frontends.
        let agent_observable_rx = agent_manager.subscribe_all();
        let agent_manager = Arc::new(tokio::sync::Mutex::new(agent_manager));

        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        // Replace the six handle-driven baseline tools with implementations
        // that own the concrete managers. The originals would find no handle
        // in `ToolContext.extra` and fail every call.
        runtime_handles.install(&mut registry);
        registry.register(Arc::new(
            AgentTool::new()
                .with_manager(Arc::clone(&agent_manager))
                .with_session_id(session_id.to_string())
                .with_cwd(cwd.clone())
                .with_parent_context_provider(move || {
                    snapshot_for_agent_tool.lock().unwrap().clone()
                }),
        ));
        // Workflow scripts spawn agents, so they are opt-in: the flag is what
        // turns the tool from "not enabled" into callable. `--betas` is
        // applied after this registry exists, so the wrapper shares the bit
        // with `apply_cli_config` rather than freezing it here.
        let lsp_defaults: Arc<std::sync::RwLock<Option<PluginLspDefaults>>> =
            Arc::new(std::sync::RwLock::new(None));
        let workflows_enabled = coderex_config::flags::FeatureFlags::from_env().workflow_scripts;
        let workflow_tool = BetaGatedWorkflowTool::new(
            coderex_workflow::WorkflowTool::new(Arc::new(WorkflowAgentRunner {
                manager: Arc::clone(&agent_manager),
                session_id: session_id.to_string(),
                cwd: cwd.clone(),
            }))
            .with_root(coderex_workflow::default_root())
            .with_project_dir(std::path::Path::new(&cwd).join(".coderex/workflows")),
        );
        workflow_tool.set_enabled(workflows_enabled);
        let workflow_enabled = workflow_tool.enabled_flag();
        // Plugin workflows are registered after this point (plugins load in a
        // later startup phase), so the session keeps the shared map.
        let workflow_scripts = workflow_tool.named_script_map();
        registry.register(Arc::new(workflow_tool));
        // The LSP default comes from plugin `lspServers`, which load later, so
        // the wrapper reads a shared handle per call.
        if let Some(original) = registry.get_tool_arc("LSP") {
            registry.register(Arc::new(LspWithPluginDefaults {
                original,
                defaults: Arc::clone(&lsp_defaults),
            }));
        }
        for tool in crate::memory_tools::memory_tools(Arc::clone(&memory_service), &memory_writable)
        {
            registry.register(tool);
        }
        // A tool's declared exemptions are a property of the *call*, not the
        // tool — the Artifact family refuses a whole-tool allow rule for a
        // publish and honours it for a read — so the engine asks the registry
        // per call rather than being handed a table up front.
        let registry = Arc::new(registry);
        coderex_tools::install_rule_exemptions(engine.permission_engine_mut(), &registry);
        let inner_executor = Arc::new(ToolsExecutor::new(ExecutionConfig::default()));
        let mut dispatcher = RegistryToolDispatcher::new(
            registry,
            inner_executor,
            ExecutionContext::Normal,
            std::path::PathBuf::from(&cwd),
        );
        // Whether this session turned feature flags on decides whether a
        // gated tool is offered at all — the tool listing the model sees is
        // filtered by it.
        dispatcher.set_feature_flags(coderex_config::flags::FeatureFlags::from_env());

        // Shared task store with persistence
        let config_home = coderex_config::config_home();
        let task_store = Arc::new(TaskStore::new().with_storage(config_home.join("tasks.jsonl")));
        let _ = task_store.load();
        dispatcher.set_task_store(Arc::clone(&task_store));

        // Shared cron scheduler with persistence. Durable definitions are
        // mirrored into this session's job records, so the next session can
        // adopt the ones that are genuinely resumable.
        let cron_scheduler = Arc::new(
            CronScheduler::new()
                .with_storage(config_home.join("scheduled_tasks.json"))
                .with_job_records(std::path::PathBuf::from(&cwd), session_id.to_string()),
        );
        let _ = cron_scheduler.load();
        dispatcher.set_cron_scheduler(Arc::clone(&cron_scheduler));

        // Shared rate limiter for web tools
        let rate_limiter = Arc::new(RateLimiter::new());
        dispatcher.set_rate_limiter(Arc::clone(&rate_limiter));

        // Chrome DevTools client for browser automation tools
        dispatcher.set_chrome_client(ChromeClient::new());

        // Tools that start durable background work attribute its record to
        // this session, so a later session can tell it apart from its own.
        dispatcher.set_session_id(session_id.to_string());

        // The file checkpoint store lets `/rewind` undo the files a turn
        // edited, not only the conversation history.
        let checkpoint_store: Option<Arc<coderex_core::checkpoints::CheckpointStore>> =
            coderex_core::checkpoints::CheckpointStore::new(&config_home.join("file-history"))
                .ok()
                .map(Arc::new);
        if let Some(store) = &checkpoint_store {
            dispatcher.set_checkpoint_store(Arc::clone(store));
        }

        let dispatcher = Arc::new(dispatcher);
        let primary_executor: Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync> =
            dispatcher.clone();
        let arc_exec = ArcToolExecutor(Arc::clone(&primary_executor));
        engine.set_tool_executor(Box::new(arc_exec));
        engine.recover_orphaned_permissions();

        // Start cron background runner
        let cron_rx = cron_scheduler.spawn_runner();

        // Create event session and start its timeout checker

        let orchestrator = coderex_core::orchestrator::Orchestrator::new(api_client.clone_box());
        let (bg_tx, bg_rx) = mpsc::unbounded_channel::<BackgroundTaskResult>();

        #[cfg(feature = "hot-reload")]
        let (config_watcher, config_changed_rx) = Self::init_config_watcher(&cwd);

        let mut this = Self {
            commands,
            model,
            fallback_model: None,
            telemetry: None,
            plugin_agents: Vec::new(),
            plugin_workflows: Some(workflow_scripts),
            plugin_themes: Vec::new(),
            plugin_output_styles: Vec::new(),
            plugin_monitors: None,
            monitor_supervisor: None,
            plugin_lsp_servers: Vec::new(),
            lsp_defaults,
            plugin_other_components: Vec::new(),
            tool_starts: std::collections::HashMap::new(),
            tool_use_summaries: std::collections::HashMap::new(),
            last_usage: coderex_core::types::UsageSummary::default(),
            max_tokens: QueryEngine::DEFAULT_MAX_TOKENS,
            explicit_model_override: false,
            cwd,
            running: true,
            setup_error: None,
            history: Vec::new(),
            engine: Some(engine),
            event_rx: Some(event_rx),
            session_effects: SessionEffects::default(),
            api_client: Some(api_client),
            orchestrator: Some(orchestrator),
            background_tasks: Vec::new(),
            background_results_tx: Some(bg_tx),
            background_results_rx: Some(bg_rx),
            session_memory_store: Some(session_memory_store),
            long_term_store: Some(long_term_store),
            session_id: Some(session_id),
            auto_dream: Some(auto_dream),
            dream_shutdown_tx: None,
            team_memory_service: Some(team_memory_service),
            memory_writable,
            workflow_enabled,
            command_history: CommandHistory::new(1000),
            macro_registry: MacroRegistry::new(),
            hooks_settings: None,
            hook_layers: None,
            active_hooks: 0,
            mod_ui_handle: None,
            checkpoint_store,
            protocol_sink: None,
            headless_permission_control: false,
            pending_destructive_cmd: None,
            plugin_manager: None,
            _primary_tool_executor: Some(primary_executor),
            dispatcher: Some(dispatcher),
            agent_manager: Some(agent_manager),
            parent_context_snapshot: Arc::clone(&parent_context_snapshot),
            agent_observable_rx,
            mcp_manager: mcp_slot,
            mcp_prompt_commands: std::collections::HashSet::new(),
            mcp_elicitation: None,
            artifact_server: None,
            artifact_store_root: None,
            running_query: None,
            pending_elicitation: None,
            pending_mcp_form: None,
            confirmation_bus: Some(confirmation_bus),
            cron_rx: Some(cron_rx),
            teleport_manager: Some(coderex_bridge::teleport::TeleportManager::new()),
            #[cfg(feature = "hot-reload")]
            config_watcher,
            #[cfg(feature = "hot-reload")]
            config_changed_rx,
            pending_setup: None,
            skip_permissions: false,
            permission_mode: None,
            no_persistence: false,
            no_slash_commands: false,
            json_schema: None,
            max_budget_usd: None,
            max_turns: None,
            replay_user_messages: false,
            include_partial_messages: false,
            betas: Vec::new(),
            feature_flags: coderex_config::flags::FeatureFlags::from_env(),
            capabilities: coderex_config::flags::FeatureFlags::from_env().capabilities(),
            context_window_settings: coderex_core::types::ContextWindowOverrides::default(),
            pr_context: None,
            ide_manager: None,
            ide_status: None,
            chrome_enabled: true,
            agent: None,
            ide: None,
            worktree: false,
            fork_session: None,
            from_pr: None,
            session_name: None,
            add_dir: Vec::new(),
            input_format: "text".into(),
            tmux_enabled: false,
            debug_file: None,
            query_start_time: None,
            vim_mode_enabled: false,
            current_theme: "dark".into(),
            settings_vim_mode: None,
            settings_statusline_theme: None,
            resume_session: None,
        };
        this.init_command_persistence();
        this
    }

    /// Start serving artifacts and register the tool family.
    ///
    /// Failure is not fatal: a session that cannot publish still has every
    /// other tool, so the reason is logged and the family is left out.
    ///
    /// The tools land in the registry's dynamic tier — the same tier MCP tools
    /// use — because they arrive after the registry was shared. That keeps
    /// them out of plan mode, which is what we want: publishing is not a
    /// read-only planning action.
    pub async fn start_artifact_serving(&mut self) {
        // Starting twice would leak the first listener and register the tool
        // family twice; callers can ask unconditionally.
        if self.artifact_server.is_some() {
            return;
        }
        let Some(dispatcher) = self.dispatcher.as_ref() else {
            return;
        };

        let store = coderex_artifacts::ArtifactStore::open(
            self.artifact_store_root
                .clone()
                .unwrap_or_else(coderex_artifacts::store::default_root),
        );

        // Prefer the well-known port so a URL stays valid across sessions; a
        // free one is better than no server when it is taken.
        let server = match coderex_artifacts::ArtifactServer::start(store.clone(), 8722).await {
            Ok(server) => server,
            Err(error) => {
                tracing::debug!("artifact port 8722 unavailable ({error}); taking a free one");
                match coderex_artifacts::ArtifactServer::start(store.clone(), 0).await {
                    Ok(server) => server,
                    Err(error) => {
                        tracing::warn!("artifact serving unavailable: {error}");
                        return;
                    }
                }
            }
        };

        let service = Arc::new(coderex_artifacts::ArtifactService::new(
            store,
            server.origin(),
            self.session_id
                .map_or_else(|| "local".to_string(), |id| id.to_string()),
        ));

        for tool in coderex_artifacts::ArtifactTools::new(service).all() {
            dispatcher.registry().register_dynamic(tool);
        }
        tracing::info!(origin = server.origin(), "artifacts are served locally");

        self.artifact_server = Some(server);
    }

    /// Stop the local artifact server, if one is running.
    ///
    /// The server owns a background task, so it is shut down explicitly rather
    /// than left to whether the caller still holds the [`ReplLoop`].
    pub async fn stop_artifact_serving(&mut self) {
        if let Some(server) = self.artifact_server.take() {
            server.shutdown().await;
        }
    }

    /// Publish artifacts somewhere other than the default location.
    pub fn with_artifact_store_root(&mut self, root: std::path::PathBuf) {
        self.artifact_store_root = Some(root);
    }

    /// Wire the MCP manager into this REPL loop.
    ///
    /// Keeps the live server connections for the session and registers the
    /// tools they expose into the same registry as the built-in tools, so
    /// there is one execution path rather than a separate MCP one.
    ///
    /// # Panics
    ///
    /// Panics if the MCP slot's lock is poisoned.
    pub fn with_mcp_manager(&mut self, mcp_manager: Arc<coderex_mcp::manager::McpManager>) {
        if let Some(ref dispatcher) = self.dispatcher {
            mcp_manager.register_tools_into(dispatcher.registry());
        }
        *self.mcp_manager.write().expect("MCP slot poisoned") = Some(mcp_manager);
        // The manager has already run `start_all` (and therefore discovery) by
        // the time the session attaches it, so the first registration is free.
        self.refresh_mcp_prompt_commands();
    }

    /// Register a slash command for every prompt the manager has discovered.
    ///
    /// Commands are named `mcp__<server>__<prompt>`, with any character that
    /// is not an ASCII identifier character replaced by `_`. The raw server
    /// and prompt names are captured for `prompts/get`; only the command name
    /// is sanitised. Registration is idempotent.
    ///
    /// Called when the manager is attached and again, lazily, before an
    /// `/mcp` or `mcp__…` command runs. A `prompts/list_changed` notification
    /// updates the manager's registry from the stream task; the next such
    /// command therefore picks up prompts added after connection. A prompt a
    /// server stops offering keeps its command and reports the server's error
    /// when invoked, because the command registry has no removal operation.
    fn refresh_mcp_prompt_commands(&mut self) {
        // `--disable-slash-commands` clears the registry; registering prompt
        // commands here anyway would resurrect them behind the flag's back.
        if self.no_slash_commands {
            return;
        }
        let Some(manager) = self
            .mcp_manager
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return;
        };

        for entry in manager.list_prompts() {
            let command_name = prompt_command_name(&entry.server, &entry.prompt.name);
            if self.mcp_prompt_commands.contains(&command_name) {
                continue;
            }
            if self.commands.find(&command_name).is_some() {
                // Somebody else already owns this name; leave it alone but do
                // not try to register it on every refresh.
                self.mcp_prompt_commands.insert(command_name);
                continue;
            }

            let description = entry.prompt.description.clone().unwrap_or_else(|| {
                format!("MCP prompt '{}' from '{}'", entry.prompt.name, entry.server)
            });
            let name_static: &'static str = Box::leak(command_name.clone().into_boxed_str());
            let description_static: &'static str = Box::leak(description.into_boxed_str());
            let slot = Arc::clone(&self.mcp_manager);
            let server = entry.server.clone();
            let prompt_name = entry.prompt.name.clone();
            let declared = entry.prompt.arguments.clone();
            let handler = Arc::new(move |_ctx: &CommandContext, args: &[String]| {
                execute_mcp_prompt_command(&slot, &server, &prompt_name, &declared, args)
            });

            self.commands.register(SlashCommand::local(
                name_static,
                description_static,
                true,
                true,
                true,
                handler,
            ));
            self.mcp_prompt_commands.insert(command_name);
        }
    }

    /// The `/mcp` registry output with the discovered prompts appended under
    /// the server that offers them.
    fn run_mcp_command(&self, args: &[String]) -> CommandResult {
        let ctx = self.build_context();
        let mut result = self.commands.execute("mcp", &ctx, args);
        let listing = self.mcp_prompt_listing();
        if !listing.is_empty() {
            if !result.output.is_empty() {
                result.output.push('\n');
            }
            result.output.push_str(&listing);
        }
        result
    }

    /// Render discovered prompts grouped by server for `/mcp`.
    fn mcp_prompt_listing(&self) -> String {
        let Some(manager) = self
            .mcp_manager
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
        else {
            return String::new();
        };

        let mut by_server: std::collections::BTreeMap<String, Vec<_>> =
            std::collections::BTreeMap::new();
        for entry in manager.list_prompts() {
            by_server
                .entry(entry.server.clone())
                .or_default()
                .push(entry);
        }
        if by_server.is_empty() {
            return String::new();
        }

        let mut output = String::from("Prompts:");
        for (server, entries) in by_server {
            output.push_str("\n  ");
            output.push_str(&server);
            output.push(':');
            for entry in entries {
                output.push_str("\n    /");
                output.push_str(&prompt_command_name(&server, &entry.prompt.name));
                if let Some(ref description) = entry.prompt.description {
                    output.push_str(" - ");
                    output.push_str(description);
                }
                if !entry.prompt.arguments.is_empty() {
                    let names: Vec<String> = entry
                        .prompt
                        .arguments
                        .iter()
                        .map(coderex_mcp::prompts::McpPromptArgument::display)
                        .collect();
                    output.push_str(" (args: ");
                    output.push_str(&names.join(", "));
                    output.push(')');
                }
            }
        }
        output
    }

    /// Give the TUI a way to answer questions from MCP servers.
    ///
    /// The manager was handed the inbound translation layer during startup;
    /// this is the other end, which `run_tui` attaches to the event loop.
    pub fn with_mcp_elicitation_handler(&mut self, handler: Arc<SessionElicitationHandler>) {
        self.mcp_elicitation = Some(handler);
    }

    /// Wire a plugin manager into this REPL loop.
    ///
    /// Registers plugin-discovered slash commands and layers the plugin
    /// tool executor as a fallback behind the primary tool registry.
    pub fn with_plugin_manager(
        &mut self,
        plugin_manager: Arc<coderex_plugins::plugins::PluginManager>,
    ) {
        // Register plugin commands as /{plugin}/{command} slash commands
        coderex_plugins::command_bridge::PluginCommandBridge::register_commands(
            &plugin_manager,
            &mut self.commands,
        );

        // If we have a primary tool executor and an engine, layer plugin fallback
        if let Some(ref dispatcher) = self.dispatcher {
            if let Some(ref mut engine) = self.engine {
                let primary: Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync> =
                    dispatcher.clone();
                let plugin_exec =
                    Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
                        Arc::clone(&plugin_manager),
                    ));
                let fallback = PluginToolFallbackExecutor {
                    primary: ArcToolExecutor(primary),
                    plugin: plugin_exec,
                };
                engine.set_tool_executor(Box::new(fallback));
            }
        }

        self.plugin_manager = Some(plugin_manager);
    }

    // ─── CLI Configuration Builder Methods ──────────────────────────────────

    pub const fn with_skip_permissions(&mut self, skip: bool) {
        self.skip_permissions = skip;
    }

    pub const fn with_permission_mode(&mut self, mode: Option<PermissionMode>) {
        self.permission_mode = mode;
    }

    /// Lifecycle hooks from settings, installed by `apply_cli_config`.
    ///
    /// The `hooks` block was parsed into settings and then dropped: the engine
    /// kept its no-op executor, so a configured hook never ran. This is the
    /// value that gets loaded onto the engine.
    pub fn with_hooks(&mut self, hooks: Option<serde_json::Value>) {
        self.hooks_settings = hooks;
    }

    /// Attach the hook documents split by source tier (see
    /// [`ConfigService::resolve`](coderex_config::settings::ConfigService::resolve)).
    ///
    /// When set, this supersedes [`with_hooks`](Self::with_hooks) at
    /// installation time.
    pub fn with_hook_layers(&mut self, layers: Option<coderex_config::settings::HookLayers>) {
        self.hook_layers = layers;
    }

    /// Emit the line-delimited protocol on stdout as the turn runs.
    ///
    /// This is the `stream-json` output format: one JSON object per line, for a
    /// host outside the process to read. There is no in-process subscriber to
    /// hand it to, and there is not meant to be one.
    pub fn with_protocol_sink(
        &mut self,
        sink: Option<coderex_events::MessageSink<Box<dyn std::io::Write + Send + Sync>>>,
    ) {
        self.protocol_sink = sink;
    }

    /// Answer tool approvals over the stream-json control channel.
    ///
    /// The caller must only enable this when stdin is free for the host's
    /// `control_response` lines: in practice, `--print --output-format
    /// stream-json` with the prompt given positionally. Interactive sessions
    /// and a prompt read from stdin keep it off.
    pub const fn with_headless_permission_control(&mut self, enabled: bool) {
        self.headless_permission_control = enabled;
    }

    /// Test accessor for the gate that
    /// [`with_headless_permission_control`](Self::with_headless_permission_control)
    /// sets.
    #[cfg(test)]
    pub(crate) const fn headless_permission_control(&self) -> bool {
        self.headless_permission_control
    }

    pub const fn with_no_persistence(&mut self, no_persist: bool) {
        self.no_persistence = no_persist;
    }

    pub const fn with_no_slash_commands(&mut self, no_slash: bool) {
        self.no_slash_commands = no_slash;
    }

    pub fn with_json_schema(&mut self, schema: Option<String>) {
        self.json_schema = schema;
    }

    pub const fn with_max_budget_usd(&mut self, budget: Option<f64>) {
        self.max_budget_usd = budget;
    }

    pub const fn with_max_turns(&mut self, turns: Option<u32>) {
        self.max_turns = turns;
    }

    /// Configure the one-retry fallback model (`--fallback-model`).
    ///
    /// The engine retries the same request once when the primary model is
    /// unavailable, then continues the turn and session on the fallback.
    /// Works for interactive and headless sessions alike.
    pub fn with_fallback_model(&mut self, fallback: Option<ModelId>) {
        self.fallback_model.clone_from(&fallback);
        if let Some(ref mut engine) = self.engine {
            engine.set_fallback_model(fallback);
        }
    }

    /// Test accessor for the configured fallback model.
    #[cfg(test)]
    pub(crate) const fn fallback_model(&self) -> Option<&ModelId> {
        self.fallback_model.as_ref()
    }

    pub const fn with_replay_user_messages(&mut self, replay: bool) {
        self.replay_user_messages = replay;
    }

    pub const fn with_include_partial_messages(&mut self, include: bool) {
        self.include_partial_messages = include;
    }

    /// Startup preferences persisted in settings.
    ///
    /// `/vim` and `/statusline-setup` write these keys; without reading them
    /// back at startup the commands claimed to persist a preference that was
    /// never applied.
    /// Attach the session's telemetry handle (only when the user opted in).
    pub fn with_telemetry_handle(&mut self, handle: Option<coderex_telemetry::TelemetryHandle>) {
        self.telemetry = handle;
    }

    /// Agent definitions contributed by active plugins.
    pub fn with_plugin_agents(&mut self, agents: Vec<(String, serde_json::Value)>) {
        self.plugin_agents = agents;
    }

    /// Components contributed by active plugins.
    ///
    /// Each map is name-keyed; the caller has already normalized the manifest
    /// shapes (see `PluginManager::plugin_*`).
    pub fn with_plugin_components(
        &mut self,
        themes: Vec<(String, serde_json::Value)>,
        output_styles: Vec<(String, serde_json::Value)>,
        workflows: std::collections::HashMap<String, String>,
        monitors: Option<serde_json::Value>,
        lsp_servers: Vec<(String, serde_json::Value)>,
        other: Vec<(String, Vec<String>)>,
    ) {
        self.plugin_themes = themes;
        self.plugin_output_styles = output_styles;
        if let Some(map) = self.plugin_workflows.as_ref() {
            let mut map = map
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (name, script) in workflows {
                map.insert(name, script);
            }
        }
        self.plugin_monitors = monitors;
        // The first configured server becomes the LSP tool's default. The
        // reference shape is `{command, args?, root_uri?}`; an `.lsp.json`
        // path was already read into this map by the plugin crate.
        let first_server = lsp_servers.iter().find_map(|(_, config)| {
            let command = config.get("command")?.as_str()?.to_string();
            let args = config
                .get("args")
                .and_then(serde_json::Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(|arg| arg.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let root_uri = config
                .get("root_uri")
                .or_else(|| config.get("rootUri"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            Some(PluginLspDefaults {
                command,
                args,
                root_uri,
            })
        });
        if first_server.is_some() {
            *self
                .lsp_defaults
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = first_server;
        }
        self.plugin_lsp_servers = lsp_servers;
        self.plugin_other_components = other;
    }

    pub fn with_startup_preferences(
        &mut self,
        vim_mode: Option<bool>,
        statusline_theme: Option<String>,
    ) {
        self.settings_vim_mode = vim_mode;
        self.settings_statusline_theme = statusline_theme;
    }

    pub fn with_betas(&mut self, betas: Vec<String>) {
        self.betas = betas;
    }

    /// Replace the base system prompt (`--system-prompt`).
    ///
    /// Interactive sessions receive this from the orchestrator's
    /// [`configure_repl`](crate::orchestrator::StartupOrchestrator::configure_repl);
    /// without the setter the flag was parsed and then silently ignored.
    pub fn with_system_prompt(&mut self, prompt: Option<String>) {
        self.session_effects.override_system_prompt = prompt;
    }

    /// Append text to the system prompt (`--append-system-prompt`).
    ///
    /// Independent of [`with_system_prompt`](Self::with_system_prompt): both
    /// may be set, in which case the replacement supplies the base prompt and
    /// this text is applied on top of it.
    pub fn with_append_system_prompt(&mut self, prompt: Option<String>) {
        self.session_effects.append_system_prompt = prompt;
    }

    /// Test accessor for the two system-prompt effects.
    ///
    /// Returns `(replacement, appended)`.
    #[cfg(test)]
    pub(crate) fn system_prompt_effects_for_test(&self) -> (Option<&str>, Option<&str>) {
        (
            self.session_effects.override_system_prompt.as_deref(),
            self.session_effects.append_system_prompt.as_deref(),
        )
    }

    /// Attach pull-request context fetched for `--from-pr`.
    pub fn with_pr_context(&mut self, context: Option<String>) {
        self.pr_context = context;
    }

    /// The feature flags resolved for this session.
    #[must_use]
    pub const fn feature_flags(&self) -> &coderex_config::flags::FeatureFlags {
        &self.feature_flags
    }

    /// The command context, for tests outside this module.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn command_context_for_test(&self) -> coderex_commands::CommandContext {
        self.build_context()
    }

    /// The hook documents split by source tier, as installed.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn hook_layers_for_test(
        &self,
    ) -> Option<&coderex_config::settings::HookLayers> {
        self.hook_layers.as_ref()
    }

    /// The capability set derived from the session's feature flags.
    #[must_use]
    pub const fn capabilities(&self) -> coderex_config::flags::CapabilitySet {
        self.capabilities
    }

    /// Wire a live IDE MCP connection into the session.
    ///
    /// Registers the server's tools into the dispatcher's registry and keeps
    /// the manager alive so calls can resolve their connection later.
    ///
    /// # Panics
    ///
    /// Panics if the MCP slot's lock is poisoned.
    pub fn with_ide_connection(
        &mut self,
        manager: Arc<coderex_mcp::manager::McpManager>,
        status: String,
    ) {
        if let Some(ref dispatcher) = self.dispatcher {
            manager.register_tools_into(dispatcher.registry());
        }
        self.ide_manager = Some(manager);
        self.ide_status = Some(status);
    }

    pub const fn with_chrome_enabled(&mut self, enabled: bool) {
        self.chrome_enabled = enabled;
    }

    pub fn with_agent(&mut self, agent: Option<String>) {
        self.agent = agent;
    }

    /// Record whether `--model` was given explicitly.
    ///
    /// A named custom agent supplies a default model, but an explicit CLI
    /// choice is more specific and must win.
    pub const fn with_explicit_model_override(&mut self, explicit: bool) {
        self.explicit_model_override = explicit;
    }

    pub fn with_ide(&mut self, ide: Option<String>) {
        self.ide = ide;
    }

    pub const fn with_worktree(&mut self, worktree: bool) {
        self.worktree = worktree;
    }

    pub fn with_fork_session(&mut self, fork: Option<String>) {
        self.fork_session = fork;
    }

    pub fn with_resume_session(&mut self, session_id: Option<String>) {
        self.resume_session = session_id;
    }

    pub fn with_from_pr(&mut self, from_pr: Option<String>) {
        self.from_pr = from_pr;
    }

    pub fn with_session_name(&mut self, name: Option<String>) {
        self.session_name = name;
    }

    pub fn with_add_dir(&mut self, dirs: Vec<String>) {
        self.add_dir = dirs;
    }

    pub fn with_input_format(&mut self, format: String) {
        self.input_format = format;
    }

    pub const fn with_tmux_enabled(&mut self, tmux: bool) {
        self.tmux_enabled = tmux;
    }

    pub fn with_debug_file(&mut self, path: Option<std::path::PathBuf>) {
        self.debug_file = path;
    }

    pub fn with_allowed_tools(&mut self, tools: Vec<String>) {
        self.session_effects.allowed_tools_override = Some(tools);
    }

    /// Apply all CLI configuration settings to the engine, dispatcher, and session.
    /// Must be called after `with_engine` (or `new`) and before `run_tui`/`run_headless`.
    pub fn apply_cli_config(&mut self) {
        // Persisted startup preferences come from settings, not from a flag.
        if let Some(vim_mode) = self.settings_vim_mode {
            self.vim_mode_enabled = vim_mode;
        }
        if let Some(ref theme) = self.settings_statusline_theme {
            self.current_theme = theme.clone();
        }

        // 0. Named custom agent. The orchestrator rejects unknown names before
        // the loop starts; applying the definition here makes the selection
        // affect the session prompt and tool whitelist rather than being
        // recorded and ignored. Explicit flags win: they are more specific
        // than the agent file.
        self.apply_agent_selection();
        self.apply_plugin_agents();

        // 1. Permission mode
        if self.skip_permissions {
            if let Some(ref mut engine) = self.engine {
                let checker = engine.permission_checker();
                let mut c = checker.lock().unwrap();
                c.set_mode(PermissionMode::BypassPermissions);
            }
        } else if let Some(mode) = self.permission_mode {
            if let Some(ref mut engine) = self.engine {
                let checker = engine.permission_checker();
                let mut c = checker.lock().unwrap();
                c.set_mode(mode);
            }
        }

        // 2. Disable slash commands
        if self.no_slash_commands {
            self.commands.clear();
        }

        // 2b. Disable session persistence — clear memory stores so nothing writes to disk
        if self.no_persistence {
            self.session_memory_store = None;
            self.long_term_store = None;
            self.auto_dream = None;
            self.team_memory_service = None;
            // The memory tools hold their own handle to the stores, so
            // dropping the fields above does not stop them writing.
            self.memory_writable
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if let Some(ref mut engine) = self.engine {
                engine.clear_transcript_path();
            }
            tracing::info!("Session persistence disabled — memory stores cleared");
        } else if let Some(ref mut engine) = self.engine {
            // Compaction backs the transcript up before overwriting it, but
            // only when it knows where transcripts live. The CLI persists them
            // here; without this the engine's backup path stayed `None` and
            // the pre-compaction backup never ran.
            engine.set_transcript_path(crate::session_resume::transcript_dir(&self.cwd));
        }

        // 3. Chrome integration. The dispatcher is behind an Arc with clones
        // (the engine and the agent manager), so `Arc::get_mut` never succeeds;
        // the flag is shared instead and both listing and execution honour it.
        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_chrome_enabled(self.chrome_enabled);
        }

        // 4. Session name / title
        if let Some(ref name) = self.session_name {
            if let Some(ref mut engine) = self.engine {
                let mut session = engine.session().clone();
                session.title = Some(name.clone());
                engine.set_session(session);
            }
        }

        // 5. Worktree mode
        //
        // This used to record the *current* directory as the active worktree,
        // with the current branch — so `--worktree` told the session it was
        // isolated while it went on working in the person's own checkout. It
        // creates one now, and the session is pointed at it. A failure is
        // reported rather than papered over: silently working in the checkout
        // is the thing the flag exists to prevent.
        if self.worktree {
            self.enter_worktree_mode();
        }

        // 6. Resume or fork an existing transcript into the current engine session
        if let Some(session_id) = self.resume_session.clone() {
            if let Err(err) = self.load_session_state(&session_id, true) {
                tracing::warn!("Failed to restore resumed session {session_id}: {err}");
            }
        } else if let Some(fork_id) = self.fork_session.clone() {
            if let Err(err) = self.load_session_state(&fork_id, false) {
                tracing::warn!("Failed to load fork session {fork_id}: {err}");
            }
        }

        // 7. Replay user messages from most recent session
        if self.replay_user_messages {
            if let Some(ref mut engine) = self.engine {
                if let Some(session_id) = session_resume::latest_session_id(&self.cwd) {
                    if let Ok(msgs) = session_resume::load_resume_messages(&self.cwd, &session_id) {
                        let user_msgs: Vec<_> = msgs
                            .into_iter()
                            .filter(|m| matches!(m.role, coderex_core::types::Role::User))
                            .collect();
                        let mut session = engine.session().clone();
                        session.messages.extend(user_msgs);
                        engine.set_session(session);
                    }
                }
            }
        }

        // 8. Add directory contents to extra context
        for dir in &self.add_dir {
            let path = std::path::Path::new(dir);
            if path.exists() && path.is_dir() {
                let listing = Self::list_directory_contents(path);
                self.session_effects
                    .extra_context
                    .push(format!("Directory listing for {dir}:\n{listing}"));
            }
        }

        // 9. Beta feature flags. `--betas` is a real overlay on the
        // environment-provided flags: it resolves into this session's
        // `FeatureFlags` and `CapabilitySet`. A typo is a startup failure, not
        // a warning, because silently dropping a mistyped name looks identical
        // to the flag not existing.
        if !self.betas.is_empty() {
            let raw = self.betas.join(",");
            let mut flags = coderex_config::flags::FeatureFlags::from_env();
            match flags.apply_comma_list(&raw) {
                Ok(()) => {
                    self.feature_flags = flags;
                    self.capabilities = self.feature_flags.capabilities();
                    tracing::info!(
                        capabilities = ?self.capabilities,
                        "--betas applied to the session capability set"
                    );
                }
                Err(error) => {
                    let message = format!("--betas: {error}");
                    eprintln!("{message}");
                    self.setup_error = Some(message);
                    self.running = false;
                }
            }
        }

        // 9. Inert flags. These names are accepted for configuration
        // compatibility but nothing in this build reads them; saying so once
        // at startup is better than letting `--betas` imply an effect it does
        // not have.
        for flag in self.feature_flags.inert_flags() {
            tracing::warn!(
                flag,
                "feature flag is accepted for compatibility but has no effect in this build"
            );
            // Tracing defaults to `off`, so a user who just passed an inert
            // flag would otherwise see nothing at all.
            eprintln!(
                "warning: feature flag '{flag}' is accepted for compatibility but has no effect in this build"
            );
        }

        // 9a. The dispatcher and the Workflow tool were built before `--betas`
        // was resolved. Both bits are shared, so the session's final flag set
        // still decides whether the tools are offered and callable.
        self.workflow_enabled.store(
            self.feature_flags.workflow_scripts,
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Some(dispatcher) = &self.dispatcher {
            dispatcher.set_feature_flags(self.feature_flags.clone());
        }

        // 9b. Context-window overrides. The engine already honors the
        // environment, client-data file and built-in table; settings and
        // experiment values are only reachable here, where both are resolved.
        if let Some(ref mut engine) = self.engine {
            let resolver = coderex_core::types::ContextWindowResolver::from_env_and_client_data()
                .with_settings(self.context_window_settings.clone())
                .with_experiment(self.feature_flags.experiment_context_windows());
            engine.set_context_window_resolver(resolver);
        }

        // 9c. Engine flag behaviours. `coordinator_mode` and `history_snip`
        // are engine behaviours the engine exposes setters for; the session's
        // flag set is only final here, after `--betas` has been applied.
        if let Some(ref mut engine) = self.engine {
            let memory_base = std::path::PathBuf::from(&self.cwd)
                .join(".coderex")
                .join("memory");
            apply_engine_flag_behaviours(engine, &self.feature_flags, Some(&memory_base));
        }

        // 9d. Auto-dream. The manager is built at construction, before `--betas`
        // is resolved, so the flag is applied here. With `auto_dream` off there
        // is no dream manager and `/auto-dream` reports as much instead of
        // starting a background consolidation the user did not ask for.
        if !self.feature_flags.auto_dream {
            self.auto_dream = None;
        }

        // 10. Pull-request context. The orchestrator fetches this before the
        // REPL exists; the fallback here keeps a direct `with_from_pr` caller
        // honest, and a fetch failure stops startup rather than continuing
        // without the context the flag promised.
        if self.pr_context.is_none() {
            if let Some(ref url) = self.from_pr {
                match crate::from_pr::fetch(url) {
                    Ok(context) => self.pr_context = Some(context.block),
                    Err(error) => {
                        let message = format!("--from-pr: {error:#}");
                        eprintln!("{message}");
                        self.setup_error = Some(message);
                        self.running = false;
                    }
                }
            }
        }
        if let Some(ref context) = self.pr_context {
            self.session_effects.extra_context.push(context.clone());
        }

        // 11. IDE connection status. The tools themselves were registered by
        // `with_ide_connection`; the connection failing is handled during
        // startup, before the REPL is built.
        if let Some(ref status) = self.ide_status {
            tracing::info!("{status}");
        }
        if let Some(ref ide) = self.ide {
            tracing::debug!("IDE mode requested: {ide}");
        }
        if self.tmux_enabled {
            tracing::debug!("tmux session/title was applied before startup");
        }

        // 12. Every flag with a product path has been applied above (workflow,
        // dispatcher, auto-dream, and the dispatcher's own tool gates). The
        // compatibility-only names were already warned about in step 9; there
        // is nothing else to install here.

        // 14. Debug file — log that it would be configured
        if let Some(ref debug_path) = self.debug_file {
            tracing::info!("Debug output file configured: {debug_path:?}");
        }

        // 15. Lifecycle hooks
        //
        // The hooks crate was complete and never installed: the engine kept the
        // no-op executor it is built with, so a `hooks` block in settings.json
        // was parsed, validated, and then did nothing whatsoever. This is the
        // call that makes a configured event actually fire.
        //
        // Hooks from user/project settings come from a source a repository can
        // write, so they are loaded in the `Core` tier. Managed hooks come from
        // an administrator-controlled carrier and are loaded separately as
        // `Managed`, ahead of every core hook regardless of priority. The two
        // documents are never combined first: doing that would either demote
        // the managed hooks or run them twice.
        //
        // `hook_layers` is the resolved split. `hooks_settings` alone is the
        // compatibility path for a hand-built `Settings` (and for callers of
        // `with_hooks`), where the merged value is treated as core.
        let mut hook_sources: Vec<(coderex_hooks::HookTier, serde_json::Value)> = Vec::new();
        if let Some(layers) = self.hook_layers.as_ref() {
            if let Some(value) = &layers.managed {
                hook_sources.push((coderex_hooks::HookTier::Managed, value.clone()));
            }
            if let Some(value) = &layers.plugin {
                hook_sources.push((coderex_hooks::HookTier::Plugin, value.clone()));
            }
            if let Some(value) = &layers.core {
                hook_sources.push((coderex_hooks::HookTier::Core, value.clone()));
            }
        } else if let Some(hooks) = &self.hooks_settings {
            hook_sources.push((coderex_hooks::HookTier::Core, hooks.clone()));
        }

        // 15a. `agent_triggers` gates the agent-kind hook path. An `agent` hook
        // is an agentic verifier the session runs with its own model, which is
        // exactly the "agent trigger" the flag names. With the flag off those
        // entries are dropped before the registry sees them, so a configured
        // agent hook cannot run; the other four kinds are untouched. Saying so
        // out loud keeps a silently-dropped hook from looking like one that ran.
        let mut skipped_agent_hooks = 0;
        if !self.feature_flags.agent_triggers {
            for (_, value) in &mut hook_sources {
                let (filtered, skipped) = strip_agent_hooks(value);
                *value = filtered;
                skipped_agent_hooks += skipped;
            }
        }
        if skipped_agent_hooks > 0 {
            tracing::warn!(
                count = skipped_agent_hooks,
                "agent hooks skipped: the 'agent_triggers' feature flag is off"
            );
            eprintln!(
                "warning: skipped {skipped_agent_hooks} agent hook(s): the 'agent_triggers' \
                 feature flag is off (enable it with --betas agent_triggers)"
            );
        }

        let mut installed_hook_count = 0;
        if !hook_sources.is_empty() {
            let hook_count: usize = hook_sources
                .iter()
                .map(|(_, value)| count_configured_hooks(value))
                .sum();
            if let Some(ref mut engine) = self.engine {
                match coderex_hooks::HookRegistry::from_settings_sources(
                    hook_sources.iter().map(|(tier, value)| (*tier, value)),
                ) {
                    Ok(registry) => {
                        // Three kinds — `prompt`, `agent`, `mcp_tool` — need a
                        // model client and the tool registry, which the hooks
                        // crate cannot reach. Without this they load and then
                        // fail, which denies on a gating event.
                        let mut registry = registry;
                        if let (Some(api), Some(dispatcher)) =
                            (self.api_client.as_ref(), self.dispatcher.as_ref())
                        {
                            registry.set_action_runner(Arc::new(
                                crate::hook_actions::SessionHookActionRunner::new(
                                    api.clone_box(),
                                    Arc::clone(dispatcher)
                                        as Arc<dyn crate::hook_actions::HookToolCaller>,
                                    self.model.clone(),
                                ),
                            ));
                        }
                        // A `command` hook running with `stream: true` can
                        // ask the session a question. Without a handler every
                        // request was answered `-32601`; this one answers the
                        // read-only session facts and refuses the rest.
                        let session_cwd = engine.session().cwd.clone();
                        let session_id = engine.session().session_id.to_string();
                        let session_model = engine.model().clone();
                        // Captured before the two values move into the request
                        // handler: a mod's read-only `$` sees these facts.
                        let permission_mode = engine.permission_checker().lock().map_or_else(
                            |poisoned| poisoned.into_inner().mode(),
                            |checker| checker.mode(),
                        );
                        let mod_session = coderex_mods::ModSession {
                            cwd: session_cwd.to_string_lossy().to_string(),
                            model: session_model.0.clone(),
                            permission_mode: format!("{permission_mode:?}"),
                        };
                        registry.set_request_handler(Arc::new(
                            crate::hook_actions::SessionHookRequestHandler::new(
                                session_cwd,
                                session_id,
                                session_model,
                                engine.permission_checker(),
                            ),
                        ));
                        // In-process JavaScript mods (opt-in via `CODEREX_MODS`
                        // or `CODEREX_MODS_DIR`): their hooks run first, then
                        // the hooks from settings.
                        let inner = registry.into_executor();
                        engine.set_hooks(
                            match coderex_mods::JsHookExecutor::load_from_env(
                                mod_session,
                                Some(Arc::clone(&inner)
                                    as Arc<dyn coderex_core::hook_executor::HookExecutor>),
                            ) {
                                Some(Ok(executor)) => {
                                    self.mod_ui_handle = Some(executor.ui_handle());
                                    Arc::new(executor)
                                }
                                Some(Err(error)) => {
                                    tracing::warn!("mods were not loaded: {error}");
                                    inner
                                }
                                None => inner,
                            },
                        );
                        installed_hook_count = hook_count;
                    }
                    Err(e) => tracing::warn!("hooks in settings were not loaded: {e}"),
                }
            }
        }
        self.active_hooks = installed_hook_count;
    }

    /// Apply the named custom agent selected with `--agent`.
    ///
    /// The definition's system prompt replaces the default prompt unless a
    /// `--system-prompt` was supplied. Its tool whitelist is applied unless
    /// `--allowed-tools` already provided one; an explicitly empty list is
    /// still a whitelist (it denies every tool), so it wins too. Resolution
    /// should already have succeeded in the orchestrator; a failure here is
    /// logged rather than silently running the default agent.
    /// Register agent definitions contributed by active plugins.
    fn apply_plugin_agents(&self) {
        if self.plugin_agents.is_empty() {
            return;
        }
        let Some(manager) = self.agent_manager.as_ref() else {
            return;
        };
        for (name, value) in &self.plugin_agents {
            let mut value = value.clone();
            if value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_none()
            {
                if let Some(object) = value.as_object_mut() {
                    object.insert("name".to_string(), serde_json::Value::String(name.clone()));
                }
            }
            match serde_json::from_value::<coderex_agents::CustomAgentDef>(value) {
                Ok(definition) => {
                    if let Ok(mut manager) = manager.try_lock() {
                        manager.register_custom_agent(definition);
                    } else {
                        tracing::warn!(
                            agent = %name,
                            "plugin agent definition skipped: the agent manager is busy"
                        );
                    }
                }
                Err(error) => tracing::warn!(
                    agent = %name,
                    %error,
                    "plugin agent definition is not a valid custom agent"
                ),
            }
        }
    }

    fn apply_agent_selection(&mut self) {
        let Some(name) = self.agent.clone() else {
            return;
        };
        match crate::agent_selection::resolve_agent(&name) {
            Ok(agent) => self.apply_resolved_agent(agent),
            Err(error) => tracing::warn!("--agent was not applied: {error}"),
        }
    }

    /// Merge a resolved agent definition into the session effects.
    ///
    /// Split from [`Self::apply_agent_selection`] so the precedence rules can
    /// be tested without reading the user's `~/.coderex/agents/`.
    fn apply_resolved_agent(&mut self, agent: crate::agent_selection::ResolvedAgent) {
        if self.session_effects.override_system_prompt.is_none() {
            self.session_effects.override_system_prompt = Some(agent.system_prompt);
        }
        if self.session_effects.allowed_tools_override.is_none() && !agent.allowed_tools.is_empty()
        {
            self.session_effects.allowed_tools_override = Some(agent.allowed_tools);
        }

        // Resource defaults from the definition apply unless the person chose
        // otherwise on the command line. `--model` is the only CLI override;
        // there is no max-token flag, so the definition's limit always wins
        // over the engine default.
        if !self.explicit_model_override {
            if let Some(model) = agent.model {
                self.model = ModelId(model);
            }
        }
        if let Some(max_tokens) = agent.max_tokens.filter(|limit| *limit > 0) {
            self.max_tokens = max_tokens;
        }
        if let Some(ref mut engine) = self.engine {
            engine.set_model(self.model.clone());
            engine.set_max_tokens(self.max_tokens);
        }
    }

    /// Create a worktree and move the session into it.
    ///
    /// The base is the session's configured directory (`--directory` or the
    /// process cwd), not `std::env::current_dir()`: the two differ whenever a
    /// caller configured the session explicitly, and using the process cwd
    /// created the worktree from the wrong repository.
    fn enter_worktree_mode(&mut self) {
        let base = std::path::PathBuf::from(&self.cwd);
        self.enter_worktree_mode_for(&base);
    }

    /// The same, for a named repository.
    ///
    /// Split out because the flag works from the process's working directory,
    /// which a test cannot change.
    fn enter_worktree_mode_for(&mut self, base: &std::path::Path) {
        use coderex_core::worktree::{WorktreeConfig, WorktreeManager};

        let base = base.to_path_buf();

        let config = WorktreeConfig {
            base_dir: base.join(".coderex").join("worktrees"),
            ..WorktreeConfig::default()
        };
        let mut manager = WorktreeManager::new(config);
        let created = match manager.create_worktree(&base, None) {
            Ok(created) => created,
            Err(error) => {
                // The alternative is pretending, which is what this used to do.
                tracing::error!(
                    "could not create a worktree for --worktree: {error} — refusing to run \
                     isolated-mode work in the checkout instead"
                );
                let message = format!(
                    "could not create a worktree: {error}\n\
                     --worktree isolates the session in its own checkout; without one the \
                     session would edit this checkout, so it has stopped."
                );
                eprintln!("{message}");
                // Headless mode does not read `running`; `run_headless` checks
                // this instead, so a failed `--worktree` cannot fall back to
                // the original checkout in `-p` mode.
                self.setup_error = Some(message);
                self.running = false;
                return;
            }
        };

        if let Some(ref mut engine) = self.engine {
            let mut session = engine.session().clone();
            session.active_worktree = Some(coderex_core::state::WorktreeState {
                path: created.path.clone(),
                base: created.branch.clone(),
                has_changes: false,
            });
            // The engine builds the model's prompt context from `session.cwd`,
            // so leaving it at the original checkout would tell the model to
            // work there while the tools ran in the worktree.
            session.cwd.clone_from(&created.path);
            engine.set_session(session);
        }

        // The tools have to run there too, or the session is isolated in name
        // only.
        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_cwd(created.path.clone());
        }
        self.cwd = created.path.to_string_lossy().to_string();
        tracing::info!(
            worktree = %created.path.display(),
            branch = %created.branch,
            "--worktree: the session runs in a worktree of its own"
        );
    }

    /// List directory contents as a simple string.
    fn list_directory_contents(path: &std::path::Path) -> String {
        let mut entries = Vec::new();
        if let Ok(dir) = std::fs::read_dir(path) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let meta = entry.metadata();
                let kind = meta
                    .as_ref()
                    .map_or("?", |m| if m.is_dir() { "d" } else { "f" });
                entries.push(format!("  [{kind}] {name}"));
            }
        }
        entries.join("\n")
    }

    fn current_session_id(&self) -> Option<String> {
        self.engine
            .as_ref()
            .map(|engine| engine.session().session_id.to_string())
    }

    fn current_session_title(&self) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|engine| engine.session().title.clone())
    }

    fn load_session_state(
        &mut self,
        session_id: &str,
        replace_session_id: bool,
    ) -> std::result::Result<
        (
            Vec<coderex_core::types::Message>,
            Option<session_resume::InteractiveResumeSnapshot>,
        ),
        String,
    > {
        let messages = session_resume::load_resume_messages(&self.cwd, session_id)
            .map_err(|e| e.to_string())?;
        let snapshot = session_resume::load_snapshot(&self.cwd, session_id);

        if let Some(ref mut engine) = self.engine {
            let mut session = engine.session().clone();
            if replace_session_id {
                if let Ok(parsed) = uuid::Uuid::parse_str(session_id) {
                    session.session_id = parsed;
                }
            }
            session.messages.clone_from(&messages);
            if session.title.is_none() {
                session.title = snapshot
                    .as_ref()
                    .and_then(|state| state.session_title.clone());
            }
            engine.set_session(session);
        }

        self.history = messages
            .iter()
            .filter_map(|message| match (&message.role, &message.content) {
                (
                    coderex_core::types::Role::User,
                    coderex_core::types::MessageContent::Text(text),
                ) => Some(text.clone()),
                _ => None,
            })
            .collect();

        Ok((messages, snapshot))
    }

    /// Report background work the previous session left behind, and settle it.
    ///
    /// The reference re-attaches to a job because its jobs are separate
    /// processes with recorded PIDs. CodeRex's run inside this one, so there is
    /// nothing to re-attach to; the honest half is to say what was interrupted
    /// instead of dropping it silently.
    fn adopt_background_jobs(&self, ui_state: &mut coderex_ui::state::UiState) {
        for text in self.background_job_notifications() {
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::System,
                content: text.into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }
    }

    /// Settle jobs the previous session left behind and return what to tell
    /// the user.
    ///
    /// Both entry paths adopt: a headless start used to skip this entirely, so
    /// a record that still claimed to be running stayed that way and nobody
    /// was told it had been interrupted.
    fn background_job_notifications(&self) -> Vec<String> {
        let session_id = self
            .current_session_id()
            .unwrap_or_else(|| "unknown".to_string());
        let store = coderex_core::JobStore::new(&self.cwd);
        let adopted = match coderex_core::adopt_for_session(
            &store,
            coderex_core::task::now_ms(),
            &session_id,
        ) {
            Ok(adopted) => adopted,
            Err(error) => {
                tracing::warn!("could not read the background job records: {error}");
                return Vec::new();
            }
        };
        adopted
            .notifications
            .into_iter()
            .map(|notification| notification.text)
            .collect()
    }

    fn persist_interactive_snapshot(&self, ui_state: &coderex_ui::state::UiState) {
        if self.no_persistence {
            return;
        }
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let session = engine.session();
        let session_id = session.session_id.to_string();
        let transcript_store = coderex_memory::transcript::TranscriptStore::new(
            session_resume::transcript_dir(&self.cwd),
        );
        if let Err(err) = transcript_store.rewrite(&session_id, &session.messages) {
            tracing::warn!("Failed to persist transcript for {session_id}: {err}");
        }
        let snapshot = build_interactive_snapshot(ui_state, self.current_session_title());
        if let Err(err) = session_resume::save_snapshot(&self.cwd, &session_id, &snapshot) {
            tracing::warn!("Failed to persist interactive snapshot for {session_id}: {err}");
        }
        // The SQLite index is a search accelerator over the transcript, not a
        // second source of truth: a failure only warns.
        session_resume::index_saved_session(
            &self.cwd,
            &session_id,
            session.title.as_deref(),
            &session.messages,
        );
        session_resume::index_session_usage(
            &self.cwd,
            &session_id,
            session.cost_usd,
            i64::try_from(
                session.usage.input_tokens
                    + session.usage.output_tokens
                    + session.usage.cache_read_tokens
                    + session.usage.cache_creation_tokens,
            )
            .unwrap_or(i64::MAX),
        );
    }

    fn clear_interactive_snapshot(&self, session_id: Option<&str>) {
        if self.no_persistence {
            return;
        }
        let session_id = session_id
            .map(ToString::to_string)
            .or_else(|| self.current_session_id());
        if let Some(session_id) = session_id {
            if let Err(err) = session_resume::remove_snapshot(&self.cwd, &session_id) {
                tracing::warn!("Failed to remove interactive snapshot for {session_id}: {err}");
            }
        }
    }

    fn reset_restored_ui(ui_state: &mut coderex_ui::state::UiState) {
        ui_state.overlays.clear();
        ui_state.notifications.dismiss_all();
        ui_state.input = coderex_ui::input::InputBuffer::new();
        ui_state.plan.reset();
        ui_state.activity_log.clear();
        ui_state.permission_bar.clear_status();
        clear_permission_overlay(ui_state);
        ui_state.layout_mode = coderex_ui::state::LayoutMode::SinglePane;
        ui_state.left_pane_content.clear();
        ui_state.left_pane_title.clear();
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking = false;
        ui_state.statusline.thinking_label = None;
    }

    fn restore_session_into_ui(
        ui_state: &mut coderex_ui::state::UiState,
        messages: &[coderex_core::types::Message],
        snapshot: Option<&session_resume::InteractiveResumeSnapshot>,
    ) {
        Self::reset_restored_ui(ui_state);
        restore_transcript_messages(ui_state, messages);
        if let Some(snapshot) = snapshot {
            apply_interactive_snapshot(ui_state, snapshot);
        }
    }

    /// Resolve a `/resume` argument to a session id.
    ///
    /// An exact transcript id wins; anything else is a title/content fragment
    /// resolved through the session index. One match is used, no match falls
    /// through (so the caller reports "no transcript"), and several matches
    /// list the candidates instead of guessing.
    fn resolve_resume_session_id(
        &self,
        requested_session: Option<&str>,
    ) -> std::result::Result<String, String> {
        match requested_session {
            Some(requested)
                if !session_resume::transcript_dir(&self.cwd)
                    .join(format!("{requested}.jsonl"))
                    .exists() =>
            {
                let matches = session_resume::search_sessions(&self.cwd, requested, 5);
                match matches.as_slice() {
                    [one] => Ok(one.session_id.clone()),
                    [] => Ok(requested.to_string()),
                    many => {
                        let listed = many
                            .iter()
                            .map(|meta| {
                                format!(
                                    "  {}  {}",
                                    meta.session_id,
                                    meta.ai_title.as_deref().unwrap_or("(untitled)")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Err(format!(
                            "'{requested}' matches several sessions:\n{listed}\nUse the exact id."
                        ))
                    }
                }
            }
            Some(requested) => Ok(requested.to_string()),
            None => session_resume::latest_session_id(&self.cwd).ok_or_else(|| {
                "No previous sessions found in .coderex/transcripts for this workspace.".to_string()
            }),
        }
    }

    fn resume_tui_session(
        &mut self,
        requested_session: Option<&str>,
        ui_state: &mut coderex_ui::state::UiState,
    ) -> std::result::Result<String, String> {
        let session_id = self.resolve_resume_session_id(requested_session)?;
        let (messages, snapshot) = self.load_session_state(&session_id, true)?;
        Self::restore_session_into_ui(ui_state, &messages, snapshot.as_ref());

        let mut message = format!(
            "Resumed session: {session_id}\nLoaded {} messages from transcript.",
            messages.len()
        );
        if let Some(summary) = snapshot
            .as_ref()
            .and_then(super::session_resume::InteractiveResumeSnapshot::unfinished_context)
        {
            message.push_str("\n\n");
            message.push_str(&summary);
        }

        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::System,
            content: message.clone().into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
        ui_state.notifications.info(format!("Resumed {session_id}"));
        self.persist_interactive_snapshot(ui_state);
        Ok(message)
    }

    /// Initialize command history and macro registry from disk.
    fn init_command_persistence(&mut self) {
        let config_dir = coderex_config::settings::config_home();
        let history_path = config_dir.join("command_history.jsonl");
        self.command_history.set_file_path(history_path);
        if let Err(e) = self.command_history.load() {
            tracing::warn!("Failed to load command history: {e}");
        }

        let macros_path = config_dir.join("macros.json");
        self.macro_registry.set_file_path(macros_path);
        if let Err(e) = self.macro_registry.load() {
            tracing::warn!("Failed to load macros: {e}");
        }
    }

    /// Initialize the shared file watcher if the hot-reload feature is on.
    ///
    /// Returns the watcher as well as its receiver: dropping the watcher stops
    /// `notify` (and drops the sender), so the session must keep it alive for
    /// the receiver to ever see a change.
    ///
    /// The watcher is created even when neither settings file exists, so a
    /// path a hook asks for later can still be added. `run_tui` drains the
    /// receiver; a headless run starts its own watcher for the turn.
    #[cfg(feature = "hot-reload")]
    fn init_config_watcher(
        cwd: &str,
    ) -> (
        Option<HookFileWatcher>,
        Option<mpsc::UnboundedReceiver<FileChangeBatch>>,
    ) {
        match HookFileWatcher::new(&default_settings_paths(cwd)) {
            Ok((watcher, rx)) => (Some(watcher), Some(rx)),
            Err(e) => {
                tracing::warn!("Failed to initialize config watcher: {e}");
                (None, None)
            }
        }
    }

    /// Add every path the hooks currently want watched to the shared watcher.
    ///
    /// Static `watch` configuration is present before the first turn;
    /// `SessionStart`'s `watchPaths` appear once the engine has started. The
    /// TUI calls this at start-up and again when a turn releases the engine.
    #[cfg(feature = "hot-reload")]
    fn sync_hook_watch_paths(&mut self) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let paths: Vec<std::path::PathBuf> = engine
            .watch_paths()
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect();
        if let Some(watcher) = self.config_watcher.as_mut() {
            watcher.add_paths(paths);
        }
    }

    /// Start a watcher and firing task for the duration of a headless turn.
    ///
    /// `SessionStart` is fired first so a `watchPaths` result is known before
    /// the watcher starts; the lazy call inside the turn is then a no-op. The
    /// task holds a clone of the engine's executor, so firing does not need
    /// the engine the turn is using.
    #[cfg(feature = "hot-reload")]
    async fn spawn_headless_file_watcher(&mut self) -> Option<HeadlessHookWatcher> {
        let engine = self.engine.as_mut()?;
        engine.ensure_session_started().await;

        let hooks = engine.hook_executor();
        let session_id = engine.session().session_id.to_string();
        let mut paths = default_settings_paths(&self.cwd);
        paths.extend(
            engine
                .watch_paths()
                .into_iter()
                .map(std::path::PathBuf::from),
        );

        let (watcher, mut rx) = match HookFileWatcher::new(&paths) {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, "headless FileChanged watcher was not started");
                return None;
            }
        };
        let task = tokio::spawn(async move {
            while let Some(batch) = rx.recv().await {
                fire_file_changed_batch(hooks.as_ref(), &session_id, &batch).await;
            }
        });
        Some(HeadlessHookWatcher {
            watcher: Some(watcher),
            task,
        })
    }

    /// Run the interactive REPL with full ratatui TUI.
    pub async fn run_tui(&mut self) -> Result<()> {
        use coderex_ui::input::{InputAction, InputBuffer};
        use coderex_ui::message_list::{Message, MessageRole};
        use coderex_ui::overlay::Overlay;
        use coderex_ui::state::{UiState, ViewMode};
        use coderex_ui::terminal::TerminalUi;
        use coderex_ui::vim::{VimAction, VimMode, VimState};
        use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use std::fmt::Write as _;
        use std::time::{Duration, Instant};

        // A configuration failure that happened during `apply_cli_config`
        // (`--betas` typo, `--from-pr` fetch, `--worktree`) must stop the
        // interactive path too, not only the headless one.
        if let Some(error) = self.setup_error.as_deref() {
            return Err(anyhow::anyhow!("{error}"));
        }

        // Static `watch` configuration is known before the first turn; add it
        // now so a FileChanged hook observes it from the start.
        #[cfg(feature = "hot-reload")]
        self.sync_hook_watch_paths();

        // Artifacts are published to a local origin, so the server has to be
        // up before the model can publish anything. It is started here rather
        // than where the registry is built because starting a server needs a
        // runtime; the tools are registered onto the live registry instead.
        self.start_artifact_serving().await;

        enable_raw_mode()?;
        let mut terminal_ui = TerminalUi::new()?;
        terminal_ui.enter()?;

        let mut ui_state = UiState::new();
        self.apply_startup_theme(&mut ui_state);
        // Plugin monitors are persistent watch scripts: arm them with the
        // session and let the supervisor stop them when the loop ends.
        if let Some(monitors) = self.plugin_monitors.clone() {
            match crate::plugin_monitors::PluginMonitorSupervisor::start(
                &monitors,
                std::path::Path::new(&self.cwd),
            ) {
                Ok(supervisor) => {
                    tracing::info!(count = supervisor.running(), "plugin monitors armed");
                    self.monitor_supervisor = Some(supervisor);
                }
                Err(error) => tracing::warn!(%error, "plugin monitors were not started"),
            }
        }
        let mut confirmation_rx = if let Some(bus) = self.confirmation_bus.clone() {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_confirmation_bus(bus.clone());
            }
            if let Some(manager) = self.agent_manager.as_ref() {
                manager.lock().await.set_confirmation_bus(bus.clone());
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ConfirmationRequest>();
            tokio::spawn(async move {
                while let Some(request) = bus.next_request().await {
                    if tx.send(request).is_err() {
                        break;
                    }
                }
            });
            Some(rx)
        } else {
            None
        };

        if let Some(ref engine) = self.engine {
            if engine.session().messages.is_empty() {
                ui_state.messages.push(Message {
                    role: MessageRole::System,
                    content: format!(
                        "CodeRex v{} | model: {} | cwd: {} | Type /help for commands, Ctrl+C to clear, Ctrl+D to exit",
                        env!("CARGO_PKG_VERSION"),
                        self.model.0,
                        self.cwd
                    )
                    .into(),
                    tool_name: None,
                    streaming: false,
                    folded: false,
                });
            } else {
                let snapshot = session_resume::load_snapshot(
                    &self.cwd,
                    &engine.session().session_id.to_string(),
                );
                Self::restore_session_into_ui(
                    &mut ui_state,
                    &engine.session().messages,
                    snapshot.as_ref(),
                );
                if self.resume_session.is_some() {
                    let mut note = format!(
                        "Resumed session: {}\nLoaded {} messages from transcript.",
                        engine.session().session_id,
                        engine.session().messages.len()
                    );
                    if let Some(summary) = snapshot.and_then(|state| state.unfinished_context()) {
                        note.push_str("\n\n");
                        note.push_str(&summary);
                    }
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: note.into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                }
            }
        } else {
            ui_state.messages.push(Message {
                role: MessageRole::System,
                content: format!(
                    "CodeRex v{} | model: {} | cwd: {} | Type /help for commands, Ctrl+C to clear, Ctrl+D to exit",
                    env!("CARGO_PKG_VERSION"),
                    self.model.0,
                    self.cwd
                )
                .into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }

        // Adopt what the last session left behind.
        //
        // A background task runs in this process, so a record that still claims
        // to be running belongs to a process that is gone: this session cannot
        // re-attach to it and says so, in the conversation, rather than leaving
        // a task that looks live and never finishes. A cleanly-exited session
        // is not aged out, and a record written by another session is left
        // alone.
        self.adopt_background_jobs(&mut ui_state);

        // Crossterm event channel — read in a blocking thread
        let (ct_tx, mut ct_rx) = tokio::sync::mpsc::unbounded_channel::<crossterm::event::Event>();
        std::thread::spawn(move || {
            loop {
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        if let Ok(event) = crossterm::event::read() {
                            if ct_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });

        let mut event_rx = self.event_rx.take();
        // Take the file-change receiver out of `self` before the loop: the
        // other `select!` branches borrow `self` mutably, so polling a
        // receiver through `self` for the whole loop would fight them. The
        // watcher itself stays in `self` so `notify` keeps running and its
        // path set can grow after `SessionStart`.
        #[cfg(feature = "hot-reload")]
        let mut config_changed_rx = self.config_changed_rx.take();
        // Without the feature there is no watcher; keep the `select!` branch
        // below compiled and permanently disabled rather than duplicating it.
        #[cfg(not(feature = "hot-reload"))]
        let mut config_changed_rx: Option<mpsc::UnboundedReceiver<FileChangeBatch>> = None;
        let mut vim = VimState::new_insert();
        let mut thinking = false;
        let mut phase = InteractivePhase::Idle;
        let mut query_done_rx: Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        > = None;
        let mut elicit_rx: Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>> = None;
        let mut tui_elicit_rx: Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>> = None;
        // Questions from MCP servers are session-level: they arrive while a
        // tool is running but are not tied to the engine's per-turn channel.
        // The sender lives in the session handler the manager already has.
        let (mcp_elicit_tx, mut mcp_elicit_rx) =
            tokio::sync::mpsc::channel::<TuiElicitationRequest>(4);
        let mcp_elicitation_active = self.mcp_elicitation.is_some();
        if let Some(ref handler) = self.mcp_elicitation {
            handler.set_tui_sender(mcp_elicit_tx);
        }
        let mut queued_prompts = std::collections::VecDeque::<String>::new();
        let mut pending_confirmation: Option<ConfirmationRequest> = None;
        let mut mod_ui_focus = ModUiFocus::default();
        let mut last_stream_snapshot = Instant::now();

        let result = loop {
            if !self.running {
                break Ok(());
            }

            // Sync statusline from REPL state
            self.sync_statusline(&mut ui_state.statusline);
            ui_state.statusline.thinking = thinking;
            ui_state.statusline.plan_status = if thinking {
                ui_state.plan.status_label()
            } else {
                None
            };
            if thinking {
                ui_state.statusline.spinner_frame =
                    ui_state.statusline.spinner_frame.wrapping_add(1);
                ui_state.statusline.thinking_label =
                    phase.status_label().or_else(|| Some("Thinking...".into()));
                if ui_state.statusline.thinking_started.is_none() {
                    ui_state.statusline.thinking_started = Some(std::time::Instant::now());
                }
            } else {
                ui_state.statusline.thinking_label = None;
                ui_state.statusline.spinner_frame = 0;
                if ui_state.statusline.thinking_started.is_some() {
                    if let Some(start) = self.query_start_time {
                        ui_state.statusline.last_duration = Some(start.elapsed());
                    }
                    ui_state.statusline.thinking_started = None;
                }
            }
            // Sync permission bar
            self.sync_permission_bar(&mut ui_state.permission_bar);

            // Poll for completed background tasks and route by kind
            for result in self.poll_background_results() {
                match result.kind {
                    BackgroundFeatureKind::PromptSuggestion => {
                        if result.success {
                            ui_state.input.suggestion = Some(result.content.trim().to_string());
                        }
                    }
                    BackgroundFeatureKind::SessionMemoryUpdate => {
                        Self::handle_memory_update_result(&result);
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: if result.success {
                                "Session memory updated.".into()
                            } else {
                                format!("Session memory update failed: {}", result.content).into()
                            },
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    BackgroundFeatureKind::MemoryExtraction => {
                        Self::handle_memory_extraction_result(&result);
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: if result.success {
                                "Long-term memories extracted.".into()
                            } else {
                                format!("Memory extraction failed: {}", result.content).into()
                            },
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    _ => {
                        let label = format!(
                            "[Background {:?}] {}",
                            result.kind,
                            if result.success { "Done" } else { "Failed" }
                        );
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: format!("{label}\n{}", result.content).into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                }
            }

            // Draw frame
            if let Err(e) = terminal_ui.draw(&mut ui_state) {
                tracing::warn!("TUI draw error: {e}");
            }

            tokio::select! {
                Some(event) = ct_rx.recv() => {
                    match event {
                        Event::Key(key) => {
                            // Windows always reports key releases, and the
                            // disambiguated protocol can too; acting on them
                            // types every character twice.
                            let Some(key) = coderex_ui::input::key_press(key) else {
                                continue;
                            };
                            // A multi-field MCP form owns the keyboard while
                            // it is open: navigation, editing, submit, cancel.
                            if self.pending_mcp_form.is_some()
                                && self.handle_mcp_form_key(&key, &mut ui_state)
                            {
                                continue;
                            }
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                if let Some(request) = pending_confirmation.take() {
                                    self.respond_to_confirmation(
                                        request,
                                        ConfirmationResponse::Denied,
                                        &mut ui_state,
                                        &mut phase,
                                    )
                                    .await;
                                    continue;
                                }
                                if thinking {
                                    self.stop_running_query(
                                        &mut ui_state,
                                        &mut thinking,
                                        &mut phase,
                                        &mut query_done_rx,
                                        &mut elicit_rx,
                                        &mut tui_elicit_rx,
                                        " (interrupted)",
                                    );
                                    pending_confirmation = None;
                                    continue;
                                }
                                if !ui_state.input.text.is_empty() {
                                    ui_state.input = InputBuffer::new();
                                } else if ui_state.overlays.is_active() {
                                    ui_state.overlays.clear();
                                }
                                continue;
                            }

                            if key.code == KeyCode::Char('d')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                break Ok(());
                            }

                            // A mod UI block owns Tab/Shift+Tab, Enter and Esc
                            // only when it has focus and no overlay is asking
                            // for the keyboard. Ordinary characters are never
                            // consumed here, so typing still reaches the line.
                            if !ui_state.overlays.is_active() && pending_confirmation.is_none() {
                                if let Some(action) = mod_ui_key_action(
                                    key.code,
                                    key.modifiers,
                                    mod_ui_focus.focused,
                                    mod_ui_focus.button_count(),
                                ) {
                                    match action {
                                        ModUiKeyAction::FocusNext
                                        | ModUiKeyAction::FocusPrevious => {
                                            mod_ui_focus
                                                .cycle(matches!(action, ModUiKeyAction::FocusNext));
                                            self.redraw_mod_ui_focus(
                                                &mut ui_state,
                                                &mut mod_ui_focus,
                                            );
                                        }
                                        ModUiKeyAction::ClearFocus => {
                                            mod_ui_focus.clear_focus();
                                            self.redraw_mod_ui_focus(
                                                &mut ui_state,
                                                &mut mod_ui_focus,
                                            );
                                        }
                                        ModUiKeyAction::Activate => {
                                            if let Some(id) =
                                                mod_ui_focus.focused_id().map(str::to_string)
                                            {
                                                let outcome = match self.mod_ui_handle.clone() {
                                                    Some(handle) => {
                                                        match tokio::task::spawn_blocking(move || {
                                                            handle.activate(&id)
                                                        })
                                                        .await
                                                        {
                                                            Ok(result) => result,
                                                            Err(error) => Err(format!(
                                                                "the activation task failed: {error}"
                                                            )),
                                                        }
                                                    }
                                                    None => Err(
                                                        "mod UI activation is unavailable in this session"
                                                            .to_string(),
                                                    ),
                                                };
                                                match outcome {
                                                    Ok(elements) => {
                                                        let rendered =
                                                            coderex_ui::mod_elements::render_mod_ui_text(
                                                                &elements,
                                                                coderex_ui::mod_elements::MOD_UI_DEFAULT_WIDTH,
                                                            );
                                                        let content = if rendered.is_empty() {
                                                            "Mod UI action completed with no elements."
                                                                .to_string()
                                                        } else {
                                                            rendered
                                                        };
                                                        ui_state.messages.push(Message {
                                                            role: MessageRole::System,
                                                            content: content.into(),
                                                            tool_name: None,
                                                            streaming: false,
                                                            folded: false,
                                                        });
                                                        let index = ui_state
                                                            .messages
                                                            .messages
                                                            .len()
                                                            .saturating_sub(1);
                                                        mod_ui_focus.reset(&elements, index);
                                                    }
                                                    Err(error) => {
                                                        ui_state.messages.push(Message {
                                                            role: MessageRole::System,
                                                            content: format!(
                                                                "Mod UI action failed: {error}"
                                                            )
                                                            .into(),
                                                            tool_name: None,
                                                            streaming: false,
                                                            folded: false,
                                                        });
                                                    }
                                                }
                                                self.persist_interactive_snapshot(&ui_state);
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }

                            // Shift+Tab: cycle permission mode
                            if key.code == KeyCode::BackTab {
                                ui_state.permission_bar.cycle_mode();
                                if let Some(ref mut engine) = self.engine {
                                    let checker = engine.permission_checker();
                                    if let Ok(mut c) = checker.lock() {
                                        c.set_mode(ui_state.permission_bar.mode);
                                    }
                                }
                                continue;
                            }

                            // Ctrl+Shift+S: toggle split-pane view
                            if key.code == KeyCode::Char('S')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                use coderex_ui::state::LayoutMode;
                                ui_state.layout_mode = match ui_state.layout_mode {
                                    LayoutMode::SinglePane => LayoutMode::HorizontalSplit { left_percent: 50 },
                                    LayoutMode::HorizontalSplit { .. } => LayoutMode::SinglePane,
                                };
                                continue;
                            }

                            // Handle Confirm overlay for destructive commands
                            if matches!(ui_state.overlays.top(), Some(coderex_ui::overlay::Overlay::Confirm { .. })) {
                                match key.code {
                                    KeyCode::Char('y' | 'Y') => {
                                        ui_state.overlays.pop();
                                        if let Some((cmd, args)) = self.pending_destructive_cmd.take() {
                                            self.execute_and_display_command(&cmd, &args, &mut ui_state);
                                        }
                                    }
                                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                                        ui_state.overlays.pop();
                                        self.pending_destructive_cmd = None;
                                        ui_state.messages.push(coderex_ui::message_list::Message {
                                            role: coderex_ui::message_list::MessageRole::System,
                                            content: "Command cancelled.".into(),
                                            tool_name: None,
                                            streaming: false,
                                            folded: false,
                                        });
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if matches!(
                                ui_state.overlays.top(),
                                Some(coderex_ui::overlay::Overlay::PermissionDialog { .. })
                            ) {
                                match key.code {
                                    KeyCode::Char('a') => {
                                        if let Some(request) = pending_confirmation.take() {
                                            self.respond_to_confirmation(
                                                request,
                                                ConfirmationResponse::Approved,
                                                &mut ui_state,
                                                &mut phase,
                                            )
                                            .await;
                                        }
                                    }
                                    KeyCode::Char('A') => {
                                        if let Some(request) = pending_confirmation.take() {
                                            self.respond_to_confirmation(
                                                request,
                                                ConfirmationResponse::ApproveSession,
                                                &mut ui_state,
                                                &mut phase,
                                            )
                                            .await;
                                        }
                                    }
                                    KeyCode::Char('d' | 'D') | KeyCode::Esc => {
                                        if let Some(request) = pending_confirmation.take() {
                                            self.respond_to_confirmation(
                                                request,
                                                ConfirmationResponse::Denied,
                                                &mut ui_state,
                                                &mut phase,
                                            )
                                            .await;
                                        } else {
                                            clear_permission_overlay(&mut ui_state);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            let inline_palette_active = matches!(
                                ui_state.overlays.top(),
                                Some(coderex_ui::overlay::Overlay::CommandPalette { query, .. })
                                    if query.starts_with('/')
                            );
                            if inline_palette_active {
                                match key.code {
                                    KeyCode::Up => {
                                        ui_state.overlays.move_palette_selection(-1);
                                        continue;
                                    }
                                    KeyCode::Down => {
                                        ui_state.overlays.move_palette_selection(1);
                                        continue;
                                    }
                                    KeyCode::Tab => {
                                        if let Some(command) = selected_palette_command(&ui_state) {
                                            ui_state.input.text = format!("{command} ");
                                            ui_state.input.cursor = ui_state.input.text.len();
                                            self.update_slash_suggestions(&mut ui_state);
                                        }
                                        continue;
                                    }
                                    KeyCode::Esc if vim.is_insert() => {
                                        if matches!(
                                            ui_state.overlays.top(),
                                            Some(coderex_ui::overlay::Overlay::CommandPalette { .. })
                                        ) {
                                            ui_state.overlays.pop();
                                        }
                                        continue;
                                    }
                                    _ => {}
                                }
                            }

                            if !self.vim_mode_enabled && key.code == KeyCode::Esc {
                                // Esc means "stop the run" whether or not Vim
                                // mode is on. Only the Vim path used to reach
                                // the interrupt, so in the default mode Esc did
                                // nothing while the model was answering — the
                                // one moment a person most wants it to.
                                if thinking {
                                    self.stop_running_query(
                                        &mut ui_state,
                                        &mut thinking,
                                        &mut phase,
                                        &mut query_done_rx,
                                        &mut elicit_rx,
                                        &mut tui_elicit_rx,
                                        " (interrupted)",
                                    );
                                    pending_confirmation = None;
                                    continue;
                                }
                                if ui_state.overlays.is_active() {
                                    ui_state.overlays.clear();
                                }
                                ui_state.active_view = ViewMode::MainRepl;
                                continue;
                            }

                            let action = vim.handle_key(key);
                            match action {
                                VimAction::Submit => {
                                    if let Some(text) = ui_state.input.take() {
                                        self.process_tui_input(
                                            &text,
                                            &mut ui_state,
                                            &mut thinking,
                                            &mut phase,
                                            &mut query_done_rx,
                                            &mut elicit_rx,
                                            &mut tui_elicit_rx,
                                            &mut queued_prompts,
                                        );
                                        // A successful `/init` queued `Setup`
                                        // inside the synchronous dispatch;
                                        // this is the async drain point for
                                        // the TUI.
                                        self.drain_pending_setup_hook().await;
                                    }
                                }
                                VimAction::Escape => {
                                    if thinking {
                                        self.stop_running_query(
                                            &mut ui_state,
                                            &mut thinking,
                                            &mut phase,
                                            &mut query_done_rx,
                                            &mut elicit_rx,
                                            &mut tui_elicit_rx,
                                            " (cancelled)",
                                        );
                                        pending_confirmation = None;
                                    } else if vim.mode == VimMode::Normal {
                                        ui_state.overlays.clear();
                                        ui_state.active_view = ViewMode::MainRepl;
                                    }
                                }
                                VimAction::Insert => {
                                    if vim.is_insert() {
                                        match key.code {
                                            // A control character never enters the
                                            // single-line buffer, whichever encoding
                                            // delivered it. `key_event_to_action` guards
                                            // this at the other input boundary; this
                                            // inline Vim insert path has to apply the
                                            // same rule.
                                            KeyCode::Char(c) if !c.is_control() => {
                                                ui_state.input.apply(&InputAction::InsertChar(c));
                                            }
                                            KeyCode::Backspace => {
                                                ui_state.input.apply(&InputAction::Backspace);
                                            }
                                            KeyCode::Delete => {
                                                ui_state.input.apply(&InputAction::Delete);
                                            }
                                            KeyCode::Left => {
                                                ui_state.input.apply(&InputAction::CursorLeft);
                                            }
                                            KeyCode::Right => {
                                                ui_state.input.apply(&InputAction::CursorRight);
                                            }
                                            KeyCode::Up => {
                                                ui_state.input.apply(&InputAction::HistoryUp);
                                            }
                                            KeyCode::Down => {
                                                ui_state.input.apply(&InputAction::HistoryDown);
                                            }
                                            KeyCode::Home => {
                                                ui_state.input.apply(&InputAction::MoveToStart);
                                            }
                                            KeyCode::End => {
                                                ui_state.input.apply(&InputAction::MoveToEnd);
                                            }
                                            KeyCode::PageUp => {
                                                let page =
                                                    ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                                ui_state.messages.scroll_up(page);
                                            }
                                            KeyCode::PageDown => {
                                                let page =
                                                    ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                                ui_state.messages.scroll_down_by(page);
                                            }
                                            _ => {}
                                        }
                                    } else if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::Left(count) => {
                                    for _ in 0..count {
                                        ui_state.input.apply(&InputAction::CursorLeft);
                                    }
                                }
                                VimAction::Right(count) => {
                                    for _ in 0..count {
                                        ui_state.input.apply(&InputAction::CursorRight);
                                    }
                                }
                                VimAction::Up => {
                                    // `k` moves to the line above once the
                                    // buffer has one. A single-line buffer has
                                    // no line above, so it keeps its original
                                    // meaning: walk back through history. A
                                    // Visual selection must never be replaced
                                    // by a history entry.
                                    if vim.is_visual() || ui_state.input.line_count() > 1 {
                                        ui_state.input.move_line(-1);
                                    } else {
                                        ui_state.input.apply(&InputAction::HistoryUp);
                                    }
                                }
                                VimAction::Down => {
                                    if vim.is_visual() || ui_state.input.line_count() > 1 {
                                        ui_state.input.move_line(1);
                                    } else {
                                        ui_state.input.apply(&InputAction::HistoryDown);
                                    }
                                }
                                VimAction::JumpToStart => {
                                    ui_state.input.apply(&InputAction::MoveToStart);
                                }
                                VimAction::JumpToEnd => {
                                    ui_state.input.apply(&InputAction::MoveToEnd);
                                }
                                VimAction::DeleteToEnd => {
                                    ui_state.input.apply(&InputAction::KillToEnd);
                                }
                                VimAction::DeleteLine(count) => {
                                    // `dd` is a delete *and* a yank, so the
                                    // removed lines can be put back with `p`.
                                    let (yanked, linewise) = ui_state.input.yank_lines(count);
                                    vim.yank(&yanked);
                                    vim.yank_linewise = linewise;
                                    ui_state.input.delete_lines(count);
                                }
                                VimAction::DeleteWordForward(count) => {
                                    ui_state.input.delete_to_word_forward(count);
                                }
                                VimAction::DeleteWordBackward(count) => {
                                    ui_state.input.delete_to_word_backward(count);
                                }
                                VimAction::DeleteChar(count) => {
                                    for _ in 0..count {
                                        ui_state.input.apply(&InputAction::Delete);
                                    }
                                }
                                VimAction::YankLine(count) => {
                                    let (yanked, linewise) = ui_state.input.yank_lines(count);
                                    vim.yank(&yanked);
                                    vim.yank_linewise = linewise;
                                }
                                VimAction::Paste(count) => {
                                    let yanked = vim.yanked().to_string();
                                    if vim.yank_linewise {
                                        // A linewise register is put back as
                                        // whole lines below the cursor line.
                                        ui_state.input.paste_linewise(&yanked, count, true);
                                    } else {
                                        // The register is text from this buffer
                                        // today, but paste is still an
                                        // insertion path: never let it carry a
                                        // control character in.
                                        let clean: String = yanked
                                            .chars()
                                            .filter(|c| !c.is_control())
                                            .collect();
                                        for _ in 0..count {
                                            ui_state.input.paste(&clean);
                                        }
                                    }
                                }
                                VimAction::Undo => {
                                    if let Some(prev) = vim.pop_undo() {
                                        vim.push_undo(&ui_state.input.text);
                                        ui_state.input.text = prev;
                                        ui_state.input.cursor = ui_state.input.text.len();
                                    }
                                }
                                VimAction::PageUp => {
                                    let page =
                                        ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                    ui_state.messages.scroll_up(page);
                                }
                                VimAction::PageDown => {
                                    let page =
                                        ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                    ui_state.messages.scroll_down_by(page);
                                }
                                VimAction::HalfPageUp => {
                                    let half_page =
                                        (ui_state.messages.viewport_height / 2).max(1);
                                    ui_state.messages.scroll_up(half_page);
                                }
                                VimAction::HalfPageDown => {
                                    let half_page =
                                        (ui_state.messages.viewport_height / 2).max(1);
                                    ui_state.messages.scroll_down_by(half_page);
                                }
                                VimAction::JumpTop => {
                                    if vim.is_visual() || ui_state.input.line_count() > 1 {
                                        // `gg` moves to the first line of the
                                        // buffer.
                                        ui_state.input.move_to_buffer_start();
                                    } else {
                                        ui_state.messages.start = 0;
                                        ui_state.messages.scroll_offset = usize::MAX;
                                    }
                                }
                                VimAction::JumpBottom => {
                                    if vim.is_visual() || ui_state.input.line_count() > 1 {
                                        ui_state.input.move_to_buffer_end();
                                    } else {
                                        ui_state.messages.scroll_to_bottom();
                                    }
                                }
                                VimAction::Command => {
                                    let cmds: Vec<String> = self
                                        .commands
                                        .list()
                                        .iter()
                                        .map(|c| format!("/{} - {}", c.name, c.description))
                                        .collect();
                                    ui_state.overlays.push(Overlay::CommandPalette {
                                        query: ":".into(),
                                        commands: cmds,
                                        selected: 0,
                                    });
                                }
                                VimAction::Search => {
                                    ui_state.active_view = ViewMode::Search;
                                }
                                VimAction::ToggleFold => {
                                    // Toggle fold on the last visible message (or focused if selector active)
                                    if let Some(msg) = ui_state.messages.messages.last_mut() {
                                        msg.folded = !msg.folded;
                                    }
                                }
                                VimAction::ToggleSplit => {
                                    use coderex_ui::state::LayoutMode;
                                    ui_state.layout_mode = match ui_state.layout_mode {
                                        LayoutMode::SinglePane => LayoutMode::HorizontalSplit { left_percent: 50 },
                                        LayoutMode::HorizontalSplit { .. } => LayoutMode::SinglePane,
                                    };
                                }
                                VimAction::SearchNext => {
                                    ui_state.search.next();
                                }
                                VimAction::SearchPrev => {
                                    ui_state.search.prev();
                                }
                                VimAction::ExportCopy => {
                                    use coderex_ui::export::{ExportFormat, export_messages, copy_to_clipboard};
                                    use coderex_ui::selector::MessageSelector;
                                    let core_msgs: Vec<coderex_core::types::Message> = ui_state.messages.messages.iter().map(|m| {
                                        let role = match m.role {
                                            coderex_ui::message_list::MessageRole::User => coderex_core::types::Role::User,
                                            coderex_ui::message_list::MessageRole::Assistant => coderex_core::types::Role::Assistant,
                                            coderex_ui::message_list::MessageRole::System => coderex_core::types::Role::System,
                                            coderex_ui::message_list::MessageRole::ToolCall | coderex_ui::message_list::MessageRole::ToolResult => coderex_core::types::Role::Tool,
                                        };
                                        coderex_core::types::Message {
                                            role,
                                            content: coderex_core::types::MessageContent::Text(m.content.to_string()),
                                        }
                                    }).collect();
                                    let selector = MessageSelector::new(core_msgs.len());
                                    let exported = export_messages(&core_msgs, &selector, ExportFormat::PlainText);
                                    if let Err(e) = copy_to_clipboard(&exported) {
                                        ui_state.notifications.err(format!("Copy failed: {e}"));
                                    } else {
                                        ui_state.notifications.success("Copied to clipboard");
                                    }
                                }
                                VimAction::Repeat
                                | VimAction::Ignore
                                => {}
                                VimAction::Append => {
                                    if self.vim_mode_enabled {
                                        // `a` inserts after the character under
                                        // the cursor — never onto the next line
                                        // and never into the middle of a
                                        // multi-byte character.
                                        let (_, line_end) = ui_state.input.current_line();
                                        let cursor = ui_state.input.cursor;
                                        if cursor < line_end {
                                            let advance = ui_state.input.text[cursor..]
                                                .chars()
                                                .next()
                                                .map_or(1, char::len_utf8);
                                            ui_state.input.cursor = cursor + advance;
                                        }
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                // `o`/`O` open a real line below/above the
                                // current one and enter insert mode on it.
                                VimAction::OpenLineBelow => {
                                    if self.vim_mode_enabled {
                                        ui_state.input.open_line_below();
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::OpenLineAbove => {
                                    if self.vim_mode_enabled {
                                        ui_state.input.open_line_above();
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::InsertAtEnd => {
                                    if self.vim_mode_enabled {
                                        // End of the cursor's line, not of the
                                        // buffer.
                                        ui_state.input.apply(&InputAction::MoveToEnd);
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::InsertAtStart => {
                                    if self.vim_mode_enabled {
                                        ui_state.input.apply(&InputAction::MoveToStart);
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::EnterVisual => {
                                    ui_state.input.start_visual(false);
                                }
                                VimAction::EnterVisualLine => {
                                    ui_state.input.start_visual(true);
                                }
                                VimAction::ExitVisual => {
                                    let linewise = ui_state.input.visual_linewise;
                                    let anchor = ui_state.input.visual_anchor;
                                    // Vim leaves the cursor at the low end of
                                    // the range when Visual mode ends: the
                                    // first line of a linewise selection, the
                                    // earlier end otherwise.
                                    let linewise_start = ui_state
                                        .input
                                        .selection_range()
                                        .map(|(start, _)| start);
                                    ui_state.input.clear_visual_selection();
                                    if linewise {
                                        ui_state.input.cursor =
                                            linewise_start.unwrap_or(ui_state.input.cursor);
                                    } else if let Some(anchor) = anchor {
                                        ui_state.input.cursor =
                                            anchor.min(ui_state.input.cursor);
                                    }
                                }
                                VimAction::WordForward(count) => {
                                    for _ in 0..count {
                                        ui_state.input.word_forward();
                                    }
                                }
                                VimAction::WordBackward(count) => {
                                    for _ in 0..count {
                                        ui_state.input.word_backward();
                                    }
                                }
                                VimAction::WordEnd(count) => {
                                    for _ in 0..count {
                                        ui_state.input.word_end();
                                    }
                                }
                                VimAction::YankSelection => {
                                    let linewise = ui_state.input.visual_linewise;
                                    if let Some(text) = ui_state.input.yank_selection() {
                                        vim.yank(&text);
                                        vim.yank_linewise = linewise;
                                    }
                                }
                                VimAction::DeleteSelection => {
                                    let linewise = ui_state.input.visual_linewise;
                                    if let Some(text) = ui_state.input.delete_selection() {
                                        vim.yank(&text);
                                        vim.yank_linewise = linewise;
                                    }
                                }
                                VimAction::PasteOverSelection => {
                                    let yanked: String = vim
                                        .yanked()
                                        .chars()
                                        .filter(|c| !c.is_control() || *c == '\n')
                                        .collect();
                                    if let Some(replaced) = ui_state.input.replace_selection(&yanked)
                                    {
                                        vim.yank(&replaced);
                                    }
                                }
                                VimAction::ChangeSelection => {
                                    let linewise = ui_state.input.visual_linewise;
                                    if let Some(text) = ui_state.input.delete_selection() {
                                        vim.yank(&text);
                                        vim.yank_linewise = linewise;
                                    }
                                    if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::ChangeLine(count) => {
                                    ui_state.input.change_lines(count);
                                    if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::ChangeWord => {
                                    ui_state.input.delete_word_end();
                                    if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::ChangeToEnd => {
                                    ui_state.input.apply(&InputAction::KillToEnd);
                                    if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::Substitute(count) => {
                                    for _ in 0..count {
                                        ui_state.input.apply(&InputAction::Delete);
                                    }
                                    if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::Indent(count) => {
                                    ui_state.input.indent(count);
                                }
                                VimAction::Outdent(count) => {
                                    ui_state.input.outdent(count);
                                }
                            }

                            // Sync slash-command suggestion overlay when typing in insert mode.
                            if vim.is_insert() {
                                self.update_slash_suggestions(&mut ui_state);
                            }
                        }
                        Event::Mouse(mouse) => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => ui_state.messages.scroll_up(3),
                                MouseEventKind::ScrollDown => ui_state.messages.scroll_down_by(3),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }

                () = tokio::time::sleep(Duration::from_millis(100)), if thinking => {
                    self.drain_agent_observable_events();
                }

                Some(engine_event) = async {
                    event_rx.as_mut().unwrap().recv().await
                }, if event_rx.is_some() => {
                    self.record_engine_event_telemetry(&engine_event);
                    match engine_event {
                        EngineEvent::TextDelta(delta) => {
                            phase.note_text_delta();
                            set_plan_step(&mut ui_state, "Thinking through response");
                            if let Some(last) = ui_state
                                .messages
                                .messages
                                .iter_mut()
                                .rev()
                                .find(|msg| msg.role == MessageRole::Assistant)
                            {
                                let mut content = last.content.to_string();
                                content.push_str(&delta);
                                last.content = content.into();
                                last.streaming = true;
                            }
                            if last_stream_snapshot.elapsed() >= Duration::from_secs(1) {
                                self.persist_interactive_snapshot(&ui_state);
                                last_stream_snapshot = Instant::now();
                            }
                        }
                        EngineEvent::ToolUseSummary { id, summary, .. } => {
                            // Render with the matching `ToolCallStart`, which
                            // also carries the summary. Holding it here keeps
                            // the two events from producing duplicate rows
                            // while still letting this event drive the label.
                            self.tool_use_summaries.insert(id, summary);
                        }
                        EngineEvent::ToolCallStart { id, name, summary } => {
                            let summary = self.tool_use_summaries.remove(&id).unwrap_or(summary);
                            phase.note_tool_start(&summary);
                            set_plan_step(&mut ui_state, &summary);
                            push_activity_row(&mut ui_state, &summary);
                            ui_state.messages.push(Message {
                                role: MessageRole::ToolCall,
                                content: summary.into(),
                                tool_name: Some(name.into()),
                                streaming: false,
                                folded: false,
                            });
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::ToolCallEnd { id, result } => {
                            // A call blocked before its start event never
                            // consumed its label; drop it here.
                            self.tool_use_summaries.remove(&id);
                            phase.note_tool_end();
                            complete_plan(&mut ui_state);
                            pending_confirmation = None;
                            clear_permission_overlay(&mut ui_state);
                            let content = if result.is_error {
                                ui_state.notifications.err("Tool failed");
                                format!("Error: {}", result.output)
                            } else {
                                ui_state.notifications.success("Tool completed");
                                result.output
                            };
                            ui_state.messages.push(Message {
                                role: MessageRole::ToolResult,
                                content: content.into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::PermissionRequest { tool, args, .. } => {
                            let label =
                                permission_activity_label(&ToolCall::summarize_activity(&tool, &args));
                            phase.note_permission_request(&tool, &args);
                            set_plan_step(&mut ui_state, &label);
                            push_activity_row(&mut ui_state, &label);
                            ui_state.permission_bar.set_pending(label.clone());
                            sync_permission_overlay(
                                &mut ui_state,
                                &tool,
                                &args.to_string(),
                                "Awaiting approval",
                                true,
                            );
                            ui_state.notifications.warn(label);
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::AgentEvent { agent_id, event } => {
                            apply_agent_event(&mut ui_state, &agent_id, &event);
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::ModUi(elements) => {
                            // A mod rendered UI during a hook. The tree is a
                            // snapshot, so it renders here without touching
                            // the mod runtime. It belongs in the transcript,
                            // not on stdout interleaved with the turn.
                            let rendered = coderex_ui::mod_elements::render_mod_ui_text(
                                &elements,
                                coderex_ui::mod_elements::MOD_UI_DEFAULT_WIDTH,
                            );
                            if rendered.is_empty() {
                                // An empty tree clears what was focusable: there
                                // is no block to route input to.
                                mod_ui_focus = ModUiFocus::default();
                            } else {
                                ui_state.messages.push(Message {
                                    role: MessageRole::System,
                                    content: rendered.into(),
                                    tool_name: None,
                                    streaming: false,
                                    folded: false,
                                });
                                let index = ui_state.messages.messages.len().saturating_sub(1);
                                mod_ui_focus.reset(&elements, index);
                                self.persist_interactive_snapshot(&ui_state);
                            }
                        }
                        EngineEvent::PermissionExplanation { tool, explanation } => {
                            if let Some(detail) = ui_state.permission_detail.as_mut() {
                                if detail.tool.eq_ignore_ascii_case(&tool) {
                                    detail.risk_summary.clone_from(&explanation);
                                }
                            }
                            if matches!(
                                ui_state.overlays.top(),
                                Some(Overlay::PermissionDialog { .. })
                            ) {
                                let args = ui_state
                                    .permission_detail
                                    .as_ref()
                                    .map(|detail| detail.args.clone())
                                    .unwrap_or_default();
                                sync_permission_overlay(
                                    &mut ui_state,
                                    &tool,
                                    &args,
                                    &explanation,
                                    true,
                                );
                            }
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        _ => {}
                    }
                }

                Some(request) = async {
                    confirmation_rx.as_mut().unwrap().recv().await
                }, if confirmation_rx.is_some() => {
                    let args = permission_request_args(&request.arguments);
                    let label =
                        permission_activity_label(&ToolCall::summarize_activity(&request.tool_name, &args));
                    phase.note_permission_request(&request.tool_name, &args);
                    set_plan_step(&mut ui_state, &label);
                    push_activity_row(&mut ui_state, &label);
                    ui_state.permission_bar.set_pending(label.clone());
                    sync_permission_overlay(
                        &mut ui_state,
                        &request.tool_name,
                        &request.arguments,
                        &request.reason,
                        request.allow_session,
                    );
                    ui_state.notifications.warn(&label);
                    pending_confirmation = Some(request);
                    self.persist_interactive_snapshot(&ui_state);
                }

                Some((engine, result)) = async {
                    query_done_rx.as_mut().unwrap().await.ok()
                }, if query_done_rx.is_some()
                    // Drain the turn's events before deciding its answer was
                    // empty: a tool-only turn has no assistant text, and its
                    // ToolCall rows may still be queued. Choosing this branch
                    // first reported a healthy tool turn as "no visible
                    // response".
                    && event_rx.as_ref().is_none_or(tokio::sync::mpsc::UnboundedReceiver::is_empty) => {
                    self.engine = Some(engine);
                    // The turn may have run `SessionStart` for the first time;
                    // its `watchPaths` are known now, so add them to the live
                    // watcher.
                    #[cfg(feature = "hot-reload")]
                    self.sync_hook_watch_paths();
                    self.running_query = None;
                    query_done_rx = None;
                    elicit_rx = None;
                    tui_elicit_rx = None;
                    self.pending_elicitation = None;
                    self.record_turn_telemetry(&result);
                    self.pending_mcp_form = None;
                    pending_confirmation = None;
                    thinking = false;
                    phase.reset();
                    complete_plan(&mut ui_state);
                    clear_permission_overlay(&mut ui_state);

                    if let Some(last) = ui_state
                        .messages
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|msg| msg.role == MessageRole::Assistant && msg.streaming)
                    {
                        last.streaming = false;
                    }

                    match result {
                        Ok(TerminalReason::ApiUnavailable { message }) => {
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: format!(
                                    "The AI service is temporarily unavailable. Your conversation context is preserved. Please try again later.\n({message})"
                                ).into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                        }
                        Ok(_) => {
                            let turn_start = ui_state
                                .messages
                                .messages
                                .iter()
                                .rposition(|msg| msg.role == MessageRole::User)
                                .unwrap_or(0);
                            let turn_slice = &ui_state.messages.messages[turn_start..];
                            let assistant_empty = turn_slice
                                .iter()
                                .rev()
                                .find(|msg| msg.role == MessageRole::Assistant)
                                .is_some_and(|msg| msg.content.trim().is_empty());
                            let had_tool_activity = turn_slice.iter().any(|msg| {
                                matches!(
                                    msg.role,
                                    MessageRole::ToolCall | MessageRole::ToolResult
                                )
                            });
                            if assistant_empty && !had_tool_activity {
                                let message =
                                    "Query failed: provider returned no visible response.".to_string();
                                ui_state.notifications.err(&message);
                                ui_state.messages.push(Message {
                                    role: MessageRole::System,
                                    content: message.into(),
                                    tool_name: None,
                                    streaming: false,
                                    folded: false,
                                });
                            }
                            ui_state.notifications.success("Turn completed");
                            // Trigger background features after successful turn
                            let prompt_text = self.history.last().cloned().unwrap_or_default();
                            self.trigger_background_features(&prompt_text);
                        }
                        Err(e) => {
                            ui_state.notifications.err("Turn failed");
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: format!("Query failed: {e}").into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                            show_detail_overlay(
                                &mut ui_state,
                                "Failure Details",
                                format!("Query failed: {e}"),
                            );
                        }
                    }

                    self.persist_interactive_snapshot(&ui_state);

                    if let Some(next_prompt) = queued_prompts.pop_front() {
                            self.start_tui_query(
                                next_prompt,
                                &mut ui_state,
                                &mut thinking,
                                &mut phase,
                                &mut query_done_rx,
                                &mut elicit_rx,
                                &mut tui_elicit_rx,
                            );
                    }
                }

                Some((job_id, prompt)) = async {
                    self.cron_rx.as_mut().unwrap().recv().await
                }, if self.cron_rx.is_some() => {
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: format!("[Cron {job_id}] {prompt}").into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                    if let Some(scheduler) =
                        self.dispatcher.as_ref().and_then(|d| d.cron_scheduler())
                    {
                        scheduler.mark_fired(&job_id);
                    }
                }

                Some(request) = async {
                    elicit_rx.as_mut().unwrap().recv().await
                }, if elicit_rx.is_some() => {
                    for (i, q) in request.questions.iter().enumerate() {
                        let text = q.get("question").and_then(|v| v.as_str()).unwrap_or("?");
                        ui_state.messages.push(Message {
                            role: MessageRole::System,
                            content: format!("[Question {}] {}", i + 1, text).into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
                            for opt in options {
                                if let (Some(label), Some(desc)) = (
                                    opt.get("label").and_then(|v| v.as_str()),
                                    opt.get("description").and_then(|v| v.as_str()),
                                ) {
                                    ui_state.messages.push(Message {
                                        role: MessageRole::System,
                                        content: format!("  {label} — {desc}").into(),
                                        tool_name: None,
                                        streaming: false,
                                        folded: false,
                                    });
                                }
                            }
                        }
                    }
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: "Type your answer and press Enter.".into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                    self.pending_elicitation = Some(request.response_tx);
                }

                Some(request) = async {
                    tui_elicit_rx.as_mut().unwrap().recv().await
                }, if tui_elicit_rx.is_some() => {
                    thinking = false;
                    phase.reset();
                    if let Some(last) = ui_state
                        .messages
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|msg| msg.role == MessageRole::Assistant && msg.streaming)
                    {
                        last.streaming = false;
                    }
                    ui_state.notifications.info(&request.prompt);
                    self.pending_elicitation = Some(request.response_tx);
                }

                // A server is blocked inside its tool call until this is
                // answered, so the turn stays running; only the prompt and
                // the answer path change.
                Some(request) = mcp_elicit_rx.recv(), if mcp_elicitation_active => {
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: format!("[MCP question] {}", request.prompt).into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });

                    if let Some(form) = request.form {
                        // Show the whole form up front: every field, with
                        // required markers and defaults, so the person can see
                        // what the server is asking for before typing.
                        for field in &form.fields {
                            let marker = if field.required { "*" } else { " " };
                            let mut line = format!(
                                "  {marker} {} ({})",
                                field.label(),
                                field_type_label(field)
                            );
                            if let Some(default) = &field.default {
                                let _ = write!(line, " [default: {default}]");
                            }
                            if let Some(description) = &field.description {
                                let _ = write!(line, " — {description}");
                            }
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: line.into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                        }
                        ui_state.messages.push(Message {
                            role: MessageRole::System,
                            content:
                                "Up/Down or Tab moves between fields; Enter submits; Esc cancels."
                                    .into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        let state = crate::mcp_elicitation::McpFormState::new(form);
                        ui_state.notifications.info(state.focus_hint());
                        self.pending_mcp_form = Some(state);
                    } else {
                        self.pending_mcp_form = None;
                        ui_state
                            .notifications
                            .info("An MCP server is waiting for an answer");
                    }
                    self.pending_elicitation = Some(request.response_tx);
                }

                // A watched file changed. This main loop drains the shared
                // watcher; a headless run uses its own task for the duration
                // of the turn. Each event names the path and the kind.
                changed = async {
                    match config_changed_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if config_changed_rx.is_some() => {
                    match changed {
                        Some(batch) => self.fire_file_changed_hook(&batch).await,
                        // The watcher (and its sender) is gone. Drop the
                        // closed receiver so the branch stops polling; a
                        // ready `None` would otherwise spin the loop.
                        None => config_changed_rx = None,
                    }
                }

                else => break Ok(()),
            }
        };

        // Cleanup
        if let Some(ref handler) = self.mcp_elicitation {
            // The event loop is going away; later server questions must not
            // wait on a channel nobody is reading.
            handler.clear_tui_sender();
        }
        let _ = terminal_ui.exit();
        let _ = disable_raw_mode();
        self.event_rx = event_rx;
        self.stop_artifact_serving().await;

        result
    }

    /// Run a single query in non-interactive (headless) mode.
    ///
    /// A headless session can publish artifacts just like an interactive one,
    /// so the local origin is started before the turn and shut down after it.
    /// A failure to serve is logged by [`Self::start_artifact_serving`] and
    /// does not stop the turn.
    pub async fn run_headless(&mut self, prompt: &str) -> Result<String> {
        if let Some(error) = self.setup_error.as_deref() {
            return Err(anyhow::anyhow!("{error}"));
        }

        self.start_artifact_serving().await;
        // Headless starts used to leave a previous session's stale "running"
        // records alone. Settle them and report on stderr: stdout carries the
        // answer (or the protocol stream) and must not gain a banner.
        for notification in self.background_job_notifications() {
            tracing::info!("{notification}");
            eprintln!("{notification}");
        }
        if let Some(monitors) = self.plugin_monitors.clone() {
            match crate::plugin_monitors::PluginMonitorSupervisor::start(
                &monitors,
                std::path::Path::new(&self.cwd),
            ) {
                Ok(supervisor) => self.monitor_supervisor = Some(supervisor),
                Err(error) => tracing::warn!(%error, "plugin monitors were not started"),
            }
        }
        let result = self.run_headless_turn(prompt).await;
        // Headless runs used to leave no transcript behind, so `--print`
        // sessions could not be resumed or found by `/resume`. Persist what
        // the turn produced; slash-command-only runs are covered too.
        self.persist_headless_session();
        if let Some(mut supervisor) = self.monitor_supervisor.take() {
            supervisor.stop().await;
        }
        self.stop_artifact_serving().await;
        result
    }

    /// Write the transcript and index entry for a headless session.
    fn persist_headless_session(&self) {
        if self.no_persistence {
            return;
        }
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let session = engine.session();
        if session.messages.is_empty() {
            return;
        }
        let session_id = session.session_id.to_string();
        let store = coderex_memory::transcript::TranscriptStore::new(
            session_resume::transcript_dir(&self.cwd),
        );
        if let Err(error) = store.rewrite(&session_id, &session.messages) {
            tracing::warn!(%error, "headless transcript was not written");
            return;
        }
        session_resume::index_saved_session(
            &self.cwd,
            &session_id,
            session.title.as_deref(),
            &session.messages,
        );
        session_resume::index_session_usage(
            &self.cwd,
            &session_id,
            session.cost_usd,
            i64::try_from(
                session.usage.input_tokens
                    + session.usage.output_tokens
                    + session.usage.cache_read_tokens
                    + session.usage.cache_creation_tokens,
            )
            .unwrap_or(i64::MAX),
        );
    }

    async fn run_headless_turn(&mut self, prompt: &str) -> Result<String> {
        if prompt.starts_with('/') {
            // Slash commands go through the same dispatch the interactive
            // non-TUI path uses. That dispatch owns the built-ins which change
            // the session — `/clear` clears, `/compact` compacts, `/model`
            // switches — and keeping a second, shorter copy of that list here
            // is how headless came to report commands it never actually ran.
            let mut buffer: Vec<u8> = Vec::new();
            self.execute_command(prompt, &mut buffer).await?;
            return Ok(String::from_utf8_lossy(&buffer).into_owned());
        }

        if self.engine.is_none() {
            return Err(anyhow::anyhow!(
                "No QueryEngine configured. Set ANTHROPIC_API_KEY or run `coderex --api-key <key>`"
            ));
        }

        // A headless run has no event loop to drain the shared watcher, so it
        // starts one for exactly this turn and stops it before returning.
        // `SessionStart` is fired first: its `watchPaths` have to be known
        // before the watcher starts, and the turn's own lazy call then no-ops.
        #[cfg(feature = "hot-reload")]
        let watcher = self.spawn_headless_file_watcher().await;

        // Route to the QueryEngine rather than a placeholder.
        let output = self.submit_prompt(prompt, true).await;

        #[cfg(feature = "hot-reload")]
        if let Some(watcher) = watcher {
            watcher.stop().await;
        }

        Ok(output)
    }

    /// Drain pending events from the event channel, appending text deltas to
    /// the output.
    fn drain_events(&mut self, output: &mut String) {
        // Collect first: recording needs `&mut self`, while the receiver
        // borrows `self.event_rx`.
        let events: Vec<EngineEvent> = self
            .event_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();
        for event in events {
            self.record_engine_event_telemetry(&event);
            match event {
                EngineEvent::TextDelta(delta) => output.push_str(&delta),
                EngineEvent::ModUi(elements) => append_mod_ui_block(output, &elements),
                _ => {}
            }
        }
        self.drain_agent_observable_events();
    }
}

/// Append a rendered mod UI block to a headless turn's output.
///
/// Headless runs have no transcript pane; the block joins the turn's output
/// rather than being printed live from the event loop. A blank line separates
/// it from model text when the preceding output did not end with one.
fn append_mod_ui_block(output: &mut String, elements: &[coderex_core::ui_protocol::UiElement]) {
    let rendered = coderex_ui::mod_elements::render_mod_ui_text(
        elements,
        coderex_ui::mod_elements::MOD_UI_DEFAULT_WIDTH,
    );
    if rendered.is_empty() {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&rendered);
    output.push('\n');
}

/// What the terminal does with a key while a mod UI button is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModUiKeyAction {
    FocusNext,
    FocusPrevious,
    Activate,
    ClearFocus,
}

/// Map a key to a mod UI action.
///
/// Ordinary typing returns `None`, so every character still reaches the input
/// line. With buttons present, `Tab` starts focus on the first button; once a
/// button is focused, `Tab`/`Shift+Tab` cycle, `Enter` activates and `Esc`
/// clears. While nothing is focused, `Shift+Tab` is left to the permission-mode
/// binding and `Enter` to the normal submit path.
fn mod_ui_key_action(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    focused: Option<usize>,
    button_count: usize,
) -> Option<ModUiKeyAction> {
    use crossterm::event::{KeyCode, KeyModifiers};

    if button_count == 0 {
        return None;
    }
    let shift_tab = matches!(code, KeyCode::BackTab)
        || (matches!(code, KeyCode::Tab) && modifiers.contains(KeyModifiers::SHIFT));
    match code {
        KeyCode::BackTab | KeyCode::Tab if shift_tab => {
            focused.is_some().then_some(ModUiKeyAction::FocusPrevious)
        }
        KeyCode::Tab => Some(ModUiKeyAction::FocusNext),
        KeyCode::Enter => focused.is_some().then_some(ModUiKeyAction::Activate),
        KeyCode::Esc => focused.is_some().then_some(ModUiKeyAction::ClearFocus),
        _ => None,
    }
}

/// The next focused index after cycling around `count` buttons.
///
/// `None` starts at the first button going forward and the last going
/// backwards. Callers guard against `count == 0`.
fn cycle_mod_ui_focus(current: Option<usize>, count: usize, forward: bool) -> usize {
    debug_assert!(count > 0);
    match (current, forward) {
        (None, true) => 0,
        (None, false) => count - 1,
        (Some(index), true) => (index + 1) % count,
        (Some(index), false) => (index + count - 1) % count,
    }
}

/// The button ids in one mod UI tree, in render order.
fn collect_button_ids(elements: &[coderex_core::ui_protocol::UiElement]) -> Vec<String> {
    fn walk(elements: &[coderex_core::ui_protocol::UiElement], ids: &mut Vec<String>) {
        for element in elements {
            match element {
                coderex_core::ui_protocol::UiElement::Button { id, .. } => ids.push(id.clone()),
                coderex_core::ui_protocol::UiElement::Box { children, .. } => walk(children, ids),
                coderex_core::ui_protocol::UiElement::Text { .. } => {}
            }
        }
    }

    let mut ids = Vec::new();
    walk(elements, &mut ids);
    ids
}

/// Which button in the most recent mod UI block is focused.
///
/// Focus is terminal state, not part of the host-owned tree, and it lives with
/// the interactive loop. The tree and the transcript index are kept together
/// so a focus change can redraw exactly the message the tree came from.
#[derive(Debug, Default)]
struct ModUiFocus {
    elements: Vec<coderex_core::ui_protocol::UiElement>,
    button_ids: Vec<String>,
    focused: Option<usize>,
    message_index: Option<usize>,
    /// The block's last rendered transcript text, used to confirm that the
    /// message at `message_index` is still this block after a `/clear` or a
    /// session restore.
    rendered: Option<String>,
}

impl ModUiFocus {
    /// Adopt a newly rendered block, with no button focused yet.
    fn reset(&mut self, elements: &[coderex_core::ui_protocol::UiElement], message_index: usize) {
        self.elements = elements.to_vec();
        self.button_ids = collect_button_ids(elements);
        self.focused = None;
        self.message_index = (!self.button_ids.is_empty()).then_some(message_index);
        self.rendered = Some(coderex_ui::mod_elements::render_mod_ui_text(
            elements,
            coderex_ui::mod_elements::MOD_UI_DEFAULT_WIDTH,
        ));
    }

    /// Drop focus without forgetting the block.
    const fn clear_focus(&mut self) {
        self.focused = None;
    }

    /// The id of the focused button, if any.
    fn focused_id(&self) -> Option<&str> {
        self.focused
            .and_then(|index| self.button_ids.get(index))
            .map(String::as_str)
    }

    /// Move focus forward or backward around the buttons.
    fn cycle(&mut self, forward: bool) {
        if self.button_ids.is_empty() {
            self.focused = None;
            return;
        }
        self.focused = Some(cycle_mod_ui_focus(
            self.focused,
            self.button_ids.len(),
            forward,
        ));
    }

    /// The number of focusable buttons.
    fn button_count(&self) -> usize {
        self.button_ids.len()
    }
}

impl ReplLoop {
    /// Redraw the transcript block that owns the focused mod UI button.
    ///
    /// The transcript stores rendered text, so focus is applied by rendering
    /// the snapshot again rather than by mutating a live widget.
    fn redraw_mod_ui_focus(
        &self,
        ui_state: &mut coderex_ui::state::UiState,
        focus: &mut ModUiFocus,
    ) {
        let Some(index) = focus.message_index else {
            return;
        };
        let Some(message) = ui_state.messages.messages.get_mut(index) else {
            focus.message_index = None;
            focus.focused = None;
            focus.rendered = None;
            return;
        };
        // The transcript index is only meaningful while the message still
        // holds the block this focus belongs to. A `/clear`, a resume or a
        // rewind can replace it; in that case forget the block rather than
        // rewrite an unrelated message.
        if focus.rendered.as_deref() != Some(message.content.as_str()) {
            focus.message_index = None;
            focus.focused = None;
            focus.rendered = None;
            return;
        }
        let rendered = coderex_ui::mod_elements::render_mod_ui_text_with_focus(
            &focus.elements,
            coderex_ui::mod_elements::MOD_UI_DEFAULT_WIDTH,
            focus.focused_id(),
        );
        message.content = rendered.clone().into();
        focus.rendered = Some(rendered);
        self.persist_interactive_snapshot(ui_state);
    }

    /// Record a tool event for telemetry, when the session opted in.
    fn record_engine_event_telemetry(&mut self, event: &EngineEvent) {
        let Some(handle) = self.telemetry.as_ref() else {
            return;
        };
        match event {
            EngineEvent::ToolCallStart { id, name, .. } => {
                self.tool_starts
                    .insert(id.clone(), (name.clone(), std::time::Instant::now()));
            }
            EngineEvent::ToolCallEnd { id, result } => {
                if let Some((tool_name, started)) = self.tool_starts.remove(id) {
                    handle.record(coderex_telemetry::TelemetryEvent::ToolUsed {
                        tool_name,
                        success: !result.is_error,
                        duration_ms: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        timestamp: chrono::Utc::now(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Record per-turn API usage and terminal errors.
    fn record_turn_telemetry(&mut self, result: &Result<TerminalReason, String>) {
        let Some(ref handle) = self.telemetry else {
            return;
        };
        if let Some(engine) = self.engine.as_ref() {
            let usage = engine.total_usage();
            let tokens_in = usage
                .input_tokens
                .saturating_sub(self.last_usage.input_tokens);
            let tokens_out = usage
                .output_tokens
                .saturating_sub(self.last_usage.output_tokens);
            if tokens_in > 0 || tokens_out > 0 {
                let provider = if self.model.0.starts_with("claude") {
                    "anthropic"
                } else {
                    "custom"
                };
                handle.record(coderex_telemetry::TelemetryEvent::ApiCall {
                    provider: provider.to_string(),
                    model: self.model.0.clone(),
                    tokens_in: u32::try_from(tokens_in).unwrap_or(u32::MAX),
                    tokens_out: u32::try_from(tokens_out).unwrap_or(u32::MAX),
                    timestamp: chrono::Utc::now(),
                });
            }
            self.last_usage = usage;
        }

        let (error_type, message) = match result {
            Ok(TerminalReason::Error(message)) => ("turn_error", message.clone()),
            Ok(TerminalReason::ApiUnavailable { message }) => ("api_unavailable", message.clone()),
            Err(message) => ("fatal", message.clone()),
            _ => return,
        };
        handle.record(coderex_telemetry::TelemetryEvent::Error {
            error_type: error_type.to_string(),
            message,
            timestamp: chrono::Utc::now(),
        });
    }

    /// Drain the agent manager's broadcast of sub-agent events.
    ///
    /// The UI already renders sub-agent lifecycle through
    /// [`EngineEvent::AgentEvent`]; this subscription is the separate surface
    /// for external observers (and what `subscribe_to_agent` is for). Keeping
    /// it drained prevents the broadcast buffer from growing and records
    /// sub-agent activity in debug logs.
    fn drain_agent_observable_events(&mut self) {
        let Some(rx) = self.agent_observable_rx.as_mut() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(observed) => tracing::debug!(
                    agent_id = %observed.agent_id,
                    event = ?observed.event,
                    "sub-agent observable event"
                ),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "sub-agent event stream lagged");
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    self.agent_observable_rx = None;
                    break;
                }
            }
        }
    }

    /// Format a headless turn's final output.
    ///
    /// Headless output is exactly what the model produced. A turn that did not
    /// complete normally gets a status line appended so the reason is not lost,
    /// but nothing here reads a file named in the prompt or rewrites the
    /// model's bytes: a prompt that says "read src/lib.rs" must not turn into
    /// file content the model never emitted.
    fn headless_final_output(output: String, reason: &TerminalReason) -> String {
        if output.is_empty() {
            format!("Query completed ({reason:?})")
        } else if !matches!(reason, TerminalReason::Completed) {
            format!("{output}\nQuery completed ({reason:?})")
        } else {
            output
        }
    }

    /// Build a QueryRequest for the main agent from current session effects.
    fn build_query_request_for_main_agent(&self) -> QueryRequest {
        QueryRequest {
            kind: QueryKind::MainAgent,
            user_prompt: String::new(), // filled at submit time
            // If override is set, it replaces the base prompt; otherwise None
            custom_system_prompt: self.session_effects.override_system_prompt.clone(),
            append_system_prompt: self.session_effects.append_system_prompt.clone(),
            prompt_profile: self
                .session_effects
                .prompt_profiles
                .last()
                .copied()
                .unwrap_or_default(),
            allowed_tools: self.session_effects.allowed_tools_override.clone(),
            model_profile: ModelProfile::Main,
            inherit_session: true,
            stream: true,
            max_tokens: 4096,
            agent_id: None,
            provider_hints: HashMap::new(),
        }
    }

    /// Build SubmitOptions incorporating active command effects.
    ///
    /// Routes through the Orchestrator so that prompt profiles, model selection,
    /// and tool whitelists are applied consistently with side/forked queries.
    fn build_submit_options(&mut self) -> SubmitOptions {
        let request = self.build_query_request_for_main_agent();
        let mut options = if let Some(ref orchestrator) = self.orchestrator {
            orchestrator.build_submit_options(&request)
        } else {
            // Fallback when no orchestrator is available
            let mut opts = SubmitOptions::default();
            if let Some(ref override_prompt) = self.session_effects.override_system_prompt {
                // Override takes precedence over profile-based composition
                opts.custom_system_prompt = Some(override_prompt.clone());
            } else if !self.session_effects.prompt_profiles.is_empty() {
                opts.custom_system_prompt = Some(prompts::compose_system_prompt_for_profiles(
                    &self.session_effects.prompt_profiles,
                ));
            }
            if let Some(ref prompt) = self.session_effects.append_system_prompt {
                opts.append_system_prompt = Some(prompt.clone());
            }
            if let Some(ref tools) = self.session_effects.allowed_tools_override {
                opts.allowed_tools_override = Some(tools.clone());
            } else if let Some(default_tools) =
                prompts::default_allowed_tools_for_profiles(&self.session_effects.prompt_profiles)
            {
                opts.allowed_tools_override = Some(default_tools);
            }
            opts
        };

        // REPL-specific: extra context from commands is not part of QueryRequest
        if !self.session_effects.extra_context.is_empty() {
            options.replay_user_messages = Some(
                self.session_effects
                    .extra_context
                    .iter()
                    .map(|ctx| Message::user(format!("Additional context: {ctx}")))
                    .collect(),
            );
        }

        // Apply CLI flag overrides
        if let Some(ref schema) = self.json_schema {
            let schema_val: Result<serde_json::Value, _> =
                serde_json::from_str::<serde_json::Value>(schema).or_else(|_| {
                    let contents = std::fs::read_to_string(schema)
                        .map_err(|_| serde_json::from_str::<serde_json::Value>("").unwrap_err())?;
                    serde_json::from_str(&contents)
                });
            if let Ok(val) = schema_val {
                options.json_schema = Some(val.clone());
                // Configure the engine's structured output validator
                if let Some(ref mut engine) = self.engine {
                    engine.set_structured_output(val);
                }
            }
        }
        if let Some(budget_usd) = self.max_budget_usd {
            if budget_usd > 0.0 {
                // Convert USD to tokens using a conservative rate ($3 per 1M tokens)
                // `budget_usd > 0.0` and `.max(1.0)` guarantee a positive value,
                // and the cast saturates for very large budgets, so this is safe.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let tokens = ((budget_usd / 3.0) * 1_000_000.0).max(1.0) as u64;
                options.task_budget_tokens = tokens;
            }
        }
        if let Some(turns) = self.max_turns {
            options.max_turns = turns;
        }
        options.include_partial_messages = self.include_partial_messages;
        // In the interactive REPL, assistant clarification questions should stay in the
        // transcript as normal messages. The engine's fallback elicitation prompt writes to
        // stderr/stdin directly, which breaks the TUI and duplicates the normal next-turn flow.
        options.handle_elicitation = false;

        // Refresh the snapshot the Agent tool reads when it forks a
        // sub-agent. This runs immediately before the turn is submitted, so a
        // delegation inside the turn reuses the parent's prefix as it was
        // when the turn began.
        if let Some(ref engine) = self.engine {
            let messages: Vec<serde_json::Value> = engine
                .session()
                .messages
                .iter()
                .filter_map(|message| serde_json::to_value(message).ok())
                .collect();
            let tool_names = self
                .dispatcher
                .as_ref()
                .map(|dispatcher| {
                    dispatcher
                        .list_tools()
                        .into_iter()
                        .map(|tool| tool.name)
                        .collect()
                })
                .unwrap_or_default();
            let snapshot = ParentContextSnapshot::new(
                messages,
                self.model.0.clone(),
                options.custom_system_prompt.clone().unwrap_or_default(),
                tool_names,
            );
            *self.parent_context_snapshot.lock().unwrap() = Some(snapshot);
        }

        options
    }

    /// Trigger background features after a successful turn.
    ///
    /// Tasks are tracked in `self.background_tasks` with cancellation tokens.
    /// Results are delivered asynchronously via `self.background_results_rx`.
    fn trigger_background_features(&mut self, _user_prompt: &str) {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return,
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return,
        };

        self.cleanup_finished_background_tasks();

        // Trigger prompt suggestions after the user has sent a few messages
        if self.history.len() >= 3 && self.history.len() % 5 == 0 {
            let conversation_summary = self
                .history
                .iter()
                .rev()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let request = BackgroundFeatureRequest {
                kind: BackgroundFeatureKind::PromptSuggestion,
                system_prompt: "You are a prompt suggestion assistant.".into(),
                user_prompt: format!(
                    "Suggest 1-2 brief follow-up prompts based on this conversation context.\n\
                     Recent messages:\n{conversation_summary}\n\
                     Return only the suggestions, one per line."
                ),
                parent_context: None,
                allowed_tools: None,
                max_tokens: 128,
                writable: false,
            };
            self.spawn_background_task(
                "prompt_suggestion",
                BackgroundFeatureKind::PromptSuggestion,
                client,
                request,
                tx,
            );
        }
    }

    /// Spawn a tracked background task with cancellation support.
    fn spawn_background_task(
        &mut self,
        id: &str,
        kind: BackgroundFeatureKind,
        client: Box<dyn ApiClient>,
        request: BackgroundFeatureRequest,
        result_tx: mpsc::UnboundedSender<BackgroundTaskResult>,
    ) {
        let cancel_token = CancellationToken::new();
        let task_id = id.to_string();
        let task_cancel = cancel_token.clone();

        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind,
            cancel_token,
        });

        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = coderex_core::orchestrator::run_background_feature(
                    client.as_ref(), request
                ) => res,
            };

            let (success, content) = match result {
                Ok(r) => (true, r.content),
                Err(e) => (false, e.to_string()),
            };

            let _ = result_tx.send(BackgroundTaskResult {
                id: task_id,
                kind,
                success,
                content,
            });
        });
    }

    /// Cancel all running background tasks.
    fn cancel_background_tasks(&mut self) {
        for task in &self.background_tasks {
            task.cancel_token.cancel();
            tracing::info!("Cancelled background task: {} ({:?})", task.id, task.kind);
        }
        self.background_tasks.clear();
    }

    /// Remove finished (cancelled or completed) background tasks from tracking.
    fn cleanup_finished_background_tasks(&mut self) {
        self.background_tasks
            .retain(|t| !t.cancel_token.is_cancelled());
    }

    /// Poll for completed background task results and return them.
    fn poll_background_results(&mut self) -> Vec<BackgroundTaskResult> {
        let mut results = Vec::new();
        if let Some(ref mut rx) = self.background_results_rx {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        results
    }

    /// Run agent generation synchronously and return the result.
    async fn run_agent_generation(&self, description: &str) -> String {
        let Some(ref client) = self.api_client else {
            return "No API client configured.".into();
        };
        match coderex_core::prompts::generate_agent(client.as_ref(), description).await {
            Ok(spec) => format!(
                "Agent Generated\n──────────────\nName: {}\nDescription: {}\nModel: {}\nAllowed Tools: {}\n\nSystem Prompt:\n{}",
                spec.name,
                spec.description,
                spec.model_profile,
                spec.allowed_tools
                    .as_ref()
                    .map_or_else(|| "all".into(), |t| t.join(", ")),
                spec.system_prompt
            ),
            Err(e) => format!("Agent generation failed: {e}"),
        }
    }

    /// Compact the session context by truncating older messages.
    /// Keeps the first user message and the most recent 3 turns.
    #[allow(dead_code)]
    fn compact_context(&mut self, ui_state: &mut coderex_ui::state::UiState) -> String {
        let Some(engine) = self.engine.as_mut() else {
            return "No engine configured.".into();
        };
        let result = engine.compact_session_messages(3);
        if result.tokens_removed == 0 {
            return "Context is already compact.".into();
        }
        // Sync UI message list: rebuild from compacted messages
        restore_transcript_messages(ui_state, &result.messages);
        self.persist_interactive_snapshot(ui_state);
        format!(
            "Context compacted: {} tokens removed, {} messages remaining.",
            result.tokens_removed,
            result.messages.len()
        )
    }

    /// Compact context in non-TUI mode.
    fn compact_context_non_tui(&mut self) -> String {
        let Some(ref mut engine) = self.engine else {
            return "No engine configured.".into();
        };
        let result = engine.compact_session_messages(3);
        if result.tokens_removed == 0 {
            return "Context is already compact.".into();
        }
        format!(
            "Context compacted: {} tokens removed, {} messages remaining.",
            result.tokens_removed,
            result.messages.len()
        )
    }

    /// Spawn a background task that updates session memory via ai_workflow.
    fn spawn_memory_update_task(&mut self) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };
        let store = match &self.session_memory_store {
            Some(s) => Arc::clone(s),
            None => return "Session memory store not available.".into(),
        };
        let Some(session_id) = self.session_id else {
            return "Session ID not available.".into();
        };
        let conversation = self
            .history
            .iter()
            .rev()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let current_memory = store
            .get_facts(session_id)
            .into_iter()
            .map(|f| f.fact)
            .collect::<Vec<_>>()
            .join("\n");

        self.cleanup_finished_background_tasks();
        let task_id = format!("memory_update_{}", self.background_tasks.len());
        let cancel_token = CancellationToken::new();
        let task_cancel = cancel_token.clone();
        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind: BackgroundFeatureKind::SessionMemoryUpdate,
            cancel_token,
        });

        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = ai_workflow::update_session_memory_ai(
                    client.as_ref(), &store, session_id, &current_memory, &conversation
                ) => res.map_err(|e| coderex_core::orchestrator::OrchestratorError::Config(e.to_string())),
            };
            let (success, content) = match result {
                Ok(text) => (true, text),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(BackgroundTaskResult {
                id: task_id_clone,
                kind: BackgroundFeatureKind::SessionMemoryUpdate,
                success,
                content,
            });
        });
        format!("Background task started: {task_id}")
    }

    /// Spawn a background task that extracts long-term memories via ai_workflow.
    fn spawn_memory_extraction_task(&mut self) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };
        let store = match &self.long_term_store {
            Some(s) => Arc::clone(s),
            None => return "Long-term memory store not available.".into(),
        };
        let conversation = self
            .history
            .iter()
            .rev()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");

        self.cleanup_finished_background_tasks();
        let task_id = format!("memory_extraction_{}", self.background_tasks.len());
        let cancel_token = CancellationToken::new();
        let task_cancel = cancel_token.clone();
        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind: BackgroundFeatureKind::MemoryExtraction,
            cancel_token,
        });

        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = ai_workflow::extract_long_term_memories_ai(
                    client.as_ref(), &store, &conversation
                ) => res.map_err(|e| coderex_core::orchestrator::OrchestratorError::Config(e.to_string())),
            };
            let (success, content) = match result {
                Ok(topics) => (true, topics.join(", ")),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(BackgroundTaskResult {
                id: task_id_clone,
                kind: BackgroundFeatureKind::MemoryExtraction,
                success,
                content,
            });
        });
        format!("Background task started: {task_id}")
    }

    /// Handle a completed session-memory-update result (no-op: already persisted in task).
    const fn handle_memory_update_result(_result: &BackgroundTaskResult) {}

    /// Handle a completed memory-extraction result (no-op: already persisted in task).
    const fn handle_memory_extraction_result(_result: &BackgroundTaskResult) {}

    /// Spawn a background feature by kind with a sensible default request.
    fn spawn_background_feature_by_kind(
        &mut self,
        kind: BackgroundFeatureKind,
        user_prompt: String,
    ) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };

        self.cleanup_finished_background_tasks();

        let (system_prompt, max_tokens) = match kind {
            BackgroundFeatureKind::AgentSummary => (
                "You are a sub-agent summarizer. Produce a concise summary of recent activity."
                    .into(),
                256,
            ),
            BackgroundFeatureKind::AwaySummary => (
                "You are a session summarizer. Summarize what happened while the user was away."
                    .into(),
                256,
            ),
            BackgroundFeatureKind::MagicDocs => (
                "You are a documentation generator. Produce concise code documentation.".into(),
                1024,
            ),
            BackgroundFeatureKind::CodebaseOverview => (
                "You are a codebase analyst. Produce a high-level overview of the project.".into(),
                1024,
            ),
            BackgroundFeatureKind::ReadmeGeneration => (
                "You are a README generator. Produce a project README in Markdown.".into(),
                2048,
            ),
            BackgroundFeatureKind::SessionMemoryUpdate => (
                "You are a memory manager. Update session memory based on recent conversation."
                    .into(),
                512,
            ),
            BackgroundFeatureKind::MemoryExtraction => (
                "You are a memory extractor. Identify long-term memories from this session.".into(),
                512,
            ),
            BackgroundFeatureKind::PromptSuggestion => {
                ("You are a prompt suggestion assistant.".into(), 128)
            }
            BackgroundFeatureKind::NextActionSuggestion => (
                "You are a task planner. Suggest the next best action.".into(),
                128,
            ),
            BackgroundFeatureKind::TeamStatus => ("You are a team status aggregator.".into(), 256),
        };

        let parent_context = if self.history.len() >= 3 {
            Some(
                self.history
                    .iter()
                    .rev()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        } else {
            None
        };

        let request = BackgroundFeatureRequest {
            kind,
            system_prompt,
            user_prompt,
            parent_context,
            allowed_tools: None,
            max_tokens,
            writable: matches!(
                kind,
                BackgroundFeatureKind::SessionMemoryUpdate
                    | BackgroundFeatureKind::MemoryExtraction
            ),
        };

        let id = format!(
            "{}_{}",
            background_feature_label(kind),
            self.background_tasks.len()
        );
        self.spawn_background_task(&id, kind, client, request, tx);
        format!("Background task started: {id}")
    }

    /// Apply effects from a CommandResult to session state.
    fn apply_command_effects(&mut self, result: &CommandResult) {
        if let Some(profile) = result.prompt_profile {
            if !self.session_effects.prompt_profiles.contains(&profile) {
                self.session_effects.prompt_profiles.push(profile);
            }
        }

        if let Some(ref prompt) = result.override_system_prompt {
            // Override takes precedence: replace the base prompt entirely
            self.session_effects.override_system_prompt = Some(prompt.clone());
        } else if let Some(ref prompt) = result.append_system_prompt {
            if let Some(ref existing) = self.session_effects.append_system_prompt {
                self.session_effects.append_system_prompt = Some(format!("{existing}\n{prompt}"));
            } else {
                self.session_effects.append_system_prompt = Some(prompt.clone());
            }
        }

        if let Some(ref ctx) = result.extra_context {
            self.session_effects.extra_context.push(ctx.clone());
        }

        if let Some(ref tools) = result.allowed_tools_override {
            self.session_effects.allowed_tools_override = Some(tools.clone());
        }
    }

    /// Apply command effects that mutate persistent REPL state.
    fn sync_command_state(&mut self, cmd_name: &str, result: &CommandResult) -> bool {
        self.apply_command_effects(result);

        // `/init` is the product's setup action. The registry dispatches it
        // synchronously, while firing a hook is async, so record a successful
        // run here; the async entry points drain it into `HookEvent::Setup`.
        if cmd_name == "init" && !Self::command_result_is_error(result) {
            self.pending_setup = Some(format!("/{cmd_name}"));
        }

        if cmd_name == "vim" && result.state_changed {
            self.vim_mode_enabled = !self.vim_mode_enabled;
            return true;
        }

        if cmd_name == "theme" && result.state_changed {
            if let Some(theme_name) = result.output.strip_prefix("Theme changed to: ") {
                self.current_theme = theme_name.trim().to_string();
                return true;
            }
        }

        false
    }

    /// Whether a command result is the registry's error shape.
    ///
    /// `CommandResult` has no `is_error` flag; `CommandResult::error` is
    /// identified by its `Error: ` prefix, which is what the transcript and
    /// the protocol already show.
    fn command_result_is_error(result: &CommandResult) -> bool {
        result.output.starts_with("Error: ")
    }

    /// Fire a lifecycle hook that a synchronous command path queued.
    ///
    /// The pending value is taken before the await, so a hook that fails is
    /// not replayed on a later turn and a successful one fires exactly once.
    async fn drain_pending_setup_hook(&mut self) {
        let Some(command) = self.pending_setup.take() else {
            return;
        };
        let Some(ref engine) = self.engine else {
            return;
        };
        let mut context = std::collections::HashMap::new();
        context.insert("command".to_string(), command.clone());
        let result = engine
            .fire_lifecycle_hook(coderex_hooks::HookEvent::Setup.as_str(), &command, &context)
            .await;
        if !result.errors.is_empty() {
            tracing::warn!(errors = ?result.errors, "Setup hook errors");
        }
    }

    /// Fire `FileChanged` for a change batch the watcher reported.
    ///
    /// Each path gets its own event with the reference input
    /// `{"file_path": "...", "event": "change|add|unlink"}`. The batch's paths
    /// also travel as a newline-separated `paths` context entry, so a hook can
    /// see a rename's two paths together.
    #[cfg(feature = "hot-reload")]
    async fn fire_file_changed_hook(&self, batch: &FileChangeBatch) {
        let Some(ref engine) = self.engine else {
            return;
        };
        let hooks = engine.hook_executor();
        let session_id = engine.session().session_id.to_string();
        fire_file_changed_batch(hooks.as_ref(), &session_id, batch).await;
    }

    fn apply_tui_theme(&self, ui_state: &mut coderex_ui::state::UiState, theme_name: &str) -> bool {
        let mut manager = coderex_ui::themes::ThemeManager::new();
        // Plugin themes are part of the session's registry, so `/theme` can
        // select them like a built-in.
        for (name, value) in &self.plugin_themes {
            if let Err(error) = manager.register_json(name, value) {
                tracing::warn!(theme = %name, %error, "plugin theme was not registered");
            }
        }
        if let Some(theme) = manager.get_theme(theme_name) {
            ui_state.theme = theme.clone();
            true
        } else {
            false
        }
    }

    /// Apply the persisted statusline theme at startup, including plugins.
    fn apply_startup_theme(&self, ui_state: &mut coderex_ui::state::UiState) {
        let name = self.current_theme.clone();
        if !self.apply_tui_theme(ui_state, &name) && name != "dark" {
            tracing::warn!(theme = %name, "configured theme was not found");
        }
    }

    async fn submit_prompt(&mut self, prompt: &str, headless_mode: bool) -> String {
        let (elicit_tx, mut elicit_rx) =
            tokio::sync::mpsc::channel::<coderex_tools::ElicitRequest>(4);

        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_elicit_tx(Some(elicit_tx));
        }

        let mut output = String::new();
        self.drain_events(&mut output);

        // The protocol stream, if this run was asked for one. Taken out of
        // `self` so the turn can hold it across the loop below without
        // borrowing the whole REPL.
        let mut protocol = self.protocol_sink.take();

        // The headless control channel, when the host asked for one and can
        // answer: the engine's `Ask` becomes a `control_request` on the
        // protocol sink and the host's answer comes back on stdin. When it is
        // off the engine keeps its existing headless behaviour (the tool does
        // not run), so nothing changes for callers that opted out.
        //
        // A host that never answers leaves the turn waiting on the bus. That
        // is deliberate: a timeout would turn a slow person into a silent
        // denial. Turn shutdown and process exit drop the pending responders
        // with the bus.
        let permission_bus = if self.headless_permission_control && protocol.is_some() {
            let bus = ConfirmationBus::new(16);
            self.confirmation_bus = Some(bus.clone());
            Some(bus)
        } else {
            None
        };
        let permission_approvals: PermissionSessionApprovals =
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (permission_tx, mut permission_rx) =
            tokio::sync::mpsc::unbounded_channel::<ConfirmationRequest>();
        let mut permission_forwarder = permission_bus.as_ref().map(|bus| {
            let bus = bus.clone();
            tokio::spawn(async move {
                while let Some(request) = bus.next_request().await {
                    if permission_tx.send(request).is_err() {
                        break;
                    }
                }
            })
        });
        let mut permission_reader = permission_bus.as_ref().map(|bus| {
            spawn_headless_control_reader(bus.clone(), std::sync::Arc::clone(&permission_approvals))
        });

        let session_id = self.current_session_id().unwrap_or_default();
        let turn_started = std::time::Instant::now();
        // Watched in the loop rather than drained at the end, so the host sees
        // the turn as it happens instead of all at once when it is over.
        let mut live_events = self.event_rx.take();

        let options = self.build_submit_options();
        let mut engine = self.engine.take().unwrap();
        // Reapply the session's fallback model: the engine crosses the task
        // boundary below, and an agent selection or `/model` may have rebuilt
        // the engine's model after `configure_repl` ran.
        engine.set_fallback_model(self.fallback_model.clone());
        // Installed before the turn task starts so an `Ask` on the very first
        // tool call sees the bus.
        if let Some(bus) = permission_bus.as_ref() {
            engine.set_confirmation_bus(bus.clone());
        }
        let prompt_owned = prompt.to_string();
        // A channel rather than a one-shot: the wait for it happens inside
        // `select!`, and a one-shot receiver is consumed by its own future —
        // so a poll that loses to another branch drops the receiver and
        // cancels the handoff, leaving the engine stranded. `recv()` on a
        // channel borrows instead, so losing a poll costs nothing.
        let (engine_tx, mut engine_rx) =
            tokio::sync::mpsc::channel::<(QueryEngine, Result<TerminalReason, String>)>(1);
        tokio::spawn(async move {
            let result = engine.submit_streaming(prompt_owned, options).await;
            let _ = engine_tx.send((engine, result)).await;
        });
        let (mut engine, result) = loop {
            tokio::select! {
            Some(event) = async {
                match live_events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if live_events.is_some() => {
                if let Some(sink) = protocol.as_mut() {
                    if let Some(frame) = protocol_frame(&event, &session_id) {
                        if let Err(error) = sink.emit(&frame) {
                            tracing::warn!("could not write a protocol frame: {error}");
                        }
                    }
                }
                if let EngineEvent::TextDelta(delta) = &event {
                    output.push_str(delta);
                }
                if let EngineEvent::ModUi(elements) = &event {
                    append_mod_ui_block(&mut output, elements);
                }
            }
            Some(request) = elicit_rx.recv() => {
                for (i, q) in request.questions.iter().enumerate() {
                    if let Some(text) = q.get("question").and_then(|v| v.as_str()) {
                        println!("  [{}] {}", i + 1, text);
                    }
                    if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
                        for opt in options {
                            if let (Some(label), Some(desc)) = (
                                opt.get("label").and_then(|v| v.as_str()),
                                opt.get("description").and_then(|v| v.as_str()),
                            ) {
                                println!("     {label} — {desc}");
                            }
                        }
                    }
                }
                print!("Your answer: ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let answer = line.trim();
                let answers = serde_json::json!({ "answer": answer });
                let _ = request.response_tx.send(answers);
            }
            Some(request) = permission_rx.recv(), if permission_forwarder.is_some() => {
                // Record whether a session-wide answer is allowed before the
                // frame reaches the host, so an immediate reply cannot race
                // the mapping.
                if let Ok(mut approvals) = permission_approvals.lock() {
                    approvals.insert(request.id.clone(), request.allow_session);
                }
                if let Some(sink) = protocol.as_mut() {
                    let frame = coderex_events::protocol::ControlRequestFrame::can_use_tool(
                        request.id.clone(),
                        request.tool_name.clone(),
                        permission_request_args(&request.arguments),
                        request.reason.clone(),
                        request.allow_session,
                    );
                    if let Err(error) =
                        sink.emit(&coderex_events::ProtocolMessage::ControlRequest(frame))
                    {
                        tracing::warn!("could not write a control request: {error}");
                    }
                }
            }
            received = engine_rx.recv() => {
                let Some(handoff) = received else {
                    // The task is gone and took the engine with it. Say so
                    // rather than waiting for a handoff that cannot come.
                    if let Some(handle) = permission_forwarder.take() {
                        handle.abort();
                    }
                    if let Some(handle) = permission_reader.take() {
                        handle.abort();
                    }
                    if permission_bus.is_some() {
                        self.confirmation_bus = None;
                    }
                    // The turn never reached its normal closing path; a
                    // stream-json consumer still needs exactly one
                    // `result`, and the sink has to survive for the next
                    // turn instead of being dropped with this task.
                    let message = "the query task ended without returning its engine";
                    if let Some(mut sink) = protocol.take() {
                        let turn = sink.begin_turn(session_id.clone());
                        let mut frame = coderex_events::ResultFrame::error(
                            session_id.clone(),
                            coderex_events::protocol::result_subtype::ERROR_DURING_EXECUTION,
                            u64::try_from(turn_started.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                            1,
                        );
                        frame.data.insert(
                            "error".to_string(),
                            serde_json::Value::String(message.to_string()),
                        );
                        if let Err(error) = turn.emit_turn_complete(frame) {
                            tracing::warn!("could not close the protocol turn: {error}");
                        }
                        self.protocol_sink = Some(sink);
                    }
                    return message.to_string();
                };
                break handoff;
            }            }
        };

        // The turn is over: no further request can arrive, and a reader left
        // blocked on stdin would outlive the turn.
        if let Some(handle) = permission_forwarder.take() {
            handle.abort();
        }
        if let Some(handle) = permission_reader.take() {
            handle.abort();
        }
        // The bus belonged to this turn. Leave it installed and a later turn
        // without the control channel would ask a host that is no longer
        // listening and wait forever.
        if permission_bus.is_some() {
            engine.clear_confirmation_bus();
            self.confirmation_bus = None;
        }

        self.engine = Some(engine);
        self.event_rx = live_events;

        self.record_turn_telemetry(&result);
        if headless_mode {
            // Persist per turn, not only when the run ends, so a long
            // headless session is resumable even if the process is stopped.
            self.persist_headless_session();
        }
        self.drain_events(&mut output);

        // One result frame closes the turn: `emit_turn_complete` consumes the
        // turn guard, so a turn cannot report itself finished twice.
        if let Some(mut sink) = protocol {
            let turn = sink.begin_turn(session_id.clone());
            let frame = match &result {
                Ok(TerminalReason::MaxTurnsReached) => coderex_events::ResultFrame::error(
                    session_id.clone(),
                    coderex_events::protocol::result_subtype::ERROR_MAX_TURNS,
                    u64::try_from(turn_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    1,
                ),
                Ok(_) => coderex_events::ResultFrame::success(
                    session_id.clone(),
                    output.trim(),
                    u64::try_from(turn_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    1,
                ),
                Err(error) => {
                    let mut frame = coderex_events::ResultFrame::error(
                        session_id.clone(),
                        coderex_events::protocol::result_subtype::ERROR_DURING_EXECUTION,
                        u64::try_from(turn_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        1,
                    );
                    // The reason goes in the detail field, not the discriminant.
                    frame.data.insert(
                        "error".to_string(),
                        serde_json::Value::String(error.clone()),
                    );
                    frame
                }
            };
            if let Err(error) = turn.emit_turn_complete(frame) {
                tracing::warn!("could not close the protocol turn: {error}");
            }
            self.protocol_sink = Some(sink);
        }

        // Trigger background features on successful turn
        if result.is_ok() {
            self.trigger_background_features(prompt);
        }

        if !self.session_effects.title_generated {
            if let Some(ref client) = self.api_client {
                if let Ok(title) = prompts::generate_session_title(client.as_ref(), prompt).await {
                    self.session_effects.title_generated = true;
                    if let Some(ref mut engine) = self.engine {
                        let mut session = engine.session().clone();
                        if session.title.is_none() {
                            session.title = Some(title.clone());
                            engine.set_session(session);
                        }
                    }
                    if output.is_empty() {
                        output = format!("[Session title: {title}]");
                    } else {
                        output.insert_str(0, &format!("[Session title: {title}]\n"));
                    }
                }
            }
        }

        match result {
            Ok(TerminalReason::ApiUnavailable { message }) => {
                format!(
                    "The AI service is temporarily unavailable. Your conversation context is preserved. Please try again later.\n({message})"
                )
            }
            Ok(reason) => Self::headless_final_output(output, &reason),
            Err(e) => format!("Query failed: {e}"),
        }
    }

    /// Sync REPL state into the TUI statusline.
    fn sync_statusline(&self, statusline: &mut coderex_ui::statusline::Statusline) {
        statusline.model.clone_from(&self.model.0);
        statusline.session_title = if self.session_effects.title_generated {
            "Active".into()
        } else {
            "New Session".into()
        };
        statusline.git_branch = current_git_branch(&self.cwd);
        statusline.total_tokens = self.history.len() * 100;
        statusline.cost_usd = 0.0;
    }

    /// Sync current permission mode into the TUI permission bar.
    fn sync_permission_bar(&self, bar: &mut coderex_ui::permission_bar::PermissionBar) {
        if let Some(ref engine) = self.engine {
            let checker = engine.permission_checker();
            if let Ok(c) = checker.lock() {
                bar.mode = c.mode();
            }
        }
    }

    /// Update slash-command suggestion overlay based on current input.
    fn update_slash_suggestions(&self, ui_state: &mut coderex_ui::state::UiState) {
        use coderex_ui::overlay::Overlay;

        if ui_state.input.text.starts_with('/') {
            let query = ui_state.input.text.trim_start_matches('/');
            let cmds: Vec<String> = self
                .commands
                .list()
                .iter()
                .filter(|c| query.is_empty() || c.name.contains(query))
                .map(|c| format!("/{} - {}", c.name, c.description))
                .collect();
            // Replace existing palette overlay so suggestions stay in sync.
            if let Some(top) = ui_state.overlays.top() {
                if matches!(top, Overlay::CommandPalette { .. }) {
                    ui_state.overlays.pop();
                }
            }
            ui_state.overlays.push(Overlay::CommandPalette {
                query: ui_state.input.text.clone(),
                commands: cmds,
                selected: 0,
            });
        } else {
            // Remove palette overlay when input no longer starts with '/'.
            if let Some(top) = ui_state.overlays.top() {
                if matches!(top, Overlay::CommandPalette { .. }) {
                    ui_state.overlays.pop();
                }
            }
        }
    }

    /// Edit, submit, or cancel the open multi-field MCP form.
    ///
    /// Returns whether the key was consumed. Every key is consumed while a
    /// form is open, so a stray key cannot edit the main input line or change
    /// the transcript underneath it.
    fn handle_mcp_form_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
        ui_state: &mut coderex_ui::state::UiState,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        if self.pending_mcp_form.is_none() {
            return false;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('c')
                if key.code == KeyCode::Esc || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.pending_mcp_form = None;
                if let Some(tx) = self.pending_elicitation.take() {
                    let _ = tx.send(serde_json::json!({ "cancelled": true }));
                }
                ui_state.messages.push(coderex_ui::message_list::Message {
                    role: coderex_ui::message_list::MessageRole::System,
                    content: "MCP form cancelled.".into(),
                    tool_name: None,
                    streaming: false,
                    folded: false,
                });
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(form) = self.pending_mcp_form.as_mut() {
                    form.move_focus(1);
                    ui_state.notifications.info(form.focus_hint());
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(form) = self.pending_mcp_form.as_mut() {
                    form.move_focus(-1);
                    ui_state.notifications.info(form.focus_hint());
                }
            }
            KeyCode::Enter => {
                let mut form = self.pending_mcp_form.take().expect("form is open");
                match form.submit() {
                    Ok(content) => {
                        if let Some(tx) = self.pending_elicitation.take() {
                            let _ = tx.send(serde_json::json!({ "content": content }));
                        }
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: "MCP form submitted.".into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    Err(message) => {
                        // Do not send: the schema rejected the answer, and a
                        // server must never receive an invalid `accept`.
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: format!("MCP form: {message}").into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        self.pending_mcp_form = Some(form);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(form) = self.pending_mcp_form.as_mut() {
                    form.backspace();
                }
            }
            KeyCode::Delete => {
                if let Some(form) = self.pending_mcp_form.as_mut() {
                    form.clear_field();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(form) = self.pending_mcp_form.as_mut() {
                    form.insert_char(c);
                }
            }
            // Left/Right/Home/End and anything else stay inside the form.
            _ => {}
        }
        true
    }

    /// Process a line of input in the TUI context.
    fn process_tui_input(
        &mut self,
        text: &str,
        ui_state: &mut coderex_ui::state::UiState,
        thinking: &mut bool,
        phase: &mut InteractivePhase,
        query_done_rx: &mut Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        >,
        elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>>,
        tui_elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>>,
        queued_prompts: &mut std::collections::VecDeque<String>,
    ) {
        // Clear previous turn's duration display so it doesn't linger.
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking_started = None;
        ui_state.statusline.spinner_frame = 0;
        ui_state.messages.scroll_to_bottom();

        // A multi-field form is edited by the key handler, not by submitting
        // the main input line.
        if self.pending_mcp_form.is_some() {
            return;
        }

        // If there's a pending elicitation, treat this input as the answer.
        if let Some(tx) = self.pending_elicitation.take() {
            self.history.push(text.to_string());
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::User,
                content: text.into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
            let answer = serde_json::json!({ "answer": text });
            let _ = tx.send(answer);
            *thinking = true;
            *phase = InteractivePhase::Thinking;
            self.persist_interactive_snapshot(ui_state);
            return;
        }

        self.history.push(text.to_string());

        // Add user message
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::User,
            content: text.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });

        if text.starts_with('/') {
            let parts: Vec<&str> = text.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();

            // Record here rather than in `execute_and_display_command`: the
            // built-ins below return before reaching it, so a history that
            // only saw registry commands left out half of what people type.
            self.command_history.append(cmd_name, &args);

            // Servers can discover prompts after they connect (and push
            // `prompts/list_changed`). Pick those up before resolving anything
            // that could name one.
            if cmd_name == "mcp" || cmd_name.starts_with("mcp__") {
                self.refresh_mcp_prompt_commands();
            }

            // Built-in REPL commands that bypass the registry. With slash
            // commands disabled none of these run either: they are still slash
            // commands, and executing them anyway made the flag only hide
            // them from `/help` (the non-TUI path already skipped them).
            if !self.no_slash_commands {
                match cmd_name {
                    "exit" | "quit" => {
                        self.running = false;
                        return;
                    }
                    "tokens" => {
                        self.execute_and_display_command("usage", &[], ui_state);
                        return;
                    }
                    "mcp" => {
                        let result = self.run_mcp_command(&args);
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: result.output.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        return;
                    }
                    "compact" => {
                        let msg = self.compact_context(ui_state);
                        ui_state.notifications.info(&msg);
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: msg.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        return;
                    }
                    "model" => {
                        if let Some(model) = args.first() {
                            let mapped = match model.to_lowercase().as_str() {
                                "opus" => ModelId::opus(),
                                "sonnet" => ModelId::sonnet(),
                                "haiku" => ModelId::haiku(),
                                other => ModelId(other.to_string()),
                            };
                            self.model = mapped;
                        }
                        self.execute_and_display_command(cmd_name, &args, ui_state);
                        return;
                    }
                    "vim" => {
                        let ctx = self.build_context();
                        let result = self.commands.execute("vim", &ctx, &args);
                        let toggled = self.sync_command_state("vim", &result);
                        if toggled {
                            ui_state.notifications.info(&result.output);
                        }
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: result.output.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        return;
                    }
                    "plan" => {
                        let registry_output =
                            self.commands.execute("plan", &self.build_context(), &args);
                        let body = ui_state.plan.render_detail();
                        show_detail_overlay(ui_state, "Current Plan", body);
                        if !registry_output.output.is_empty() {
                            ui_state.messages.push(coderex_ui::message_list::Message {
                                role: coderex_ui::message_list::MessageRole::System,
                                content: registry_output.output.into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                        }
                        return;
                    }
                    "resume" => {
                        if *thinking || query_done_rx.is_some() {
                            let message =
                                "Finish or cancel the active turn before resuming another session.";
                            ui_state.notifications.warn(message);
                            ui_state.messages.push(coderex_ui::message_list::Message {
                                role: coderex_ui::message_list::MessageRole::System,
                                content: message.into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                            return;
                        }
                        match self.resume_tui_session(args.first().map(String::as_str), ui_state) {
                            Ok(_) => {}
                            Err(err) => {
                                ui_state.notifications.err(&err);
                                ui_state.messages.push(coderex_ui::message_list::Message {
                                    role: coderex_ui::message_list::MessageRole::System,
                                    content: format!("Error: {err}").into(),
                                    tool_name: None,
                                    streaming: false,
                                    folded: false,
                                });
                            }
                        }
                        return;
                    }
                    "activity" => {
                        let body = if ui_state.activity_log.is_empty() {
                            "No recent activity yet.".to_string()
                        } else {
                            ui_state.activity_log.join("\n")
                        };
                        show_detail_overlay(ui_state, "Recent Activity", body);
                        return;
                    }
                    "open" => {
                        if args.is_empty() {
                            ui_state.messages.push(coderex_ui::message_list::Message {
                                role: coderex_ui::message_list::MessageRole::System,
                                content: "Usage: /open <path>".into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                            ui_state.notifications.warn("Usage: /open <path>");
                            return;
                        }
                        let raw_path = std::path::PathBuf::from(&args[0]);
                        let path = if raw_path.is_absolute() {
                            raw_path
                        } else {
                            std::path::Path::new(&self.cwd).join(raw_path)
                        };
                        match std::fs::read_to_string(&path) {
                            Ok(content) => {
                                ui_state.left_pane_content = content;
                                ui_state.left_pane_title.clone_from(&args[0]);
                                ui_state.layout_mode =
                                    coderex_ui::state::LayoutMode::HorizontalSplit {
                                        left_percent: 50,
                                    };
                                ui_state
                                    .notifications
                                    .success(format!("Opened {}", args[0]));
                                push_activity_row(ui_state, &format!("Opened {}", args[0]));
                                self.persist_interactive_snapshot(ui_state);
                            }
                            Err(err) => {
                                let detail = format!("Failed to open {}: {}", args[0], err);
                                ui_state.notifications.err(&detail);
                                show_detail_overlay(ui_state, "Failure Details", detail.clone());
                                ui_state.messages.push(coderex_ui::message_list::Message {
                                    role: coderex_ui::message_list::MessageRole::System,
                                    content: detail.into(),
                                    tool_name: None,
                                    streaming: false,
                                    folded: false,
                                });
                            }
                        }
                        return;
                    }
                    "style" => {
                        let message = self.handle_output_style_command(&args);
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: message.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        return;
                    }
                    "macro" => {
                        match self.handle_macro_command(&args) {
                            MacroCommandOutcome::Message(message) => {
                                ui_state.messages.push(coderex_ui::message_list::Message {
                                    role: coderex_ui::message_list::MessageRole::System,
                                    content: message.into(),
                                    tool_name: None,
                                    streaming: false,
                                    folded: false,
                                });
                            }
                            MacroCommandOutcome::Run(commands) => {
                                for command in commands {
                                    self.execute_single_command(&command, &mut *ui_state);
                                }
                            }
                        }
                        return;
                    }
                    _ => {}
                }
            }

            // Check for macro expansion first. A macro expands into slash
            // commands, so it is subject to the same switch.
            if !self.no_slash_commands && self.macro_registry.has(cmd_name) {
                if let Some(commands) = self.macro_registry.get(cmd_name) {
                    let cmds: Vec<String> = commands.to_vec();
                    for macro_cmd in cmds {
                        self.execute_single_command(&macro_cmd, ui_state);
                    }
                    return;
                }
            }

            // Check for pipe operator
            if text.contains('|') {
                self.execute_piped_commands(text, ui_state);
                return;
            }

            // Check for destructive command confirmation
            if let Some(cmd) = self.commands.find(cmd_name) {
                if cmd.destructive {
                    self.pending_destructive_cmd = Some((cmd_name.to_string(), args));
                    ui_state
                        .overlays
                        .push(coderex_ui::overlay::Overlay::Confirm {
                            message: format!(
                                "Are you sure you want to run /{}? This action may be destructive.",
                                cmd.name
                            ),
                        });
                    return;
                }
            }

            // Execute the command directly
            self.execute_and_display_command(cmd_name, &args, ui_state);
        } else if self.engine.is_some() {
            self.start_tui_query(
                text.to_string(),
                ui_state,
                thinking,
                phase,
                query_done_rx,
                elicit_rx,
                tui_elicit_rx,
            );
        } else if *thinking || query_done_rx.is_some() {
            queued_prompts.push_back(text.to_string());
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::System,
                content: "[Queued until current turn finishes]".into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        } else {
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::Assistant,
                content: "[No engine configured]".into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }
    }

    fn start_tui_query(
        &mut self,
        prompt: String,
        ui_state: &mut coderex_ui::state::UiState,
        thinking: &mut bool,
        phase: &mut InteractivePhase,
        query_done_rx: &mut Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        >,
        elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>>,
        tui_elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>>,
    ) {
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking_started = None;
        ui_state.statusline.spinner_frame = 0;
        ui_state.messages.scroll_to_bottom();
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::Assistant,
            content: "".into(),
            tool_name: None,
            streaming: true,
            folded: false,
        });

        let options = self.build_submit_options();
        let mut engine = self.engine.take().unwrap();
        engine.set_fallback_model(self.fallback_model.clone());
        // The engine moves into the task below, so the handle is what Ctrl+C
        // can reach: without it the key abandoned the result while the work
        // carried on.
        self.running_query = Some(engine.interrupt_handle());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (elicit_tx, erx) = tokio::sync::mpsc::channel(4);
        let (tui_elicit_tx, terx) = tokio::sync::mpsc::channel(1);
        engine.set_elicitation_handler(Box::new(TuiElicitationHandler::new(tui_elicit_tx)));
        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_elicit_tx(Some(elicit_tx));
        }
        self.query_start_time = Some(std::time::Instant::now());
        ui_state.plan.reset();
        ui_state.permission_bar.clear_status();
        set_plan_step(ui_state, "Planning next step");
        push_activity_row(ui_state, "Planning next step");
        self.persist_interactive_snapshot(ui_state);
        tokio::spawn(async move {
            let result = engine.submit_streaming(prompt, options).await;
            let _ = tx.send((engine, result));
        });
        *query_done_rx = Some(rx);
        *elicit_rx = Some(erx);
        *tui_elicit_rx = Some(terx);
        *thinking = true;
        *phase = InteractivePhase::Planning;
    }

    async fn respond_to_confirmation(
        &self,
        request: ConfirmationRequest,
        response: ConfirmationResponse,
        ui_state: &mut coderex_ui::state::UiState,
        phase: &mut InteractivePhase,
    ) {
        if let Some(bus) = self.confirmation_bus.as_ref() {
            let _ = bus.respond(&request.id, response).await;
        }

        let args = permission_request_args(&request.arguments);
        let activity = ToolCall::summarize_activity(&request.tool_name, &args);
        let approved = matches!(
            response,
            ConfirmationResponse::Approved | ConfirmationResponse::ApproveSession
        );
        let resolution = match response {
            ConfirmationResponse::Approved => format!("approved {activity}"),
            ConfirmationResponse::ApproveSession => {
                format!("approved {activity} for the session")
            }
            ConfirmationResponse::Denied => format!("denied {activity}"),
        };

        clear_permission_overlay(ui_state);
        ui_state.permission_bar.set_resolution(approved, activity);
        push_activity_row(ui_state, &resolution);
        phase.note_tool_end();

        if approved {
            ui_state.notifications.success(&resolution);
        } else {
            ui_state.notifications.warn(&resolution);
        }
        self.persist_interactive_snapshot(ui_state);
    }

    /// Stop the running query, and close the TUI's side of it.
    ///
    /// Ctrl+C and Esc mean the same thing here — end the turn — and the two
    /// sequences had already drifted into near-duplicates that differed only in
    /// the word appended to the streaming message. Esc in the *default*
    /// (non-Vim) mode never reached either of them, so "Esc stops the run" was
    /// only true with Vim mode on.
    fn stop_running_query(
        &mut self,
        ui_state: &mut coderex_ui::state::UiState,
        thinking: &mut bool,
        phase: &mut InteractivePhase,
        query_done_rx: &mut Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        >,
        elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>>,
        tui_elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>>,
        marker: &str,
    ) {
        // Tell the engine to stop before letting go of its result: the turn is
        // abandoned *and* ended.
        if let Some(handle) = self.running_query.take() {
            handle.interrupt();
        }
        *thinking = false;
        phase.reset();
        *query_done_rx = None;
        *elicit_rx = None;
        *tui_elicit_rx = None;
        self.pending_elicitation = None;
        self.pending_mcp_form = None;
        clear_permission_overlay(ui_state);
        if let Some(last) = ui_state.messages.messages.iter_mut().rev().find(|msg| {
            msg.role == coderex_ui::message_list::MessageRole::Assistant && msg.streaming
        }) {
            last.streaming = false;
            let mut msg = last.content.to_string();
            msg.push_str(marker);
            last.content = msg.into();
        }
        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_elicit_tx(None);
        }
    }

    /// Execute a single command through the registry and display the result in the TUI.
    fn execute_and_display_command(
        &mut self,
        cmd_name: &str,
        args: &[String],
        ui_state: &mut coderex_ui::state::UiState,
    ) {
        // `/rewind` needs the session and the checkpoint store, which the
        // registry cannot reach; it is handled here so the TUI path restores
        // files the same way the non-TUI path does.
        if cmd_name == "rewind" {
            let message = args
                .first()
                .and_then(|value| value.parse::<usize>().ok())
                .map_or_else(
                    || "Usage: /rewind <message-number>".to_string(),
                    |n| match self.perform_rewind(n) {
                        Ok((removed, restored, removed_files)) => format!(
                            "Rewound to message {n}. {removed} messages removed. {restored} \
                             file(s) restored, {removed_files} created file(s) removed."
                        ),
                        Err(error) => error,
                    },
                );
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::System,
                content: message.into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
            return;
        }
        let ctx = self.build_context();
        let result = self.commands.execute(cmd_name, &ctx, args);
        self.sync_command_state(cmd_name, &result);
        if cmd_name == "theme" && result.state_changed {
            if let Some(theme_name) = args.first() {
                if self.apply_tui_theme(ui_state, theme_name) {
                    ui_state
                        .notifications
                        .info(format!("Theme changed to: {theme_name}"));
                }
            }
        }

        if cmd_name == "clear" && self.commands.find("clear").is_some() {
            // Whatever the registry said about the message count, the session
            // is cleared here, and the message says that rather than echoing
            // how empty it already was.
            let message = self.perform_session_clear();
            self.clear_tui_session(ui_state, message);
            return;
        }

        ui_state.messages.push(coderex_ui::message_list::Message {
            role: if result.scrollable {
                coderex_ui::message_list::MessageRole::Assistant
            } else {
                coderex_ui::message_list::MessageRole::System
            },
            content: result.output.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
    }

    /// Clear the session and say what happened.
    ///
    /// One implementation for every entry point, because `/clear` that reports
    /// success and leaves the conversation in place is worse than no `/clear`
    /// at all — and the interactive, headless, and scripted paths had drifted
    /// into exactly that: only the interactive one cleared anything.
    fn perform_session_clear(&mut self) -> String {
        let removed = self
            .engine
            .as_ref()
            .map_or(0, |engine| engine.session().messages.len());

        if let Some(ref mut engine) = self.engine {
            let mut session = engine.session().clone();
            session.messages.clear();
            engine.set_session(session);
        }
        self.history.clear();
        self.session_effects = SessionEffects::default();

        if removed == 0 {
            "Session cleared. (There was nothing in it.)".to_string()
        } else {
            format!("Session cleared. ({removed} message(s) removed)")
        }
    }

    fn clear_tui_session(&mut self, ui_state: &mut coderex_ui::state::UiState, message: String) {
        let prior_session_id = self.current_session_id();
        self.history.clear();
        ui_state.messages = coderex_ui::message_list::MessageView::new(1000);
        ui_state.overlays.clear();
        ui_state.notifications.dismiss_all();
        ui_state.input = coderex_ui::input::InputBuffer::new();
        ui_state.plan.reset();
        ui_state.activity_log.clear();
        ui_state.permission_bar.clear_status();
        clear_permission_overlay(ui_state);
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking = false;
        ui_state.statusline.thinking_label = None;
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::System,
            content: message.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
        self.clear_interactive_snapshot(prior_session_id.as_deref());
    }

    /// Execute piped commands: split on `|`, run sequentially, pass output as pipe_input.
    fn execute_piped_commands(&mut self, text: &str, ui_state: &mut coderex_ui::state::UiState) {
        let segments: Vec<&str> = text.trim_start_matches('/').split('|').collect();
        let mut pipe_input: Option<String> = None;
        let mut outputs = Vec::new();

        for segment in &segments {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            let parts: Vec<&str> = segment.split_whitespace().collect();
            // Every segment but the first still carries its slash: the whole
            // line was trimmed once, not per segment, so `/version | /env`
            // splits into "/version" and "/env".
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();

            self.command_history.append(cmd_name, &args);

            let mut ctx = self.build_context();
            ctx.pipe_input = pipe_input;

            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            outputs.push(result.output.clone());
            pipe_input = Some(result.output);
        }

        let combined = outputs.join("\n---\n");
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::Assistant,
            content: combined.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
    }

    /// Execute a single command string (from macro expansion) in the TUI.
    fn execute_single_command(&mut self, cmd_str: &str, ui_state: &mut coderex_ui::state::UiState) {
        let trimmed = cmd_str.trim();
        if trimmed.starts_with('/') {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            self.command_history.append(cmd_name, &args);
            if cmd_name.starts_with("mcp__") {
                self.refresh_mcp_prompt_commands();
            }
            let ctx = self.build_context();
            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: if result.scrollable {
                    coderex_ui::message_list::MessageRole::Assistant
                } else {
                    coderex_ui::message_list::MessageRole::System
                },
                content: result.output.into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        } else {
            // Non-command string: treat as a user message
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::System,
                content: format!("[Macro] {trimmed}").into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }
    }

    /// Apply `/rewind <n>`: truncate the conversation and undo the file edits
    /// the removed tool calls made.
    ///
    /// Returns `(messages_removed, files_restored, created_files_removed)`.
    fn perform_rewind(&mut self, n: usize) -> Result<(usize, usize, usize), String> {
        let Some(engine) = self.engine.as_mut() else {
            return Err("No engine configured.".to_string());
        };
        let mut session = engine.session().clone();
        if n >= session.messages.len() {
            return Err(format!(
                "Message {n} not found. Session has {} messages.",
                session.messages.len()
            ));
        }
        let removed = session.messages.len() - n;
        // The tool calls this rewind undoes, by id: the checkpoint store can
        // then restore the files those calls wrote.
        let undone_call_ids: std::collections::HashSet<String> = session
            .messages
            .iter()
            .skip(n)
            .filter_map(|message| match &message.content {
                coderex_core::types::MessageContent::ToolCall(call) => Some(call.id.clone()),
                coderex_core::types::MessageContent::ToolResult(result) => {
                    Some(result.tool_call_id.clone())
                }
                _ => None,
            })
            .collect();
        session.messages.truncate(n);
        engine.set_session(session);
        self.history.truncate(n);

        let (restored, removed_files) = self.checkpoint_store.as_ref().map_or((0, 0), |store| {
            let report = store.restore(&undone_call_ids);
            (report.restored.len(), report.removed.len())
        });
        Ok((removed, restored, removed_files))
    }

    /// Run one command line outside the TUI.
    ///
    /// Unlike the interactive path, this does not stop to confirm a command
    /// marked `destructive`: there is nobody to ask. An interactive session
    /// asks first (the `Confirm` overlay before running the command); a
    /// headless run carries out what it was told to, which is what running it
    /// without a person present means.
    async fn execute_command(
        &mut self,
        line: &str,
        stdout: &mut (impl Write + Send),
    ) -> std::io::Result<()> {
        // Check for pipe operator
        if line.contains('|') {
            let result = self.execute_piped_commands_non_tui(line, stdout);
            // A pipe may end in `/init`; the drain is async and the piped
            // helper is not, so it happens here.
            self.drain_pending_setup_hook().await;
            return result;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd_name = parts[0].trim_start_matches('/');
        let args: Vec<String> = parts[1..]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();

        // Record before dispatch: the built-in arms below return early, and a
        // history that only recorded registry commands was missing them.
        self.command_history.append(cmd_name, &args);

        // Pick up prompts discovered after the manager was attached (a fresh
        // connection or a `prompts/list_changed` notification).
        if cmd_name == "mcp" || cmd_name.starts_with("mcp__") {
            self.refresh_mcp_prompt_commands();
        }

        // Check for macro expansion. A macro expands into slash commands, so
        // it follows the same switch as the built-ins below.
        if !self.no_slash_commands && self.macro_registry.has(cmd_name) {
            if let Some(commands) = self.macro_registry.get(cmd_name) {
                let cmds: Vec<String> = commands.to_vec();
                for macro_cmd in cmds {
                    // The full entry path, so a macro segment can be a
                    // built-in or a pipe, not only a registry command.
                    Box::pin(self.execute_command(&macro_cmd, &mut *stdout)).await?;
                }
                return Ok(());
            }
        }

        // Built-in REPL commands that don't go through registry.
        //
        // A session with slash commands turned off has none of these either:
        // the built-ins are still slash commands, and running one anyway made
        // `--no-slash-commands` a setting that only hid them from /help.
        if !self.no_slash_commands {
            match cmd_name {
                "exit" | "quit" => {
                    self.running = false;
                    writeln!(stdout, "Exiting...")?;
                    return Ok(());
                }
                "help" => {
                    self.show_help(stdout)?;
                    return Ok(());
                }
                "mcp" => {
                    let result = self.run_mcp_command(&args);
                    writeln!(stdout, "{}", result.output)?;
                    return Ok(());
                }
                "cancel" => {
                    self.cancel_background_tasks();
                    writeln!(stdout, "All background tasks cancelled.")?;
                    return Ok(());
                }
                "compact" => {
                    let msg = self.compact_context_non_tui();
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "agent-summary" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::AgentSummary,
                        "Summarize what the agent has done recently.".into(),
                    );
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "magic-docs" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::MagicDocs,
                        "Generate documentation for the current codebase.".into(),
                    );
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "generate-readme" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::ReadmeGeneration,
                        "Generate a README for the current project.".into(),
                    );
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "away-summary" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::AwaySummary,
                        "Summarize what happened while the user was away.".into(),
                    );
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "codebase-overview" => {
                    let msg = self.spawn_background_feature_by_kind(
                        BackgroundFeatureKind::CodebaseOverview,
                        "Generate a high-level overview of the codebase.".into(),
                    );
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "update-memory" => {
                    let msg = self.spawn_memory_update_task();
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "extract-memories" => {
                    let msg = self.spawn_memory_extraction_task();
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "agent-generation" => {
                    let description = args.join(" ");
                    if description.is_empty() {
                        writeln!(stdout, "Usage: /agent-generation <description>")?;
                    } else {
                        let msg = self.run_agent_generation(&description).await;
                        writeln!(stdout, "{msg}")?;
                    }
                    return Ok(());
                }
                "auto-dream" => {
                    let msg = self.handle_auto_dream_command(&args);
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "team-memory" => {
                    let msg = self.handle_team_memory_command(&args);
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "workspace" => {
                    let msg = self.handle_workspace_command(&args);
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "recover" => {
                    let msg = self.handle_recover_command();
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "update-check" => {
                    let msg = self.handle_update_check_command().await;
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "feedback" => {
                    let msg = self.handle_feedback_command(&args).await;
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "teleport" => {
                    let msg = self.handle_teleport_command(&args);
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "agents" => {
                    let msg = self.handle_agents_command(&args).await;
                    writeln!(stdout, "{msg}")?;
                    return Ok(());
                }
                "clear" => {
                    let message = self.perform_session_clear();
                    writeln!(stdout, "{message}")?;
                    return Ok(());
                }
                "resume" => {
                    // Same resolution as the TUI: exact id, unique fragment,
                    // ambiguous (list candidates), or none.
                    let session_id =
                        match self.resolve_resume_session_id(args.first().map(String::as_str)) {
                            Ok(session_id) => session_id,
                            Err(message) => {
                                writeln!(stdout, "{message}")?;
                                return Ok(());
                            }
                        };
                    if session_id.is_empty() {
                        writeln!(stdout, "No previous sessions found.")?;
                    } else {
                        match self.load_session_state(&session_id, true) {
                            Ok((messages, _)) => {
                                writeln!(
                                    stdout,
                                    "Resumed session: {session_id} ({} messages)",
                                    messages.len()
                                )?;
                            }
                            Err(e) => {
                                writeln!(stdout, "Failed to resume session: {e}")?;
                            }
                        }
                    }
                    return Ok(());
                }
                "rename" => {
                    if args.is_empty() {
                        writeln!(stdout, "Usage: /rename <new-title>")?;
                    } else {
                        let new_title = args.join(" ");
                        if let Some(ref mut engine) = self.engine {
                            let mut session = engine.session().clone();
                            session.title = Some(new_title.clone());
                            engine.set_session(session);
                        }
                        writeln!(stdout, "Session renamed to: '{new_title}'")?;
                    }
                    return Ok(());
                }
                "rewind" => {
                    if args.is_empty() {
                        writeln!(stdout, "Usage: /rewind <message-number>")?;
                    } else {
                        match args[0].parse::<usize>() {
                            Ok(n) => match self.perform_rewind(n) {
                                Ok((removed, restored, removed_files)) => writeln!(
                                    stdout,
                                    "Rewound to message {n}. {removed} messages removed. \
                                     {restored} file(s) restored, {removed_files} \
                                     created file(s) removed."
                                )?,
                                Err(error) => writeln!(stdout, "{error}")?,
                            },
                            Err(_) => {
                                writeln!(stdout, "Invalid message number.")?;
                            }
                        }
                    }
                    return Ok(());
                }
                "model" => {
                    if args.is_empty() {
                        writeln!(stdout, "Current model: {}", self.model.0)?;
                    } else {
                        let model_name = args[0].clone();
                        self.model = ModelId(model_name.clone());
                        writeln!(stdout, "Model switched to: {model_name}")?;
                    }
                    return Ok(());
                }
                "permissions" => {
                    if args.is_empty() {
                        let mode = if let Some(ref engine) = self.engine {
                            engine
                                .permission_checker()
                                .lock()
                                .map(|c| c.mode())
                                .unwrap_or_default()
                        } else {
                            self.permission_mode.unwrap_or_default()
                        };
                        writeln!(stdout, "Current permission mode: {mode:?}")?;
                    } else {
                        let mode = match args[0].to_lowercase().as_str() {
                            "default" => PermissionMode::Default,
                            "acceptedits" | "accept_edits" => PermissionMode::AcceptEdits,
                            "bypasspermissions" | "bypass" | "bypass_permissions" => {
                                PermissionMode::BypassPermissions
                            }
                            "dontask" | "dont_ask" => PermissionMode::DontAsk,
                            "plan" | "planmode" | "plan_mode" => PermissionMode::Plan,
                            "auto" => PermissionMode::Auto,
                            _ => {
                                writeln!(
                                    stdout,
                                    "Unknown mode: {}. Available: default, acceptedits, bypasspermissions, dontask, plan, auto",
                                    args[0]
                                )?;
                                return Ok(());
                            }
                        };
                        if let Some(ref engine) = self.engine {
                            if let Ok(mut checker) = engine.permission_checker().lock() {
                                checker.set_mode(mode);
                            }
                        }
                        self.permission_mode = Some(mode);
                        writeln!(stdout, "Permission mode set to: {mode:?}")?;
                    }
                    return Ok(());
                }
                "break-cache" => {
                    if let Some(ref mut engine) = self.engine {
                        let mut session = engine.session().clone();
                        let previous = session.usage.cache_read_tokens;
                        session.usage.cache_read_tokens = 0;
                        session.usage.cache_creation_tokens = 0;
                        engine.set_session(session);
                        writeln!(
                            stdout,
                            "Prompt cache cleared. Previous cache: {previous} tokens"
                        )?;
                    } else {
                        writeln!(stdout, "No engine configured.")?;
                    }
                    return Ok(());
                }
                "style" => {
                    let message = self.handle_output_style_command(&args);
                    writeln!(stdout, "{message}")?;
                    return Ok(());
                }
                "macro" => {
                    match self.handle_macro_command(&args) {
                        MacroCommandOutcome::Message(message) => {
                            writeln!(stdout, "{message}")?;
                        }
                        MacroCommandOutcome::Run(commands) => {
                            for command in commands {
                                // A macro expands into the same entry path;
                                // box the future rather than duplicating it.
                                Box::pin(self.execute_command(&command, &mut *stdout)).await?;
                            }
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        }

        let ctx = self.build_context();
        let result = self.commands.execute(cmd_name, &ctx, &args);
        self.sync_command_state(cmd_name, &result);
        writeln!(stdout, "{}", result.output)?;
        // The registry dispatch is synchronous; fire anything it queued now,
        // while this async entry point owns the engine.
        self.drain_pending_setup_hook().await;
        Ok(())
    }

    fn show_help(&self, stdout: &mut impl Write) -> std::io::Result<()> {
        writeln!(stdout, "Available commands:")?;
        for cmd in self.commands.list() {
            writeln!(stdout, "  /{:<20} {}", cmd.name, cmd.description)?;
        }
        Ok(())
    }

    /// Execute piped commands in non-TUI mode.
    fn execute_piped_commands_non_tui(
        &mut self,
        line: &str,
        stdout: &mut impl Write,
    ) -> std::io::Result<()> {
        let segments: Vec<&str> = line.trim_start_matches('/').split('|').collect();
        let mut pipe_input: Option<String> = None;
        let mut written_any = false;

        for segment in &segments {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            let parts: Vec<&str> = segment.split_whitespace().collect();
            // Every segment but the first arrives with its slash still on:
            // `/diff | /env` splits into "/diff" and "/env", and the name is
            // what the registry is asked for.
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();

            self.command_history.append(cmd_name, &args);
            if cmd_name == "mcp" || cmd_name.starts_with("mcp__") {
                self.refresh_mcp_prompt_commands();
            }

            let mut ctx = self.build_context();
            ctx.pipe_input = pipe_input.take();

            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);

            // A separator between segments: in a transcript, two commands'
            // output running together reads as one.
            if written_any {
                writeln!(stdout, "---")?;
            }
            writeln!(stdout, "{}", result.output)?;
            written_any = true;
            pipe_input = Some(result.output);
        }
        Ok(())
    }

    /// `/style [name]` — list or apply a plugin output style.
    ///
    /// An output style is assigned to `append_system_prompt`, so the style's
    /// instructions are added to the base prompt rather than replacing it.
    /// Values may be a string or an object with `prompt`/`content`/`text`.
    fn handle_output_style_command(&mut self, args: &[String]) -> String {
        if args.is_empty() {
            if self.plugin_output_styles.is_empty() {
                return "No output styles available.".to_string();
            }
            let names: Vec<&str> = self
                .plugin_output_styles
                .iter()
                .map(|(name, _)| name.as_str())
                .collect();
            return format!(
                "Output styles: {}\nUse /style <name> to apply one.",
                names.join(", ")
            );
        }
        let name = args[0].as_str();
        let Some((_, value)) = self
            .plugin_output_styles
            .iter()
            .find(|(style, _)| style == name)
        else {
            let names: Vec<&str> = self
                .plugin_output_styles
                .iter()
                .map(|(style, _)| style.as_str())
                .collect();
            return format!(
                "No output style named '{name}'. Available: {}",
                names.join(", ")
            );
        };
        let prompt = value.as_str().map(str::to_string).or_else(|| {
            for key in ["prompt", "content", "text"] {
                if let Some(text) = value.get(key).and_then(serde_json::Value::as_str) {
                    return Some(text.to_string());
                }
            }
            None
        });
        let Some(prompt) = prompt else {
            return format!("Output style '{name}' has no prompt text.");
        };
        self.session_effects.append_system_prompt = Some(prompt);
        format!("Output style '{name}' applied to the system prompt.")
    }

    /// What `/macro` asks the REPL to do.
    ///
    /// Defining and listing are pure bookkeeping; running expands into the
    /// normal command path, which has to happen in the caller because it
    /// needs the session (TUI state or an output stream).
    fn handle_macro_command(&mut self, args: &[String]) -> MacroCommandOutcome {
        const USAGE: &str = "Usage: /macro <list | define <name> <command[; command…]> | run <name> | delete <name>>";
        match args.first().map(String::as_str) {
            None | Some("list") => {
                let names = self.macro_registry.list();
                if names.is_empty() {
                    MacroCommandOutcome::Message(
                        "No macros defined. Use /macro define <name> <command>.".into(),
                    )
                } else {
                    let mut output = String::from("Macros:");
                    for name in names {
                        output.push_str("\n  ");
                        output.push_str(&name);
                    }
                    MacroCommandOutcome::Message(output)
                }
            }
            Some("define") => {
                if args.len() < 3 {
                    return MacroCommandOutcome::Message(USAGE.into());
                }
                let name = args[1].trim_start_matches('/');
                if name.is_empty() {
                    return MacroCommandOutcome::Message("A macro needs a name.".into());
                }
                let body = args[2..].join(" ");
                let commands: Vec<String> = body
                    .split(';')
                    .map(str::trim)
                    .filter(|command| !command.is_empty())
                    .map(str::to_string)
                    .collect();
                if commands.is_empty() {
                    return MacroCommandOutcome::Message(USAGE.into());
                }
                let count = commands.len();
                self.macro_registry.define(name, commands);
                MacroCommandOutcome::Message(format!(
                    "Defined macro '{name}' with {count} command(s). Run it with /{name} or /macro run {name}."
                ))
            }
            Some("delete") => {
                let Some(name) = args.get(1) else {
                    return MacroCommandOutcome::Message(USAGE.into());
                };
                if self.macro_registry.delete(name.trim_start_matches('/')) {
                    MacroCommandOutcome::Message(format!("Deleted macro '{name}'."))
                } else {
                    MacroCommandOutcome::Message(format!("No macro named '{name}'."))
                }
            }
            Some("run") => {
                let Some(name) = args.get(1) else {
                    return MacroCommandOutcome::Message(USAGE.into());
                };
                self.macro_registry
                    .get(name.trim_start_matches('/'))
                    .map_or_else(
                        || MacroCommandOutcome::Message(format!("No macro named '{name}'.")),
                        |commands| MacroCommandOutcome::Run(commands.to_vec()),
                    )
            }
            Some(other) => MacroCommandOutcome::Message(format!(
                "Unknown /macro subcommand '{other}'.\n{USAGE}"
            )),
        }
    }

    fn handle_auto_dream_command(&mut self, args: &[String]) -> String {
        let Some(dream) = &self.auto_dream else {
            return "Auto-dream is not initialized.".into();
        };

        if args.is_empty() {
            return "Usage: /auto-dream <enable|disable|status>".into();
        }

        match args[0].as_str() {
            "enable" => {
                let mut config = dream.config.clone();
                config.enabled = true;
                if args.len() >= 2 {
                    if let Ok(n) = args[1].parse::<usize>() {
                        config.session_threshold = n;
                    }
                }
                if args.len() >= 3 {
                    if let Ok(n) = args[2].parse::<i64>() {
                        config.time_threshold_hours = n;
                    }
                }
                // Record a session to trigger immediate check if threshold is 1
                let _ = dream.record_session();

                // Spawn the watcher if API client is available
                if let Some(api) = &self.api_client {
                    let dream = dream.clone();
                    let api = api.clone_box();
                    let model = self.model.clone();
                    let _memory_dir = dream.memory_dir.clone();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    self.dream_shutdown_tx = Some(shutdown_tx);

                    tokio::spawn(async move {
                        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
                        let watcher_handle = tokio::spawn(async move {
                            // Collect recent session summaries as context
                            let recent_sessions: Vec<String> = Vec::new();
                            dream
                                .spawn_watcher_with_model(
                                    tx,
                                    std::time::Duration::from_secs(30),
                                    shutdown_rx,
                                    api.as_ref(),
                                    &model,
                                    recent_sessions,
                                )
                                .await;
                        });

                        // Forward dream results to memory dir
                        while let Some(session) = rx.recv().await {
                            tracing::info!(
                                "Auto-dream completed: analyzed {} sessions",
                                session.sessions_analyzed
                            );
                            // Dream artifacts are already written by complete_dream
                            let _ = session;
                        }
                        let _ = watcher_handle.await;
                    });

                    format!(
                        "Auto-dream enabled (threshold: {} sessions or {}h)",
                        config.session_threshold, config.time_threshold_hours
                    )
                } else {
                    "Auto-dream requires an API client and memory store.".into()
                }
            }
            "disable" => self.dream_shutdown_tx.take().map_or_else(
                || "Auto-dream is not running.".into(),
                |tx| {
                    let _ = tx.send(());
                    "Auto-dream disabled.".into()
                },
            ),
            "status" => {
                let config = &dream.config;
                let running = self.dream_shutdown_tx.is_some();
                let threshold = dream.check_threshold();
                let threshold_msg = match threshold {
                    DreamCheckResult::ShouldDream {
                        sessions_since_last,
                        hours_since_last,
                    } => format!(
                        "thresholds met: {sessions_since_last} sessions, {hours_since_last}h since last dream"
                    ),
                    DreamCheckResult::NotReady => "thresholds not yet met".into(),
                    DreamCheckResult::InProgress => "dream in progress".into(),
                };
                format!(
                    "Auto-dream: {} | config: {} sessions or {}h | {threshold_msg}",
                    if running { "running" } else { "stopped" },
                    config.session_threshold,
                    config.time_threshold_hours,
                )
            }
            _ => "Usage: /auto-dream <enable|disable|status>".into(),
        }
    }

    /// Handle /team-memory command.
    fn handle_team_memory_command(&self, args: &[String]) -> String {
        let Some(svc) = &self.team_memory_service else {
            return "Team memory sync is not initialized.".into();
        };

        if args.is_empty() {
            return "Usage: /team-memory <upsert|read|list|delete|sync> [args...]".into();
        }

        match args[0].as_str() {
            "upsert" => {
                if args.len() < 3 {
                    return "Usage: /team-memory upsert <topic> <content>".into();
                }
                let topic = &args[1];
                let content = args[2..].join(" ");
                match svc.upsert(topic, &content) {
                    Ok(entry) => format!(
                        "Team memory upserted: '{}' (checksum: {})",
                        entry.topic,
                        entry.checksum.to_hex()
                    ),
                    Err(e) => format!("Failed to upsert team memory: {e}"),
                }
            }
            "read" => {
                if args.len() < 2 {
                    return "Usage: /team-memory read <topic>".into();
                }
                match svc.read(&args[1]) {
                    Ok(Some(entry)) => format!("[{}]\n{}", entry.topic, entry.content),
                    Ok(None) => "No entry found.".into(),
                    Err(e) => format!("Failed to read team memory: {e}"),
                }
            }
            "list" => match svc.list() {
                Ok(entries) if entries.is_empty() => "No team memories.".into(),
                Ok(entries) => entries
                    .iter()
                    .map(|e| {
                        format!(
                            "- {} (updated: {})",
                            e.topic,
                            e.updated_at.format("%Y-%m-%d %H:%M")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => format!("Failed to list team memories: {e}"),
            },
            "delete" => {
                if args.len() < 2 {
                    return "Usage: /team-memory delete <topic>".into();
                }
                match svc.delete(&args[1]) {
                    Ok(()) => "Team memory deleted.".into(),
                    Err(e) => format!("Failed to delete team memory: {e}"),
                }
            }
            "sync" => {
                // Sync via in-memory relay or HTTP transport
                // For now, show local state info
                match svc.list() {
                    Ok(entries) => {
                        format!(
                            "Local team memories: {} entries. Use /team-memory sync --remote to sync with relay.",
                            entries.len()
                        )
                    }
                    Err(e) => format!("Failed to list for sync: {e}"),
                }
            }
            _ => "Usage: /team-memory <upsert|read|list|delete|sync> [args...]".into(),
        }
    }

    /// Handle /workspace command — switch or show current workspace directory.
    fn handle_workspace_command(&mut self, args: &[String]) -> String {
        if args.is_empty() {
            return format!("Current workspace: {}", self.cwd);
        }
        let path = &args[0];
        let expanded = if let Some(stripped) = path.strip_prefix('~') {
            if let Some(home) = home::home_dir() {
                home.join(stripped).to_string_lossy().to_string()
            } else {
                return "Could not expand ~ path.".into();
            }
        } else {
            path.clone()
        };
        let target = std::path::Path::new(&expanded);
        if !target.exists() {
            return format!("Workspace path does not exist: {expanded}");
        }
        if !target.is_dir() {
            return format!("Path is not a directory: {expanded}");
        }
        let abs = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());
        let old = std::mem::replace(&mut self.cwd, abs.to_string_lossy().to_string());
        format!("Workspace switched: {old} → {}", self.cwd)
    }

    /// Handle /recover command — restore session from crash recovery snapshot.
    fn handle_recover_command(&self) -> String {
        let Some(state) = crate::recovery::load(&self.cwd) else {
            return "No recovery snapshot found.".into();
        };
        format!(
            "Recovery snapshot found: session={}, messages={}, last_input='{}', saved_at={}",
            state.session_id, state.message_count, state.last_input, state.saved_at
        )
    }

    /// Handle /update-check command — check for new versions.
    async fn handle_update_check_command(&self) -> String {
        crate::version::check_for_updates()
            .await
            .unwrap_or_else(|| {
                format!(
                    "You are running the latest version ({})",
                    crate::version::CURRENT_VERSION
                )
            })
    }

    /// Handle /feedback command — generate a title for user feedback.
    async fn handle_feedback_command(&self, args: &[String]) -> String {
        let text = args.join(" ");
        if text.is_empty() {
            return "Use /feedback <message> to generate a title for your feedback.".into();
        }
        let Some(ref client) = self.api_client else {
            return "No API client available.".into();
        };
        match coderex_core::prompts::generate_session_title(client.as_ref(), &text).await {
            Ok(title) => format!("Feedback title: \"{title}\"\n\nFeedback: {text}"),
            Err(e) => format!("Failed to generate title: {e}"),
        }
    }

    /// Handle the /teleport command: directory switching, share, join, list, revoke.
    fn handle_teleport_command(&mut self, args: &[String]) -> String {
        let mgr = match self.teleport_manager {
            Some(ref mgr) => mgr.clone(),
            None => return "Teleport manager not initialized.".into(),
        };

        if args.is_empty() {
            return "Usage: /teleport <directory> | --share | --join <token> | --list | --revoke <token>".into();
        }

        match args[0].as_str() {
            "--share" => {
                let session = self.engine.as_ref().map(|e| e.session().clone());
                session.map_or_else(
                    || "No active session to share.".into(),
                    |session| match mgr.share_session(&session) {
                        Ok(token) => format!("Share token: {token}"),
                        Err(e) => format!("Failed to share session: {e}"),
                    },
                )
            }
            "--join" => {
                if args.len() < 2 {
                    return "Usage: /teleport --join <token>".into();
                }
                let token = &args[1];
                match mgr.join_session(token) {
                    Ok(state) => {
                        let new_cwd = state.cwd.to_string_lossy().to_string();
                        self.cwd.clone_from(&new_cwd);
                        let _ = std::env::set_current_dir(&new_cwd);
                        if let Some(ref mut engine) = self.engine {
                            engine.set_session(state);
                        }
                        format!("Joined shared session. CWD: {new_cwd}")
                    }
                    Err(e) => format!("Failed to join session: {e}"),
                }
            }
            "--list" => {
                let shares = mgr.list_shares();
                if shares.is_empty() {
                    "No active shares.".into()
                } else {
                    format!("Active shares: {}", shares.join(", "))
                }
            }
            "--revoke" => {
                if args.len() < 2 {
                    return "Usage: /teleport --revoke <token>".into();
                }
                let token = &args[1];
                match mgr.revoke_share(token) {
                    Ok(true) => format!("Revoked share: {token}"),
                    Ok(false) => format!("Share not found: {token}"),
                    Err(e) => format!("Failed to revoke share: {e}"),
                }
            }
            dir => {
                let target_dir = std::path::PathBuf::from(dir);
                if !target_dir.exists() {
                    return format!("Directory does not exist: {dir}");
                }
                if !target_dir.is_dir() {
                    return format!("Not a directory: {dir}");
                }

                // Save current session to current directory
                if let Some(ref engine) = self.engine {
                    let session = engine.session().clone();
                    if let Err(e) = mgr.save_to_directory(&session, std::path::Path::new(&self.cwd))
                    {
                        return format!("Failed to save session: {e}");
                    }
                }

                // Try to load session from target directory
                let loaded = match mgr.load_from_directory(&target_dir) {
                    Ok(state) => state,
                    Err(e) => return format!("Failed to load session: {e}"),
                };

                // Update CWD and process working directory
                let new_cwd = target_dir
                    .canonicalize()
                    .unwrap_or(target_dir)
                    .to_string_lossy()
                    .to_string();
                self.cwd.clone_from(&new_cwd);
                let _ = std::env::set_current_dir(&new_cwd);

                if let Some(ref mut engine) = self.engine {
                    if let Some(state) = loaded {
                        engine.set_session(state);
                    } else {
                        let mut fresh = SessionState::new(std::path::PathBuf::from(&new_cwd));
                        fresh.session_id = engine.session().session_id;
                        engine.set_session(fresh);
                    }
                }

                format!("Teleported to: {new_cwd}")
            }
        }
    }

    /// Handle `/agents` — inspect sub-agents and their event logs.
    ///
    /// Three subcommands, all backed by the real [`AgentManager`]:
    /// - `list` — the agents this session has spawned and their status.
    /// - `logs <id> [count]` — read the agent's NDJSON event log, the same
    ///   file the manager writes while the agent runs. Reading it is the
    ///   production consumer of that log.
    /// - `follow <id|all>` — point the session's observable subscription at a
    ///   single agent (or back at all of them). This is the production user of
    ///   the per-agent filter that used to be exercised only by tests.
    async fn handle_agents_command(&mut self, args: &[String]) -> String {
        let Some(manager) = self.agent_manager.as_ref().map(Arc::clone) else {
            return "Agent manager not initialized.".into();
        };

        match args.first().map(String::as_str) {
            None | Some("list") => {
                let manager = manager.lock().await;
                let agents = manager.list_agents();
                if agents.is_empty() {
                    return "No agents have been spawned in this session.".into();
                }
                let mut lines = vec![format!("{} agent(s):", agents.len())];
                for agent in agents {
                    lines.push(format!(
                        "  {} [{}] {}",
                        agent.task_handle.id,
                        format!("{:?}", agent.status()).to_lowercase(),
                        agent.prompt.lines().next().unwrap_or("").trim()
                    ));
                }
                // `lines` owns its strings; the borrow of the agent list ends
                // with the loop, so release the manager lock before returning.
                drop(manager);
                lines.join("\n")
            }
            Some("logs") => {
                let Some(agent_id) = args.get(1) else {
                    return "Usage: /agents logs <agent-id> [count]".into();
                };
                let count = match args.get(2) {
                    Some(raw) => match raw.parse::<usize>() {
                        Ok(n) if n > 0 => n,
                        _ => return format!("Invalid count {raw:?}; expected a positive integer."),
                    },
                    None => 20,
                };
                match coderex_agents::read_agent_log(agent_id) {
                    Err(error) => format!("Could not read the log for {agent_id}: {error}"),
                    Ok(events) if events.is_empty() => {
                        format!("No logged events for {agent_id}.")
                    }
                    Ok(events) => {
                        let total = events.len();
                        let shown = events.iter().rev().take(count).rev();
                        let mut lines = vec![format!(
                            "{total} event(s) for {agent_id} (showing last {}):",
                            total.min(count)
                        )];
                        for event in shown {
                            lines.push(format!("  {event:?}"));
                        }
                        lines.join("\n")
                    }
                }
            }
            Some("follow") => {
                let Some(target) = args.get(1) else {
                    return "Usage: /agents follow <agent-id|all>".into();
                };
                let manager = manager.lock().await;
                let subscription = manager.subscribe_to_agent(target);
                drop(manager);
                match subscription {
                    Some(rx) => {
                        self.agent_observable_rx = Some(rx);
                        if target == "all" {
                            "Following observable events from all agents.".into()
                        } else {
                            format!("Following observable events from agent {target}.")
                        }
                    }
                    None => "The agent manager has no event channel to subscribe to.".into(),
                }
            }
            Some(other) => {
                format!("Unknown /agents subcommand {other:?}. Usage: /agents <list|logs|follow>")
            }
        }
    }

    /// Connection state and server/conflict lists for the command context.
    fn mcp_state(&self) -> (bool, Vec<String>, Vec<String>) {
        let slot = self
            .mcp_manager
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(manager) = slot.as_ref() else {
            return (false, Vec::new(), Vec::new());
        };
        let servers = manager.list_servers();
        let connected = servers.iter().any(|name| manager.is_running(name));
        let conflicts = manager.tool_conflicts();
        drop(slot);
        (connected, servers, conflicts)
    }

    fn build_context(&self) -> CommandContext {
        let (session_id, session_title, message_count, permission_mode) =
            self.engine.as_ref().map_or_else(
                || {
                    (
                        "repl_session".to_string(),
                        "REPL Session".to_string(),
                        self.history.len(),
                        PermissionMode::default(),
                    )
                },
                |engine| {
                    let session = engine.session();
                    let permission_mode = engine
                        .permission_checker()
                        .lock()
                        .map(|checker| checker.mode())
                        .unwrap_or_default();
                    (
                        session.session_id.to_string(),
                        session
                            .title
                            .clone()
                            .unwrap_or_else(|| "REPL Session".to_string()),
                        session.messages.len(),
                        permission_mode,
                    )
                },
            );

        let mut ctx = CommandContext::new(
            self.cwd.clone(),
            session_id,
            session_title,
            self.model.clone(),
        );
        ctx.message_count = message_count;
        ctx.permission_mode = permission_mode;
        ctx.vim_mode = self.vim_mode_enabled;
        ctx.theme.clone_from(&self.current_theme);
        ctx.git_branch = current_git_branch(&self.cwd);
        ctx.version = env!("CARGO_PKG_VERSION").to_string();
        // Commands gate themselves by name (`is_feature_enabled`), so the
        // resolved flags have to reach the context. Leaving this empty made
        // every flag-gated command dead in production, even when the user
        // passed `--betas` or `CODEREX_FEATURE_FLAGS`.
        ctx.feature_flags = self.feature_flags.as_map();
        // These two fields were never assigned, so `/mcp`, `/status`,
        // `/doctor`, `/config` always said "disconnected" and `/hooks`
        // always said zero.
        ctx.active_hooks = self.active_hooks;
        let (mcp_connected, mcp_servers, mcp_conflicts) = self.mcp_state();
        ctx.mcp_connected = mcp_connected;
        ctx.mcp_servers = mcp_servers;
        ctx.mcp_conflicts = mcp_conflicts;
        let mut components: Vec<(String, String)> = Vec::new();
        for (name, _) in &self.plugin_themes {
            components.push(("theme".into(), name.clone()));
        }
        for (name, _) in &self.plugin_output_styles {
            components.push(("output-style".into(), name.clone()));
        }
        for (name, _) in &self.plugin_lsp_servers {
            components.push(("lsp-server".into(), name.clone()));
        }
        if let Some(monitors) = self.plugin_monitors.as_ref().and_then(|m| m.as_object()) {
            for name in monitors.keys() {
                components.push(("monitor".into(), name.clone()));
            }
        }
        for (kind, names) in &self.plugin_other_components {
            for name in names {
                components.push((kind.clone(), name.clone()));
            }
        }
        ctx.plugin_components = components;
        ctx.plugin_themes = self
            .plugin_themes
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        // Remote environment variables live in their own file, never in
        // settings (a repository-controlled settings file must not be able to
        // inject environment variables).
        ctx.env = coderex_commands::load_remote_env();
        ctx.recent_commands = self
            .command_history
            .list(1000)
            .iter()
            .map(|entry| {
                if entry.args.is_empty() {
                    format!("/{}", entry.cmd)
                } else {
                    format!("/{} {}", entry.cmd, entry.args.join(" "))
                }
            })
            .collect();
        ctx.api_key_set = self.api_client.is_some();
        ctx.elapsed_secs = self.query_start_time.map_or(0, |t| t.elapsed().as_secs());
        if let Some(ref engine) = self.engine {
            ctx.cost_summary = engine.cost_summary();
            let usage = engine.total_usage();
            ctx.input_tokens = usage.input_tokens;
            ctx.output_tokens = usage.output_tokens;
            ctx.cache_read_tokens = usage.cache_read_tokens;
            ctx.cache_creation_tokens = usage.cache_creation_tokens;
            ctx.cost_usd = engine.session().cost_usd;
        }
        ctx
    }
}

/// Get the current git branch for the statusline.
fn current_git_branch(cwd: &str) -> String {
    coderex_core::git_hardening::git_command()
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Session state for the REPL.
#[derive(Debug)]
pub struct ReplSession {
    /// The session ID.
    pub id: String,
    /// The session title.
    pub title: String,
    /// Whether the session is active.
    pub active: bool,
    /// Message count.
    pub message_count: usize,
}

impl ReplSession {
    /// Create a new session.
    #[must_use]
    pub const fn new(id: String, title: String) -> Self {
        Self {
            id,
            title,
            active: true,
            message_count: 0,
        }
    }
}

/// Parse a line of input into a command and arguments.
#[must_use]
pub fn parse_input(line: &str) -> Option<(&str, Vec<String>)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('/') {
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let cmd = parts[0].trim_start_matches('/');
        let args: Vec<String> = parts[1..]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        Some((cmd, args))
    } else {
        Some(("message", vec![trimmed.to_string()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::api_client::{ApiError, ApiResponse, StreamEvent};
    use coderex_core::tool_executor::{
        ToolDispatchOutcome, ToolExecutor as CoreToolExecutor, ToolInfo,
    };
    use coderex_core::types::{ToolResult, UsageSummary};

    // ── Mod UI focus ────────────────────────────────────────────────────────

    #[test]
    fn mod_ui_keys_map_to_focus_and_activation_only_when_focused() {
        use crossterm::event::{KeyCode, KeyModifiers};

        assert_eq!(
            mod_ui_key_action(KeyCode::Tab, KeyModifiers::NONE, None, 2),
            Some(ModUiKeyAction::FocusNext),
            "Tab starts focus on the first button"
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Tab, KeyModifiers::NONE, Some(0), 2),
            Some(ModUiKeyAction::FocusNext)
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::BackTab, KeyModifiers::SHIFT, Some(0), 2),
            Some(ModUiKeyAction::FocusPrevious)
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Tab, KeyModifiers::SHIFT, Some(1), 2),
            Some(ModUiKeyAction::FocusPrevious)
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Enter, KeyModifiers::NONE, Some(1), 2),
            Some(ModUiKeyAction::Activate)
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Esc, KeyModifiers::NONE, Some(1), 2),
            Some(ModUiKeyAction::ClearFocus)
        );
    }

    #[test]
    fn mod_ui_keys_leave_ordinary_typing_alone() {
        use crossterm::event::{KeyCode, KeyModifiers};

        assert_eq!(
            mod_ui_key_action(KeyCode::Char('a'), KeyModifiers::NONE, Some(0), 2),
            None,
            "typing must reach the input line"
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Backspace, KeyModifiers::NONE, Some(0), 2),
            None
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Enter, KeyModifiers::NONE, None, 2),
            None,
            "Enter submits when no button is focused"
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Esc, KeyModifiers::NONE, None, 2),
            None
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::BackTab, KeyModifiers::SHIFT, None, 2),
            None,
            "unfocused Shift+Tab still belongs to the permission bar"
        );
        assert_eq!(
            mod_ui_key_action(KeyCode::Tab, KeyModifiers::NONE, Some(0), 0),
            None,
            "a block with no buttons has no focus"
        );
    }

    #[test]
    fn focus_cycles_and_wraps() {
        assert_eq!(cycle_mod_ui_focus(None, 3, true), 0);
        assert_eq!(cycle_mod_ui_focus(None, 3, false), 2);
        assert_eq!(cycle_mod_ui_focus(Some(2), 3, true), 0);
        assert_eq!(cycle_mod_ui_focus(Some(0), 3, false), 2);
    }

    #[test]
    fn mod_ui_focus_walks_nested_buttons_and_clears() {
        use coderex_core::ui_protocol::UiElement;

        let tree = vec![
            UiElement::Box {
                title: None,
                children: vec![
                    UiElement::Button {
                        label: "A".to_string(),
                        id: "a".to_string(),
                    },
                    UiElement::Button {
                        label: "B".to_string(),
                        id: "b".to_string(),
                    },
                ],
            },
            UiElement::Button {
                label: "C".to_string(),
                id: "c".to_string(),
            },
        ];

        let mut focus = ModUiFocus::default();
        focus.reset(&tree, 7);
        assert_eq!(focus.button_count(), 3);
        assert_eq!(focus.focused_id(), None);
        assert_eq!(focus.message_index, Some(7));

        focus.cycle(true);
        assert_eq!(focus.focused_id(), Some("a"));
        focus.cycle(false);
        assert_eq!(focus.focused_id(), Some("c"));

        focus.clear_focus();
        assert_eq!(focus.focused_id(), None);

        focus.reset(
            &[UiElement::Text {
                text: "nothing to press".to_string(),
                style: coderex_core::ui_protocol::UiStyle::Plain,
            }],
            9,
        );
        assert_eq!(focus.button_count(), 0);
        assert_eq!(
            focus.message_index, None,
            "a block without buttons owns no keyboard focus"
        );
    }

    /// A hook executor that records what fired, for the lifecycle tests.
    #[derive(Clone, Default)]
    struct RecordingHookExecutor {
        events:
            Arc<std::sync::Mutex<Vec<(String, String, std::collections::HashMap<String, String>)>>>,
    }

    #[async_trait::async_trait]
    impl coderex_core::hook_executor::HookExecutor for RecordingHookExecutor {
        async fn fire_hook(
            &self,
            event_name: &str,
            input: &str,
            context: &std::collections::HashMap<String, String>,
        ) -> coderex_core::hook_executor::HookFireResult {
            self.events
                .lock()
                .expect("the recording hook lock is not poisoned")
                .push((event_name.to_string(), input.to_string(), context.clone()));
            coderex_core::hook_executor::HookFireResult::default()
        }
    }

    /// `/init` is the product's setup action: a successful run queues
    /// `Setup`, while an error result must not.
    #[test]
    fn setup_is_queued_only_for_a_successful_init() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());

        repl.sync_command_state("init", &CommandResult::success("Initialized project"));
        assert_eq!(repl.pending_setup.as_deref(), Some("/init"));

        repl.pending_setup = None;
        repl.sync_command_state("init", &CommandResult::error("permission denied"));
        assert!(
            repl.pending_setup.is_none(),
            "a failed /init must not queue the Setup hook"
        );
    }

    /// Only `/init` queues setup; another successful command leaves the flag
    /// alone.
    #[test]
    fn a_non_init_command_does_not_queue_setup() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.sync_command_state("version", &CommandResult::success("CodeRex 0.1.0"));
        assert!(repl.pending_setup.is_none());
    }

    /// The async drain fires the queued `Setup` hook exactly once and clears
    /// the flag, so the next turn cannot replay it.
    #[tokio::test]
    async fn draining_setup_fires_the_hook_exactly_once_and_clears_it() {
        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        let recorder = RecordingHookExecutor::default();
        let events = Arc::clone(&recorder.events);
        repl.engine
            .as_mut()
            .expect("with_engine installs an engine")
            .set_hooks(Arc::new(recorder));

        repl.sync_command_state("init", &CommandResult::success("Initialized project"));
        repl.drain_pending_setup_hook().await;

        assert!(
            repl.pending_setup.is_none(),
            "the drain must clear the flag"
        );
        {
            let recorded = events.lock().expect("the recording hook lock is clean");
            assert_eq!(
                recorded.len(),
                1,
                "Setup fired more than once: {recorded:?}"
            );
            assert_eq!(recorded[0].0, "Setup");
            assert_eq!(recorded[0].1, "/init");
            assert_eq!(
                recorded[0].2.get("command").map(String::as_str),
                Some("/init")
            );
        }

        repl.drain_pending_setup_hook().await;
        assert_eq!(
            events
                .lock()
                .expect("the recording hook lock is clean")
                .len(),
            1,
            "a second drain fired the hook again"
        );
    }

    #[test]
    fn command_context_carries_the_session_feature_flags() {
        // The command context used to be built with an empty map, so a command
        // that asked whether its flag was on always heard "no".
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.feature_flags = coderex_config::flags::FeatureFlags::parse("+voice_mode,+heapdump");
        let ctx = repl.build_context();
        assert!(ctx.is_feature_enabled("voice_mode"));
        assert!(ctx.is_feature_enabled("heapdump"));
        assert!(!ctx.is_feature_enabled("ultraplan"));
    }

    #[tokio::test]
    async fn macro_command_defines_lists_runs_and_deletes() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());

        let mut out = Vec::new();
        repl.execute_command("/macro define greet /version", &mut out)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("Defined macro 'greet'"));

        let mut out = Vec::new();
        repl.execute_command("/macro list", &mut out).await.unwrap();
        assert!(String::from_utf8_lossy(&out).contains("greet"));

        let mut out = Vec::new();
        repl.execute_command("/macro run greet", &mut out)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("CodeRex"),
            "the macro did not run its command: {}",
            String::from_utf8_lossy(&out)
        );

        let mut out = Vec::new();
        repl.execute_command("/macro delete greet", &mut out)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("Deleted macro 'greet'"));
        assert!(!repl.macro_registry.has("greet"));
    }

    #[tokio::test]
    async fn a_named_macro_can_run_a_builtin_command() {
        // Macro expansion used to call the registry-only helper, so a macro
        // whose segment was a built-in (here /exit) did nothing in headless
        // mode. It now goes through the full entry path.
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.macro_registry.define("bye", vec!["/exit".into()]);
        let mut out = Vec::new();
        repl.execute_command("/bye", &mut out).await.unwrap();
        assert!(!repl.running, "the macro's built-in did not run");
    }

    #[tokio::test]
    async fn agents_command_lists_follows_and_reads_logs() {
        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );

        // With nothing spawned, `list` says so rather than printing an empty table.
        let mut out = Vec::new();
        repl.execute_command("/agents list", &mut out)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("No agents have been spawned"),
            "{}",
            String::from_utf8_lossy(&out)
        );

        // `follow <id>` swaps the manager-wide subscription for a filtered one.
        let mut out = Vec::new();
        repl.execute_command("/agents follow agent-42", &mut out)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("agent-42"));
        assert_eq!(
            repl.agent_observable_rx.as_ref().unwrap().agent_id(),
            Some("agent-42"),
            "the production subscription should be filtered to the followed agent"
        );

        // `follow all` restores the manager-wide subscription.
        let mut out = Vec::new();
        repl.execute_command("/agents follow all", &mut out)
            .await
            .unwrap();
        assert!(
            repl.agent_observable_rx
                .as_ref()
                .unwrap()
                .agent_id()
                .is_none(),
            "`all` should clear the per-agent filter"
        );

        // A log for an agent that never ran is empty, not an error.
        let mut out = Vec::new();
        repl.execute_command("/agents logs repl-test-never-ran", &mut out)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("No logged events"),
            "{}",
            String::from_utf8_lossy(&out)
        );

        let mut out = Vec::new();
        repl.execute_command("/agents logs", &mut out)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("Usage: /agents logs"));

        let mut out = Vec::new();
        repl.execute_command("/agents wat", &mut out).await.unwrap();
        assert!(String::from_utf8_lossy(&out).contains("Unknown /agents subcommand"));
    }

    #[test]
    fn tool_events_are_recorded_in_telemetry() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = coderex_telemetry::TelemetryStore::open(file.path()).unwrap();
        let (handle, _uploader) = coderex_telemetry::TelemetryHandle::new(&store, None);

        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_telemetry_handle(Some(handle.clone()));
        repl.record_engine_event_telemetry(&EngineEvent::ToolCallStart {
            id: "call-1".into(),
            name: "Read".into(),
            summary: "read a file".into(),
        });
        repl.record_engine_event_telemetry(&EngineEvent::ToolCallEnd {
            id: "call-1".into(),
            result: ToolResult {
                tool_call_id: "call-1".into(),
                output: "ok".into(),
                is_error: false,
                images: Vec::new(),
            },
        });
        handle.shutdown();

        let store = coderex_telemetry::TelemetryStore::open(file.path()).unwrap();
        let events = store.pending_events(10).unwrap();
        assert!(
            events.iter().any(|event| event.event_type == "tool_used"),
            "tool_used was not recorded: {events:?}"
        );
    }

    #[test]
    fn startup_preferences_from_settings_are_applied() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_startup_preferences(Some(true), Some("light".into()));
        repl.apply_cli_config();
        assert!(repl.vim_mode_enabled, "vim_mode was not applied");
        assert_eq!(repl.current_theme, "light");
    }

    #[test]
    fn command_context_reports_installed_hooks_and_mcp_state() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.active_hooks = 2;
        let ctx = repl.build_context();
        assert_eq!(ctx.active_hooks, 2, "the hook count was not reported");
        assert!(
            !ctx.mcp_connected,
            "a session with no MCP manager must report disconnected"
        );
    }

    #[test]
    fn resume_resolution_handles_exact_unique_ambiguous_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().into_owned();
        let repl = ReplLoop::new(cwd.clone(), ModelId::sonnet());

        // An exact id needs only the transcript file.
        let transcripts = session_resume::transcript_dir(&cwd);
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join("sess-exact.jsonl"), "").unwrap();
        assert_eq!(
            repl.resolve_resume_session_id(Some("sess-exact")).unwrap(),
            "sess-exact"
        );

        // A fragment resolves through the index.
        session_resume::index_saved_session(&cwd, "sess-a", Some("deploy pipeline"), &[]);
        assert_eq!(
            repl.resolve_resume_session_id(Some("deploy")).unwrap(),
            "sess-a"
        );

        // Two matches are listed, not guessed.
        session_resume::index_saved_session(&cwd, "sess-b", Some("deploy rollback"), &[]);
        let error = repl
            .resolve_resume_session_id(Some("deploy"))
            .expect_err("an ambiguous fragment must be refused");
        assert!(error.contains("matches several"), "{error}");
        assert!(
            error.contains("sess-a") && error.contains("sess-b"),
            "{error}"
        );

        // No match falls through to the transcript-load error path.
        assert_eq!(
            repl.resolve_resume_session_id(Some("nothing-matches"))
                .unwrap(),
            "nothing-matches"
        );
    }

    #[test]
    fn command_context_includes_recent_command_lines() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.command_history.append("model", &["opus".into()]);
        repl.command_history.append("diff", &[]);
        let ctx = repl.build_context();
        assert!(ctx.recent_commands.contains(&"/model opus".to_string()));
        assert!(ctx.recent_commands.contains(&"/diff".to_string()));
    }

    #[tokio::test]
    async fn disabled_slash_commands_skip_builtins_and_macros() {
        // `--disable-slash-commands` used to clear the registry only: the
        // built-ins and macro expansion still ran, so the flag hid commands
        // from `/help` without disabling them.
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_no_slash_commands(true);
        // Production applies the flag through `apply_cli_config`, which is
        // also what clears the registry.
        repl.apply_cli_config();
        repl.macro_registry
            .define("show-help", vec!["/help".into()]);

        let mut out = Vec::new();
        repl.execute_command("/help", &mut out).await.unwrap();
        let direct = String::from_utf8_lossy(&out).into_owned();
        assert!(
            !direct.contains("Available commands"),
            "built-in /help ran despite the flag: {direct}"
        );

        let mut out = Vec::new();
        repl.execute_command("/show-help", &mut out).await.unwrap();
        let via_macro = String::from_utf8_lossy(&out).into_owned();
        assert!(
            !via_macro.contains("Available commands"),
            "a macro expanded despite the flag: {via_macro}"
        );
    }

    #[derive(Clone)]
    struct StubToolExecutor {
        tools: Vec<ToolInfo>,
    }

    #[async_trait::async_trait]
    impl CoreToolExecutor for StubToolExecutor {
        fn list_tools(&self) -> Vec<ToolInfo> {
            self.tools.clone()
        }

        async fn execute(
            &self,
            tool_call_id: &str,
            _tool_name: &str,
            _input: &serde_json::Value,
        ) -> ToolDispatchOutcome {
            ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: "ok".into(),
                    is_error: false,
                    images: Vec::new(),
                },
            }
        }
    }

    #[derive(Clone)]
    struct FixedResultToolExecutor {
        output: String,
        is_error: bool,
    }

    #[async_trait::async_trait]
    impl CoreToolExecutor for FixedResultToolExecutor {
        async fn execute(
            &self,
            tool_call_id: &str,
            _tool_name: &str,
            _input: &serde_json::Value,
        ) -> ToolDispatchOutcome {
            ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: self.output.clone(),
                    is_error: self.is_error,
                    images: Vec::new(),
                },
            }
        }
    }

    /// Two-turn API double: the sub-agent must read `note.txt` with the real
    /// tool dispatcher before it can produce the successful final answer.
    #[derive(Clone)]
    struct ReadThenAnswerApi {
        call: Arc<std::sync::Mutex<usize>>,
        expected_contents: &'static str,
    }

    #[async_trait::async_trait]
    impl ApiClient for ReadThenAnswerApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            let call = {
                let mut call = self.call.lock().unwrap();
                *call += 1;
                *call
            };
            let usage = UsageSummary {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            if call == 1 {
                return Ok(ApiResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "child_read".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "note.txt"}),
                    }],
                    thinking: None,
                    usage,
                    stop_reason: "tool_use".into(),
                });
            }

            let saw_tool_output = messages
                .iter()
                .any(|message| message.to_string().contains(self.expected_contents));
            Ok(ApiResponse {
                content: if saw_tool_output {
                    "sub-agent read the file"
                } else {
                    "sub-agent could not read the file"
                }
                .into(),
                tool_calls: Vec::new(),
                thinking: None,
                usage,
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }

    /// Counts the turn's own requests, so a test can tell "the turn ran" from
    /// "only the side queries around it ran".
    ///
    /// A headless turn goes through `submit_streaming`, and this stream is
    /// empty, so the turn produces no text at all — the answer string is not a
    /// usable signal. The count is.
    #[derive(Clone)]
    struct CountingApi {
        turn_requests: Arc<std::sync::Mutex<usize>>,
        /// What a side query (a hook's `prompt`, a title, a classifier) answers.
        side_answer: &'static str,
    }

    impl CountingApi {
        fn new() -> Self {
            Self {
                turn_requests: Arc::new(std::sync::Mutex::new(0)),
                side_answer: "Side query answer",
            }
        }

        /// A side query that answers with hook JSON, for the hook kinds that run
        /// through the session's model client.
        fn answering_hooks_with(answer: &'static str) -> Self {
            Self {
                turn_requests: Arc::new(std::sync::Mutex::new(0)),
                side_answer: answer,
            }
        }

        fn turn_requests(&self) -> usize {
            *self.turn_requests.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl ApiClient for CountingApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            Ok(ApiResponse {
                content: self.side_answer.into(),
                tool_calls: Vec::new(),
                thinking: None,
                usage: UsageSummary {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            *self.turn_requests.lock().unwrap() += 1;
            Box::pin(futures::stream::empty())
        }
    }

    /// A model client that changes a watched file during the turn and waits
    /// for the `FileChanged` hook to acknowledge it.
    ///
    /// Waiting on the marker rather than sleeping a fixed time makes the
    /// headless watcher test deterministic: the turn cannot finish before the
    /// hook has run.
    #[cfg(feature = "hot-reload")]
    #[derive(Clone)]
    struct FileChangingApi {
        watched: std::path::PathBuf,
        marker: std::path::PathBuf,
    }

    #[cfg(feature = "hot-reload")]
    #[async_trait::async_trait]
    impl ApiClient for FileChangingApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            Ok(ApiResponse {
                content: String::new(),
                tool_calls: Vec::new(),
                thinking: None,
                usage: UsageSummary {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            std::fs::write(&self.watched, "changed during the turn").unwrap();

            // Give the watcher and the hook subprocess a bounded window. If
            // the hook never runs the test fails on the assertion, not by
            // hanging forever.
            for _ in 0..200 {
                if self.marker.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }

            Box::pin(futures::stream::empty())
        }
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_input("").is_none());
        assert!(parse_input("   ").is_none());
    }

    #[test]
    fn parse_command() {
        let result = parse_input("/clear").unwrap();
        assert_eq!(result.0, "clear");
        assert_eq!(result.1, [] as [std::string::String; 0]);
    }

    #[test]
    fn parse_command_with_args() {
        let result = parse_input("/model sonnet").unwrap();
        assert_eq!(result.0, "model");
        assert_eq!(result.1, vec!["sonnet"]);
    }

    #[test]
    fn parse_message() {
        let result = parse_input("Hello, world!").unwrap();
        assert_eq!(result.0, "message");
        assert_eq!(result.1, vec!["Hello, world!"]);
    }

    #[test]
    fn parse_command_multiple_args() {
        let result = parse_input("/btw this is important").unwrap();
        assert_eq!(result.0, "btw");
        assert_eq!(result.1, vec!["this", "is", "important"]);
    }

    #[test]
    fn repl_loop_new() {
        let loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        assert!(!loop_obj.commands.is_empty());
    }

    #[test]
    fn arc_tool_executor_forwards_list_tools() {
        let tool = ToolInfo {
            name: "Read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> =
            Arc::new(StubToolExecutor { tools: vec![tool] });
        let wrapped = ArcToolExecutor(inner);
        let tools = wrapped.list_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
    }

    #[test]
    fn plugin_fallback_executor_advertises_primary_tools() {
        let tool = ToolInfo {
            name: "Read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> =
            Arc::new(StubToolExecutor { tools: vec![tool] });
        let primary = ArcToolExecutor(inner);
        let plugin = Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
            Arc::new(coderex_plugins::plugins::PluginManager::new()),
        ));
        let wrapped = PluginToolFallbackExecutor { primary, plugin };
        let tools = wrapped.list_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
    }

    #[tokio::test]
    async fn plugin_fallback_preserves_primary_tool_errors_that_mention_not_found() {
        let primary_output =
            "Skill 'frontend-design' not found.\nAvailable skills: pdf".to_string();
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> = Arc::new(FixedResultToolExecutor {
            output: primary_output.clone(),
            is_error: true,
        });
        let primary = ArcToolExecutor(inner);
        let plugin = Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
            Arc::new(coderex_plugins::plugins::PluginManager::new()),
        ));
        let wrapped = PluginToolFallbackExecutor { primary, plugin };

        let result = wrapped
            .execute(
                "tool_1",
                "Skill",
                &serde_json::json!({"skill":"frontend-design"}),
            )
            .await
            .result;

        assert_eq!(result.output, primary_output);
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn plugin_fallback_runs_only_when_primary_tool_is_missing() {
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> = Arc::new(FixedResultToolExecutor {
            output: "tool 'PluginCommand' not found in registry".into(),
            is_error: true,
        });
        let primary = ArcToolExecutor(inner);
        let plugin = Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
            Arc::new(coderex_plugins::plugins::PluginManager::new()),
        ));
        let wrapped = PluginToolFallbackExecutor { primary, plugin };

        let result = wrapped
            .execute("tool_1", "PluginCommand", &serde_json::json!({}))
            .await
            .result;

        assert_eq!(result.output, "Plugin tool 'PluginCommand' not found");
        assert!(result.is_error);
    }

    #[test]
    fn repl_session_new() {
        let session = ReplSession::new("s1".into(), "Test".into());
        assert_eq!(session.id, "s1");
        assert!(session.active);
        assert_eq!(session.message_count, 0);
    }

    #[tokio::test]
    async fn headless_command() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/status").await.unwrap();
        assert!(result.contains("System Status"));
    }

    #[tokio::test]
    async fn headless_message_without_engine() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("Hello").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("QueryEngine"),
            "expected QueryEngine in error: {err}"
        );
    }

    #[tokio::test]
    async fn headless_help() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/help").await.unwrap();
        assert!(result.contains("System Status") || result.contains("Available"));
    }

    /// A mod's UI block joins the headless output at the point the event was
    /// drained, instead of being printed live from the event loop.
    #[tokio::test]
    async fn headless_output_contains_a_rendered_mod_ui_block() {
        let mut repl = ReplLoop::new("/src".into(), ModelId::sonnet());
        let (tx, rx) = mpsc::unbounded_channel();
        repl.event_rx = Some(rx);
        tx.send(EngineEvent::ModUi(vec![
            coderex_core::ui_protocol::UiElement::Box {
                title: Some("Mods".to_string()),
                children: vec![coderex_core::ui_protocol::UiElement::Button {
                    label: "Retry".to_string(),
                    id: "retry".to_string(),
                }],
            },
        ]))
        .unwrap();

        let mut output = String::new();
        repl.drain_events(&mut output);

        assert!(output.contains("┌─ Mods"), "{output}");
        assert!(output.contains("[Retry]"), "{output}");
        assert!(output.ends_with('\n'), "{output:?}");
    }

    /// Headless output must be exactly what the model produced. The old path
    /// read the file named in the prompt and either appended its content or
    /// rewrote the model's bytes to strip a duplicate of it; a prompt naming a
    /// file could therefore print file content the model never emitted.
    #[test]
    fn headless_output_is_the_model_output_verbatim() {
        let output = "The answer is 4.".to_string();
        let formatted = ReplLoop::headless_final_output(output.clone(), &TerminalReason::Completed);
        assert_eq!(
            formatted, output,
            "a completed turn must return the model's bytes unchanged"
        );
    }

    /// A prompt that names a file must not turn into that file's content.
    #[test]
    fn headless_output_never_reads_the_prompt_named_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn secret() {}").unwrap();

        // The formatter takes no prompt and touches no filesystem, so the only
        // thing it can return is the model's output.
        let model_output = "I summarized src/lib.rs without printing it.".to_string();
        let formatted =
            ReplLoop::headless_final_output(model_output.clone(), &TerminalReason::Completed);
        assert_eq!(formatted, model_output);
        assert!(
            !formatted.contains("pub fn secret"),
            "file content leaked into headless output: {formatted}"
        );
    }

    /// A non-completed turn still carries the reason, so the caller is not left
    /// guessing why the run stopped.
    #[test]
    fn headless_output_annotates_abnormal_termination() {
        let formatted = ReplLoop::headless_final_output(
            "partial answer".to_string(),
            &TerminalReason::MaxTurnsReached,
        );
        assert!(formatted.starts_with("partial answer"));
        assert!(formatted.contains("MaxTurnsReached"));
    }

    #[test]
    fn session_effects_default() {
        let effects = SessionEffects::default();
        assert_eq!(
            effects.prompt_profiles,
            [] as [coderex_core::PromptProfile; 0]
        );
        assert!(effects.append_system_prompt.is_none());
        assert!(effects.allowed_tools_override.is_none());
        assert_eq!(effects.extra_context, [] as [std::string::String; 0]);
        assert!(!effects.title_generated);
    }

    #[test]
    fn apply_command_effects_from_profile() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = CommandResult::success("advisor mode")
            .with_prompt_profile(coderex_core::query_profiles::PromptProfile::Advisor);
        loop_obj.apply_command_effects(&result);
        assert_eq!(loop_obj.session_effects.prompt_profiles.len(), 1);
        assert_eq!(
            loop_obj.session_effects.prompt_profiles[0],
            coderex_core::query_profiles::PromptProfile::Advisor
        );
    }

    #[test]
    fn build_submit_options_includes_effects() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        loop_obj.session_effects.prompt_profiles = vec![
            coderex_core::query_profiles::PromptProfile::Advisor,
            coderex_core::query_profiles::PromptProfile::Reviewer,
        ];
        let options = loop_obj.build_submit_options();
        assert!(options.custom_system_prompt.is_some());
        let prompt = options.custom_system_prompt.unwrap();
        assert!(prompt.contains("senior technical advisor"));
        assert!(prompt.contains("code review specialist"));
        assert_eq!(
            options.allowed_tools_override.unwrap(),
            vec!["Read", "Grep"]
        );
        assert!(!options.handle_elicitation);
    }

    #[test]
    fn current_git_branch_falls_back() {
        let branch = current_git_branch("/nonexistent");
        assert_eq!(branch, "");
    }

    #[test]
    fn interactive_phase_labels_track_activity() {
        let mut phase = InteractivePhase::Planning;
        assert_eq!(phase.status_label().as_deref(), Some("Planning..."));

        phase.note_text_delta();
        assert_eq!(phase.status_label().as_deref(), Some("Thinking..."));

        phase.note_tool_start("Running cargo test -p coderex-cli...");
        assert_eq!(
            phase.status_label().as_deref(),
            Some("Running cargo test -p coderex-cli...")
        );

        phase.note_permission_request("Read", &serde_json::json!({"file_path":"src/lib.rs"}));
        assert_eq!(
            phase.status_label().as_deref(),
            Some("Waiting for permission: Reading src/lib.rs...")
        );

        phase.note_tool_end();
        assert_eq!(phase.status_label().as_deref(), Some("Thinking..."));

        phase.reset();
        assert!(phase.status_label().is_none());
    }

    /// Pick the hook command for this platform.
    ///
    /// The hook engine runs commands through `sh -c` on Unix and `cmd /C` on
    /// Windows, so a test command has to be written for the shell it will
    /// actually meet.
    fn platform_hook_command(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    /// The TUI path uses the same firing routine as the headless one: one
    /// event per path, the reference input, and the batch in `paths`. A PTY
    /// test of the interactive loop is not practical here, so this pins the
    /// handler the TUI calls.
    #[cfg(feature = "hot-reload")]
    #[tokio::test]
    async fn tui_file_changed_handler_fires_one_reference_shaped_event_per_path() {
        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        let recorder = RecordingHookExecutor::default();
        let events = Arc::clone(&recorder.events);
        repl.engine
            .as_mut()
            .expect("with_engine installs an engine")
            .set_hooks(Arc::new(recorder));

        let batch = FileChangeBatch {
            paths: vec!["/tmp/a.txt".into(), "/tmp/b.txt".into()],
            kind: "change",
        };
        repl.fire_file_changed_hook(&batch).await;

        let recorded = events.lock().expect("the recording hook lock is clean");
        assert_eq!(recorded.len(), 2, "one event per path: {recorded:?}");
        for (index, path) in ["/tmp/a.txt", "/tmp/b.txt"].iter().enumerate() {
            let (event, input, context) = &recorded[index];
            assert_eq!(event, "FileChanged");
            let parsed: serde_json::Value = serde_json::from_str(input).unwrap();
            assert_eq!(
                parsed,
                serde_json::json!({"file_path": path, "event": "change"})
            );
            assert_eq!(
                context.get("paths").map(String::as_str),
                Some("/tmp/a.txt\n/tmp/b.txt"),
                "the batch context must keep both paths together"
            );
        }
    }

    /// A `hooks` block in settings used to be parsed, validated, and then
    /// dropped: the engine kept the no-op executor it is constructed with, so a
    /// configured hook never ran. This pins the install, not the registry —
    /// `coderex-hooks` has its own tests for loading.
    #[tokio::test]
    async fn hooks_from_settings_run_before_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("hook-ran");
        let hooks = serde_json::json!([{
            "event": "UserPromptSubmit",
            // The hook's sandbox workspace is its configured `cwd`; without
            // one it is this process's directory, and the marker in the temp
            // directory would be outside the policy on Windows.
            "cwd": temp.path().to_string_lossy(),
            // The hook engine runs hooks through `sh -c` on Unix and
            // `cmd /C` on Windows, so the command has to be written for one.
            "command": platform_hook_command(
                &format!("touch {} && echo '{{}}'", marker.display()),
                &format!("echo hook-ran> \"{}\" & echo {{}}", marker.display()),
            ),
        }]);

        let api = CountingApi::new();
        let mut repl = ReplLoop::with_engine(
            Box::new(api.clone()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_hooks(Some(hooks));
        repl.apply_cli_config();

        repl.run_headless("Hello").await.unwrap();

        assert!(
            marker.exists(),
            "the configured hook never ran — settings hooks are not installed on the engine"
        );
        assert_eq!(
            api.turn_requests(),
            1,
            "a hook that answered 'continue' should not have stopped the turn"
        );
    }

    /// The `FileChanged` input is the reference shape for every kind the
    /// watcher can report. The path is the file that changed, not a batch.
    #[test]
    fn file_changed_input_names_one_path_and_its_kind() {
        for kind in ["change", "add", "unlink"] {
            let input: serde_json::Value =
                serde_json::from_str(&file_changed_input("/tmp/watched.txt", kind)).unwrap();
            assert_eq!(
                input,
                serde_json::json!({"file_path": "/tmp/watched.txt", "event": kind}),
                "the FileChanged input drifted from the reference shape"
            );
        }
    }

    /// A headless one-shot has no event loop, so it must keep its own watcher
    /// alive for the turn. The watched path is a real temp file and the change
    /// happens during the model request, not from a mock event.
    #[cfg(feature = "hot-reload")]
    #[tokio::test]
    async fn headless_run_fires_file_changed_for_a_real_file_change() {
        let temp = tempfile::tempdir().unwrap();
        let watched = temp.path().join("watched.txt");
        std::fs::write(&watched, "before").unwrap();
        let marker = temp.path().join("file-changed-hook-ran");

        let hooks = serde_json::json!([{
            "event": "FileChanged",
            // Static watch configuration, the path a FileChanged hook wants
            // observed.
            "watch": watched.to_string_lossy(),
            "cwd": temp.path().to_string_lossy(),
            "command": platform_hook_command(
                &format!("touch {} && echo '{{}}'", marker.display()),
                &format!("echo changed> \"{}\" & echo {{}}", marker.display()),
            ),
        }]);

        let api = FileChangingApi {
            watched: watched.clone(),
            marker: marker.clone(),
        };
        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_hooks(Some(hooks));
        repl.apply_cli_config();

        repl.run_headless("Hello").await.unwrap();

        assert!(
            marker.exists(),
            "the headless run did not fire FileChanged for a changed watched file"
        );
        assert_eq!(
            std::fs::read_to_string(&watched).unwrap(),
            "changed during the turn",
            "the test's file change did not happen during the turn"
        );
    }

    /// A hook that cannot answer is a failure, and on a gating event a failure
    /// denies: the turn must not reach the model. Without the install this test
    /// would pass for the wrong reason, which is why it counts requests rather
    /// than asserting on an answer the empty stream never produces.
    #[tokio::test]
    async fn a_hook_that_cannot_answer_stops_the_turn() {
        let temp = tempfile::tempdir().unwrap();
        let hooks = serde_json::json!([{
            "event": "UserPromptSubmit",
            "command": "exit 3",
        }]);

        let api = CountingApi::new();
        let mut repl = ReplLoop::with_engine(
            Box::new(api.clone()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_hooks(Some(hooks));
        repl.apply_cli_config();

        repl.run_headless("Hello").await.unwrap();

        assert_eq!(
            api.turn_requests(),
            0,
            "a hook that failed was treated as 'continue' and the turn ran anyway"
        );
    }

    /// A `prompt` hook runs through the session's own model client — the wiring
    /// `repl.rs` does when it installs the registry. An answer of `stop` must
    /// keep the turn from reaching the model at all.
    #[tokio::test]
    async fn a_prompt_hook_answering_stop_keeps_the_turn_from_the_model() {
        let temp = tempfile::tempdir().unwrap();
        let hooks = serde_json::json!([{
            "event": "UserPromptSubmit",
            "type": "prompt",
            "prompt": "may this run? $ARGUMENTS",
        }]);

        let api = CountingApi::answering_hooks_with("{\"action\":\"stop\"}");
        let mut repl = ReplLoop::with_engine(
            Box::new(api.clone()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_hooks(Some(hooks));
        repl.apply_cli_config();

        repl.run_headless("Hello").await.unwrap();

        assert_eq!(
            api.turn_requests(),
            0,
            "a prompt hook that refused was ignored and the turn ran anyway"
        );
    }

    /// And an answer of `continue` lets it through — which is what tells the
    /// test above apart from "the hook never ran".
    #[tokio::test]
    async fn a_prompt_hook_answering_continue_lets_the_turn_run() {
        let temp = tempfile::tempdir().unwrap();
        let hooks = serde_json::json!([{
            "event": "UserPromptSubmit",
            "type": "prompt",
            "prompt": "may this run? $ARGUMENTS",
        }]);

        let api = CountingApi::answering_hooks_with("{\"action\":\"continue\"}");
        let mut repl = ReplLoop::with_engine(
            Box::new(api.clone()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_hooks(Some(hooks));
        repl.apply_cli_config();

        repl.run_headless("Hello").await.unwrap();

        assert_eq!(api.turn_requests(), 1);
    }

    /// The hook is told which prompt it is gating. It used to be handed
    /// `custom_system_prompt`, which is almost always empty.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // ENV_LOCK keeps a PATH-mutating test out for the whole run; held across the await on purpose.
    async fn the_prompt_hook_is_given_the_prompt() {
        // The hook runs through `sh` from PATH; a concurrent test that
        // temporarily replaces PATH would break it.
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let seen = temp.path().join("seen.json");
        let hooks = serde_json::json!([{
            "event": "UserPromptSubmit",
            "cwd": temp.path().to_string_lossy(),
            "command": platform_hook_command(
                &format!("cat > {}; echo '{{}}'", seen.display()),
                &format!("more > \"{}\" & echo {{}}", seen.display()),
            ),
        }]);

        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_hooks(Some(hooks));
        repl.apply_cli_config();

        repl.run_headless("Hello there").await.unwrap();

        let input = std::fs::read_to_string(&seen).expect("the hook wrote its input");
        assert!(
            input.contains("Hello there"),
            "the hook was not told the prompt it is gating: {input}"
        );
    }

    /// The protocol is what a host outside the process reads, so the mapping
    /// from engine events has to be exactly one frame per thing it cares
    /// about — and nothing for this process's own dialogs.
    #[test]
    fn engine_events_map_to_the_protocol_frames_a_host_reads() {
        use coderex_events::ProtocolMessage;

        let delta = protocol_frame(&EngineEvent::TextDelta("hello".into()), "s1");
        let Some(ProtocolMessage::StreamEvent(frame)) = delta else {
            panic!("a text delta should be a stream_event frame");
        };
        assert_eq!(frame.session_id, "s1");
        assert_eq!(frame.event["delta"]["text"], "hello");

        let start = protocol_frame(
            &EngineEvent::ToolCallStart {
                id: "call_1".into(),
                name: "Read".into(),
                summary: "Reading src/lib.rs".into(),
            },
            "s1",
        );
        let Some(ProtocolMessage::Assistant(frame)) = start else {
            panic!("a tool call start should be an assistant frame");
        };
        assert_eq!(frame.message.content[0]["type"], "tool_use");
        assert_eq!(frame.message.content[0]["name"], "Read");

        let end = protocol_frame(
            &EngineEvent::ToolCallEnd {
                id: "call_1".into(),
                result: ToolResult {
                    tool_call_id: "call_1".into(),
                    output: "file contents".into(),
                    is_error: false,
                    images: Vec::new(),
                },
            },
            "s1",
        );
        let Some(ProtocolMessage::User(frame)) = end else {
            panic!("a tool result is a user-side message in the protocol");
        };
        assert_eq!(frame.message.content[0]["type"], "tool_result");

        let boundary = protocol_frame(&EngineEvent::CompactBoundary, "s1");
        let Some(ProtocolMessage::System(frame)) = boundary else {
            panic!("a compact boundary should be a system frame");
        };
        assert_eq!(frame.subtype, "compact_boundary");
    }

    /// A shared buffer the sink can write into while the test holds the other
    /// end, so a turn's stream can be read back.
    #[derive(Clone, Default)]
    struct SharedBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuffer {
        fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(ToString::to_string)
                .collect()
        }
    }

    /// The whole point of the format: a host reads one JSON object per line,
    /// and gets exactly one `result` telling it the turn is over.
    #[tokio::test]
    async fn a_headless_turn_writes_one_line_per_frame_and_one_result() {
        let temp = tempfile::tempdir().unwrap();
        let buffer = SharedBuffer::default();
        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_protocol_sink(Some(coderex_events::MessageSink::new(
            Box::new(buffer.clone()) as Box<dyn std::io::Write + Send + Sync>,
        )));

        repl.run_headless("Hello").await.unwrap();

        let lines = buffer.lines();
        assert!(!lines.is_empty(), "the turn wrote nothing to the stream");

        let mut results = 0;
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("every line must be one JSON object, got {line:?}: {e}")
            });
            if parsed["type"] == "result" {
                results += 1;
                assert!(parsed["subtype"].is_string(), "a result carries a subtype");
            }
        }
        assert_eq!(results, 1, "a turn must close with exactly one result");
    }

    #[test]
    fn control_response_lines_map_to_confirmation_responses() {
        let allow = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"behavior":"allow","message":"ok"}}}"#;
        let deny = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r2","response":{"behavior":"deny","message":"no"}}}"#;
        let session = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r3","response":{"behavior":"allow","message":"always","allow_session":true}}}"#;
        let error = r#"{"type":"control_response","response":{"subtype":"error","request_id":"r4","error":"unsupported"}}"#;

        assert_eq!(
            control_response_from_line(allow, false),
            Some(("r1".to_string(), ConfirmationResponse::Approved))
        );
        assert_eq!(
            control_response_from_line(deny, true),
            Some(("r2".to_string(), ConfirmationResponse::Denied))
        );
        assert_eq!(
            control_response_from_line(session, true),
            Some(("r3".to_string(), ConfirmationResponse::ApproveSession)),
            "a request that offered a session answer may receive one"
        );
        assert_eq!(
            control_response_from_line(session, false),
            Some(("r3".to_string(), ConfirmationResponse::Approved)),
            "a session answer must not widen a request that did not offer it"
        );
        assert_eq!(
            control_response_from_line(error, false),
            Some(("r4".to_string(), ConfirmationResponse::Denied)),
            "an unusable answer fails closed"
        );
    }

    #[test]
    fn lines_that_are_not_control_responses_are_ignored() {
        assert_eq!(control_response_from_line("not json", false), None);
        // A normal protocol frame is not an answer.
        assert_eq!(
            control_response_from_line(
                &protocol_frame(&EngineEvent::TextDelta("hi".into()), "s1")
                    .unwrap()
                    .to_line(),
                false,
            ),
            None
        );
        // A response without a decision is unusable.
        assert_eq!(
            control_response_from_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"r"}}"#,
                false
            ),
            None
        );
    }

    /// The producer half of the channel is off unless the host explicitly
    /// enabled it; the CLI gate is what decides that.
    #[test]
    fn headless_permission_control_is_off_by_default() {
        let repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        assert!(!repl.headless_permission_control());
    }

    /// A dialog this process asks a person is not something a host can act on,
    /// so it is not in the stream.
    #[test]
    fn process_local_events_are_not_protocol_frames() {
        assert!(protocol_frame(&EngineEvent::SessionEnd, "s1").is_none());
        assert!(
            protocol_frame(
                &EngineEvent::PermissionRequest {
                    id: "p1".into(),
                    tool: "Bash".into(),
                    args: serde_json::json!({}),
                },
                "s1",
            )
            .is_none()
        );
    }

    /// Every way a person can say "stop" runs the same sequence: interrupt the
    /// engine, drop the TUI's side of the turn, and say on the message that it
    /// was cut short.
    #[tokio::test]
    async fn stopping_a_running_query_asks_the_engine_and_closes_the_ui() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        let handle = coderex_core::engine::InterruptHandle::default();
        repl.running_query = Some(handle.clone());

        let mut ui_state = coderex_ui::state::UiState::new();
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::Assistant,
            content: "answering…".into(),
            tool_name: None,
            streaming: true,
            folded: false,
        });
        let mut thinking = true;
        let mut phase = InteractivePhase::Thinking;
        let mut query_done = None;
        let mut elicit = None;
        let mut tui_elicit = None;

        repl.stop_running_query(
            &mut ui_state,
            &mut thinking,
            &mut phase,
            &mut query_done,
            &mut elicit,
            &mut tui_elicit,
            " (interrupted)",
        );

        assert!(handle.is_interrupted(), "the engine was never told to stop");
        assert!(!thinking, "the UI still thinks a query is running");
        assert!(repl.running_query.is_none());
        assert!(matches!(phase, InteractivePhase::Idle));
        let last = ui_state.messages.messages.last().unwrap();
        assert!(
            last.content.contains("(interrupted)"),
            "the message does not say it was cut short: {}",
            last.content
        );
        assert!(!last.streaming);
    }

    #[test]
    fn permission_activity_labels_are_user_facing() {
        assert_eq!(
            permission_activity_label("Running cargo test -p coderex-cli..."),
            "Waiting for permission: Running cargo test -p coderex-cli..."
        );
    }

    /// Every sub-agent lifecycle event that means something to a person shows
    /// up on screen; the agent's own text does not, because the Agent tool's
    /// result already carries it.
    #[test]
    fn sub_agent_events_reach_the_activity_log() {
        let mut ui_state = coderex_ui::state::UiState::new();

        apply_agent_event(
            &mut ui_state,
            "agent-1234-uuid",
            &AgentLifecycleEvent::Started,
        );
        assert_eq!(
            ui_state.activity_log.last().map(String::as_str),
            Some("Sub-agent agent-12 started")
        );
        assert!(
            ui_state
                .notifications
                .latest()
                .is_some_and(|note| note.message.contains("started"))
        );

        apply_agent_event(
            &mut ui_state,
            "agent-1234-uuid",
            &AgentLifecycleEvent::Completed {
                summary: "did the thing".into(),
            },
        );
        assert!(
            ui_state
                .notifications
                .latest()
                .is_some_and(|note| note.message.contains("did the thing")),
            "the completion summary should reach the notification"
        );

        apply_agent_event(
            &mut ui_state,
            "agent-1234-uuid",
            &AgentLifecycleEvent::Failed {
                error: "it broke".into(),
            },
        );
        let latest = ui_state.notifications.latest().unwrap();
        assert!(latest.message.contains("it broke"), "{}", latest.message);
        assert_eq!(
            latest.kind,
            coderex_ui::notification_center::NotificationKind::Error
        );

        // Text belongs to the Agent tool's result, not the main transcript.
        let rows = ui_state.activity_log.len();
        let notes = ui_state.notifications.active().len();
        apply_agent_event(
            &mut ui_state,
            "agent-1234-uuid",
            &AgentLifecycleEvent::TextDelta("streaming".into()),
        );
        assert_eq!(ui_state.activity_log.len(), rows);
        assert_eq!(ui_state.notifications.active().len(), notes);
    }

    #[tokio::test]
    async fn agent_tool_runs_a_real_subagent_with_parent_tools_and_cwd() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "needle-from-child").unwrap();
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "needle-from-child",
        };
        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        let dispatcher = repl.dispatcher.take().unwrap();
        let manager = repl.agent_manager.take().unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            dispatcher.execute(
                "parent_agent_call",
                "Agent",
                &serde_json::json!({
                    "description": "read a local note",
                    "prompt": "Read note.txt and report its contents"
                }),
            ),
        )
        .await
        .expect("sub-agent did not complete");

        assert!(
            !outcome.result.is_error,
            "agent tool failed: {}",
            outcome.result.output
        );
        let payload: serde_json::Value = serde_json::from_str(&outcome.result.output).unwrap();
        assert_eq!(payload["status"], "Completed");
        assert_eq!(payload["output"], "sub-agent read the file");
        let output_file = payload["output_file"].as_str().unwrap();
        assert!(
            std::path::Path::new(output_file).is_file(),
            "missing sub-agent output: {output_file}"
        );

        let manager = manager.lock().await;
        assert_eq!(manager.list_agents().len(), 1);
        assert_eq!(
            manager.list_agents()[0].status(),
            coderex_agents::AgentStatus::Completed
        );
        drop(manager);

        let mut saw_started = false;
        let mut saw_completed = false;
        while let Ok(event) = repl.event_rx.as_mut().unwrap().try_recv() {
            if let EngineEvent::AgentEvent { event, .. } = event {
                match event {
                    coderex_core::types::AgentLifecycleEvent::Started => saw_started = true,
                    coderex_core::types::AgentLifecycleEvent::Completed { .. } => {
                        saw_completed = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_started, "missing sub-agent start event");
        assert!(saw_completed, "missing sub-agent completion event");
    }

    /// `--worktree` creates a worktree and moves the session into it. It used
    /// to record the *current* directory as the active worktree, so the flag
    /// told the session it was isolated while it went on editing the person's
    /// own checkout.
    #[tokio::test]
    async fn worktree_mode_creates_a_worktree_and_runs_there() {
        // `git` is resolved through PATH, which other tests replace while
        // they hold `ENV_LOCK`.
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let repo = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["commit", "--quiet", "--allow-empty", "-m", "init"],
        ] {
            let status = coderex_core::git_hardening::git_command()
                .args(&args)
                .current_dir(repo.path())
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git");
            assert!(status.status.success(), "{args:?} failed");
        }

        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            repo.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_worktree(true);

        // The base comes from the session's cwd, not the process's: this is
        // the `--directory /repo --worktree` case, and the test process is not
        // in the repo.
        let before = repl.dispatcher.as_ref().unwrap().cwd();
        repl.enter_worktree_mode();

        let session = repl.engine.as_ref().unwrap().session().clone();
        let active = session
            .active_worktree
            .expect("--worktree recorded no worktree");
        assert_ne!(
            active.path, before,
            "the session was pointed at the original checkout"
        );
        assert!(
            active.path.is_dir(),
            "the recorded worktree is not a directory: {}",
            active.path.display()
        );
        assert!(
            active.path.join(".git").exists(),
            "the recorded path is not a worktree: {}",
            active.path.display()
        );
        assert_eq!(
            repl.dispatcher.as_ref().unwrap().cwd(),
            active.path,
            "the tools still run in the original checkout"
        );
        assert_eq!(
            session.cwd, active.path,
            "the engine still tells the model to work in the original checkout"
        );
        assert_eq!(
            repl.cwd,
            active.path.to_string_lossy(),
            "the loop's cwd was not moved"
        );
    }

    /// Compaction's transcript backup needs a directory. The CLI was already
    /// writing transcripts under `<cwd>/.coderex/transcripts`, but the engine
    /// never learned that path, so `--no-session-persistence` was not the only
    /// reason the backup could be skipped.
    #[tokio::test]
    async fn apply_cli_config_gives_the_engine_the_transcript_directory() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_string_lossy().into_owned();
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), cwd.clone(), ModelId::sonnet());

        repl.apply_cli_config();
        let expected = crate::session_resume::transcript_dir(&cwd);
        assert_eq!(
            repl.engine.as_ref().unwrap().transcript_path(),
            Some(expected.as_path()),
            "the engine was not told where transcripts live"
        );

        repl.with_no_persistence(true);
        repl.apply_cli_config();
        assert!(
            repl.engine.as_ref().unwrap().transcript_path().is_none(),
            "--no-session-persistence must also disable the engine's backup"
        );
    }

    #[test]
    fn resolved_agent_sets_prompt_and_tools_unless_flags_win() {
        use crate::agent_selection::ResolvedAgent;

        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.apply_resolved_agent(ResolvedAgent {
            name: "reviewer".into(),
            system_prompt: "You review code.".into(),
            allowed_tools: vec!["Read".into(), "Grep".into()],
            model: None,
            max_tokens: None,
        });
        assert_eq!(
            repl.session_effects.override_system_prompt.as_deref(),
            Some("You review code.")
        );
        assert_eq!(
            repl.session_effects.allowed_tools_override,
            Some(vec!["Read".into(), "Grep".into()])
        );

        // Explicit CLI flags are more specific than the agent file.
        repl.session_effects.override_system_prompt = Some("explicit prompt".into());
        repl.session_effects.allowed_tools_override = Some(vec!["Bash".into()]);
        repl.apply_resolved_agent(ResolvedAgent {
            name: "reviewer".into(),
            system_prompt: "You review code.".into(),
            allowed_tools: vec!["Read".into()],
            model: None,
            max_tokens: None,
        });
        assert_eq!(
            repl.session_effects.override_system_prompt.as_deref(),
            Some("explicit prompt")
        );
        assert_eq!(
            repl.session_effects.allowed_tools_override,
            Some(vec!["Bash".into()])
        );

        // An explicitly empty whitelist denies every tool; the agent file
        // must not silently re-grant its own list.
        repl.session_effects.allowed_tools_override = Some(Vec::new());
        repl.apply_resolved_agent(ResolvedAgent {
            name: "reviewer".into(),
            system_prompt: "You review code.".into(),
            allowed_tools: vec!["Read".into()],
            model: None,
            max_tokens: None,
        });
        assert_eq!(
            repl.session_effects.allowed_tools_override,
            Some(Vec::new())
        );
    }

    fn custom_agent(
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> crate::agent_selection::ResolvedAgent {
        crate::agent_selection::ResolvedAgent {
            name: "reviewer".into(),
            system_prompt: "You review code.".into(),
            allowed_tools: vec!["Read".into()],
            model: model.map(ToString::to_string),
            max_tokens,
        }
    }

    #[test]
    fn resolved_agent_model_and_max_tokens_apply_unless_cli_model_wins() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.apply_resolved_agent(custom_agent(Some("agent-model"), Some(2048)));
        assert_eq!(repl.model.0, "agent-model");
        assert_eq!(repl.max_tokens, 2048);

        // `--model` is more specific than the agent file.
        let cli_model = ModelId::sonnet();
        let mut explicit = ReplLoop::new("/tmp".into(), cli_model.clone());
        explicit.with_explicit_model_override(true);
        explicit.apply_resolved_agent(custom_agent(Some("agent-model"), Some(2048)));
        assert_eq!(explicit.model, cli_model);
        assert_eq!(
            explicit.max_tokens, 2048,
            "there is no CLI max-token override; the definition still applies"
        );
    }

    #[tokio::test]
    async fn resolved_agent_settings_reach_the_session_engine() {
        let temp = tempfile::tempdir().unwrap();
        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );

        repl.apply_resolved_agent(custom_agent(Some("agent-model"), Some(2048)));

        let engine = repl.engine.as_ref().expect("engine is wired");
        assert_eq!(engine.model().0, "agent-model");
        assert_eq!(engine.max_tokens(), 2048);
    }

    #[tokio::test]
    async fn teammate_events_reach_the_session_event_channel() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut repl =
            ReplLoop::with_engine(Box::new(CountingApi::new()), cwd.clone(), ModelId::sonnet());
        let manager = Arc::clone(
            repl.agent_manager
                .as_ref()
                .expect("with_engine installs an agent manager"),
        );

        let agent_id = {
            let mut mgr = manager.lock().await;
            let instance = mgr
                .spawn_agent(
                    coderex_agents::AgentType::GeneralPurpose,
                    "wait for work",
                    coderex_agents::SpawnOptions::default(),
                    "sess_teammate",
                    &cwd,
                    0,
                )
                .await
                .unwrap();
            let agent_id = instance.task_handle.id.clone();
            mgr.mark_idle(&agent_id, "waiting for work").await.unwrap();
            drop(mgr);
            agent_id
        };

        let mut saw_idle = false;
        while let Ok(event) = repl.event_rx.as_mut().unwrap().try_recv() {
            if let EngineEvent::AgentEvent {
                agent_id: seen,
                event: AgentLifecycleEvent::Idle { reason },
            } = event
            {
                assert_eq!(seen, agent_id);
                assert_eq!(reason, "waiting for work");
                saw_idle = true;
            }
        }
        assert!(
            saw_idle,
            "the teammate callback never reached the engine event channel"
        );
    }

    #[test]
    fn teammate_events_map_onto_engine_lifecycle_events() {
        let (agent_id, lifecycle) = teammate_event_to_lifecycle(&TeammateEvent::TaskCompleted {
            agent_id: "agent-7".into(),
            subject: "review".into(),
            summary: "done".into(),
        });
        assert_eq!(agent_id, "agent-7");
        assert_eq!(
            lifecycle,
            AgentLifecycleEvent::Completed {
                summary: "review: done".into()
            }
        );

        let (agent_id, lifecycle) = teammate_event_to_lifecycle(&TeammateEvent::Idle {
            agent_id: "agent-8".into(),
            reason: "no work".into(),
        });
        assert_eq!(agent_id, "agent-8");
        assert_eq!(
            lifecycle,
            AgentLifecycleEvent::Idle {
                reason: "no work".into()
            }
        );
    }

    #[tokio::test]
    async fn build_submit_options_refreshes_the_parent_context_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );

        let mut session = repl.engine.as_ref().unwrap().session().clone();
        session.messages.push(Message::user("earlier turn"));
        repl.engine.as_mut().unwrap().set_session(session);

        let expected_model = repl.model.0.clone();
        let options = repl.build_submit_options();
        let expected_prompt = options.custom_system_prompt.unwrap_or_default();
        let snapshot = repl
            .parent_context_snapshot
            .lock()
            .unwrap()
            .clone()
            .expect("build_submit_options did not refresh the parent snapshot");

        assert_eq!(
            snapshot.messages.len(),
            1,
            "parent messages were not captured"
        );
        assert_eq!(snapshot.messages[0]["role"], "User");
        assert_eq!(snapshot.model, expected_model);
        assert_eq!(snapshot.system_prompt, expected_prompt);
        assert!(
            snapshot.tool_names.iter().any(|name| name == "Read"),
            "the snapshot did not capture the advertised tools: {:?}",
            snapshot.tool_names
        );
        assert!(snapshot.has_prefix());
    }

    /// A failed `--worktree` must stop a headless run, not fall back to the
    /// checkout. The interactive loop reads `running`; `run_headless` reads
    /// `setup_error`, which is the only thing that reached this path.
    #[tokio::test]
    async fn a_failed_worktree_stops_a_headless_run() {
        let not_a_repo = tempfile::tempdir().unwrap();
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            not_a_repo.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        repl.with_worktree(true);
        repl.enter_worktree_mode();

        let error = repl
            .run_headless("hello")
            .await
            .expect_err("a failed worktree did not stop the headless run");
        assert!(
            error.to_string().contains("could not create a worktree"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn the_workflow_tool_is_registered_and_runs_a_script_through_the_session() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "needle-from-child",
        };
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "needle-from-child").unwrap();

        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        // Off by default: a workflow spawns agents.
        let dispatcher = repl.dispatcher.take().unwrap();
        let tool = dispatcher.registry().get_tool_arc("Workflow").unwrap();
        let refused = tool
            .execute(
                coderex_tools::ToolInput {
                    call_id: "c".into(),
                    name: "Workflow".into(),
                    arguments: serde_json::json!({"script": "export const meta = { name: 'a', description: 'b' }"}),
                },
                coderex_tools::ToolContext::new(
                    coderex_tools::ExecutionContext::Normal,
                    temp.path(),
                ),
            )
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("not enabled"), "{refused}");
    }

    #[tokio::test]
    async fn artifact_tools_join_the_registry_and_serve_a_real_page() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());
        let store = tempfile::tempdir().unwrap();
        repl.with_artifact_store_root(store.path().to_path_buf());

        repl.start_artifact_serving().await;
        assert!(
            repl.artifact_server.is_some(),
            "the artifact server did not start"
        );

        let dispatcher = repl.dispatcher.take().unwrap();
        let registry = dispatcher.registry();
        for name in [
            "Artifact",
            "ArtifactData",
            "ArtifactComments",
            "ArtifactCheck",
        ] {
            assert!(
                registry.get_tool_arc(name).is_some(),
                "{name} was not registered"
            );
        }

        // Publishing through the registered tool produces a page the server
        // actually answers for.
        let tool = registry.get_tool_arc("Artifact").unwrap();
        let output = tool
            .execute(
                coderex_tools::ToolInput {
                    call_id: "call_1".into(),
                    name: "Artifact".into(),
                    arguments: serde_json::json!({
                        "title": "Session report",
                        "files": { "index.html": "<html><body>hello</body></html>" }
                    }),
                },
                coderex_tools::ToolContext::new(
                    coderex_tools::ExecutionContext::Normal,
                    std::path::Path::new("/tmp"),
                ),
            )
            .await
            .unwrap();

        let url = output
            .output
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(body.contains("hello"), "{body}");

        if let Some(server) = repl.artifact_server.take() {
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn headless_starts_artifacts_and_stops_the_server_when_the_turn_ends() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());
        let store = tempfile::tempdir().unwrap();
        repl.with_artifact_store_root(store.path().to_path_buf());
        let dispatcher = repl.dispatcher.clone().expect("engine has a dispatcher");

        // A slash command completes a headless turn without an API call.
        let output = repl.run_headless("/status").await.unwrap();
        assert!(output.contains("System Status"));

        for name in [
            "Artifact",
            "ArtifactData",
            "ArtifactComments",
            "ArtifactCheck",
        ] {
            assert!(
                dispatcher.registry().get_tool_arc(name).is_some(),
                "{name} was not registered in headless mode"
            );
        }

        // The server is stopped on exit, and stopping again is harmless.
        assert!(
            repl.artifact_server.is_none(),
            "the artifact server outlived the headless turn"
        );
        repl.stop_artifact_serving().await;
    }

    #[test]
    fn cli_betas_update_the_session_flags_and_capabilities() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_betas(vec![
            "+team_collaboration".into(),
            "-web_browser_tool".into(),
            "voice_mode".into(),
        ]);

        repl.apply_cli_config();

        assert!(repl.setup_error.is_none(), "{:?}", repl.setup_error);
        assert!(repl.feature_flags().team_collaboration);
        assert!(!repl.feature_flags().web_browser_tool);
        assert!(repl.feature_flags().voice_mode);
        assert!(
            repl.capabilities()
                .contains(coderex_config::flags::CapabilitySet::TEAM),
            "an enabled beta did not reach the session capability set"
        );
        assert!(
            repl.capabilities()
                .contains(coderex_config::flags::CapabilitySet::VOICE)
        );
        assert!(
            !repl
                .capabilities()
                .contains(coderex_config::flags::CapabilitySet::WEB_BROWSER)
        );
    }

    #[tokio::test]
    async fn engine_flag_behaviours_install_the_wired_flags() {
        use coderex_config::flags::FeatureFlags;

        struct FlagTestApi;

        #[async_trait::async_trait]
        impl ApiClient for FlagTestApi {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(FlagTestApi)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> Result<ApiResponse, ApiError> {
                Ok(ApiResponse {
                    content: String::new(),
                    tool_calls: Vec::new(),
                    thinking: None,
                    usage: UsageSummary::default(),
                    stop_reason: "end_turn".into(),
                })
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
                Box::pin(futures::stream::empty())
            }
        }

        fn engine() -> QueryEngine {
            let (tx, _rx) = mpsc::unbounded_channel();
            QueryEngine::new(
                Box::new(FlagTestApi),
                ModelId::sonnet(),
                SessionState::new(std::path::PathBuf::from("/tmp/team-flag-test")),
                PermissionEngine::new(PermissionChecker::new(PermissionMode::BypassPermissions)),
                tx,
            )
        }

        // A real stored team-memory entry is what the flag turns into context.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("coderex-team-flag-{}-{unique}", std::process::id()));
        let memory_base = dir.join("memory");
        TeamMemorySyncService::new("default".to_string(), memory_base.join("team"))
            .upsert("architecture", "Use Postgres for durable state")
            .expect("the test writes a team memory");

        let mut off = engine();
        apply_engine_flag_behaviours(&mut off, &FeatureFlags::default(), Some(&memory_base));
        assert!(off.team_context().is_none(), "off must install nothing");
        assert!(!off.streaming_tool_execution());
        assert!(!off.tool_use_summaries());

        let mut on = engine();
        let flags = FeatureFlags {
            team_collaboration: true,
            streaming_tool_execution: true,
            tool_use_summaries: true,
            ..FeatureFlags::default()
        };
        apply_engine_flag_behaviours(&mut on, &flags, Some(&memory_base));
        assert!(on.streaming_tool_execution());
        assert!(on.tool_use_summaries());
        let context = on
            .team_context()
            .expect("the flag installs a team context")
            .lock()
            .unwrap();
        assert_eq!(context.shared_notes.len(), 1);
        assert_eq!(context.shared_notes[0].title, "architecture");
        assert!(
            context.shared_notes[0]
                .content
                .contains("Use Postgres for durable state")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_agent_hooks_drops_only_agent_entries_from_an_object() {
        let doc = serde_json::json!({"hooks": [
            {"event": "PreToolUse", "type": "command", "command": "true"},
            {"event": "PreToolUse", "type": "agent", "prompt": "verify"},
            {"event": "Stop"}
        ]});
        let (filtered, skipped) = strip_agent_hooks(&doc);
        assert_eq!(skipped, 1, "only the agent entry is removed");
        assert_eq!(filtered["hooks"].as_array().unwrap().len(), 2);
        // The command hook and the type-less (command) hook both survive.
        assert_eq!(filtered["hooks"][0]["type"], "command");
        assert_eq!(filtered["hooks"][1]["event"], "Stop");
    }

    #[test]
    fn strip_agent_hooks_handles_a_bare_array() {
        let doc = serde_json::json!([
            {"event": "PreToolUse", "type": "agent", "prompt": "verify"},
            {"event": "PreToolUse", "type": "command", "command": "true"}
        ]);
        let (filtered, skipped) = strip_agent_hooks(&doc);
        assert_eq!(skipped, 1);
        assert_eq!(filtered.as_array().unwrap().len(), 1);
    }

    #[test]
    fn strip_agent_hooks_leaves_other_shapes_untouched() {
        // A document that is neither a hooks array nor an object with one is
        // not this function's to repair; it is returned as-is so the hooks
        // crate still reports the structural error.
        let doc = serde_json::json!({"not_hooks": true});
        let (filtered, skipped) = strip_agent_hooks(&doc);
        assert_eq!(skipped, 0);
        assert_eq!(filtered, doc);
    }

    /// The auto-dream manager is built before `--betas` is resolved, so the
    /// `auto_dream` flag is applied at startup: with it off there is no
    /// manager and `/auto-dream` reports as much.
    #[tokio::test]
    async fn auto_dream_flag_gates_the_dream_manager() {
        let mut off = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        off.apply_cli_config();
        assert!(
            off.auto_dream.is_none(),
            "auto_dream is off by default and must leave no manager behind"
        );
        assert_eq!(
            off.handle_auto_dream_command(&[]),
            "Auto-dream is not initialized."
        );

        let mut on = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        on.with_betas(vec!["+auto_dream".into()]);
        on.apply_cli_config();
        assert!(
            on.auto_dream.is_some(),
            "enabling auto_dream must keep the dream manager"
        );
    }

    /// `coordinator_mode` installs a started `CoordinatorState` on the session
    /// engine, so the engine's per-turn phase advance actually runs. Without
    /// the flag there is no coordinator and the whole mechanism stays dead.
    #[tokio::test]
    async fn coordinator_mode_flag_installs_a_coordinator_on_the_engine() {
        let mut off = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        off.apply_cli_config();
        assert!(
            off.engine.as_ref().expect("engine").coordinator().is_none(),
            "no coordinator is installed while coordinator_mode is off"
        );

        let mut on = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        on.with_betas(vec!["+coordinator_mode".into()]);
        on.apply_cli_config();
        let coordinator = on
            .engine
            .as_ref()
            .expect("engine")
            .coordinator()
            .expect("coordinator_mode must install a coordinator");
        assert!(
            coordinator.is_running,
            "the installed coordinator is started"
        );
        assert!(
            coordinator.current_phase.is_first(),
            "a fresh coordinator begins in the first phase"
        );
    }

    /// `history_snip` reaches the engine's compaction configuration, where it
    /// makes compaction prefer the snip strategy as soon as it is warranted.
    #[tokio::test]
    async fn history_snip_flag_reaches_the_engine_compaction_config() {
        let mut off = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        off.apply_cli_config();
        assert!(
            !off.engine.as_ref().expect("engine").history_snip(),
            "snip preference is off by default"
        );

        let mut on = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        on.with_betas(vec!["+history_snip".into()]);
        on.apply_cli_config();
        assert!(
            on.engine.as_ref().expect("engine").history_snip(),
            "history_snip must reach the engine's compaction config"
        );
    }

    /// `agent_triggers` gates the agent-kind hook path: with it off a
    /// configured `agent` hook is dropped before the registry sees it, and the
    /// remaining kinds still install.
    #[tokio::test]
    async fn agent_triggers_flag_gates_agent_hooks() {
        let hooks = serde_json::json!({"hooks": [
            {"event": "PreToolUse", "type": "command", "command": "true"},
            {"event": "PreToolUse", "type": "agent", "prompt": "verify"}
        ]});

        let mut off = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        off.with_hooks(Some(hooks.clone()));
        off.apply_cli_config();
        assert_eq!(
            off.active_hooks, 1,
            "the agent hook must be dropped while the flag is off"
        );

        let mut on = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        on.with_hooks(Some(hooks));
        on.with_betas(vec!["+agent_triggers".into()]);
        on.apply_cli_config();
        assert_eq!(on.active_hooks, 2, "both hooks install once the flag is on");
    }

    /// `--betas` arrives after the tool registry is shared. The Workflow tool
    /// has to read the final flag at call/listing time, not at construction.
    #[tokio::test]
    async fn workflow_tool_is_enabled_by_betas_after_registration() {
        let mut repl = ReplLoop::with_engine(
            Box::new(CountingApi::new()),
            "/tmp".into(),
            ModelId::sonnet(),
        );
        let dispatcher = repl.dispatcher.clone().expect("engine has a dispatcher");
        let tool = dispatcher
            .registry()
            .get_tool_arc("Workflow")
            .expect("Workflow is registered");

        // The default is off: a workflow spawns agents, so it must not be
        // discoverable without an opt-in.
        assert!(
            !tool.descriptor().is_enabled,
            "the Workflow tool was advertised without --betas"
        );

        repl.with_betas(vec!["+workflow_scripts".into()]);
        repl.apply_cli_config();

        assert!(repl.setup_error.is_none(), "{:?}", repl.setup_error);
        assert!(repl.feature_flags().workflow_scripts);
        assert!(
            tool.descriptor().is_enabled,
            "--betas +workflow_scripts did not reach the already-registered tool"
        );
    }

    #[test]
    fn org_memory_is_only_scoped_when_an_org_id_is_configured() {
        assert_eq!(configured_org_id(None), None);
        assert_eq!(configured_org_id(Some(String::new())), None);
        assert_eq!(configured_org_id(Some("   ".into())), None);
        assert_eq!(
            configured_org_id(Some(" acme-corp ".into())),
            Some("acme-corp".to_string())
        );

        let temp = tempfile::tempdir().unwrap();
        assert!(org_memory_service(temp.path(), None).is_none());

        let service = org_memory_service(temp.path(), Some("acme-corp".into()))
            .expect("a configured org gets a store");
        assert_eq!(service.org_id.as_deref(), Some("acme-corp"));
    }

    /// Remote-managed settings already carry the org id; the REPL reads the
    /// orchestrator's cache rather than asking enterprise users to duplicate
    /// it in an environment variable.
    #[test]
    fn org_id_is_read_from_the_remote_settings_cache_shape() {
        let settings = r#"{
            "settings": { "org_id": "acme-corp", "enforced_model": null },
            "checksum": "abc",
            "fetched_at": "2026-01-01T00:00:00Z"
        }"#;
        assert_eq!(
            org_id_from_remote_settings_json(settings),
            Some("acme-corp".into())
        );

        assert_eq!(
            org_id_from_remote_settings_json(r#"{"settings": {"org_id": "  "}}"#),
            None
        );
        assert_eq!(
            org_id_from_remote_settings_json(r#"{"settings": {}}"#),
            None
        );
        assert_eq!(org_id_from_remote_settings_json("not json"), None);
    }

    #[test]
    fn an_unknown_beta_is_a_setup_error_not_a_silent_drop() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_betas(vec!["+definitely_not_a_flag".into()]);

        repl.apply_cli_config();

        let error = repl
            .setup_error
            .as_deref()
            .expect("an unknown beta must stop startup");
        assert!(error.contains("--betas"), "{error}");
        assert!(error.contains("definitely_not_a_flag"), "{error}");
        assert!(!repl.running);
    }

    #[test]
    fn fetched_pr_context_is_added_to_the_session_context() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_pr_context(Some("<pr_context>the PR</pr_context>".into()));

        repl.apply_cli_config();

        assert!(
            repl.session_effects
                .extra_context
                .iter()
                .any(|context| context.contains("the PR")),
            "the fetched PR block never reached the session context"
        );
    }

    #[tokio::test]
    async fn ide_connection_tools_join_the_registry() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());

        let manager = Arc::new(coderex_mcp::manager::McpManager::new());
        manager.register_tool(coderex_mcp::proxy::McpToolInfo {
            server: "ide".into(),
            name: "executeCode".into(),
            description: "Run code in the IDE".into(),
            input_schema: serde_json::json!({"type": "object"}),
        });

        repl.with_ide_connection(Arc::clone(&manager), "connected".into());

        let dispatcher = repl.dispatcher.take().unwrap();
        assert!(
            dispatcher
                .registry()
                .get_tool_arc("mcp__ide__executeCode")
                .is_some(),
            "an IDE tool did not reach the registry the model dispatches through"
        );
    }

    #[tokio::test]
    async fn mcp_tools_join_the_registry_the_model_dispatches_through() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());

        let manager = Arc::new(coderex_mcp::manager::McpManager::new());
        manager.register_tool(coderex_mcp::proxy::McpToolInfo {
            server: "mock".into(),
            name: "ping".into(),
            description: "Ping the mock server".into(),
            input_schema: serde_json::json!({"type": "object"}),
        });

        repl.with_mcp_manager(Arc::clone(&manager));

        let dispatcher = repl.dispatcher.take().unwrap();
        let registry = dispatcher.registry();
        assert!(
            registry.get_tool_arc("mcp__mock__ping").is_some(),
            "MCP tool did not reach the registry the model dispatches through"
        );
        // `Read` is a baseline tool on every platform; the shell is `Bash` on
        // Unix and `PowerShell` on Windows, so it must not be named here.
        assert!(
            registry.get_tool_arc("Read").is_some(),
            "built-in tools disappeared"
        );
    }

    /// `--system-prompt` / `--append-system-prompt` reach the effects the
    /// engine reads when it builds the prompt.
    #[test]
    fn system_prompt_builders_write_the_session_effects() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        repl.with_system_prompt(Some("replace the prompt".into()));
        repl.with_append_system_prompt(Some("add this".into()));
        assert_eq!(
            repl.session_effects.override_system_prompt.as_deref(),
            Some("replace the prompt")
        );
        assert_eq!(
            repl.session_effects.append_system_prompt.as_deref(),
            Some("add this")
        );

        // Explicit option values can be cleared, too.
        repl.with_system_prompt(None);
        assert!(repl.session_effects.override_system_prompt.is_none());
    }

    /// The baseline team tools are replaced with ones that own a real
    /// `TeamManager`, so a call gets a team id instead of "manager not
    /// available in this context".
    #[tokio::test]
    async fn team_tools_use_the_session_manager() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());
        let dispatcher = repl.dispatcher.take().unwrap();
        let registry = dispatcher.registry();
        let ctx = coderex_tools::ToolContext::new(
            coderex_tools::ExecutionContext::Normal,
            std::path::Path::new("/tmp"),
        );

        let created = registry
            .get_tool("TeamCreate")
            .unwrap()
            .execute(
                coderex_tools::ToolInput {
                    call_id: "1".into(),
                    name: "TeamCreate".into(),
                    arguments: serde_json::json!({"name": "alpha"}),
                },
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(created.output, "team_0");

        let deleted = registry
            .get_tool("TeamDelete")
            .unwrap()
            .execute(
                coderex_tools::ToolInput {
                    call_id: "2".into(),
                    name: "TeamDelete".into(),
                    arguments: serde_json::json!({"team_id": "team_0"}),
                },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(deleted.output, "deleted team_0");
    }

    /// The MCP resource listing is wired to the session slot: with no manager
    /// it reports an empty list rather than "handle not available".
    #[tokio::test]
    async fn mcp_resource_tools_are_wired_to_the_session_slot() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());
        let dispatcher = repl.dispatcher.take().unwrap();
        let ctx = coderex_tools::ToolContext::new(
            coderex_tools::ExecutionContext::Normal,
            std::path::Path::new("/tmp"),
        );
        let listed = dispatcher
            .registry()
            .get_tool("ListMcpResources")
            .unwrap()
            .execute(
                coderex_tools::ToolInput {
                    call_id: "1".into(),
                    name: "ListMcpResources".into(),
                    arguments: serde_json::json!({}),
                },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(listed.output, "[]");
    }

    #[test]
    fn prompt_command_names_keep_only_identifier_characters() {
        assert_eq!(
            prompt_command_name("my server", "review/file"),
            "mcp__my_server__review_file"
        );
        assert_eq!(
            prompt_command_name("srv-1", "ask.questions"),
            "mcp__srv_1__ask_questions"
        );
        assert_eq!(sanitize_prompt_component("a-b c"), "a_b_c");
        assert_eq!(sanitize_prompt_component("keep_1"), "keep_1");
    }

    fn sample_prompt() -> coderex_mcp::prompts::ServerPrompt {
        coderex_mcp::prompts::ServerPrompt {
            server: "my server".into(),
            prompt: coderex_mcp::prompts::McpPromptInfo {
                name: "review/file".into(),
                description: Some("Review a file".into()),
                arguments: vec![coderex_mcp::prompts::McpPromptArgument {
                    name: "path".into(),
                    description: None,
                    required: true,
                }],
            },
        }
    }

    #[test]
    fn prompt_commands_are_registered_for_discovered_prompts() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        let manager = Arc::new(coderex_mcp::manager::McpManager::new());
        manager.register_prompt(sample_prompt());
        repl.with_mcp_manager(manager);

        let command = repl
            .commands
            .find("mcp__my_server__review_file")
            .expect("the sanitised prompt command is registered");
        assert_eq!(command.description, "Review a file");

        // A second refresh must not register the same command twice.
        let before = repl.commands.len();
        repl.refresh_mcp_prompt_commands();
        assert_eq!(repl.commands.len(), before);
    }

    #[test]
    fn refresh_registers_prompts_discovered_after_the_manager_was_attached() {
        let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
        let manager = Arc::new(coderex_mcp::manager::McpManager::new());
        repl.with_mcp_manager(Arc::clone(&manager));
        assert!(repl.commands.find("mcp__my_server__review_file").is_none());

        // A server that connects (or pushes prompts/list_changed) updates the
        // manager's registry; the next refresh exposes the command.
        manager.register_prompt(sample_prompt());
        repl.refresh_mcp_prompt_commands();
        assert!(repl.commands.find("mcp__my_server__review_file").is_some());
    }

    #[test]
    fn prompt_result_renders_messages_for_the_transcript() {
        use coderex_mcp::prompts::{McpPromptContent, McpPromptMessage, McpPromptResult};

        let result = McpPromptResult {
            description: Some("Resolved".into()),
            messages: vec![
                McpPromptMessage {
                    role: "user".into(),
                    content: McpPromptContent::Text {
                        text: "hello".into(),
                    },
                },
                McpPromptMessage {
                    role: "assistant".into(),
                    content: McpPromptContent::Image {
                        data: "AAAA".into(),
                        mime_type: "image/png".into(),
                    },
                },
            ],
        };
        let command = prompt_result_command(&result);
        assert!(!command.state_changed);
        assert!(
            command.output.starts_with("Resolved\n[user]\nhello"),
            "{}",
            command.output
        );
        assert!(command.output.contains("[assistant]\n[image: image/png]"));
        assert!(!command.output.contains("AAAA"));
    }
}
