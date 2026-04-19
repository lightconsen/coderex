# CodeRex vs 主流 Code Agent 架构全面对比

> 对比对象：**CodeRex**、**Claude Code**（CC-src）、**Kimi Code CLI**（kimi-cli）、**Qwen Code**、**Gemini CLI**（gemini-cli）
> 日期：2026-04-18

---

## 1. 概览

| 维度 | CodeRex | Claude Code (CC-src) | Kimi Code CLI | Qwen Code | Gemini CLI |
|------|---------|----------------------|---------------|-----------|------------|
| **语言** | Rust | TypeScript | Python (+ Rust 重写了 kimi-agent-rs) | TypeScript | TypeScript |
| **构建系统** | Cargo workspace | Bun/Node 打包 | uv / pip | npm workspaces | npm workspaces |
| **架构风格** | 14 crate 模块化 monorepo | 单目录 ~512K 行 TS 单体 | 分层 Python 模块 | 双包 monorepo (cli + core) | 双包 monorepo (cli + core) |
| **模型绑定** | 多模型 (通过 `ModelId`) | 仅 Claude | 仅 Kimi (Kosong 抽象层) | 仅 Qwen | 仅 Gemini |
| **TUI** | Ratatui (Rust) | React + Ink (TS) | 自定义 TUI shell | React + Ink (TS) | React + Ink (TS) |
| **许可证** | MIT/Apache-2.0 | 闭源 (npm 分发) | 开源 (MIT) | 开源 | 开源 |
| **IDE 集成** | Bridge/Server 远程协议 | 内建 | ACP 协议 | VS Code / Zed / JetBrains | VS Code / JetBrains |

---

## 2. 模块级详细对比

### 2.1 入口层 / CLI

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **文件** | `crates/cli/src/main.rs` + `orchestrator.rs` | `src/main.tsx` | `src/kimi_cli/cli/` (Typer) | `packages/cli/` | `packages/cli/` |
| **解析** | Clap derive | 自定义 CLI 解析 | Typer (Python) | 自定义 CLI 解析 | 自定义 CLI 解析 |
| **启动方式** | 交互式 TUI / 守护进程 (serve) / Resume | 交互式 / Headless | 交互式 / ACP server | 交互式 / Headless / GitHub Action | 交互式 / Headless / GitHub Action |
| **亮点** | `StartupOrchestrator` 分离启动流程 | 完整的 React 生命周期 | `--ui=shell` 轻量模式 | 丰富的配置系统 | 扩展市场 |

**CodeRex 参考建议：**
- CodeRex 的 `StartupOrchestrator` 设计清晰，但可借鉴 Claude Code 的完整启动流程（OAuth 登录检测、配置验证、workspace 初始化）
- Qwen/Gemini 的 GitHub Action 集成模式值得引入

### 2.2 核心引擎 / Agent Loop

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **核心组件** | `QueryEngine` (state machine) | `QueryEngine.ts` (~46K 行) | `KimiSoul.run()` | Agent loop in core | ReAct Loop |
| **状态机** | `TurnState` 15 种状态 | 内部状态机 | Wire 协议事件流 | 内部状态管理 | Reason-Act-Observe |
| **流式** | `submit_streaming()` 独立实现 | 原生流式 | Wire 协议流式 | 流式支持 | 流式支持 |
| **结构化输出** | 内置 `StructuredOutputValidator` | 内建 JSON schema 验证 | 无显式实现 | 内建 | 内建 |
| **Context 压缩** | 4 种策略 (Micro/Auto/Reactive/Snip) | 智能压缩 (auto-compact) | `llm_ce` 层级管理 | 上下文压缩 | 上下文管理 |
| **多 Agent** | `Orchestrator` + `TeamManager` | Subagent 系统 | 100+ 并行 Sub-agent | Sub-agent 架构 | 提案中 (Multi-Agent Proposal) |

**CodeRex 参考建议：**
- **最强项**：CodeRex 的 `TurnState` 状态机是 5 个项目中最清晰的，有完整的状态流转图
- Claude Code 的 46K 行 `QueryEngine.ts` 证明了这种 state machine 方向是正确的，但 CodeRex 更简洁
- Kimi 的 100+ 并行 sub-agent 能力值得学习：CodeRex 的 `TeamManager` 可以增强为 swarm 模式
- CodeRex 的 4 种 compact 策略（Micro/Auto/Reactive/Snip）比其他项目更丰富

