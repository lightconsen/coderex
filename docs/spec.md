CodeRex 功能对等设计方案

目标不是只复刻一个”会调工具的 REPL”，而是尽量对齐 coderex 的完整能力面：核心对话引擎、工具系统、上下文压缩、Hook、子代理、任务与团队、Bridge 远程控制、Server/SDK、MCP、插件与 marketplace、企业托管配置、团队记忆同步、自动记忆整理、语音/Vim/UI 产品能力，以及 feature flag 驱动的可选子系统。

一、设计目标

1. 功能对等优先：对齐 coderex 的主干能力和重要产品能力，而不是只做最小可用代理。
2. 核心与边缘分层：把“必须兼容”的内核能力和“feature gate/产品化”的扩展能力分别建模。
3. 强安全边界：工具执行、权限、远程控制、MCP、插件都必须有独立的安全模型。
4. Rust 原生实现：单一二进制、稳定并发、长期运行、跨平台、一致的状态模型。
5. 可裁剪发布：支持 core、full、enterprise 三个发行层级，避免把所有 Anthropic 内部特性硬编码成单版本耦合物。

二、与原版对齐的能力范围

本 spec 以功能面而不是文件数对齐，覆盖以下几类系统：

- 核心引擎：QueryEngine、消息组装、流式响应、工具编排、structured output、budget/cost、compact、stop hooks。
- 工具系统：公开基线工具 + feature-gated/internal 工具 + MCP 动态工具。
- 命令系统：80+ slash commands、prompt/local/ui/standalone 四类命令、remote-safe/bridge-safe 可用性矩阵。
- 代理系统：主代理、Explore/Plan/Verification/statusline-setup 等专用子代理、后台执行、SendMessage、隔离 worktree。
- 任务与协作：Task、Team/Swarm、Coordinator Mode、后台 shell/agent 任务、任务输出与终止。
- 记忆系统：消息历史、session memory、长期记忆、team memory sync、auto dream consolidation。
- 安全系统：权限模式、沙箱、path allowlist、permission hooks、orphaned permission recovery、trusted device。
- 远程系统：Bridge、Remote Session、Server/daemon、SDK structured I/O、mobile/web 客户端、安全命令过滤。
- 扩展系统：MCP、plugin、marketplace、skills、bundled/plugin/MCP skills。
- 产品能力：Vim、voice、themes、statusline、buddy/companion、notifications、share/export/tag、review/security review。
- 企业能力：remote managed settings、security checks、checksum sync、org-scoped team memory。

三、技术栈选型

组件 | Rust 生态选型 | 理由
---|---|---
运行时 | tokio | 流式 API、多路工具执行、文件监听、后台任务、WebSocket
终端 UI | ratatui + crossterm | 替代 Ink/React，适合复杂 REPL 与 overlay
CLI | clap | 子命令、补全、非交互参数、远程/daemon 模式
序列化 | serde + serde_json + toml | settings、session、tool schema、RPC 载荷
日志 | tracing + tracing-subscriber | 结构化日志和调试追踪
HTTP | reqwest | API、OAuth、remote managed settings、team memory sync
WebSocket | tokio-tungstenite | Bridge、Server、remote session、SDK transport
JSON Schema | jsonschema | StructuredOutput、tool input/output 校验
进程执行 | tokio::process | Shell、hooks、MCP stdio server、plugin helper
沙箱 | nix + 平台原生机制 | Linux seccomp/rlimit；macOS sandbox-exec 等效；Windows Job Objects
SQLite | sqlx 或 rusqlite | 长期记忆、会话索引、缓存元数据
LSP | tower-lsp 客户端封装 | 跳转定义、引用、hover、workspace symbols
文件遍历 | ignore + walkdir | gitignore 兼容、高效遍历
模糊匹配 | nucleo-matcher 或 fuzzy-matcher | 命令、skills、themes 搜索
Markdown | comrak | Brief、消息渲染、导出
高亮 | syntect | 代码块和 diff 渲染
cron | cron | 定时任务解析
哈希与校验 | sha2 | team memory checksum、settings checksum、cache key
OAuth2 | oauth2 | MCP OAuth、登录、托管配置拉取

四、整体架构

```text
┌──────────────────────────────────────────────────────────────────────┐
│ User / Client Layer                                                 │
│ - 本地 CLI / REPL                                                   │
│ - Headless / SDK                                                    │
│ - Bridge Web / Mobile Remote Client                                 │
│ - Server / daemon WebSocket client                                  │
└──────────────────────────────┬───────────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│ UX Layer                                                            │
│ - ratatui REPL                                                      │
│ - slash commands                                                    │
│ - overlays / permissions / picker / statusline                      │
│ - voice / vim / buddy / notifications                               │
└──────────────────────────────┬───────────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Runtime Kernel                                                      │
│ - QueryEngine                                                       │
│ - ContextManager                                                    │
│ - ToolOrchestrator / StreamingToolExecutor                          │
│ - HookEngine                                                        │
│ - PermissionChecker / Sandbox                                       │
│ - TokenBudget / CostTracker                                         │
│ - SessionStore / TranscriptStore                                    │
│ - AgentManager / TaskManager / TeamManager                          │
│ - CompactEngine / StopHookRunner                                    │
└──────────────────────────────┬───────────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Extensibility Layer                                                 │
│ - Built-in tools                                                    │
│ - Feature-gated tools                                               │
│ - MCP tools/resources                                               │
│ - Slash commands                                                    │
│ - Skills (bundled / plugin / MCP)                                   │
│ - Plugins / marketplaces                                            │
└──────────────────────────────┬───────────────────────────────────────┘
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Services Layer                                                      │
│ - API client / model routing / retry                                │
│ - LSP manager                                                       │
│ - Bridge / Remote Session / Server                                  │
│ - Cron scheduler                                                    │
│ - Team memory sync                                                  │
│ - Remote managed settings                                           │
│ - Auto dream / memory extraction                                    │
│ - Analytics / diagnostics / feature flags                           │
└──────────────────────────────────────────────────────────────────────┘
```

