# coderex 向 cc-src 风格 AI 调用方式演进计划

## 目标

目标不是把 `coderex` 机械地改成 `cc-src` 的逐文件翻版，而是让 `coderex` 具备和 `cc-src` 类似的 AI 调用能力与分层：

1. 主会话调用有清晰的编排层，而不只是 `QueryEngine -> ApiClient`。
2. 除主会话外，支持轻量判定、小模型工具任务、后台 forked agent 等多种 AI 调用形态。
3. Prompt 不再只有统一 compiler，而是形成“默认系统 prompt + profile prompt + feature prompt + context 注入”的体系。
4. 命令、memory、summary、session naming、permission explanation 等能力可以作为独立 AI feature 复用。

一句话概括：

把 `coderex` 从“统一内核可调用模型”升级为“像 `cc-src` 一样有产品化 AI 编排能力的 agent 系统”。

---

## 一、当前现状

从现有实现看，`coderex` 的 AI 主链路主要是：

- `crates/coderex-cli/src/repl.rs`
- `crates/coderex-core/src/engine.rs`
- `crates/coderex-core/src/prompt.rs`
- `crates/coderex-core/src/api_client.rs`

当前已经具备的能力：

- 统一的 `QueryEngine`
- 统一的 `PromptCompiler`
- streaming / non-streaming API 调用
- tool call 解析与执行
- compact / permission / elicitation / hooks
- 命令上下文注入，例如 `/advisor`、`/bughunter`

当前缺少的关键层：

- 缺少像 `cc-src/query.ts` 那样面向“调用形态”的 AI 编排入口
- 缺少 `queryHaiku` / `sideQuery` / `runForkedAgent` 这类按用途封装的调用原语
- 缺少 feature-specific prompt registry
- 缺少后台 AI 工作流能力层
- 命令系统目前主要是“注入额外 context”，还不是“切换完整 prompt 行为模式”

所以真正要改的，不是单个函数，而是补出一层 `orchestration/product-layer`。

---

## 二、目标架构

建议把 `coderex` 最终演进成 4 层。

### 1. API 传输层

保留现有：

- `ApiClient::create_message(...)`
- `ApiClient::create_message_streaming(...)`

这一层只负责：

- 发 Anthropic 请求
- 处理 streaming
- 重试与错误处理

不要把产品语义塞进这一层。

### 2. 核心执行层

保留并增强现有：

- `QueryEngine`
- `PromptCompiler`
- tool loop
- compact / permission / hooks / elicitation

这一层负责：

- turn state machine
- tool execution
- 上下文组装
- budget / cost / transcript

### 3. AI 编排层

这是当前 `coderex` 最缺的一层，建议新增。职责类似 `cc-src` 的 `REPL + QueryEngine.ts + query.ts + sideQuery + runForkedAgent` 组合。

这一层应该提供统一入口，例如：

- `run_main_agent(...)`
- `run_side_query(...)`
- `run_lightweight_query(...)`
- `run_forked_agent(...)`
- `run_background_feature(...)`

这一层负责：

- 按调用用途选择模型
- 按调用用途选择 prompt profile
- 构造 user/system context
- 选择是否流式
- 选择是否带工具
- 选择是否允许文件写入
- 组织后台任务与结果消费

### 4. Feature 层

像 `cc-src` 一样，把具体 AI 应用做成 feature：

- session title
- summary / compact summary
- permission explanation
- session memory update
- extract long-term memories
- prompt suggestion
- agent generation
- issue / review / bughunter 模式 prompt

这些 feature 不应该直接操作 `ApiClient`，而应该都走编排层。

---

## 三、与 cc-src 对齐时，最值得抄的不是“代码形式”，而是“调用形态”

`cc-src` 的优势不在于某个 prompt 文本，而在于它把 AI 调用拆成了几种稳定形态：

1. 主代理流式调用
2. 非流式单次 side query
3. 小模型轻任务调用
4. forked agent / 后台 agent 调用
5. 命令/skill 改写 agent 行为

`coderex` 要接近 `cc-src`，最优路线不是先复制大量 feature，而是先把这 5 种调用形态抽出来。

---

## 四、改造原则

### 原则 1：不要破坏 `QueryEngine` 的“内核”角色

