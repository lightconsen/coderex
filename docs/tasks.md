# CodeRex — 实施任务清单

> 每个任务尽可能小、可测试、功能完备。后序任务依赖前序。
> 标记 `[ ]` 为未实现，`[x]` 为已实现。
> 测试要求：每个模块需有单元测试；跨模块交互需有集成测试；关键用户流程需有端到端测试。

---

## Phase 0: 项目骨架与基础设施

### 0.1 初始化 Rust 项目结构
- [x] 创建 `Cargo.toml`，配置 workspace 结构，包含以下 crate：`core`、`tools`、`agents`、`commands`、`hooks`、`memory`、`mcp`、`plugins`、`bridge`、`server`、`ui`、`sdk`、`config`、`cli`
- [x] 配置 `rust-toolchain.toml`（stable channel）
- [x] 配置 `clippy.toml` 与 `.cargo/config.toml`
- [x] 添加基础 CI：`cargo build`、`cargo test`、`cargo clippy`、`cargo fmt --check`
- **测试**：CI 通过，所有 crate 空测试通过
- **依赖**：无

### 0.2 定义公共类型与枚举 (crate: `core`)
- [x] 定义 `Message` 结构体（role: system/user/assistant/tool, content: text/tool_call/tool_result）
- [x] 定义 `ToolCall` 结构体（id, name, input JSON）
- [x] 定义 `ToolResult` 结构体（tool_call_id, output, is_error）
- [x] 定义 `UsageSummary` 结构体（input_tokens, output_tokens, cache_read, cache_creation）
- [x] 定义 `ModelId` 类型与已知模型枚举
- [x] 定义 `EngineEvent` 枚举（TextDelta, ToolCallStart, ToolCallEnd, CompactBoundary, PermissionRequest, ElicitationRequest, SessionEnd）
- **测试**：serde 序列化/反序列化 roundtrip 单元测试（9 tests）
- **依赖**：0.1

### 0.3 定义 AppState 与 SessionState (crate: `core`)
- [x] 定义 `SessionState` 结构体（session_id, title, messages, usage, cost_usd, created_at, last_active_at, cwd, active_worktree）
- [x] 定义 `AppState` 结构体骨架（settings, tasks, mcp, plugins, agents, ui, todos, notifications 等字段，先用占位类型）
- [x] 实现 `SessionState::new(cwd)` 构造函数
- **测试**：构造函数初始化后字段正确的单元测试（4 tests）
- **依赖**：0.2

### 0.4 通道与事件循环骨架 (crate: `core`)
- [x] 定义 `UiEvent` 枚举（Input, KeyPress, Tick, EngineUpdate, Resize）
- [x] 定义 `Runtime::bootstrap()` 骨架函数，返回通信通道和 handle
- [x] 实现主事件循环 `tokio::select!` 消费通道的骨架
- **测试**：发送事件到通道、确认被消费的集成测试（5 tests）
- **依赖**：0.2, 0.3

### 0.5 定义 UiState (crate: `ui`)
- [x] 定义 `UiState` 结构体（active_view, input_buffer, cursor_position, overlays, scroll_offset, notifications, companion 字段）
- [x] 实现 `UiState` 默认构造函数
- **测试**：UiState 初始化后字段正确的单元测试
- **依赖**：0.2

---

## Phase 1: 配置系统

### 1.1 配置目录与文件加载 (crate: `config`)
- [x] 实现 `~/.coderex/` 目录路径解析（兼容 `CLAUDE_CONFIG_DIR` 环境变量覆盖）
- [x] 实现 `settings.json` 读取与解析为 `Settings` 结构体
- [x] 实现三级配置合并：built-in defaults → user config → project config
- [x] 定义 `RuntimeOverrides` 结构体
- [x] 实现 `ConfigService::resolve()` → 返回最终合并后的 `Settings`
- **测试**：6 tests — 三级配置合并正确性、缺失配置文件 fallback、集成测试
- **依赖**：0.2

### 1.2 Feature Flags 与 CapabilitySet (crate: `config`)
- [x] 定义 `FeatureFlags` 结构体（包含 spec 中列出的所有 flag 字段）
- [x] 实现从 settings + env overrides 解析 FeatureFlags
- [x] 实现 `CapabilitySet` 派生逻辑（根据 FeatureFlags + 运行时条件决定可用能力）
- [x] 实现 build-time feature gate 宏 `#[cfg(feature = "...")]` 封装
- **测试**：6 tests — env override、CapabilitySet 正确开启/关闭
- **依赖**：1.1

### 1.3 Tracing 日志与 panic hooks
- [x] 配置 `tracing-subscriber`（JSON 输出、环境变量控制级别）
- [x] 安装 panic hook（打印 backtrace、写入 crash log）
- [x] 实现 `StartupPhase` 枚举与启动阶段事件发射
- **测试**：集成到 main.rs，CODEREX_LOG 环境变量控制
- **依赖**：1.1

### 1.4 FeatureFlags 完整性 (crate: `config`)
- [x] 确保 `FeatureFlags` 包含 spec 中所有 flag：streaming_tool_execution, tool_use_summaries, fast_mode, coordinator_mode, history_snip, agent_triggers, bridge_mode, daemon, voice_mode, buddy, workflow_scripts, web_browser_tool, context_collapse, terminal_panel, kairos, team_collaboration, auto_dream
- [x] 实现 remote experimentation source（从远程拉取实验分组）
- [x] 实现 stale cache 容忍逻辑（过期缓存仍可使用的降级策略）
- **测试**：17 flag 完整性测试、remote experiment 降级行为
- **依赖**：1.2

### 1.5 基础设施 crate 集成 (workspace)
- [x] 集成 LSP 客户端封装（基于 `lsp-types` 的 stdio JSON-RPC 客户端，支持跳转定义、引用、hover、workspace symbols；`tower-lsp` 为服务端框架，这里改用直接 `lsp-types` 方案）
- [x] 集成 `ignore` + `walkdir`（gitignore 兼容文件遍历）
- [x] 集成 `nucleo-matcher` 或 `fuzzy-matcher`（命令、skills、themes 模糊搜索）
- [x] 集成 `comrak`（Markdown 解析与渲染）
- [x] 集成 `syntect`（代码块与 diff 语法高亮）
- [x] 集成 `cron` crate（定时任务解析与调度引擎）
- **测试**：
  - LSP 客户端连接并获取 symbol 的集成测试（mock LSP server）
  - gitignore 感知文件遍历正确的集成测试
  - 模糊搜索匹配度排序正确的单元测试
  - Markdown 解析 roundtrip 的单元测试
  - 语法高亮生成正确 token 的单元测试
  - cron 表达式解析与下一次触发时间计算正确的单元测试
- **依赖**：0.1

### 1.6 数据目录管理 (crate: `config`)
- [x] 实现 `managed_settings.json` 读写与 checksum 校验
- [x] 实现 `policy_limits.json`（API 速率、Budget 策略限额缓存）读写
- [x] 实现 `agents/` 目录管理（自定义子代理存放）
- [x] 实现 `uploads/` 目录管理（Bridge 上传缓存区）
- [x] 实现 `telemetry/` 目录管理（离线埋点暂存与后台清空）
- **测试**：5 tests — 目录创建、checksum 校验/失败、project_dir 路径
- **依赖**：1.1

---

## Phase 2: 存储层

### 2.1 JSONL Transcript Store (crate: `memory`)
- [x] 实现 `TranscriptStore`：按 `~/.coderex/projects/<project_hash>/<session_id>.jsonl` 追加写入
- [x] 实现 `append(message)` 方法
- [x] 实现 `read_all(session_id)` 迭代器
- [x] 实现 `SKIP_FIRST_PROMPT_PATTERN` 正则跳过机制
- **测试**：6 tests — 写入后读取一致、多消息顺序、缺失文件、skip pattern、大消息量
- **依赖**：0.2