五、模块设计

5.1 QueryEngine

QueryEngine 是内核，必须覆盖 coderex 中真正影响行为的关键路径，而不只是一个“LLM 调工具循环”。

```rust
pub struct QueryEngine {
    api_client: Arc<dyn ApiClient>,
    tool_registry: Arc<ToolRegistry>,
    command_registry: Arc<CommandRegistry>,
    context_manager: Arc<ContextManager>,
    hook_engine: Arc<HookEngine>,
    permission_checker: Arc<PermissionChecker>,
    budget: Arc<TokenBudget>,
    cost_tracker: Arc<CostTracker>,
    session_store: Arc<SessionStore>,
    agent_manager: Arc<AgentManager>,
    task_manager: Arc<TaskManager>,
    feature_flags: Arc<FeatureFlags>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
}
```

必须支持的行为：

1. 流式响应与增量 UI 更新。
2. StreamingToolExecution：模型输出中一旦稳定解析出 tool call，即可开始执行，不必等待整条消息结束。
3. Structured output enforcement：当调用方提供 JSON Schema 时，强制通过 StructuredOutput 工具或等效机制返回合法结果。
4. Tool use retry / stop hook / compact interplay：避免“上下文超限 -> compact -> stop hook -> 重试 -> 再 compact”的死循环。
5. Permission denial tracking：跟踪被拒绝工具调用，进入后续消息上下文。
6. Elicitation：工具或 MCP 服务需要用户补充信息时，能进入 REPL/UI 或 SDK structured I/O 提问流程。
7. Orphaned permission recovery：如果远程侧发来的 permission request 失配或断连，内核要能恢复并继续。
8. Snip/compact boundary：SDK/headless 模式支持历史截断边界；REPL 模式保留 scrollback。

建议接口：

```rust
pub struct SubmitOptions {
    pub max_turns: Option<usize>,
    pub max_budget_usd: Option<f64>,
    pub task_budget_tokens: Option<usize>,
    pub json_schema: Option<serde_json::Value>,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub replay_user_messages: bool,
    pub include_partial_messages: bool,
    pub handle_elicitation: Option<Arc<dyn ElicitationHandler>>,
    pub orphaned_permission: Option<OrphanedPermission>,
}
```

5.1.1 回合生命周期状态机

为了避免 Rust 实现重新退化成一个巨型 `query.ts`，这里把单次 turn 的业务流程显式定义成状态机。`QueryEngine` 负责调度，但不直接承载所有状态分支；建议引入 `TurnRuntime` 或 `TurnStateMachine` 作为运行时编排层。

```rust
pub enum TurnState {
    Initialize,
    BuildContext,
    RunPreSubmitHooks,
    StartRequest,
    StreamAssistant,
    ExecuteTools,
    AwaitPermission,
    AwaitElicitation,
    ApplyToolResults,
    EvaluateContinuation,
    CompactRecovery,
    StopHooks,
    Finalize,
    Terminated(TerminalReason),
}

pub enum TerminalReason {
    AssistantCompleted,
    MaxTurnsReached,
    BudgetExhausted,
    StopHookPrevented,
    UserInterrupted,
    FatalApiError,
    PermissionDeniedAndNotRecoverable,
}
```

推荐业务时序：

1. `Initialize`：建立本轮上下文，继承 session 状态，清理 turn-scoped 缓存。
2. `BuildContext`：组装消息、CLAUDE.md、memory attachments、git/system context、permission denial 摘要。
3. `RunPreSubmitHooks`：执行 `UserPromptSubmit`、必要的 classifier 或 prompt augmentation。
4. `StartRequest`：选择模型、预算参数、structured output 约束，发起流式请求。
5. `StreamAssistant`：持续接收 token、增量渲染、解析 tool call、累积 assistant message。
6. `ExecuteTools`：通过 `StreamingToolExecutor` 或批量 orchestration 执行工具。
7. `AwaitPermission`：若工具需要授权，则进入 permission 子流程；可被 hooks 直接放行/拒绝。
8. `AwaitElicitation`：若工具或 MCP 服务要求补充输入，则进入 REPL/SDK structured I/O 交互。
9. `ApplyToolResults`：落盘 tool results、生成 tool summary、更新 session storage 与 file history。
10. `EvaluateContinuation`：判断是否继续，依据包括 tool calls 是否未完成、token budget、max turns、max_output_tokens 恢复逻辑。
11. `CompactRecovery`：仅在上下文过长、max_output_tokens 或 compact 策略要求时进入，执行 micro/auto/reactive/snip compact，并产生 compact boundary。
12. `StopHooks`：执行 `Stop`/`PostCompact`/失败后 stop hooks，决定是否继续或终止。
13. `Finalize`：写入 transcript、usage、cost、session 标题、副作用摘要。

关键转移规则：

- `StreamAssistant -> ExecuteTools`：只要稳定解析出可执行 tool call，就允许提前进入工具执行，不等待整条 assistant message 结束。
- `ExecuteTools -> AwaitPermission`：工具被权限系统拦截，且无法由 `PermissionRequest` hook 直接裁决时进入。
- `ExecuteTools -> AwaitElicitation`：MCP 或工具返回显式 elicitation 请求时进入。
- `EvaluateContinuation -> CompactRecovery`：满足 prompt-too-long、reactive compact 或 snip 条件时进入。
- `CompactRecovery -> StartRequest`：恢复成功后重新发起请求，并保留 compact boundary。
- `StopHooks -> StartRequest`：仅当 stop hooks 允许 continuation，且未触发 budget/turn 上限时发生。
- 任意状态均可 `-> Terminated(UserInterrupted)`：用户中断、abort signal、远程 stop work 触发。

必须避免的坏路径：

- `compact -> stop hook blocking -> retry -> compact` 无限循环。
- `max_output_tokens` 错误提前暴露给 SDK 调用方，导致外部消费者退出但恢复循环仍在后台继续。
- permission request 或 elicitation 丢失后，本轮卡死在无法恢复的等待态。

建议实现：