`QueryEngine` 现在已经承担了稳定执行引擎职责。不要把各种产品 feature 直接塞回 `engine.rs`，否则很快会失控。

建议：

- `engine.rs` 保持“通用执行机”定位
- 新增 orchestrator 层承接 `cc-src` 风格功能

### 原则 2：不要把所有 prompt 都塞进 `PromptCompiler`

`PromptCompiler` 适合装：

- base system prompt
- static blocks
- dynamic session blocks
- cache boundary

但不适合承载所有 feature-specific prompt。

建议：

- `PromptCompiler` 负责基础 prompt 骨架
- feature prompt 独立模块化
- orchestrator 负责把二者拼起来

### 原则 3：先补抽象，再补 feature

如果先做 session title、summary、memory 等 feature，而没有 side query / lightweight / forked agent 这些调用原语，后面会不断重复造轮子。

正确顺序应该是：

1. 先补调用形态抽象
2. 再补 prompt profile
3. 最后批量接 feature

---

## 五、具体改造方案

## Phase 0：先做能力盘点与边界定义

目标：明确哪些能力必须做成 `cc-src` 对齐版本，哪些先不做。

建议输出一张 capability matrix：

| 能力 | cc-src 状态 | coderex 当前状态 | 目标阶段 |
| --- | --- | --- | --- |
| 主代理流式调用 | 完整 | 已有基础版 | P1 |
| side query | 完整 | 缺失 | P1 |
| 小模型工具任务 | 完整 | 缺失 | P1 |
| forked agent | 完整 | 缺失 | P2 |
| session title | 有 | 缺失/弱 | P2 |
| permission explanation | 有 | 弱 | P2 |
| session memory AI 更新 | 有 | 缺失 | P3 |
| extract memories | 有 | 缺失 | P3 |
| prompt suggestion | 有 | 缺失 | P3 |

产物：

- 一张能力对齐表
- 一张模块职责图

---

## Phase 1：补齐 3 个基础调用原语

这是第一优先级。

### 1. 新增 `run_main_agent(...)`

封装当前主链路，统一处理：

- system prompt/profile 选择
- context message 注入
- streaming on/off
- tools on/off
- max turns / budget

它本质上是给 `QueryEngine::submit(...)` 外面再套一层“产品语义”。

### 2. 新增 `run_side_query(...)`

用途对齐 `cc-src` 的 side query：

- 单次判定
- 单次解释
- 不进入完整主会话
- 不跑长 tool loop
- 一般非流式

典型场景：

- permission explainer
- prompt classifier
- memory selection
- mode validation

建议接口：

```rust
pub struct SideQueryRequest {
    pub system_prompt: String,
    pub user_prompt: String,
    pub model: ModelId,
    pub max_tokens: u32,
}
```

实现上不要复用整个 turn loop，可以直接基于 `ApiClient::create_message(...)` 做薄封装。

### 3. 新增 `run_lightweight_query(...)`

用途对齐 `queryHaiku(...)`：

- 小模型
- 短 prompt
- 快速返回
- 常用于 title、rename、summary、parser、classification

建议新增一个轻量模型策略层：

- `ModelProfile::Main`
- `ModelProfile::Lightweight`
- `ModelProfile::Background`

这样后面所有 feature 都不用直接写死模型名。

建议改动位置：

- 新增 `crates/coderex-core/src/orchestrator.rs`
- 或新增 `crates/coderex-agent/src/orchestrator.rs`

我更建议新建 orchestrator 模块，而不是继续膨胀 `engine.rs`。

---

## Phase 2：建立 Prompt Profile 体系

这是第二优先级。

当前 `coderex` 有 `PromptCompiler`，但还没有 `cc-src` 那样的 prompt profile 分层。

建议新增 3 类 prompt 概念。

### 1. Base Prompt

由 `PromptCompiler` 生成，负责：

- agent 基本行为规范
- tool usage guide
- output style
- system context
- cache boundary

### 2. Mode Prompt

对应 `/advisor`、`/bughunter`、`/review` 这类模式。

建议从当前 `CommandResult::with_context(...)` 升级为：

- `extra_context`
- `mode_prompt`
- `append_system_prompt`