### 2.3 工具系统

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **架构** | `Tool` trait + `ToolExecutor` + `ToolRegistry` | `Tool.ts` 基类 + `src/tools/` | `KimiToolset` | `packages/core/src/tools/` | `packages/core/src/tools/` |
| **内置工具数** | ~45 | 38-60+ (lazy loading) | ~20 | ~30 | ~25 |
| **注册方式** | `register_builtin_tools()` 函数 | 按 tier 加载 | YAML 声明 + Python 模块 | 注册表 | 注册表 |
| **工具层级** | `ToolTier` 枚举 | Tier1/Tier2 分层 | 无显式分层 | 分层 | 分层 |
| **权限** | 5 级权限 (Allow/Ask/Deny/Elevated/Bypass) | 3 级 (auto/yolo/default) | 内建 | 3 级 | 3 级 |

**CodeRex 参考建议：**
- CodeRex 的 45+ 内置工具和 `ToolTier` 设计已非常成熟
- CodeRex 的权限系统 (5 级) 是所有项目中最细粒度的，**优势项**
- Claude Code 的 lazy loading（按需加载 ~40/60+ 工具）值得引入，减少内存

### 2.4 Prompt 系统

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **编译** | `PromptCompiler` + `PromptContext` | 内建 prompt 组装 | YAML agent.yaml | 模块化 prompt 系统 | 内建 |
| **模板** | `crates/core/src/prompts/` 10+ prompt 模板 | `src/prompts/` | `src/kimi_cli/agents/default/` | `.qwen/` 目录 | 内建 |
| **Skill** | `crates/plugins/src/skills.rs` | Slash skills | Agent Skills | `.qwen/skills/` | Extensions / Skills |

**CodeRex 参考建议：**
- CodeRex 的 `PromptCompiler` 设计合理，但 prompt 模板集中在 core crate 中
- Qwen 的 `.qwen/skills/e2e-testing/SKILL.md` 标准格式值得借鉴
- Claude Code 的 skills 系统有完善的生命周期管理（加载、执行、卸载）

### 2.5 记忆系统

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **架构** | `coderex-memory` 独立 crate | 3 层内存 (`MEMORY.md` 指针索引) | 无独立内存模块 | 上下文管理 | `save_memory` 工具 |
| **类型** | Transcript / SessionIndex / LongTerm / SessionMemory / TeamMemory | Pointer index + 主题文件 + 压缩上下文 | 层级管理 | 会话记忆 | 跨会话记忆 |
| **持久化** | SQLite (rusqlite) | `.coderex/` 目录文件 | 本地文件 | 本地文件 | 本地文件 |
| **团队** | `TeamMemory` + `TeamMemorySyncService` | 无 | 无 | 无 | 无 |
| **AutoDream** | CodeRex 独有 (梦境检查) | 无 | 无 | 无 | 无 |

**CodeRex 参考建议：**
- **CodeRex 独有强项**：`TeamMemory`、`AutoDream` 是独特设计
- CodeRex 使用 SQLite 做持久化，比文件方案更高效
- Claude Code 的 `MEMORY.md` 指针索引系统（而非直接写入内容）设计优雅，值得参考

### 2.6 MCP 集成

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **模块** | `crates/mcp/` 9 个子模块 | `src/services/mcp/` | MCP 配置 | MCP 服务器 | MCP 集成 |
| **传输** | Channel (stdio/SSE/HTTP) + Proxy + OAuth | stdio/SSE/HTTP/SDK | stdio | stdio/SSE | stdio/SSE |
| **Elicitation** | 支持 (elicitation.rs) | 支持 | 支持 | 支持 | 支持 |
| **Resources** | 独立资源管理 (resources.rs) | 支持 | 支持 | 支持 | 支持 |

**CodeRex 参考建议：**
- CodeRex 的 MCP 模块完整度最高，包含 OAuth、Proxy、Channel、Config、Elicitation、Resources、Server
- 与其他项目持平