```rust
pub struct TurnRuntime {
    pub state: TurnState,
    pub messages: Vec<Message>,
    pub pending_tool_calls: Vec<ToolCall>,
    pub pending_permission: Option<PermissionRequest>,
    pub pending_elicitation: Option<ElicitationRequest>,
    pub budget_tracker: BudgetTracker,
    pub recovery_count: usize,
    pub stop_hook_active: bool,
}
```

5.2 TokenBudget 与 CostTracker

TokenBudget 不只是“达到阈值即停”，而应复刻 coderex 的两个核心判断：

1. 消耗比例是否接近预算上限。
2. 连续若干次 continuation 后，delta token 是否已接近收益递减。

```rust
pub struct TokenBudget {
    continuation_count: usize,
    last_delta_tokens: usize,
    last_global_turn_tokens: usize,
    started_at: Instant,
    completion_threshold: f64,
    diminishing_threshold: usize,
}
```

CostTracker 需要统计：

- input / output tokens
- cache creation / cache read tokens
- API duration
- model-specific pricing
- per-turn / per-session / per-agent / per-task cost

并能够驱动 `/cost`、`/stats`、session summary、analytics 输出。

5.3 工具系统

coderex 真实能力不是“固定 43 个工具”这么简单，而是：

1. 一组稳定公开基线工具。
2. 一组 feature-gated 或环境限定工具。
3. 一组 MCP 动态工具。

所以 CodeRex应拆成三层注册：

```rust
pub enum ToolTier {
    Baseline,
    FeatureGated,
    DynamicMcp,
    Internal,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn tier(&self) -> ToolTier;
    fn input_schema(&self) -> serde_json::Value;
    fn output_schema(&self) -> Option<serde_json::Value>;
    fn requires_approval(&self, args: &serde_json::Value) -> bool;
    fn is_enabled(&self, ctx: &RuntimeContext) -> bool;
    fn is_read_only(&self) -> bool;
    fn is_concurrency_safe(&self) -> bool;
    fn interrupt_behavior(&self) -> InterruptBehavior;
    async fn execute(&self, args: serde_json::Value, ctx: ToolExecutionContext)
        -> Result<ToolOutput, ToolError>;
}
```

5.4 基线工具集

对外公开的基线工具按功能分组如下。

文件操作：

1. Read
2. Write
3. Edit
4. NotebookEdit

Shell：

5. Bash
6. PowerShell
7. Sleep

搜索与代码理解：

8. Glob
9. Grep
10. LSP
11. ToolSearch

网络：

12. WebFetch
13. WebSearch

代理与消息：

14. Agent
15. SendMessage
16. RemoteTrigger
17. Brief
18. AskUserQuestion

任务：

19. TaskCreate
20. TaskGet
21. TaskList
22. TaskUpdate
23. TaskOutput
24. TaskStop

团队：

25. TeamCreate
26. TeamDelete

计划与工作树：

27. EnterPlanMode
28. ExitPlanMode
29. EnterWorktree
30. ExitWorktree

Cron：

31. CronCreate
32. CronDelete
33. CronList

MCP：

34. McpToolProxy
35. McpAuth
36. ListMcpResources
37. ReadMcpResource

配置与输出：

38. Config
39. Skill
40. StructuredOutput
41. TodoWrite
42. REPL

兼容补位：

43. REPL 内部兼容工具位或 SyntheticOutput 等价层

注：这里保留“43 个公开基线工具”的概念，以对齐 coderex 的系统提示与用户感知；但实现层必须允许 feature-gated/internal 工具在运行时扩展，不把 43 这个数字当作硬上限。

5.5 feature-gated / internal 工具

早期版本为下列 13 个名字注册了“永远不可用”的占位工具（`is_enabled: false`，
任何开关都无法启用，模型也看不到它们）：WebBrowser、TerminalCapture、CtxInspect、
Monitor、WorkflowTool、SnipTool、VerifyPlanExecution、SuggestBackgroundPR、
PushNotification、SendUserFile、SubscribePR、ListPeers、Tungsten。

它们已全部删除，不再作为“已实现”计数。真正需要 feature gate 的只有 5 个 Chrome
DevTools 工具（`ToolTier::FeatureGated`、`is_enabled: true`，由 `--no-chrome` 控制）。
将来若要新增上述能力，应当连同真实实现与开关一起加入，而不是先注册一个只会拒绝的桩。

建模：

```rust
pub struct ToolRegistry {
    baseline: HashMap<String, Arc<dyn Tool>>,
    gated: HashMap<String, Arc<dyn Tool>>,
    dynamic_mcp: DashMap<String, Arc<dyn Tool>>,
    context_allowlists: HashMap<ExecutionContext, HashSet<String>>,
    deny_rules: Vec<ToolDenyRule>,
}
```

5.6 执行上下文与可用工具矩阵

coderex 中工具不是“全时可用”。CodeRex应支持：

```rust
pub enum ExecutionContext {
    Normal,
    AsyncAgent,
    CoordinatorMode,
    InProcessTeam,
    PlanMode,
    ReplOnly,
    HeadlessSdk,
    RemoteBridge,
}
```

差异例子：

- PlanMode 只允许读和分析类工具。
- AsyncAgent 避免 UI 依赖工具与交互式 permission prompt。
- CoordinatorMode 只允许调度、观察、聚合结果。
- REPL 工具模式会收窄直接文件/搜索/shell 工具，强制批处理工作流。

5.7 子代理系统

子代理不只是 “spawn 一个新 QueryEngine”。功能对等实现必须支持：

- 专用 agent type：general-purpose、Explore、Plan、Verification、ClaudeCodeGuide、StatuslineSetup、自定义 agent
- fork 继承上下文
- run_in_background
- worktree 隔离
- 自定义工具白名单
- 继续已存在代理
- 子代理与主代理互发消息
- 子代理作为命令实现后端，例如 `/statusline` -> `statusline-setup`

```rust
pub enum AgentType {
    GeneralPurpose,
    Explore { thoroughness: Thoroughness },
    Plan,
    Verification,
    ClaudeCodeGuide,
    StatuslineSetup,
    Custom { prompt: String },
}
```