### 2.2 SQLite Session Meta Index (crate: `memory`)
- [x] 创建 `~/.coderex/sessions.db` 与 schema（sessions 表、索引）
- [x] 实现 `SessionIndex::create(session_id, project_path, ...)` 插入
- [x] 实现 `SessionIndex::update_last_active(session_id, ...)` 更新
- [x] 实现 `SessionIndex::list_by_project(project_path)` 查询
- [x] 实现 `SessionIndex::list_recent(limit)` 查询
- **测试**：5 tests — create+get、update、list_recent 排序、list_by_project 过滤
- **依赖**：2.1

### 2.3 长期记忆存储 (crate: `memory`)
- [x] 实现 `LongTermStore`：`~/.coderex/memory/` 目录下的文件存储
- [x] 实现 `read()` / `write(topic, content)` 接口
- [x] 实现 `search(query)` 模糊匹配
- **测试**：5 tests — 写入/读取、更新、缺失主题、搜索匹配、搜索排序
- **依赖**：2.1

### 2.4 Session Memory Store (crate: `memory`)
- [x] 实现 `SessionMemoryStore`：session 级别的 facts 提取与存储
- [x] 实现 `extract_fact(session_id, fact)` / `get_facts(session_id)`
- **测试**：4 tests — add/get、category 过滤、persist/load roundtrip
- **依赖**：2.1

---

## Phase 3: API 客户端与预算

### 3.1 Anthropic API 客户端 (crate: `core`)
- [x] 定义 `ApiClient` trait（`create_message`, `create_message_streaming`）
- [x] 实现 `AnthropicApiClient`：基于 `reqwest`，支持 API key 认证
- [x] 实现请求重试逻辑（指数退避、最大重试次数可配置）
- [x] 实现模型路由（根据 ModelId 选择正确端点）
- **测试**：
  - mock server 下正确发送请求格式的单元测试
  - 重试逻辑行为的集成测试
  - API key 缺失时报错的单元测试（6 tests）
- **依赖**：1.1, 0.2

### 3.2 TokenBudget (crate: `core`)
- [x] 定义 `TokenBudget` 结构体（continuation_count, last_delta_tokens, last_global_turn_tokens, started_at, thresholds）
- [x] 实现 `record_tokens(usage)` 方法
- [x] 实现 `is_near_limit() -> bool` 判断消耗比例
- [x] 实现 `is_diminishing_returns() -> bool` 判断连续 continuation 收益递减
- [x] 实现 `should_continue() -> bool` 综合判断
- **测试**：
  - 阈值判断边界条件的单元测试
  - 收益递减逻辑的单元测试
  - budget 耗尽时正确停止的集成测试（11 tests）
- **依赖**：0.2, 3.1

### 3.3 CostTracker (crate: `core`)
- [x] 定义 `CostTracker` 结构体
- [x] 实现模型定价表（input/output/cache_read/cache_creation 每 token 价格）
- [x] 实现 `record_usage(usage, model) -> cost` 计算
- [x] 实现 per-turn / per-session / per-agent / per-task cost 累计
- [x] 实现 `get_total_cost()`, `get_turn_cost()` 查询
- **测试**：
  - 不同模型定价计算正确的单元测试
  - 累计成本不丢失的单元测试
  - `/cost` 输出格式正确的集成测试（11 tests）
- **依赖**：0.2, 3.1, 3.2

---

## Phase 4: 工具系统基础

### 4.1 Tool Trait 与注册表 (crate: `tools`)
- [x] 定义 `ToolTier` 枚举（Baseline, FeatureGated, DynamicMcp, Internal）
- [x] 定义 `Tool` trait（name, tier, input_schema, output_schema, requires_approval, is_enabled, is_read_only, is_concurrency_safe, interrupt_behavior, execute）
- [x] 定义 `InterruptBehavior` 枚举
- [x] 定义 `ToolContext` 结构体
- [x] 定义 `ToolError` 枚举
- [x] 定义 `ToolOutput` 结构体
- [x] 实现 `ToolRegistry`（baseline + gated + dynamic_mcp + context_allowlists + deny_rules）
- **测试**：
  - 工具注册与按 tier 查询的单元测试
  - context allowlist 过滤正确的单元测试
  - deny rule 拦截的单元测试（7 tests）
- **依赖**：0.2, 1.2

### 4.2 执行上下文矩阵 (crate: `tools`)
- [x] 定义 `ExecutionContext` 枚举（Normal, AsyncAgent, CoordinatorMode, InProcessTeam, PlanMode, ReplOnly, HeadlessSdk, RemoteBridge）
- [x] 实现 `ToolRegistry::available_tool_names(context)` 过滤
- [x] 实现 PlanMode 只允许读工具的规则
- [x] 实现 AsyncAgent 避免 UI 依赖工具的规则
- [x] 实现 CoordinatorMode 只允许调度工具的规则
- **测试**：各执行上下文下工具列表正确的单元测试（3 tests）
- **依赖**：4.1

### 4.3 EchoTool / NoOpTool (crate: `tools`)
- [x] 实现 `EchoTool`：返回输入的 JSON 参数
- [x] 实现 `NoOpTool`：返回空成功
- **测试**：执行结果符合预期的单元测试（5 tests）
- **依赖**：4.1

### 4.4 工具执行器 (crate: `tools`)
- [x] 定义 `ToolExecutor` 结构体
- [x] 实现 `execute_single(tool, input, ctx)` 方法
- [x] 实现并发安全工具的并行执行
- [x] 实现非并发安全工具的串行执行
- [x] 实现执行超时控制
- **测试**：
  - 工具执行结果正确传递的单元测试
  - 超时中止的集成测试
  - 并发安全与不安全工具调度正确的集成测试（3 tests）
- **依赖**：4.1, 4.2

---

## Phase 5: 权限系统

### 5.1 权限模式与规则 (crate: `core`)
- [x] 定义 `PermissionMode` 枚举（Default, AcceptEdits, BypassPermissions, DontAsk, Plan, Auto）
- [x] 定义 `PermissionResult` 枚举（Allow, Ask, Deny, AskForSession, AllowForPath, Elevated）
- [x] 定义 `PermissionChecker` 结构体
- [x] 实现 allow/deny/ask rule 合并逻辑
- [x] 实现 path allowlist 匹配
- [x] 实现 dangerous rule stripping
- [x] 实现 background agent auto-deny
- **测试**：
  - 各模式下权限决策正确的单元测试
  - path allowlist 匹配边界情况的单元测试
  - dangerous rule stripping 生效的单元测试
- **依赖**：1.1, 4.1

### 5.2 权限请求与 Hook 短路 (crate: `core`)
- [x] 定义 `PermissionRequest` 结构体（tool_name, args, context）
- [x] 实现 `PermissionRequest` hook 可短路授权
- [x] 实现 `pending_permission_store`（持久化待决权限请求）
- [x] 实现 orphaned permission recovery（失配/断连后恢复）
- **测试**：
  - hook 短路授权的集成测试
  - pending 请求持久化与恢复的集成测试
  - orphaned recovery 流程的集成测试
- **依赖**：5.1

### 5.3 沙箱实现 (crate: `core`)
- [x] 实现 Linux seccomp + rlimit 沙箱封装
- [x] 实现 macOS sandbox-exec 沙箱封装
- [x] 实现 Windows Job Objects 沙箱封装
- [x] 实现跨平台沙箱抽象 trait（`SandboxConfig`、`SandboxHandle`）
- [x] 实现 additional working directories 沙箱白名单
- **测试**：
  - 沙箱内进程无法访问禁止路径的集成测试
  - rlimit 资源限制生效的集成测试
  - 跨平台沙箱 trait mock 的单元测试
- **依赖**：5.1

### 5.4 权限高级特性 (crate: `core`)
- [x] 实现 automated checks before dialog（对话框前的自动安全校验）
- [x] 实现 prePlanMode restore（进入 PlanMode 时恢复之前权限状态）
- [x] 实现 elevated permission 请求与降级流程
- **测试**：
  - automated checks 拦截不安全操作的集成测试
  - prePlanMode 恢复后权限状态正确的集成测试
- **依赖**：5.1, 5.2

---

## Phase 6: QueryEngine 核心