### 2.7 Hooks / 事件系统

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **模块** | `crates/hooks/` 独立 crate | 内建 hooks | Wire 协议 | Hooks | Webhooks |
| **事件类型** | `HookEvent` 枚举 + `HookExecutor` | 内建 | 事件流 | Hooks | Webhooks |
| **钩子点** | TurnEnd / UserPromptSubmit / PreToolUse / PostToolUse / PostToolUseFail / PreCompact / PostCompact / StopHook | 类似 | Wire 事件 | 类似 | 类似 |
| **配置** | `settings.json` + settings hook | `settings.json` | YAML | 配置文件 | 配置文件 |

**CodeRex 参考建议：**
- CodeRex 的 hook 生命周期最完整（9+ 钩子点），优于其他项目
- Kimi 的 Wire 协议作为事件总线的设计更通用，适合 IDE 集成

### 2.8 配置系统

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **模块** | `crates/config/` 9 个子模块 | `src/config/` | `src/kimi_cli/` | 配置文件 | 配置文件 |
| **功能** | Dirs / Experiments / Flags / OrgPolicy / RemoteSettings / Settings / TrustedDevice | Feature flags (44+) | YAML 配置 | JSON/TOML | JSON |
| **多模型** | `ModelId` 抽象 | 仅 Claude | Kosong 抽象层 | 仅 Qwen | 仅 Gemini |

**CodeRex 参考建议：**
- CodeRex 的 `OrgPolicy` + `RemoteSettings` + `TrustedDevice` 是企业级特性，其他项目大多没有
- Claude Code 的 44+ feature flags 值得参考，用于灰度发布

### 2.9 Server / 远程 / Bridge

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **模块** | `crates/server/` + `crates/bridge/` | 内建 | ACP server | 无 | 无 |
| **功能** | 远程会话 / 心跳检测 / 轮询 / 状态管理 / 传输 / 权限 / 归档 | 内建 | ACP 协议 (IDE 集成) | IDE 插件 | IDE 插件 |
| **协议** | 自定义协议 + Websocket | 自定义 | ACP + Wire | 自定义 | 自定义 |

**CodeRex 参考建议：**
- **CodeRex 独有强项**：Bridge 系统是 5 个项目中唯一独立的远程会话编排系统
- Kimi 的 ACP (Agent Client Protocol) 标准化了 IDE 集成协议，值得参考

### 2.10 SDK / 嵌入式

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **模块** | `crates/sdk/` | 无公开 SDK | `kimi-agent-sdk` (Python + Rust) | `@qwen-code/qwen-code-core` (NPM) | 无 |
| **用途** | 结构化 I/O 用于 headless/embedded | 无 | 自定义 Agent 构建 | NPM 包 | 无 |
| **事件流** | `SdkEventStream` | 无 | Wire 协议 | 事件订阅 | 无 |

**CodeRex 参考建议：**
- CodeRex 有 SDK crate 但功能较薄（仅事件流）
- Kimi 的 `kimi-agent-sdk` 允许用户构建自定义 Agent，值得参考

### 2.11 TUI / 终端界面

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **框架** | Ratatui + Crossterm | React + Ink | 自定义 TUI | React + Ink | React + Ink |
| **功能** | 消息列表 / 输入 / 搜索 / 选择器 / 状态栏 / 通知中心 / Vim 模式 / 语音 / 主题 | 200+ React 组件 | Shell / Print 模式 | 共享组件库 | 共享组件库 |
| **特色** | Vim 模式 / 主题系统 / Buddy 动画 / Voice | 完全响应式 | 轻量 shell 模式 | 统一 UI 架构 | 统一 UI 架构 |

**CodeRex 参考建议：**
- CodeRex 的 Ratatui 方案是唯一不用 React 的，**性能优势**
- CodeRex 的 Vim 模式、主题系统、Buddy 动画是独特功能

---

## 3. 测试策略对比

### 3.1 测试概览

| | CodeRex | Claude Code | Kimi CLI | Qwen Code | Gemini CLI |
|---|---------|-------------|----------|-----------|------------|
| **框架** | Rust `#[test]` + `cargo test` | Vitest / Jest | pytest + pytest-asyncio | Vitest / Jest | Vitest / Jest |
| **单元测试** | `#[cfg(test)]` 模块内联测试 | 文件级 `*.test.ts` | `tests/test_*.py` | `*.test.ts` | `*.test.ts` |
| **集成测试** | `tests/` 目录 | 集成测试 | pytest fixtures | Vitest 集成测试 | `vitest run --root ./integration-tests` |
| **E2E 测试** | `crates/cli/tests/e2e_*.rs` | 无公开 E2E 框架 | Scripted echo provider + PTY 测试 | Headless + tmux 模式 | Binary 执行测试 |
| **Mock** | `mockall` + trait mock | Mock / Fake | Scripted echo provider | 录制/回放 | Mock |
| **CI** | GitHub Actions | GitHub Actions | GitHub Actions | GitHub Actions | GitHub Actions |