5.8 Task、Team 与 Coordinator

Task 系统需要统一承载：

- 本地 shell 任务
- 本地 agent 任务
- 主会话后台化
- dream task
- in-process teammate task

```rust
pub enum TaskKind {
    LocalShell,
    LocalAgent,
    MainSession,
    Dream,
    InProcessTeammate,
}
```

Team/Swarm 对等能力：

- TeamCreate / TeamDelete
- 队员共享任务视图
- teammate idle / task completed 事件
- 团队上下文记忆

Coordinator Mode 需要显式支持四阶段：

1. Research
2. Synthesis
3. Implementation
4. Verification

5.9 Hook 系统

旧 spec 对 hook 覆盖不够完整。功能对等版必须把事件面扩展到真实使用场景。

```rust
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    SessionStart,
    Setup,
    SubagentStart,
    Notification,
    PermissionRequest,
    PermissionDenied,
    CwdChanged,
    FileChanged,
    WorktreeCreate,
    PreCompact,
    PostCompact,
    Stop,
    SubagentStop,
    TaskCompleted,
    TeammateIdle,
    Elicitation,
    ElicitationResult,
    TurnEnd,
}
```

Hook 返回能力：

- continue / stop
- suppressOutput
- permission decision
- updated input
- additional context
- watchPaths
- updated MCP tool output
- permission request short-circuit
- elicitation accept / decline / cancel

同步与异步 hook 都需要支持，且要有超时、环境变量注入、匹配器、内部 hook 标记。

5.10 Slash Commands

CodeRex命令系统除了命令本身，还要对齐两个实际运行特征：

1. 命令类型：Local、LocalUi、Prompt、Standalone。
2. 命令可用性矩阵：非交互可用、remote-safe、bridge-safe、feature-gated。

```rust
pub struct SlashCommand {
    pub name: &'static str,
    pub kind: CommandType,
    pub supports_non_interactive: bool,
    pub remote_safe: bool,
    pub bridge_safe: bool,
    pub allowed_tools_override: Option<Vec<String>>,
}
```

核心命令族：

- 会话管理：/session /resume /rename /clear /rewind /exit
- 配置：/config /model /theme /permissions /hooks /mcp /plugin /memory /skills /tasks /plan
- 帮助/统计：/help /version /stats /status /usage /cost /doctor
- 内容操作：/copy /init /compact /summary /files /context /ctx_viz /add-dir
- 集成：/ide /chrome /desktop /mobile /voice /vim /remote-env /remote-setup
- Git/PR：/commit /commit-push-pr /review /security-review /autofix-pr /pr_comments /branch
- 调试诊断：/debug-tool-call /heapdump /mock-limits /perf-issue /ant-trace /break-cache /reload-plugins
- 高级：/bridge /bridge-kick /teleport /ultraplan /insights /thinkback /thinkback-play /statusline
- 其他：/advisor /btw /good-claude /stickers /bughunter /issue /env /diff /oauth-refresh /backfill-sessions

5.11 Context / Compact 系统

需要完整支持：

- micro compact
- auto compact
- reactive compact
- snip compact
- compact boundary message
- pre/post compact hooks
- compact 后的 transcript cleanup

系统上下文来源：

- git branch / status / recent commits
- CLAUDE.md / nested memory attachments
- tool result summaries
- permission denials
- loaded skills / commands / agents / MCP resources
- 当前日期与运行环境

5.11.1 提示词引擎与上下文装配管线 (Prompt & Context Rendering)

为了最大化利用 Anthropic Prompt Caching 并在对话过程中保持缓存命中率，coderex 的 System Prompt 和上下文装配并非简单的字符串拼接，而是经过了精心设计的分层架构：

1. **System Prompt Registry (系统提示词装配)**：
   提示词被拆分为 Static（静态可高度缓存）与 Dynamic（单回合动态生成）两部分：
   - **Static Sections**：基础设定、输出格式(`outputStyle`)、工具使用规范(`getUsingYourToolsSection`)。这部分前置，内部严格禁止环境变动变量渗入，通过特定分隔符作为 Prompt Caching 的第一级边界。
   - **Dynamic Sections**：每回合重新计算，如 `session_guidance`、`memory` 最新快照、Feature Flags 控制的约束（如 `[token_budget]` 限额）、最近接入动态刷新的 `mcp_instructions`。

2. **User Context vs System Context**：
   在真正合并时，coderex 刻意分离了 `systemContext` 与 `userContext` 的存放位置，体现了深度的工程优化：
   - **System Context (系统基准)**：如 `git status`、`recent commits` 等仓库环境快照。这部分被调用 `appendSystemContext` 直接附加在组装好的 System Prompt 尾部。
   - **User Context (用户文档/指令)**：如贯穿全本的多层级 `CLAUDE.md`、`currentDate` 等。此部分**并非放入系统提示词**，而是通过 `prependUserContext`，**伪装成整个对话历史数组 `messages[]` 的第 1 条用户消息**，将其巧妙地包裹在 `<system-reminder>` XML 标签内。

CodeRex应使用 `PromptCompiler` 负责此管线：

```rust
pub struct PromptCompiler {
    static_blocks: Vec<PromptBlock>,
    dynamic_registry: Mutex<HashMap<String, Box<dyn DynamicSectionFn>>>,
}

impl PromptCompiler {
    pub async fn build_runtime_context(&self, state: &AppState) -> (String, Vec<Message>) {
        // 1. 构建 System Prompt (Cache Hit 关键段)
        let mut system_prompt = self.render_static_blocks();
        system_prompt.push_str("\n\n=== BOUNDARY MARKER - DO NOT MOVE OR REMOVE ===\n\n");
        system_prompt.push_str(&self.render_dynamic_blocks(state).await);
        
        let sys_ctx = self.get_system_context(state).await; // 注入 git status 等
        system_prompt.push_str(&sys_ctx);

        // 2. 构建 User Context 层 (CLAUDE.md 等长文本)
        let mut context_messages = Vec::new();
        let user_ctx = self.get_user_context(state).await;
        if !user_ctx.is_empty() {
            context_messages.push(Message::user(format!(
                "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n{}\n\nIMPORTANT: this context may or may not be relevant... \n</system-reminder>", 
                user_ctx
            )));
        }
        
        (system_prompt, context_messages) // context_messages 将作为历史消息序列的前缀 prepend 进去
    }
}
```
隔离的工程意义：1. 将频繁用户变动的冗长 `CLAUDE.md` 放入首条独立消息里，不仅能独立命中 Message Cache 前缀，还不会在修改 Markdown 时导致稳定昂贵的 System Prompt 缓存失效。2. `git status` 留给系统层，给模型保持对“所处环境绝对基座”的最强约束力。