### 6.1 TurnState 状态机 (crate: `core`)
- [x] 定义 `TurnState` 枚举（Initialize → BuildContext → RunPreSubmitHooks → StartRequest → StreamAssistant → ExecuteTools → AwaitPermission → AwaitElicitation → ApplyToolResults → EvaluateContinuation → CompactRecovery → StopHooks → Finalize → Terminated）
- [x] 定义 `TerminalReason` 枚举
- [x] 定义 `TurnRuntime` 结构体（state, messages, pending_tool_calls, pending_permission, pending_elicitation, budget_tracker, recovery_count, stop_hook_active）
- [x] 实现状态转移逻辑与坏路径保护（compact→stop→retry→compact 死循环检测）
- **测试**：
  - 各状态转移正确的单元测试
  - 死循环检测触发的单元测试
  - 各状态均可被 UserInterrupted 终止的单元测试
- **依赖**：0.2, 0.3, 3.2, 4.1, 4.4, 5.1

### 6.2 SubmitOptions 与单轮对话 (crate: `core`)
- [x] 定义 `SubmitOptions` 结构体（max_turns, max_budget_usd, task_budget_tokens, json_schema, custom_system_prompt, append_system_prompt, replay_user_messages, include_partial_messages, handle_elicitation, orphaned_permission）
- [x] 实现 `QueryEngine::submit(prompt, options)` 单轮对话
- [x] 实现消息发送与响应接收
- [x] 实现 max_turns 限制
- **测试**：
  - 单轮对话返回 assistant 消息的集成测试
  - max_turns 限制生效的集成测试
  - json_schema 约束传递的单元测试
- **依赖**：3.1, 6.1

### 6.3 流式响应 (crate: `core`)
- [x] 实现 SSE/JSON stream 解析框架
- [x] 实现 token 增量发射到 `EngineEvent::TextDelta`
- [x] 实现流式中断处理
- **测试**：
  - 流式 token 逐字输出的集成测试
  - 流式中断不丢失已输出内容的集成测试
- **依赖**：3.1, 6.2

### 6.4 StreamingToolExecution (crate: `core`)
- [x] 实现流式 tool call 解析（不等整条 assistant message 结束）
- [x] 实现 tool call assembler（处理分片片段组装）
- [x] 实现稳定解析后提前进入工具执行
- **测试**：
  - 分片 tool call 正确组装的单元测试
  - 提前执行不等待消息结束的集成测试
- **依赖**：6.3, 4.4

### 6.5 StructuredOutput (crate: `core`)
- [x] 实现 JSON Schema 校验逻辑框架
- [x] 实现 StructuredOutput 工具封装
- [x] 实现不合法输出自动重试/修正框架
- **测试**：
  - 合法 JSON 通过的单元测试
  - 不合法 JSON 重试的集成测试
- **依赖**：6.4

### 6.6 Permission Denial Tracking (crate: `core`)
- [x] 实现被拒绝工具调用跟踪
- [x] 实现拒绝摘要注入后续消息上下文
- **测试**：拒绝记录正确注入上下文的集成测试
- **依赖**：5.1, 6.4

### 6.7 Elicitation (crate: `core`)
- [x] 定义 `ElicitationRequest` 结构体（已在 EngineEvent 中）
- [x] 定义 `ElicitationHandler` trait 框架
- [x] 实现 REPL 模式下的 elicitation 提问流程框架
- [x] 实现 SDK 模式下的 structured I/O elicitation 框架
- [x] 实现 elicitation accept/decline/cancel hook 返回框架
- **测试**：elicitation 请求与响应完整的集成测试
- **依赖**：6.4

---

## Phase 7: Compact 系统

### 7.1 上下文装配 (crate: `core`)
- [x] 定义 `PromptBlock` 结构体
- [x] 实现 `PromptCompiler` 骨架
- [x] 实现 Static Sections 渲染（基础设定、outputStyle、工具使用规范）
- [x] 实现 Dynamic Sections 渲染（session_guidance、memory、mcp_instructions）
- [x] 实现 `=== BOUNDARY MARKER ===` 分隔符插入
- [x] 实现 `appendSystemContext()`（git status 等）
- [x] 实现 `prependUserContext()`（CLAUDE.md 包装为 `<system-reminder>` 首条用户消息）
- **测试**：
  - Static/Dynamic 分离正确的单元测试
  - CLAUDE.md 正确包装为首条用户消息的单元测试
  - 缓存边界标记存在的单元测试
- **依赖**：2.3, 6.1

### 7.2 Micro Compact (crate: `core`)
- [x] 实现 micro compact：移除早期 tool result 保留摘要
- **测试**：compact 后消息长度减少且保留关键信息的单元测试
- **依赖**：7.1

### 7.3 Auto Compact (crate: `core`)
- [x] 实现 auto compact：达到 token 阈值后自动触发
- **测试**：阈值触发自动 compact 的集成测试
- **依赖**：7.2, 3.2

### 7.4 Reactive Compact (crate: `core`)
- [x] 实现 reactive compact：上下文超限被动触发
- **测试**：超限触发 compact 的集成测试
- **依赖**：7.3

### 7.5 Snip Compact (crate: `core`)
- [x] 实现 snip compact：截断历史保留边界
- [x] 实现 compact boundary message 标记
- **测试**：截断后边界正确的单元测试
- **依赖**：7.4

### 7.6 Compact 后 Transcript Cleanup (crate: `memory`)
- [x] 实现 compact 后旧消息从 transcript 移除
- [x] 实现 compact boundary 写入 transcript
- **测试**：compact 后 transcript 内容正确的集成测试
- **依赖**：7.5, 2.1

### 7.7 Pre/Post Compact Hooks (crate: `hooks`)
- [x] 实现 `PreCompact` hook 触发
- [x] 实现 `PostCompact` hook 触发
- **测试**：compact 前后 hook 各执行一次的集成测试
- **依赖**：7.5, Phase 8 Hook 基础

---

## Phase 8: Hook 系统

### 8.1 Hook 事件与返回值 (crate: `hooks`)
- [x] 定义 `HookEvent` 枚举（spec 列出的全部 22 个事件）
- [x] 定义 `HookResult` 结构体（continue/stop, suppressOutput, permission decision, updated input, additional context, watchPaths, updated MCP tool output, elicitation response）
- [x] 实现同步 hook 执行
- [x] 实现异步 hook 执行
- [x] 实现 hook 超时控制
- **测试**：
  - 同步 hook 返回结果影响行为的集成测试
  - 异步 hook 超时中止的单元测试
  - hook 结果中 updated input 传递的集成测试
- **依赖**：0.2, 1.1

### 8.2 Hook 配置加载 (crate: `hooks`)
- [x] 实现 settings.json 中 hook 配置解析
- [x] 实现 hook matcher（按事件类型匹配）
- [x] 实现环境变量注入到 hook 子进程
- [x] 实现 internal hook 标记
- **测试**：hook 配置正确解析与匹配的单元测试
- **依赖**：1.1, 8.1

### 8.3 Stop Hook (crate: `hooks`)
- [x] 实现 `Stop` hook 执行
- [x] 实现 stop hook 决定是否继续或终止
- [x] 实现 `Stop` hook 与 compact/retry 的死循环防护
- **测试**：
  - stop hook 阻止继续执行的集成测试
  - stop hook 允许继续执行的集成测试
- **依赖**：8.2, 6.1