### 3.2 各项目测试详解

#### CodeRex
- **位置**：`#[cfg(test)]` 模块内联 + `crates/cli/tests/e2e_*.rs`
- **策略**：每个模块自带测试，CLI crate 有 E2E 测试
- **Mock**：`mockall` 用于 trait mock，`WireMock` (wiremock) 用于 HTTP mock
- **E2E**：`e2e_core_flows.rs` / `e2e_harness.rs` / `e2e_stress.rs` — 覆盖核心流程
- **特点**：利用 Rust 类型系统做编译期检查，测试隔离性好

#### Claude Code (CC-src)
- **框架**：Vitest (主) + Jest
- **规模**：512K 行代码对应大量测试
- **策略**：文件级 `*.test.ts` 测试，组件用 React Testing Library
- **E2E**：CLI 工具测试通过 `child_process` 派生 + 断言文件系统变化
- **特点**：React 组件有完整的 snapshot 测试

#### Kimi CLI
- **框架**：pytest + pytest-asyncio
- **策略**：`tests/test_*.py`，覆盖率目标 90%+
- **E2E**：
  - Scripted echo provider（无需真实 API）
  - Shell PTY 测试
  - Session 管理测试
- **特点**：Echo provider 设计巧妙，用预定义回复模拟 LLM，无需 API key 即可跑 E2E

#### Qwen Code
- **框架**：Vitest / Jest
- **策略**：单元测试 + 集成测试 + Benchmarks
- **E2E**：
  - `.qwen/skills/e2e-testing/SKILL.md` 标准化技能
  - Headless 模式测试
  - Interactive (tmux) 模式测试
  - MCP 服务器测试
  - API 流量检查
- **特点**：录制/回放 (recording/playback) 机制 + Skill 测试框架

#### Gemini CLI
- **框架**：Vitest (集成) + Jest (单元)
- **策略**：两级 — 单元测试（直接导入库）+ E2E 测试（执行编译后的二进制）
- **E2E**：通过执行构建的二进制在受控环境中验证端到端功能
- **特点**：有 proposal 增加 unit 和 e2e 之间的中间层测试（类似 integration tests）

### 3.3 CodeRex 测试参考建议

1. **借鉴 Kimi 的 Scripted Echo Provider**：CodeRex 的 E2E 测试目前依赖真实 API。可设计一个 mock LLM provider，返回预定义回复，实现无 API key 的 E2E 测试
2. **借鉴 Qwen 的录制/回放机制**：将 API 请求录制到文件，回放时直接读取缓存结果，加速测试
3. **借鉴 Gemini 的两级测试分层**：CodeRex 目前只有 `cargo test`（单元）和 E2E 文件，可增加中间层的 integration tests（如 mock 网络但使用真实工具链）
4. **借鉴 Qwen 的 tmux E2E 模式**：Qwen 用 tmux 自动化测试交互式 TUI 行为，CodeRex 可用类似方式测试 Ratatui 界面
5. **增加 snapshot 测试**：Claude Code 对 React 组件做 snapshot，CodeRex 的 TUI 渲染可对 `ratatui::Buffer` 做 snapshot 比较

---

## 4. 最佳设计总结

### 4.1 CodeRex 的优势设计