5.12 记忆系统

旧 spec 只写了三层记忆，功能对等版要补齐两个真实子系统：team memory sync 和 auto dream。

基础层：

- short-term conversation history
- session memory / extracted facts
- long-term memory store

扩展层：

- team memory sync：repo 维度、服务端 upsert、checksum 增量同步、删除不传播
- auto dream：满足时间和 session 数阈值后，fork 只读受限子代理进行背景整理

5.12.1 数据持久化与存储 Schema (Data Storage)

coderex 完全依赖文件系统和特定组织架构来进行状态管理。为了保证与 coderex 生态的兼容性（包括允许未来两个版本共享配置或离线存储），CodeRex本的本地数据目录必须严格对齐 `getClaudeConfigHomeDir()`（默认 `~/.coderex`）的路径范式：

目录拓扑规范：
```text
~/.coderex/
├── settings.json            # 用户全局配置
├── managed_settings.json    # 企业远程托管配置的缓存 (checksum 校验)
├── policy_limits.json       # API 速率、Budget 等服务端策略限额缓存
├── .mcp.json                # MCP Server 注册表
├── memory/                  # 全局个人长期记忆 (Memdir)
├── projects/                # 按项目 (通常按路径) 拆分的会话存档
│   └── <project_hash>/
│       ├── <session_id>.jsonl    # 对话流水账 (JSONL 追加写入)
│       └── memory/               # 项目维度的提取记忆体系 (Auto Dream 产物)
├── skills/                  # 用户级自定义 Skill 存放区
├── agents/                  # 用户级自定义 Sub-agents 存放区
├── uploads/                 # Bridge 客户端上传缓存区
└── telemetry/               # 待后台清空的离线埋点事件暂存
```

为什么使用 JSONL 而不是全量放入 SQLite？
coderex **没有**使用关系型数据库保存对话流 (Transcript)，而是使用 JSONL `O_APPEND` 追加写入。
- 落盘抗毁：在后台崩溃时不易掉数据，追加行为原子且安全。
- 内存可控：流式读取（部分 session 文件可达 GB 级别），且支持 `SKIP_FIRST_PROMPT_PATTERN` 正则跳过无用进度事件。
- 第三方兼容：可以直接供外部编辑器或其他工具直接 `tail -f` 监控大模型输出。

附属索引层 (SQLite Session Meta Index) 设计：
虽然内容体使用 JSONL，但为了支持快速的 `/history` 搜索、`/cost` 聚合统计，CodeRex建议引入 SQLite 数据库（如 `~/.coderex/sessions.db`）建立元数据索引层。仅依靠文件扫描会产生 OOM 与“I/O 读取刺客”问题：

```sql
-- 推荐的 rusqlite Schema
CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY,
    project_path TEXT NOT NULL,
    ai_title TEXT,
    tag TEXT,
    pr_url TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    last_active_at DATETIME,
    total_cost_usd REAL DEFAULT 0.0,
    total_tokens INTEGER DEFAULT 0
);

CREATE INDEX idx_project_path ON sessions(project_path);
CREATE INDEX idx_last_active ON sessions(last_active_at DESC);
```
此架构同时保障了 coderex 级别的安全恢复机制与 Rust 原生的高效检索能力。

```rust
pub struct MemorySystem {
    short_term: Arc<Mutex<Vec<Message>>>,
    session_memory: Arc<SessionMemoryStore>,
    long_term: Arc<LongTermStore>,
    team_memory: Option<Arc<TeamMemorySyncService>>,
    auto_dream: Option<Arc<AutoDreamService>>,
}
```

5.13 权限系统与沙箱

权限模式：

- Default
- AcceptEdits
- BypassPermissions
- DontAsk
- Plan
- Auto

权限结果：

- Allow
- Ask
- Deny
- AllowForPath
- Elevated

功能对等特性：

- allow/deny/ask rule sources 合并
- additional working directories
- dangerous rule stripping
- background agent auto-deny permission prompt
- automated checks before dialog
- prePlanMode restore
- permission request hooks 可直接短路授权

5.14 MCP 服务

MCP 对等能力必须包括：

- stdio JSON-RPC server lifecycle
- tool discovery
- resource discovery/read
- OAuth
- channel permissions / allowlist
- elicitation 处理
- reconnect / connection manager UI
- in-process transport 和 SDK control transport 的扩展点

```rust
pub struct McpManager {
    servers: DashMap<String, McpServer>,
    tools: DashMap<String, McpToolInfo>,
    resources: DashMap<String, McpResourceInfo>,
}
```

5.15 Plugin、Marketplace 与 Skills

功能对等实现不能只停留在“加载本地 JS 插件”。还必须支持：

- bundled plugins
- plugin commands
- plugin skills
- marketplace reconciliation
- 后台 marketplace 安装
- 安装后自动 refresh 或标记 needsRefresh
- skills from bundled / user dir / repo dir / plugin / MCP
- frontmatter hooks / agent / allowed tools / metadata 解析

```rust
pub struct PluginManager {
    plugins: DashMap<String, PluginHandle>,
    marketplaces: DashMap<String, MarketplaceHandle>,
    needs_refresh: AtomicBool,
}
```

5.16 Bridge、Remote Session 与 Server