不要只把它们当普通 user context，因为很多模式本质上是在改写 agent 行为边界。

### 3. Feature Prompt

专门给单一任务使用，例如：

- session title prompt
- permission explanation prompt
- compact summary prompt
- session memory update prompt
- extract memories prompt

建议新增目录：

- `crates/coderex-core/src/prompts/`
- `crates/coderex-memory/src/prompts/`
- `crates/coderex-commands/src/prompts/`

避免所有 prompt 散落在业务文件里。

---

## Phase 3：引入 forked agent / background agent 能力

这是和 `cc-src` 差距最大的能力层。

`cc-src` 的很多高级能力都不是主会话直接完成的，而是后台 agent 用独立 prompt 和独立上下文去完成。

`coderex` 如果要接近它，必须支持“派生任务”。

### 需要的能力

- 从主会话派生一个子任务
- 继承一部分 cache-safe context
- 指定独立 prompt messages
- 指定是否允许写文件
- 指定工具白名单
- 跑完后把结果回收给主线程

### 建议接口

```rust
pub struct ForkedAgentRequest {
    pub task_name: String,
    pub system_prompt: String,
    pub prompt_messages: Vec<Message>,
    pub inherit_context: bool,
    pub allowed_tools: Vec<String>,
    pub writable: bool,
    pub model_profile: ModelProfile,
}
```

### 第一批适合改造成 forked agent 的功能

1. session memory update
2. extract long-term memories
3. prompt suggestion
4. agent summary / away summary
5. magic docs / background documentation

建议不要一开始就并发执行很多后台 agent，先实现“串行、可观测、可取消”的单后台任务模型。

---

## Phase 4：把命令系统从“文本上下文注入”升级为“Prompt 行为切换”

当前命令系统的问题是：

- `/advisor` 和 `/bughunter` 只是 `.with_context(...)`
- 这更像加一句话，而不是切换 agent profile

要更像 `cc-src`，建议把 Prompt 类型命令升级成下面的结构：

```rust
pub struct PromptCommandEffect {
    pub extra_context: Option<String>,
    pub override_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub prompt_profile: Option<String>,
    pub allowed_tools_override: Option<Vec<String>>,
}
```

这样命令才能真正做到：

- 切模式
- 限工具
- 改系统 prompt
- 附加执行规范

这一步会显著提升 `coderex` 的“可塑性”。

---

## Phase 5：补第一批 `cc-src` 风格 AI feature

等调用原语和 prompt profile 稳定后，再补 feature，优先做投入产出比高的。

### 优先级 A

1. `session title`
原因：实现简单、体验提升明显、适合用 lightweight query。

2. `permission explanation`
原因：能直接提升审批体验，适合用 side query。

3. `compact summary`
原因：和现有 compact 机制天然衔接。

### 优先级 B

1. `session memory update`
2. `extract long-term memories`
3. `agent summary`

这些更适合基于 forked agent 做。

### 优先级 C

1. `prompt suggestion`
2. `agent generation`
3. `magic docs`

这些是高级产品能力，应该在前面几层稳定后再做。

---

## 六、推荐的代码落点

为了避免把逻辑继续堆进 `engine.rs`，建议这样组织。

### 1. `crates/coderex-core`

保留内核职责：

- `engine.rs`
- `prompt.rs`
- `api_client.rs`

新增：

- `orchestrator.rs`
- `query_profiles.rs`
- `prompts/mod.rs`
- `prompts/base.rs`
- `prompts/features/*.rs`

### 2. `crates/coderex-commands`

扩展命令效果结构：

- 从 `with_context(...)` 升级到可表达 `append_system_prompt / prompt_profile / allowed_tools_override`

### 3. `crates/coderex-memory`

新增 AI memory workflow：

- `session_memory_agent.rs`
- `extract_memories.rs`
- `prompts/session_memory.rs`

### 4. `crates/coderex-agents`

如果这个 crate 是空的或能力较弱，建议把 forked/background agent 优先放这里。

它适合承接：

- 子任务执行器
- 后台 agent 生命周期
- 结果回收协议

---

## 七、建议的数据结构改动

为了支持更像 `cc-src` 的调用方式，建议补这些结构。