| 设计 | 说明 | 对比其他项目 |
|------|------|-------------|
| **TurnState 状态机** | 15 种状态 + `is_user_waiting()` + `is_terminal()` | 最清晰，Claude Code 的 46K 行也走此路线 |
| **权限引擎 (5 级)** | Allow/Ask/Deny/Elevated/Bypass + 路径感知 | 最细粒度 |
| **工具系统 (Trait-based)** | `Tool` trait + `ToolExecutor` + `ToolRegistry` | Rust trait 比 TS 接口更严谨 |
| **4 种 Compact 策略** | Micro/Auto/Reactive/Snip | 最丰富 |
| **Bridge 远程系统** | 心跳/轮询/状态/传输/权限/归档 | 唯一独立的远程编排 |
| **TeamMemory + AutoDream** | 团队记忆 + 梦境检查 | 独有 |
| **OrgPolicy + RemoteSettings** | 企业级配置 | 独有 |
| **Ratatui TUI** | 原生 Rust，无 Node 依赖 | 性能最好 |
| **Hook 系统 (9+ 钩子点)** | TurnEnd/UserPromptSubmit/PreToolUse/... | 最完整 |

### 4.2 其他项目值得 CodeRex 参考的设计

| 来源 | 设计 | 对 CodeRex 的价值 |
|------|------|------------------|
| **Claude Code** | Feature flags (44+) | 灰度发布控制 |
| **Claude Code** | Lazy loading 工具 (~40/60+) | 减少内存/启动开销 |
| **Claude Code** | React 组件 snapshot 测试 | TUI 渲染可测试性 |
| **Claude Code** | `.coderex/` 目录约定 | 用户习惯，降低学习成本 |
| **Kimi CLI** | Scripted Echo Provider | 无 API 依赖的 E2E 测试 |
| **Kimi CLI** | Wire 协议事件总线 | IDE 集成标准化 |
| **Kimi CLI** | YAML Agent 声明 | 用户可自定义 Agent |
| **Kimi CLI** | Kosong 多模型抽象 | CodeRex 已有 `ModelId`，可扩展为完整抽象层 |
| **Kimi CLI** | ACP 协议 | 标准化 IDE 集成协议 |
| **Kimi CLI** | `kimi-agent-sdk` | 允许第三方构建自定义 Agent |
| **Qwen Code** | 录制/回放测试 | 加速 E2E，降低 API 成本 |
| **Qwen Code** | `.qwen/skills/` 标准化格式 | Skill 系统规范化 |
| **Qwen Code** | Headless + tmux E2E | 交互式 TUI 自动化测试 |
| **Qwen Code** | GitHub Action 集成 | CI/CD 场景覆盖 |
| **Qwen Code** | Sub-agent 架构 | 并行任务委托 |
| **Gemini CLI** | 扩展市场 (Extensions) | 第三方插件生态 |
| **Gemini CLI** | 两级测试分层 (unit + e2e binary) | 测试金字塔完善 |

---

## 5. 架构演进建议

### 5.1 近期 (1-2 周)

1. **Scripted Echo Provider**：借鉴 Kimi CLI，实现 mock LLM provider，支持无 API 依赖的 E2E 测试
2. **录制/回放机制**：借鉴 Qwen Code，增加 API 响应缓存，加速回归测试
3. **Feature Flags**：借鉴 Claude Code 的 44+ flags，在 `crates/config` 中增加灰度控制

### 5.2 中期 (1-2 月)

4. **ACP 协议支持**：借鉴 Kimi CLI，标准化 IDE 集成协议
5. **Agent SDK 完善**：借鉴 Kimi 的 `kimi-agent-sdk`，完善 `crates/sdk`，支持第三方自定义 Agent
6. **工具 Lazy Loading**：借鉴 Claude Code，按需加载工具减少内存
7. **GitHub Action 集成**：借鉴 Qwen/Gemini，支持 CI/CD 场景

### 5.3 长期 (3-6 月)

8. **Extension Marketplace**：借鉴 Gemini CLI，建立第三方插件生态
9. **Swarm 模式**：增强 `TeamManager` 支持 100+ 并行 sub-agent
10. **Snapshot 测试**：为 TUI 渲染增加 snapshot 比较

---

## 6. 总结

CodeRex 在架构严谨性、权限控制、远程编排、记忆系统等方面 **已领先** 多数竞品。核心差距在于：
- **测试工程化**（录制/回放、echo provider、snapshot）
- **生态集成**（GitHub Action、Extension 市场、标准化 IDE 协议）
- **用户心智模型**（`.coderex/` 目录约定、YAML Agent 声明）

CodeRex 的 Rust + Cargo workspace 架构在类型安全、编译期检查、性能上具有天然优势，保持这一技术路线的同时吸收上述竞品的工程实践，可以快速缩小差距。