Bridge 是功能对等的关键系统之一，不能只写成一个普通 WebSocket 服务。

必须支持：

- 环境注册与 capacity 报告
- 轮询/推送工作分配
- 权限请求转发
- 会话归档
- 心跳与断线恢复
- spawn mode：single-session / same-dir / worktree
- 在 worktree mode 下为每个远程 session 创建隔离 git worktree
- recent session pointer 与跨 worktree 恢复
- 移动端 / web 远程 client 的命令过滤

Server/daemon 必须支持：

- detached session server
- WebSocket authenticated sessions
- idle cleanup
- session listing
- SDK/headless structured I/O

5.16.1 Bridge 远程会话业务时序

Bridge 的本质不是“开个 ws 服务器”，而是一个远程工作编排系统。CodeRex应把会话生命周期显式定义出来，避免把 spawn、cleanup、title、permission forwarding 和 stopWork 逻辑揉在一个巨大函数里。

推荐时序：

1. 启动时注册环境，声明 capacity、spawn mode、工作目录能力。
2. 进入长轮询或 WebSocket work feed，接收远程 work assignment。
3. 为每个 assignment 决定 spawn mode：`single-session`、`same-dir`、`worktree`。
4. 若是 `worktree`，先尝试创建隔离 worktree；失败则调用远端 `stopWork` 并记录清理失败原因。
5. 构建本地 session 目录、SDK URL、access token、logger、pending permission store。
6. spawn 本地 session 进程或内嵌 runtime，并把首条用户消息绑定到 title derivation 逻辑。
7. 运行期持续桥接：消息流、permission request、elicitation、stop work、session archive、heartbeat。
8. 会话结束后清理 worktree、释放 capacity、写 session archive、上报终止原因。

建议引入显式状态：

```rust
pub enum BridgeSessionState {
        Assigned,
        Preparing,
        CreatingWorktree,
        Spawning,
        Running,
        AwaitingPermission,
        Archiving,
        CleaningUp,
        Completed,
        Failed(String),
}
```

建议时序图：

```text
Remote Control Plane
    -> BridgeService.register_environment()
    -> BridgeService.poll_work()
    -> BridgeService.prepare_session(work)
         -> maybe create_worktree()
         -> spawn session runtime
    -> Session runtime emits messages / tool calls / permission requests
    -> BridgeService forwards permission requests to remote client
    -> Remote client responds allow/deny
    -> BridgeService resumes local tool execution
    -> Session ends
    -> archive_session()
    -> cleanup_worktree_if_needed()
    -> stopWork/completeWork acknowledgement
```

实现约束：

- `spawnMode` 必须在进入耗时 await 前冻结，避免 UI 切换模式导致遥测和清理状态矛盾。
- `worktree` 创建与清理都必须幂等，且允许 hook-based worktree provider 替代 git-native worktree。
- `initial session` 与 `on-demand session` 的目录策略必须区分，否则会破坏原有 same-dir UX。
- `permission request` 必须持久化到 pending store，避免远程响应晚到时丢失关联。
- `recent session pointer` 要支持跨 worktree 恢复，否则远程 resume 流会出现“明明有会话却找不到”。

桥接层建议拆分为以下组件：

- `BridgeWorkPoller`：拉取/确认远程 work。
- `BridgeSessionSpawner`：决定 spawn mode，准备目录和 runtime。
- `BridgePermissionRouter`：维护 pending permission 与响应回写。
- `BridgeArchiveWriter`：归档 transcript、usage、metadata。
- `BridgeCleanupManager`：worktree、session process、in-flight cleanup futures。

5.17 Structured I/O / SDK

要实现与 coderex headless path 对等，必须提供：

- structured event stream
- tool call / tool result / status / compact boundary 事件
- permission request 事件
- elicitation 请求与响应
- JSON schema structured output
- pending permission request tracking

5.18 REPL/UI 与产品能力

REPL 不只是消息滚动区。功能对等实现至少要支持：

- alternate screen / overlay / modal
- global keybindings
- search highlight
- copy/export/share 流程
- permissions dialog
- notifications
- message selector
- scrollbox / large transcript virtualization
- theme system
- Vim mode
- voice input / STT integration
- statusline setup flow
- buddy / companion sprite 与 teaser notification

Buddy 属于可选产品能力，但 spec 必须承认它存在：

- deterministic companion generation from user id
- persisted soul / regenerated bones
- startup teaser notification
- sprite reaction / petting / idle presence

5.18.1 UI 范式转换与状态渲染体系 (UI Paradigm Shift)

coderex 的 UI 层使用了基于 React 的 `Ink` 框架，这意味着它深度依赖虚拟 DOM、组件生命周期和响应式状态（如 `useState`, `useAppState`）。而 CodeRex采纳了 `ratatui`，这是一个即时渲染 GUI (Immediate Mode GUI) 框架。要实现平滑迁移并保证与后台引擎的高效互动，必须对 UI 架构做以下范式颠覆：

1. 分离状态所有权结构（全局状态分离）
在 React 中，状态散布在全局 `AppState` 和各类组件自身的 Hooks 内。在 CodeRex中，UI 状态（焦点、层级、按键输入）必须与业务内核彻底解耦，形成独立、集中持有的结构树，只读借用内核数据做快照。
```rust
pub struct UiState {
    pub active_view: ViewMode, // 比如 MainRepl, PermissionModal, MessageSelector
    pub input_buffer: String,
    pub cursor_position: usize,
    pub overlays: Vec<OverlayType>, // 支持多层浮层覆盖
    pub scroll_offset: usize,
    pub notifications: Vec<Notification>,
    // 将 coderex 原有的零散 useEffect 变量全部集中化
    pub companion: CompanionState, 
}
```
渲染线程应拥有 `UiState` 的**排他可变借用权**。业务层传递过来的数据仅作无锁或浅锁极快读取，必须保证终端在任何后台重 I/O 阻塞期间，光标与按键响应绝不能假死。