### 1. 在 `SubmitOptions` 外面增加更高层请求对象

现在 `SubmitOptions` 偏执行参数，不够表达“调用意图”。

建议新增：

```rust
pub enum QueryKind {
    MainAgent,
    SideQuery,
    Lightweight,
    ForkedAgent,
}

pub struct QueryRequest {
    pub kind: QueryKind,
    pub user_prompt: String,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub prompt_profile: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub model_profile: ModelProfile,
    pub inherit_session: bool,
}
```

`orchestrator` 负责把 `QueryRequest` 翻译成 `SubmitOptions + prompt/messages + model`。

### 2. 为 prompt 选择引入 profile

```rust
pub enum PromptProfile {
    DefaultAgent,
    Advisor,
    BugHunter,
    Reviewer,
    MemoryUpdater,
    TitleGenerator,
    PermissionExplainer,
}
```

### 3. 为模型选择引入 profile

```rust
pub enum ModelProfile {
    Main,
    Lightweight,
    Background,
}
```

这样以后更换模型时，不会把业务逻辑绑死在具体型号上。

---

## 八、迁移顺序建议

建议按下面顺序推进。

### 第 1 周

- 明确 capability matrix
- 新增 `orchestrator` 骨架
- 抽出 `run_main_agent / run_side_query / run_lightweight_query`

### 第 2 周

- 建立 `PromptProfile` 和 `ModelProfile`
- 改造命令系统支持 profile / append_system_prompt
- 把 `/advisor`、`/bughunter` 升级成真正模式切换

### 第 3 周

- 接入 `session title`
- 接入 `permission explanation`
- 接入 `compact summary`

### 第 4 周

- 实现 `run_forked_agent`
- 打通一个后台 feature，例如 `session memory update`

### 第 5 周以后

- 扩展 `extract memories`
- 扩展 `agent summary`
- 扩展 `prompt suggestion`
- 扩展文档/自动生成能力

---

## 九、风险点

### 1. 不要让 `engine.rs` 再变成产品层大杂烩

如果所有 feature 都直接往 `QueryEngine` 里塞，最后会把 `coderex` 做成比 `cc-src` 更难维护的形态。

### 2. 不要把 feature prompt 和 base prompt 混在一起

否则 prompt cache、行为边界、回归测试都会很难做。

### 3. 不要一开始就上复杂并发后台 agent

先做串行版本，保证：

- 可取消
- 可追踪
- 可记录 usage
- 可限制工具权限

### 4. 不要先追“功能数量”

先把三类调用原语做稳：

- main
- side
- lightweight

再做 forked/background。

---

## 十、验收标准

当下面这些条件成立时，可以认为 `coderex` 已经开始接近 `cc-src` 风格：

1. 主会话、side query、lightweight query、forked agent 都有统一入口。
2. 命令不只是注入普通文本，而是能切换 prompt profile。
3. 至少 3 个 feature 不直接调 API，而是复用 orchestrator。
4. prompt 拆成 base/mode/feature 三层，而不是单点字符串拼接。
5. memory 类能力至少有一个通过后台 agent 跑通。

---

## 十一、最小可行版本

如果只做最小版本，我建议先做这 5 项：

1. 新增 `orchestrator` 模块。
2. 实现 `run_side_query(...)`。
3. 实现 `run_lightweight_query(...)`。
4. 为命令系统增加 `append_system_prompt` 和 `prompt_profile`。
5. 先落地 `session title` 和 `permission explanation` 两个 feature。

这是最小但最有效的一步，因为它已经把 `coderex` 从“只有主会话引擎”推进到“开始具备 `cc-src` 风格多轨 AI 调用”。

---

## 十二、结论

如果要把 `coderex` 改成类似 `cc-src` 那样调用 AI，核心不是重写 API client，也不是重写 `PromptCompiler`，而是补出一层产品化 AI 编排层。

最重要的改造顺序是：

1. 先抽出调用形态：`main / side / lightweight / forked`
2. 再建立 prompt profile：`base / mode / feature`
3. 再把 session title、permission explanation、memory update 等 feature 接上去

这样改，`coderex` 会保留当前 Rust 内核的清晰结构，同时逐步获得 `cc-src` 那种成熟的 AI 使用方式。