### 8.4 各事件 Hook 触发点集成 (crate: `hooks`)
- [x] 在 QueryEngine 各状态转移点集成 hook 触发
- [x] 实现 `UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`PostToolUseFail` 等关键触发点
- **测试**：完整对话流程中各 hook 按序触发的集成测试
- **依赖**：6.1, 8.3

---

## Phase 9: 基线工具实现

### 9.1 文件操作工具 (crate: `tools`)
- [x] 实现 `Read` 工具（读取文件内容、图片、PDF 分页）
- [x] 实现 `Write` 工具（写入文件、创建父目录）
- [x] 实现 `Edit` 工具（精确字符串替换、replace_all）
- [x] 实现 `NotebookEdit` 工具（Jupyter notebook cell 编辑）
- **测试**：
  - 各工具操作后文件系统状态正确的单元测试
  - Edit 非唯一 old_string 报错的单元测试
  - Read 图片返回可视内容的单元测试
- **依赖**：4.1, 4.4, 5.1

### 9.2 Shell 工具 (crate: `tools`)
- [x] 实现 `Bash` 工具（`tokio::process::Command`，timeout、timeout 可配置到 600s）
- [x] 实现 `PowerShell` 工具（Windows 平台）
- [x] 实现 `Sleep` 工具（受控延迟）
- [x] 实现 run_in_background 支持
- **测试**：
  - Bash 命令输出正确捕获的单元测试
  - timeout 中止的集成测试
  - 后台任务可查询状态的集成测试
- **依赖**：4.1, 4.4, 5.1

### 9.3 搜索工具 (crate: `tools`)
- [x] 实现 `Glob` 工具（glob 模式匹配文件搜索）
- [x] 实现 `Grep` 工具（正则内容搜索、支持 context/head_limit/output_mode）
- **测试**：
  - Glob 模式匹配正确的单元测试
  - Grep 正则搜索结果正确的单元测试
  - Grep 大目录搜索不超时的集成测试
- **依赖**：4.1, 4.4

### 9.4 网络工具 (crate: `tools`)
- [x] 实现 `WebFetch` 工具（HTTP GET、HTML→markdown、prompt 提取）
- [x] 实现 `WebSearch` 工具（搜索查询、domain 过滤）
- **测试**：
  - WebFetch 正确提取内容的集成测试（mock HTTP server）
  - WebSearch domain 过滤生效的单元测试
- **依赖**：4.1, 4.4

### 9.5 TodoWrite 工具 (crate: `tools`)
- [x] 实现 `TodoWrite` 工具（创建/更新结构化任务列表）
- **测试**：todo 列表状态正确的单元测试
- **依赖**：4.1, 4.4

### 9.6 AskUserQuestion 工具 (crate: `tools`)
- [x] 定义 `AskUserQuestion` 工具参数结构
- [x] 实现 REPL 模式下的提问 UI 交互
- [x] 实现 SDK 模式下的 structured I/O 提问
- **测试**：用户回答正确返回工具的集成测试
- **依赖**：4.1, 4.4, 6.7

### 9.7 搜索与代码理解工具 (crate: `tools`)
- [x] 实现 `LSP` 工具（跳转定义、引用查找、hover、workspace symbols，基于 tower-lsp 客户端）
- [x] 实现 `ToolSearch` 工具（按名称/描述搜索可用工具）
- **测试**：
  - LSP 工具返回正确 symbol 信息的集成测试（mock LSP server）
  - ToolSearch 搜索结果按相关度排序的单元测试
- **依赖**：4.1, 4.4, 1.5

### 9.8 任务管理工具 (crate: `tools`)
- [x] 实现 `TaskCreate` 工具（创建结构化任务、关联 agent/shell）
- [x] 实现 `TaskGet` 工具（获取单个任务详情）
- [x] 实现 `TaskList` 工具（列出所有任务、支持状态过滤）
- [x] 实现 `TaskUpdate` 工具（更新任务状态/描述）
- [x] 实现 `TaskOutput` 工具（获取任务输出、支持 block 模式）
- [x] 实现 `TaskStop` 工具（停止运行中的任务）
- **测试**：
  - 各工具操作后 TaskManager 状态正确的集成测试
  - TaskOutput block 模式等待任务完成的集成测试
  - TaskStop 后任务状态为 Stopped 的集成测试
- **依赖**：4.1, 4.4, 10.2

### 9.9 代理与通信工具 (crate: `tools`)
- [x] 实现 `Agent` 工具（spawn 子代理、支持 subagent_type、run_in_background、isolation、model override）
- [x] 实现 `SendMessage` 工具（向指定 agent/task 发送消息）
- **测试**：
  - Agent spawn 后子代理独立运行的集成测试
  - SendMessage 消息送达并返回结果的集成测试
- **依赖**：4.1, 4.4, 10.3

### 9.10 计划模式与定时任务工具 (crate: `tools`)
- [x] 实现 `EnterPlanMode` 工具（进入计划模式）
- [x] 实现 `ExitPlanMode` 工具（退出计划模式）
- [x] 实现 `CronCreate` 工具（创建定时任务、支持 recurring/one-shot、durable）
- [x] 实现 `CronDelete` 工具（取消定时任务）
- [x] 实现 `CronList` 工具（列出所有定时任务）
- **测试**：
  - 计划模式切换后工具列表变更的集成测试
  - Cron 定时任务到点触发的集成测试
  - CronDelete 后任务不再触发的集成测试
- **依赖**：4.1, 1.5, 10.2

### 9.11 网络与配置工具 (crate: `tools`)
- [x] 实现 `RemoteTrigger` 工具（触发远程 Bridge work assignment）
- [x] 实现 `Brief` 工具（生成会话/任务摘要）
- [x] 实现 `Config` 工具（运行时查询/覆盖配置）
- [x] 实现 `Skill` 工具（加载并执行 skill 脚本）
- [x] 实现 `McpAuth` 工具（触发 MCP OAuth 认证流程）
- **测试**：
  - Brief 生成摘要内容完整的集成测试
  - Config 工具查询返回正确设置的单元测试
  - McpAuth 触发后 OAuth flow 完成的集成测试
- **依赖**：4.1, 4.4, 15.4

### 9.12 REPL 兼容与结构化输出工具 (crate: `tools`)
- [x] 实现 `StructuredOutput` 工具（强制模型按 JSON Schema 输出、不合法自动重试）
- [x] 实现 `REPL` 工具（REPL 内部兼容层）
- [x] 实现 `SyntheticOutput` 工具（REPL 兼容补位层，将内部输出格式化为 REPL 可消费格式）
- **测试**：
  - StructuredOutput 合法 JSON 通过的单元测试
  - StructuredOutput 不合法 JSON 重试的集成测试
  - SyntheticOutput 格式化结果正确的单元测试
- **依赖**：4.1, 6.5

### 9.13 Feature-Gated/Internal 工具 (crate: `tools`)
- [x] 清理 13 个占位工具（WebBrowser、TerminalCapture、CtxInspect、Monitor、
  WorkflowTool、SnipTool、VerifyPlanExecution、SuggestBackgroundPR、
  PushNotification、SendUserFile、SubscribePR、ListPeers、Tungsten）——
  它们 `is_enabled: false`，没有任何开关能启用，模型也看不到，唯一行为是返回拒绝信息。
  模块与注册已删除，不再计入工具数。
- 真正 feature-gated 的工具只有 5 个 Chrome DevTools 工具，由 `--no-chrome` 控制。
- **测试**：`the_registry_has_no_refusing_stubs` 断言注册表只有 45 个 baseline + 5 个
  Chrome，且没有任何 `is_enabled: false` 的工具。
- **依赖**：4.1, 4.4, 1.2

---

## Phase 10: 任务与代理系统

### 10.1 Task 数据结构与状态机 (crate: `core`)
- [x] 定义 `TaskKind` 枚举（LocalShell, LocalAgent, MainSession, Dream, InProcessTeammate）
- [x] 定义 `TaskState` 枚举（Pending, Running, Completed, Failed, Stopped）
- [x] 定义 `TaskHandle` 结构体（id, kind, state, output_path, pid/child_handle）
- [x] 实现 `TaskHandle::cancel()` / `TaskHandle::get_output()` / `TaskHandle::get_status()`
- **测试**：
  - 状态转换正确的单元测试
  - cancel 后状态为 Stopped 的单元测试
- **依赖**：0.2

### 10.2 TaskManager (crate: `core`)
- [x] 定义 `TaskManager` 结构体
- [x] 实现 `create_task(kind, command) -> TaskHandle`（另加 `create_task_at(kind, owner, now_ms)` 用于确定性时间戳与归属）
- [x] 实现 `list_tasks()`, `get_task(id)`, `update_task(id, state)`
- [x] 实现 `stop_task(id)`, `get_task_output(id)`
- [x] 实现 foreground/background 任务切换
- **测试**：
  - task CRUD 操作正确的单元测试
  - 后台任务输出可查询的集成测试
  - 前后台切换的集成测试
  - 序列化往返保留归属与时间戳（`crates/core/src/task.rs`）
- **依赖**：10.1
- **说明**：`TaskManager` 是**进程内**任务的生命周期管理器：它的 `TaskHandle` 随进程消失，所以这里**没有** adopt 路径。持久化与 adopt 属于 `JobRecord`（见 10.6），阈值只有一套。`TaskKind` 是 shell/agent/dream/teammate/main-session 五类，与 `JobKind`（agent/shell/workflow/cron）是两个不同的问题域：前者是"正在跑什么"，后者是"磁盘上记了什么"。

### 10.3 Agent 类型与 AgentManager (crate: `agents`)
- [x] 定义 `AgentType` 枚举（GeneralPurpose, Explore, Plan, Verification, ClaudeCodeGuide, StatuslineSetup, Custom）
- [x] 定义 `AgentManager` 结构体
- [x] 实现 `spawn_agent(type, prompt, options) -> TaskHandle`
- [x] 实现 fork 继承上下文
- [x] 实现**缓存感知 fork**：`ForkedContext` 携带父会话的对话前缀（`conversation_prefix` / `fork_point`）与 `CacheSafeParams`（model / system prompt / 工具集），`fork_messages()` 逐字节复用父前缀（而非重算）并在 fork 点后追加子任务，`prefix_fingerprint()` 做漂移检测，`is_cache_safe()` 判定参数是否仍命中父前缀缓存；`SpawnOptions.parent_prefix` / `parent_cache_safe_params` 供调用方注入（CodeRex 消息无稳定 uuid，fork 点用下标而非 id）
- [x] 实现自定义工具白名单
- [x] 实现 `SendMessage` 到指定 agent
- **测试**：
  - agent spawn 后独立运行的集成测试
  - fork 上下文继承正确的集成测试
  - fork 逐字节复用父前缀、无前缀则全新、指纹随前缀变化、缓存安全判定需参数一致、spawn 记录父前缀（`crates/agents/src/manager.rs`）
  - SendMessage 消息送达的集成测试
- **依赖**：10.2, 6.2, 4.1

### 10.4 Worktree 隔离 (crate: `core`)
- [x] 实现 `git worktree add` 封装
- [x] 实现 `EnterWorktree` / `ExitWorktree` 工具
- [x] 实现 worktree 创建失败回退
- [x] 实现 `isolation: "worktree"` 模式（无变更时自动清理）
- **测试**：
  - worktree 创建与切换正确的集成测试
  - 无变更时自动清理的集成测试
- **依赖**：10.2

### 10.5 Agent 扩展特性 (crate: `agents`)
- [x] 实现 resume existing agent（继续已存在的代理，恢复上下文与状态）
- [x] 实现子代理作为命令实现后端（如 `/statusline` → `statusline-setup` agent）
- [x] 实现 teammate idle / task completed 事件系统（空闲通知、任务完成广播）
- **测试**：
  - resume 后代理上下文与之前一致的集成测试
  - 命令通过子代理执行结果正确的集成测试
  - teammate idle 事件触发的集成测试
  - task completed 广播送达的集成测试
- **依赖**：10.3

### 10.6 后台作业记录与 adopt (crate: `core`)
- [x] `crates/core/src/job_record.rs`：`JobKind { Agent, Shell, Workflow, Cron }`、`JobState { Running, Interrupted, Done, Failed }`、`JobRecord { id, kind, description, cwd, session_id, pid, started_at_ms, updated_at_ms, state }`
- [x] `JobStore`：每个作业一个 JSON 文件，路径 `<cwd>/.coderex/jobs/<id>.json`；**原子写**（同目录临时文件 + `rename`）；`write` / `load` / `list` / `remove` / `prune_done`；id 只允许 `[A-Za-z0-9_-]`，杜绝 `../` 逃逸
- [x] **纯函数** `adopt(records, now_ms, session_id) -> AdoptOutcome`，五个桶 `resumable` / `interrupted` / `stale` / `foreign` / `terminal`，分类是全函数（每条记录恰好落一个桶）
  - `STALE_THRESHOLD_MS = 120_000`：超过即 stale
  - `CLOCK_SKEW_ALLOWANCE_MS = 60_000`：**非对称**上界，仅放宽"未来"方向；标称时间戳比本地时钟超前不超过 60s 仍可信，落后不受此容忍影响
  - **foreign**：`session_id` 非空且不等于当前会话 → 不接管；**terminal**：`Done`/`Failed` 先于年龄判定，因此**豁免老化**（对应参照物"干净退出的会话不参与老化"）
  - **resumable 只有 cron**：`JobKind::is_resumable()` 仅 `Cron` 为真；agent/shell/workflow 是**进程内 tokio 任务**，记录只可能是 `interrupted`，不做假的重连
- [x] `JobStore::mark_interrupted(&outcome, now_ms)`：把 `interrupted` 里仍标 `Running` 的记录改写为 `Interrupted`（首个接管它的会话负责改写；已 `Interrupted` 的不重复报），返回每条一次 `TaskNotification` —— **宁可报告，绝不留 phantom running**
- [x] `AdoptOutcome::notifications()`：`interrupted` 桶的纯函数视图，无存储时（如无头）也能报告
- [x] 总入口 `adopt_for_session(store, now_ms, session_id) -> Result<StartupAdopt, JobStoreError>`；`adopt_and_reconcile` 是不看开关的纯核心
- [x] 开关 `CODEREX_DISABLE_ADOPT`（对应参照物 `CLAUDE_DISABLE_ADOPT`）：非空且非 `0`/`false`/`no` 即整体停用。参照物**没有 adopt 时的同意弹窗**——它沿用此前已授予的同意并**披露**而非询问——所以这里是开关而不是对话框
- **测试**：
  - 可收养性矩阵（fresh / foreign / stale / 未来时间戳在 skew 内 / 超出 skew / 终态豁免老化 / 已 Interrupted）单元测试，含两个边界（阈值恰等、skew 恰等）（`crates/core/src/job_record.rs`）
  - 存储往返：write → load → list → remove → prune_done；原子写不留临时文件；并发写/读不产生解码失败；损坏文件被跳过而不是让 adopt 整体失败；非法 id 被拒（`crates/core/tests/job_record_integration.rs`）
  - **interrupted 记录被上报而不是丢掉**：写入一条 `Running` 记录 → `adopt_and_reconcile` → 得到一条 notification，且磁盘上该记录变为 `Interrupted`（不再是 `Running`）
  - 变异验证：去掉 skew 上界 → `future_timestamp_within_skew_is_trusted` 必须失败；去掉 foreign 判定 → `foreign_record_is_not_adopted` 必须失败
- **依赖**：10.2
- **说明（为什么不需要 roster 索引）**：参照物在每作业 `state.json` 旁另存 `roster.json` 索引，是因为它的守护进程要在每次重启时扫描一棵大小未知的作业树。CodeRex 的作业目录由一个会话写入、只装少量文件，`list()` 直接读目录即可；再加一份索引等于引入第二个事实来源，会与它索引的文件互相矛盾。故**不设索引**。
- **说明（cron 的真相）**：持久化 cron 定义**已经存在**——`CronScheduler`（`crates/tools/src/cron_scheduler.rs`）把 `durable` 作业写入 `~/.coderex/scheduled_tasks.json`，`repl.rs:941` 会话启动时 `load()`，随后 `spawn_runner()` 在同一个 scheduler 上重新调度。也就是说参照物 `adoptCron` 想做的事（按存储去重后重排）在这里由 `CronScheduler` 自己完成，**adopt 不去重排、也不会重复登记**：它把 cron 记录归入 `resumable` 仅供披露。`CronCreate` 目前不写 `JobRecord`，所以实践中该桶通常为空——如实成陈述，不假造生产者。

---

## Phase 11: 团队与协调器

### 11.1 TeamCreate / TeamDelete (crate: `core`)
- [x] 实现 `TeamCreate` 工具（创建团队、初始化共享上下文）
- [x] 实现 `TeamDelete` 工具（清理团队资源）
- [x] 实现队员共享任务视图
- **测试**：团队创建后队员可见彼此的集成测试（20 tests）
- **依赖**：10.2, 10.3

### 11.2 Coordinator Mode (crate: `core`)
- [x] 定义 `CoordinatorState` 结构体
- [x] 实现四阶段流程：Research → Synthesis → Implementation → Verification
- [x] 实现 CoordinatorMode 下的工具白名单
- **测试**：四阶段顺序执行的集成测试（15 tests）
- **依赖**：11.1, 10.3

### 11.3 团队上下文与跨 Worktree 恢复 (crate: `core`)
- [x] 实现团队上下文记忆（队员共享 live 上下文视图、非仅 sync）
- [x] 实现 recent session pointer 跨 worktree 恢复（远程 resume 时能找到最近会话）
- **测试**：
  - 团队成员能看到彼此 live 上下文的集成测试（13 tests）
  - 跨 worktree session 恢复后状态一致的集成测试
- **依赖**：11.1, 10.4

---

## Phase 12: Slash Commands 系统

### 12.1 Command Registry (crate: `commands`)
- [x] 定义 `CommandType` 枚举（Local, LocalUi, Prompt, Standalone）
- [x] 定义 `SlashCommand` 结构体（name, kind, supports_non_interactive, remote_safe, bridge_safe, allowed_tools_override）
- [x] 实现 `CommandRegistry` 注册与查找
- [x] 实现命令可用性矩阵（non-interactive / remote-safe / bridge-safe）
- **测试**：
  - 命令注册与查找正确的单元测试
  - 可用性矩阵过滤正确的单元测试
- **依赖**：1.2, 4.1

### 12.2 会话管理命令 (crate: `commands`)
- [x] 实现 `/clear`：清空当前会话消息
- [x] 实现 `/exit`：优雅退出
- [x] 实现 `/session`：显示当前会话信息
- **测试**：各命令执行后状态正确的集成测试
- **依赖**：12.1, 0.3

### 12.3 配置命令 (crate: `commands`)
- [x] 实现 `/config`：显示/编辑配置
- [x] 实现 `/model`：切换模型
- [x] 实现 `/permissions`：查看/设置权限模式
- **测试**：配置变更后生效的集成测试
- **依赖**：12.1, 1.1

### 12.4 统计命令 (crate: `commands`)
- [x] 实现 `/stats`：显示会话统计
- [x] 实现 `/cost`：显示成本统计
- [x] 实现 `/usage`：显示 token 使用量
- **测试**：统计数据来源正确的集成测试
- **依赖**：12.1, 3.2, 3.3

### 12.5 内容与集成命令 (crate: `commands`)
- [x] 实现 `/compact`：手动触发 compact
- [x] 实现 `/help`：显示帮助
- [x] 实现 `/version`：显示版本
- **测试**：命令输出正确的集成测试
- **依赖**：12.1, 7.1

### 12.6 会话管理扩展命令 (crate: `commands`)
- [x] 实现 `/resume`：恢复之前的会话
- [x] 实现 `/rename`：重命名当前会话
- [x] 实现 `/rewind`：回退到会话中某个历史点
- **测试**：各命令执行后状态正确的集成测试
- **依赖**：12.1, 2.1, 2.2

### 12.7 配置扩展命令 (crate: `commands`)
- [x] 实现 `/theme`：切换/查看主题
- [x] 实现 `/hooks`：查看/管理已注册的 hooks
- [x] 实现 `/mcp`：查看/管理 MCP 服务状态
- [x] 实现 `/plugin`：查看/管理插件
- [x] 实现 `/memory`：查看/搜索长期记忆
- [x] 实现 `/skills`：查看/加载 skills
- [x] 实现 `/tasks`：查看任务列表（命令版）
- [x] 实现 `/plan`：查看/编辑当前计划
- **测试**：配置变更后生效的集成测试
- **依赖**：12.1, 1.1, 15.1, 16.1, 2.3

### 12.8 统计与诊断命令 (crate: `commands`)
- [x] 实现 `/status`：显示系统运行状态
- [x] 实现 `/doctor`：诊断环境健康度（API key、配置、MCP 连接等）
- **测试**：诊断输出包含期望条目的集成测试
- **依赖**：12.1, 3.2, 15.1

### 12.9 内容与上下文命令 (crate: `commands`)
- [x] 实现 `/copy`：复制消息内容到剪贴板
- [x] 实现 `/init`：初始化项目（生成 CLAUDE.md 等）
- [x] 实现 `/summary`：生成当前会话摘要
- [x] 实现 `/files`：列出项目文件结构
- [x] 实现 `/context`：查看当前上下文装配内容
- [x] 实现 `/ctx_viz`：上下文可视化（feature-gated）
- [x] 实现 `/add-dir`：添加目录到项目上下文
- **测试**：命令输出与当前状态一致的集成测试
- **依赖**：12.1, 7.1, 2.1

### 12.10 集成命令 (crate: `commands`)
- [x] 实现 `/ide`：IDE 集成连接/断开
- [x] 实现 `/chrome`：Chrome DevTools 集成
- [x] 实现 `/desktop`：桌面应用集成
- [x] 实现 `/mobile`：移动端远程连接
- [x] 实现 `/voice`：语音模式开关
- [x] 实现 `/vim`：Vim 模式开关
- [x] 实现 `/remote-env`：查看/设置远程环境变量
- [x] 实现 `/remote-setup`：远程环境初始化
- **测试**：集成连接后状态正确的集成测试
- **依赖**：12.1, 17.1, 18.1

### 12.11 Git/PR 命令 (crate: `commands`)
- [x] 实现 `/commit`：创建提交
- [x] 实现 `/commit-push-pr`：提交并推送为 PR
- [x] 实现 `/review`：代码审查辅助
- [x] 实现 `/security-review`：安全审查辅助
- [x] 实现 `/autofix-pr`：自动修复 PR 问题
- [x] 实现 `/pr_comments`：查看/回复 PR 评论
- [x] 实现 `/branch`：分支管理
- **测试**：
  - Git 操作后仓库状态正确的集成测试
  - PR 相关命令与 GitHub API 交互正确的集成测试（mock）
- **依赖**：12.1, 4.1, 9.1, 9.2

### 12.12 调试诊断命令 (crate: `commands`)
- [x] 实现 `/debug-tool-call`：查看最近 tool call 的详细参数/返回
- [x] 实现 `/heapdump`：内存快照（feature-gated）
- [x] 实现 `/mock-limits`：模拟 API 速率限制（测试用）
- [x] 实现 `/perf-issue`：性能问题诊断
- [x] 实现 `/ant-trace`：Anthropic API 调用追踪
- [x] 实现 `/break-cache`：清除 Prompt Cache（调试用）
- [x] 实现 `/reload-plugins`：热重载插件
- **测试**：调试命令输出包含期望信息的集成测试
- **依赖**：12.1, 1.3, 16.2

### 12.13 高级命令 (crate: `commands`)
- [x] 实现 `/bridge`：Bridge 状态查看/管理
- [x] 实现 `/bridge-kick`：踢掉远程 Bridge 连接
- [x] 实现 `/teleport`：切换工作目录与会话（feature-gated）
- [x] 实现 `/ultraplan`：超级计划模式（feature-gated）
- [x] 实现 `/insights`：项目洞察分析
- [x] 实现 `/thinkback`：回顾历史会话（feature-gated）
- [x] 实现 `/thinkback-play`：回看并执行（feature-gated）
- [x] 实现 `/statusline`：statusline 自定义配置
- **测试**：高级命令功能正确的集成测试
- **依赖**：12.1, 17.1, 10.3

### 12.14 杂项命令 (crate: `commands`)
- [x] 实现 `/advisor`：顾问模式
- [x] 实现 `/btw`：顺便信息插入
- [x] 实现 `/good-claude`：正向反馈
- [x] 实现 `/stickers`：贴纸/表情（feature-gated）
- [x] 实现 `/bughunter`：Bug 猎手模式
- [x] 实现 `/issue`：创建/管理 Issue
- [x] 实现 `/env`：查看/设置环境变量
- [x] 实现 `/diff`：显示当前 git diff
- [x] 实现 `/oauth-refresh`：刷新 OAuth token
- [x] 实现 `/backfill-sessions`：补全历史会话索引
- **测试**：命令执行结果正确的集成测试
- **依赖**：12.1, 1.1, 2.2

---

## Phase 13: REPL/UI 基础

### 13.1 ratatui 终端初始化 (crate: `ui`)
- [x] 实现 alternate screen 进入与退出
- [x] 实现终端布局：transcript 区 + statusline + input 区
- [x] 实现 `TerminalUi::draw()` 帧渲染
- **测试**：布局尺寸正确的集成测试（term 尺寸模拟）
- **依赖**：0.3, 0.4

### 13.2 输入处理 (crate: `ui`)
- [x] 实现键盘事件读取
- [x] 实现 input buffer 编辑（光标移动、插入、删除）
- [x] 实现 Enter 提交到 QueryEngine
- [x] 实现 Ctrl+C 中断
- **测试**：
  - 按键到 input buffer 变更正确的单元测试
  - Ctrl+C 发送中断信号的集成测试
- **依赖**：13.1

### 13.3 历史消息渲染 (crate: `ui`)
- [x] 实现 assistant/user/tool 消息差异化渲染
- [x] 实现滚动（PgUp/PgDown）
- [x] 实现长文本 virtualization（只渲染可见区域）
- **测试**：滚动边界正确的集成测试
- **依赖**：13.2, 6.3

### 13.4 Statusline (crate: `ui`)
- [x] 实现状态栏渲染（模型名、token 用量、成本、权限模式）
- [x] 实现 statusline 实时更新
- **测试**：状态栏数据与 AppState 一致的集成测试
- **依赖**：13.3, 3.3

### 13.5 Overlays 与模态框 (crate: `ui`)
- [x] 实现 overlay 层渲染（权限对话框、picker、通知）
- [x] 实现权限审批模态框 UI
- **测试**：overlay 在最顶层渲染的集成测试
- **依赖**：13.4, 5.2

### 13.6 UI 增强功能 (crate: `ui`)
- [ ] 键绑定可配置(`ui::keybindings` 曾提供一份**无人使用**的按键表,与 `input::action()` 重复且更弱;已删除,键位分类现在只有 `input.rs` 一处。若要做可配置键位,应从这里扩展)
- [x] 实现搜索高亮（终端内文本搜索、匹配结果高亮显示）
- [x] 实现消息选择器（多消息选择、用于 copy/export）
- **测试**：
  - 可配置键位的集成测试(尚未有可配置键位)
  - 搜索高亮匹配位置正确的集成测试
  - 消息选择器多选结果正确的集成测试
- **依赖**：13.2, 13.3

---

## Phase 14: REPL 完整对话流程

### 14.1 REPL 端到端循环 (crate: `cli`)
- [x] 串联：用户输入 → QueryEngine.submit → 流式渲染 → tool 执行 → 结果回写 → 继续/结束
- [x] 实现 prompt 历史维护
- [x] 实现多轮对话循环
- **测试**：
  - 端到端：用户提问 → assistant 回答 → 多轮对话的集成测试
  - 端到端：用户提问 → tool call → 执行 → 回答的集成测试
- **依赖**：6.4, 13.5, 9.1-9.5, 10.2

### 14.2 CLI 子命令 (crate: `cli`)
- [x] 使用 clap 定义 CLI 子命令结构
- [x] 实现非交互模式（headless：stdin→query, stdout→response）
- [x] 实现 `--version`、`--help`
- **测试**：CLI 参数解析正确的单元测试
- **依赖**：14.1

---

## Phase 15: MCP 服务

### 15.1 MCP Server 生命周期 (crate: `mcp`)
- [x] 实现 stdio JSON-RPC server spawn
- [x] 实现 server 连接管理（reconnect、health check）
- [x] 实现 `.mcp.json` 配置解析
- **测试**：
  - server spawn 与连接建立的集成测试
  - 断线重连的集成测试
- **依赖**：1.1, 4.1

### 15.2 MCP 工具发现与代理 (crate: `mcp`)
- [x] 实现 MCP tool discovery
- [x] 实现 `McpToolProxy` 工具
- [x] 实现 MCP tool 注册到 ToolRegistry（DynamicMcp tier）
- **测试**：MCP 工具被发现并可执行的集成测试
- **依赖**：15.1, 4.1

### 15.3 MCP 资源发现与读取 (crate: `mcp`)
- [x] 实现 MCP resource discovery
- [x] 实现 `ListMcpResources` / `ReadMcpResource` 工具
- **测试**：资源列表与读取正确的集成测试
- **依赖**：15.2

### 15.4 MCP OAuth (crate: `mcp`)
- [x] 实现 MCP OAuth 认证流程
- [x] 实现 channel permissions / allowlist
- **测试**：OAuth 认证成功后的集成测试
- **依赖**：15.3

### 15.5 MCP Elicitation (crate: `mcp`)
- [x] 实现 MCP 服务 elicitation 请求处理
- [x] 实现 elicitation 转发到 REPL/SDK
- **测试**：MCP elicitation 完整流程的集成测试
- **依赖**：15.4, 6.7

---

## Phase 16: Plugin 与 Skills

### 16.1 Skill 加载 (crate: `plugins`)
- [x] 实现 skill frontmatter 解析（hooks/agent/allowed_tools/metadata）
- [x] 实现 bundled skill 加载
- [x] 实现 user dir skill 加载
- [x] 实现 repo dir skill 加载
- [x] 实现 MCP skill 加载（从 MCP 服务发现并注册 skill）
- **测试**：
  - 各来源 skill 被发现并加载的集成测试
  - MCP skill 从 MCP 服务发现后可用的集成测试
- **依赖**：1.1, 4.1, 15.2

### 16.2 Plugin Loader (crate: `plugins`)
- [x] 实现 `PluginManager` 骨架
- [x] 实现 plugin 加载与 handle 管理
- [x] 实现 plugin commands 注册
- [x] 实现 plugin skills 注册
- **测试**：plugin 加载后命令和 skill 可用的集成测试
- **依赖**：16.1, 12.1

### 16.3 Marketplace (crate: `plugins`)
- [x] 实现 marketplace 连接
- [x] 实现后台 marketplace reconciliation
- [x] 实现安装后 refresh / needsRefresh 标记
- [x] 实现后台安装
- **测试**：
  - reconciliation 发现新 plugin 的集成测试
  - 安装后自动刷新的集成测试
- **依赖**：16.2

---

## Phase 17: Bridge 远程会话

### 17.1 Bridge 注册与容量 (crate: `bridge`)
- [x] 实现 `BridgeWorkPoller`：环境注册与 capacity 报告
- [x] 实现轮询/推送 work 分配
- **测试**：注册后能接收 work assignment 的集成测试
- **依赖**：1.1

### 17.2 Bridge Session Spawner (crate: `bridge`)
- [x] 实现 `BridgeSessionSpawner`：spawn mode 决策（single-session / same-dir / worktree）
- [x] 实现 worktree 创建与幂等清理
- [x] 实现 session 目录准备
- **测试**：
  - 各 spawn mode 目录策略正确的集成测试
  - worktree 创建失败调用 stopWork 的集成测试
- **依赖**：17.1, 10.4

### 17.3 Bridge Permission Router (crate: `bridge`)
- [x] 实现 `BridgePermissionRouter`：pending permission store 与远程转发
- [x] 实现远程 allow/deny 响应回写
- **测试**：权限请求不丢失且响应回写的集成测试
- **依赖**：17.2, 5.2

### 17.4 Bridge Archive & Cleanup (crate: `bridge`)
- [x] 实现 `BridgeArchiveWriter`：transcript/usage/metadata 归档
- [x] 实现 `BridgeCleanupManager`：worktree 清理、session 进程终止
- [x] 实现 capacity 释放与 stopWork 确认
- **测试**：会话结束后完整清理的集成测试
- **依赖**：17.3, 2.1

### 17.5 Bridge State Machine (crate: `bridge`)
- [x] 定义 `BridgeSessionState` 枚举
- [x] 实现完整桥接时序：Assigned → Preparing → Spawning → Running → Archiving → CleaningUp → Completed
- **测试**：状态机完整流转的集成测试
- **依赖**：17.1-17.4

### 17.6 Bridge 心跳与重连 (crate: `bridge`)
- [x] 实现 Bridge heartbeat（定期心跳、超时检测）
- [x] 实现断线重连（WebSocket 断开后自动重连、状态恢复）
- [x] 实现 in-process transport 扩展点（MCP in-process 与 SDK control transport）
- **测试**：
  - 心跳超时后触发清理的集成测试
  - 断线重连后状态恢复的集成测试
  - in-process transport 端点可达的单元测试
- **依赖**：17.1, 17.5

---

## Phase 18: Server/SDK

### 18.1 Daemon Server (crate: `server`)
- [x] 实现 detached session server
- [x] 实现 WebSocket 认证 session
- [x] 实现 idle cleanup
- [x] 实现 session listing
- **测试**：
  - WebSocket 连接认证成功的集成测试
  - idle session 自动清理的集成测试
- **依赖**：0.3, 10.2

### 18.2 SDK Structured I/O (crate: `sdk`)
- [x] 实现 structured event stream（tool call/result/status/compact boundary 事件）
- [x] 实现 permission request 事件
- [x] 实现 elicitation 请求与响应
- [x] 实现 JSON schema structured output
- [x] 实现 pending permission request tracking
- **测试**：
  - SDK 客户端接收完整事件流的集成测试
  - structured output 符合 JSON schema 的集成测试
- **依赖**：6.5, 6.7, 5.2, 18.1

### 18.3 Remote Command Filtering (crate: `server`)
- [x] 实现 remote-safe / bridge-safe 命令过滤
- [x] 实现 mobile/web client 命令子集
- **测试**：不安全命令被拦截的集成测试
- **依赖**：12.1, 18.2

---

## Phase 19: 记忆系统扩展

### 19.1 Team Memory Sync (crate: `memory`)
- [x] 实现 `TeamMemorySyncService`：repo 维度、服务端 upsert
- [x] 实现 checksum 增量同步
- [x] 实现删除不传播语义
- **测试**：
  - checksum 增量同步正确的集成测试
  - 删除操作不传播的单元测试
- **依赖**：2.3, 11.1

### 19.2 Auto Dream (crate: `memory`)
- [x] 实现时间与 session 数阈值检查
- [x] 实现 fork 只读受限子代理进行背景整理
- [x] 实现 dream 产物写入项目 memory 目录
- **测试**：阈值触发 dream 整理的集成测试
- **依赖**：19.1, 10.3

---

## Phase 20: 企业功能

### 20.1 Remote Managed Settings (crate: `config`)
- [x] 实现企业用户 eligibility 检查
- [x] 实现 checksum-based fetch
- [x] 实现 security check
- [x] 实现 loading barrier（允许其他系统等待远程配置拉取完成）
- [x] 实现 fail open（失败不阻塞启动）
- **测试**：
  - 远程配置加载并合并的集成测试
  - 网络失败时 fail open 的集成测试
  - loading barrier 等待完成的集成测试
- **依赖**：1.1, 3.1

### 20.2 启动编排 (crate: `cli`)
- [x] 实现 `StartupOrchestrator`
- [x] 实现四阶段启动：Bootstrap → Configuration → Extensions → RuntimeReady
- [x] 按 spec 时序实现各子系统初始化
- [x] 实现远程配置 barrier 不阻塞整个启动链
- [x] 实现 MCP 异步连接不阻塞启动
- **测试**：
  - 各阶段按序执行的集成测试
  - 某个子系统初始化失败不阻断整体的集成测试
  - MCP 超时不拖死启动的集成测试
- **依赖**：所有 Phase 0-19 的基础模块

### 20.3 企业安全与策略 (crate: `config`, `core`)
- [x] 实现 trusted device 机制（设备注册、信任验证、增强认证）
- [x] 实现 org-scoped remote policy（组织级远程策略、权限范围控制）
- [x] 实现 org-scoped team memory（组织维度的团队记忆隔离与共享）
- **测试**：
  - 未信任设备被拒绝访问的集成测试
  - org-scoped 策略限制跨组织操作的集成测试
  - team memory 按组织隔离的集成测试
- **依赖**：20.1, 19.1, 17.1

---

## Phase 21: UI 产品化

### 21.1 Vim Mode (crate: `ui`)
- [x] 实现 Vim normal/insert 模式切换
- [x] 实现基础 Vim 键绑定（hjkl、i、a、Esc、:、dd、yy、p）
- **测试**：Vim 模式下按键行为正确的集成测试
- **依赖**：13.2

### 21.2 Theme System (crate: `ui`)
- [x] 实现主题配置格式与加载
- [x] 实现主题切换
- **测试**：主题变更后渲染颜色正确的集成测试
- **依赖**：13.1

### 21.3 Copy/Export/Share (crate: `ui`)
- [x] 实现消息选择器
- [x] 实现复制到剪贴板
- [x] 实现导出流程
- **测试**：选中内容导出正确的集成测试
- **依赖**：13.5

### 21.4 Notifications (crate: `ui`)
- [x] 实现通知系统
- [x] 实现 buddy/companion teaser notification
- **测试**：通知显示与清除的集成测试
- **依赖**：13.5

---

## Phase 22: 产品扩展

### 22.1 Voice STT (crate: `ui`)
- [x] 实现语音输入 STT integration（feature-gated 状态机）
- **测试**：4 tests — 状态转换、错误恢复、空转录、连续输入
- **依赖**：13.2

### 22.2 Buddy Companion (crate: `ui`)
- [x] 实现 deterministic companion generation from user id
- [x] 实现 persisted soul / regenerated bones
- [x] 实现 sprite reaction / petting / idle presence
- **测试**：8 tests — 确定性生成、不同ID不同灵魂、骨骼再生、宠物计数、情绪转换、JSON往返、空闲动画
- **依赖**：2.3, 21.4

### 22.3 Statusline Setup Flow (crate: `ui`)
- [x] 实现 statusline setup 引导流程
- **测试**：6 tests — 步骤前进/后退、切换选项、应用到Statusline、完成标志、每步渲染、环绕保护
- **依赖**：13.4, 10.3

---

## Phase 23: 端到端测试与质量

### 23.1 端到端测试框架 (crate: `cli`)
- [x] 搭建 e2e 测试基础设施（MockServer、E2EContext、MockApiClient）
- [x] 实现 e2e 测试 runner
- **依赖**：14.1

### 23.2 核心流程 E2E 测试
- [x] E2E：用户提问 → assistant 流式回答 → 正确内容
- [x] E2E：多轮对话 → 对话继续不丢失
- [x] E2E：权限命令存在
- [x] E2E：slash command `/clear` → 会话清空
- [x] E2E：vim 切换
- [x] E2E：model 切换
- [x] E2E：CLI 版本/help 参数
- [x] E2E：mock server 入队/响应/历史
- [x] E2E：empty input 处理
- **依赖**：23.1, 所有实现 Phase

### 23.3 性能与压力测试
- [x] 大消息列表（5000/10000条）virtualization 渲染正确
- [x] 长时间对话模拟（1000次命令查找）
- [x] 并发通知（1000条）、主题（100个）、输入（10000字符）
- [x] vim 操作（1000次命令）不崩溃
- [x] 搜索（10000行）、undo（200条）、overlay（100个）、导出（1000条消息）
- **依赖**：23.1

---

## Phase 24: 发行层级

### 24.1 Core 发行 (feature flags)
- [x] 配置 `core` feature：QueryEngine、基线工具、compact、hooks、tasks、agent、MCP、REPL
- [x] 验证 `cargo build --features core` 成功
- **依赖**：所有 core 模块

### 24.2 Full 发行 (feature flags)
- [x] 配置 `full` feature：core + bridge/server/plugins + feature-gated 工具 + voice/buddy/statusline + cron/team/coordinator
- [x] 验证 `cargo build --features full` 成功
- **依赖**：24.1, 所有 full 模块

### 24.3 Enterprise 发行 (feature flags)
- [x] 配置 `enterprise` feature：remote managed settings、org policy、trusted device
- [x] 企业模块 feature-gated：`coderex-config/enterprise`、`coderex-cli/enterprise`
- [x] orchestrator 按 feature 条件编译企业阶段
- **依赖**：24.2, 20.1, 19.1, 20.3