2. 基于 MPSC Channel 的事件循环宏观抽象
脱离了 React 响应式渲染，我们需要显式的单向数据流与事件分发把 I/O 操作和重绘解耦。终端按键、系统时钟（Tick）、以及来自 `QueryEngine` 的底层业务事件必须汇入同一个 `mpsc::unbounded_channel` 控制管道进行主线程削峰：
```rust
pub enum UiEvent {
    Input(crossterm::event::KeyEvent),
    Tick, // 高频触发，用于驱动动画(Spinner)、小助手发呆、长连接心跳
    EngineUpdate(EngineEvent), // 包含后台大模型逐字文本流、进度更新、权限拒绝等
    Resize(u16, u16),
}
```
整个 UI 生命周期的底层将通过 `tokio::select!` 统一消费 `UiEvent`。拿到事件后立即更新对应的 `AppState` 和 `UiState`，然后无论状态变与不变，触发一次全屏 Frame 重绘。

3. Ratatui 即时绘制树 (Immediate Mode Rendering Tree)
coderex 的 `<Box><Text color="green">{reaction}</Text></Box>` 组件体系，必须从“常驻内存对象树”重构为“在每一帧 (Tick) 由顶向下的全量重新计算”。
所有部件的绘制抽象为纯函数 `render(&mut Frame, &AppState, &UiState)`，不存在状态留滞污染。
```rust
impl TerminalUi {
    pub fn draw(&mut self, frame: &mut Frame, app: &AppState) {
        let chunks = Layout::default()
            .constraints([
                Constraint::Min(1),    // Transcript / 历史消息聊天区
                Constraint::Length(1), // Statusline / 状态与提示栏
                Constraint::Length(3), // Input / 用户输入区域
            ])
            .split(frame.size());
            
        // 1. 各个面板纯函数式地计算当前快照并画入缓冲区
        self.draw_history_transcript(frame, chunks[0], &app.session);
        self.draw_statusline(frame, chunks[1], &app);
        self.draw_input_repl(frame, chunks[2], &app.ui);
        
        // 2. 若存在模态框、权限拒绝或小助手通知，悬浮画在最顶层
        if let Some(overlay) = app.ui.overlays.last() {
            self.draw_overlay(frame, overlay);
        }
    }
}
```
**这一层变革的工程收益**：
彻底消灭了 coderex 基于 CSS-like Flexbox 模型在长文本输出和特殊尺寸终端下偶尔导致的渲染警告、组件高度重叠、滚屏撕裂等痼疾。让 Rust 具有比 Ink 更加像素级且绝对硬实时的布局操控能力。

5.19 配置、托管配置与企业功能

配置层级：

- built-in defaults
- user config
- project config
- runtime overrides
- remote managed settings

remote managed settings 对等能力：

- 企业用户 eligibility 检查
- checksum-based fetch
- security check
- loading barrier，允许其他系统等待远程配置拉取完成
- 失败时 fail open，不阻塞启动

```rust
pub struct ConfigService {
    defaults: Settings,
    user: Option<Settings>,
    project: Option<Settings>,
    runtime: RuntimeOverrides,
    remote_managed: Option<ManagedSettings>,
}
```

5.20 Analytics、Feature Flags 与诊断

Feature flags 不能只是本地 bool map，必须支持：

- env overrides
- remote experimentation source
- stale cache 容忍
- build-time gates

关键 flags 族：

- streaming_tool_execution
- tool_use_summaries
- fast_mode
- coordinator_mode
- history_snip
- agent_triggers
- bridge_mode
- daemon
- voice_mode
- buddy
- workflow_scripts
- web_browser_tool
- context_collapse
- terminal_panel
- kairos / proactive
- team_collaboration
- auto_dream

六、关键数据结构

```rust
pub struct AppState {
    pub settings: Settings,
    pub main_loop_model: ModelId,
    pub tool_permission_context: ToolPermissionContext,
    pub tasks: IndexMap<TaskId, TaskInfo>,
    pub foregrounded_task_id: Option<TaskId>,
    pub viewing_agent_task_id: Option<TaskId>,
    pub mcp: McpState,
    pub plugins: PluginState,
    pub agents: AgentState,
    pub team_context: Option<TeamContext>,
    pub bridge: BridgeState,
    pub ui: UiState,
    pub todos: Vec<TodoItem>,
    pub notifications: Vec<Notification>,
    pub coordinator: Option<CoordinatorState>,
    pub remote_settings_loaded: bool,
}
```

```rust
pub struct SessionState {
    pub session_id: Uuid,
    pub title: Option<String>,
    pub messages: Vec<Message>,
    pub usage: UsageSummary,
    pub cost_usd: f64,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub cwd: PathBuf,
    pub active_worktree: Option<WorktreeState>,
}
```

七、并发模型

1. UI 主线程负责渲染与输入，不做重 I/O。
2. tokio 多线程运行时负责 API、工具、MCP、LSP、Bridge、Cron、文件与网络 I/O。
3. 长时后台任务统一抽象成 TaskHandle，可取消、可前后台切换、可查询输出。
4. 所有跨模块状态同步优先事件流，其次读多写少共享状态。

```rust
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (engine_tx, engine_rx) = mpsc::unbounded_channel();

    let runtime = Runtime::bootstrap(ui_tx.clone(), engine_rx).await?;
    let ui = TerminalUi::new(ui_rx, runtime.handle());

    tokio::try_join!(runtime.run(), ui.run())?;
    Ok(())
}
```

七点一 启动初始化时序

从架构设计角度看，启动流程是 coderex 和 spec 最大的差异之一。真实系统启动时会并行加载配置、命令、skills、plugins、MCP、remote settings、UI 状态；如果顺序没定义清楚，CodeRex很容易出现隐式依赖和竞态。

建议把启动流程拆成 4 个阶段：

1. `Bootstrap`：只初始化最小运行前提，不触发重 I/O。
2. `Configuration`：收敛所有设置来源，得到可用 `CapabilitySet`。
3. `Extensions`：基于 capability 加载 commands、skills、plugins、MCP。
4. `RuntimeReady`：启动 UI、remote services、background workers。

建议时序：

```text
main()
    -> init tracing / panic hooks / environment
    -> create bootstrap AppState
    -> initialize remote managed settings loading barrier
    -> load local settings (default + user + project + runtime flags)
    -> if eligible, fetch remote managed settings and merge
    -> derive CapabilitySet / FeatureFlags
    -> load commands registry
    -> load skills (bundled / user / repo / plugin / MCP)
    -> initialize plugin manager
    -> schedule background marketplace reconciliation
    -> initialize MCP manager and configured servers
    -> initialize memory services (session memory / team memory sync / auto dream)
    -> initialize bridge/server services if enabled
    -> construct QueryEngine / registries / task managers
    -> start REPL UI or SDK/headless transport
```

设计原则：

- `remote managed settings` 必须尽早发起，但不能把整个启动链条硬阻塞到网络成功；需要 barrier 供依赖方等待。
- `commands` 和 `skills` 的加载必须发生在 `CapabilitySet` 确定之后，否则 feature-gated 命令和 MCP skills 会错配。
- `plugin marketplace reconciliation` 应该后台运行，不阻塞主 UI；但其安装状态要能写入 `AppState` 以渲染启动中的 pending 状态。
- `MCP manager` 应支持“先建管理器，后异步连服务器”，避免启动路径因某个 MCP server 超时而拖死。
- `memory services` 中 team memory sync 和 auto dream 都不应阻塞 REPL ready；它们是 background service。

建议启动编排对象：

```rust
pub struct StartupOrchestrator {
        config_loader: Arc<ConfigLoader>,
        command_loader: Arc<CommandLoader>,
        skill_loader: Arc<SkillLoader>,
        plugin_manager: Arc<PluginManager>,
        mcp_manager: Arc<McpManager>,
        memory_system: Arc<MemorySystem>,
        bridge_service: Option<Arc<BridgeService>>,
}
```

建议显式输出启动阶段事件，供 UI 或日志使用：

```rust
pub enum StartupPhase {
        Bootstrap,
        LoadSettings,
        MergeRemoteManagedSettings,
        BuildCapabilities,
        LoadCommands,
        LoadSkills,
        InitPlugins,
        ReconcileMarketplaces,
        InitMcp,
        InitMemory,
        InitRemoteServices,
        Ready,
}
```

八、发行层级建议

为避免一开始把 Anthropic 内部/产品化特性和核心代理耦死，建议拆三层发行：

Core：

- QueryEngine
- 基线工具
- compact
- hooks
- tasks
- agent
- MCP
- REPL

Full：

- Bridge/server
- workflow tool
- web browser / terminal capture / ctx inspect
- voice / vim / statusline / buddy
- plugin marketplace
- cron / team / coordinator

Enterprise：

- remote managed settings
- team memory sync
- trusted device / enhanced auth
- org-scoped remote policy

九、实施路线图

Phase 0: 骨架

- CLI + REPL 启动
- 基础 AppState
- QueryEngine 单轮对话
- EchoTool / NoOpTool

Phase 1: 核心内核

- Streaming API
- TokenBudget / CostTracker
- StructuredOutput
- 权限系统基础版
- transcript/session store

Phase 2: 基线工具

- Read / Write / Edit / NotebookEdit
- Bash / PowerShell / Sleep
- Glob / Grep / WebFetch / WebSearch
- Tool registry + execution contexts

Phase 3: 上下文与 Hook

- CLAUDE.md / nested memory
- Micro/Auto/Reactive/Snip compact
- Hook engine 全事件模型
- stop hook / compact boundary

Phase 4: Agent / Task / Team

- AgentTool + AgentManager
- Task manager
- background tasks
- TeamCreate / TeamDelete / SendMessage
- worktree isolation

Phase 5: 命令系统与 UI 产品化

- slash commands
- overlays / permissions / picker
- statusline flow
- vim mode
- notifications

Phase 6: MCP / Plugin / Skills

- MCP server lifecycle + OAuth + resources + elicitation
- bundled/user/repo/plugin/MCP skills
- plugin loader + marketplace reconciliation

Phase 7: Remote / Server / SDK

- Bridge service
- daemon server
- structured SDK I/O
- remote-safe / bridge-safe command filtering
- worktree spawn mode

Phase 8: 企业与高级特性

- remote managed settings
- team memory sync
- auto dream
- voice STT
- buddy
- analytics / diagnostics / feature flags polish

十、关键挑战与应对

挑战 | Rust 方案
---|---
流式工具执行与消息拼接 | 采用增量 parser + tool call assembler，稳定片段先执行
stop hooks 与 compact 的相互作用 | 将 stop hook、compact、retry 建模成状态机，而不是散落 if/else
SDK 与 REPL 两条执行路径复用 | 统一 EngineEvent 流，REPL 和 headless 只在消费端分叉
远程 permission request 丢失 | 引入 pending permission store + orphaned recovery
命令可用性矩阵复杂 | 在 CommandRegistry 中显式维护 non-interactive/remote/bridge 能力位
插件和 marketplace 热更新 | 后台 reconcile，成功后自动 refresh，失败时 needs_refresh
team memory 与长期记忆边界 | team memory 单独服务，不与个人 long-term memory 混存
企业托管设置与本地覆盖冲突 | 配置合并器按优先级合并并记录来源
feature flags 太多导致代码分叉 | 使用 CapabilitySet 统一收束运行时可用能力

十一、总结

这版 spec 的定位是“功能对等”，而不是“核心原型”。

它保留了原方案中正确的主干：QueryEngine、工具系统、Task/Agent、MCP、Bridge、Server、Cron、Hook、Compact、权限、记忆、UI；同时补齐了之前缺失或过度简化的真实子系统：feature-gated/internal tools、expanded hook event surface、remote-safe/bridge-safe command availability、plugin marketplace、team memory sync、remote managed settings、auto dream、buddy/statusline 等产品能力。

如果按这份 spec 落地，CodeRex不只是”像原版”，而是能够在架构上承接 coderex 的完整能力面，并允许按发行层级裁剪。
