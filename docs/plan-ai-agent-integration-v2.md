# AI Agent 集成设计 v2：rs-iot 作为 Agent Host（spawn 驱动模型）

> 本文是 [`plan-ai-agent-integration.md`](./plan-ai-agent-integration.md)（v1，serve 反代方案）的**并列新方案**，
> 基于 dbx（`G:\AI_Agent\dbx`，Rust + Tauri 数据库工具）深度研究后产生。**v1 保留不删**——它是"rs-iot 做轻量代理"路线的基准；
> 本文 v2 是"rs-iot 做 agent host"路线。两者代表不同的职责划分哲学，最终选哪个待 P0 验证后定。

## 一、为什么出 v2（dbx 带来的新认知）

三路研究（Reasonix / Zacp / dbx）中，dbx 提供了 v1 没有的三个决定性事实：

### 事实 1：MCP server 有官方 Rust 范本，零手写协议风险

dbx-mcp（`crates/dbx-mcp/`）证明 rs-iot 的 MCP server 可以**整段照抄**：

| 维度 | dbx 实现 | rs-iot 照抄 |
|---|---|---|
| MCP 库 | `rmcp = "2.2.0"` 官方 SDK（`Cargo.toml:20`） | 同 |
| 工具声明 | `#[tool]` 宏 + `schemars::JsonSchema` derive（零手写 schema，`server.rs:241`） | 同 |
| 传输 | `rmcp::transport::stdio()`（`main.rs:17`），21 行入口 | 同 |
| 后端抽象 | `DbxBackend` trait + LocalBackend（直连 core）+ WebBackend（调 REST，`backend.rs:90-177`） | `RsIotBackend` trait + 同双实现 |
| 权限三档 | 运行时 `read_only/safe_write/high_risk_write`（`server.rs:1000-1047`），不用静态注解 | 改 SQL 风险分类为"写操作风险分类" |
| 入口 | `main.rs` 21 行（环境变量二选一 backend，`serve(transport)`） | 几乎逐行照搬 |

**v1 把 MCP 实现列为"中等风险点"，dbx 证明这个风险不存在**——rmcp 把 NDJSON 帧解析、JSON-RPC、schema 生成都封装了。

### 事实 2：dbx 用通用 spawn 模型驱动外部 CLI agent（第三条路）

dbx 的 `ai_cli_agent.rs::run_cli_jsonl_agent`（`:1003`）spawn 8 种外部 agent（Codex/Claude Code/Cursor/Grok/CodeBuddy/Qoder/Pi/OpenCode），用统一的 `CliAgentProcessSpec`（`ai_cli_agent.rs:55-64`）+ `CliAgentJsonlDialect`（`:34-43`，7 种 NDJSON 方言）适配它们的输出格式。**这是 v1 两版方案都漏掉的模型**：

| 路线 | 谁管 agent 进程 | rs-iot 职责 | 协议耦合 |
|---|---|---|---|
| v1 serve 反代 | OS 服务（agent 独立常驻） | 反代 + 健康探针 | HTTP/SSE（松耦合，但 agent 必须常驻） |
| ACP stdio（曾否决） | rs-iot spawn 持管道 | 完整 ACP host | ACP JSON-RPC（紧耦合） |
| **v2 spawn 驱动**（本文） | **rs-iot 按需 spawn** | **通用 host，按 agent 方言适配** | **NDJSON 方言层（中等耦合，但 rs-iot 主控）** |

v2 的本质：rs-iot 像 dbx 一样，**自己当 agent 的 host**——按需 spawn `reasonix run --output-format stream-json`（或 ACP），读写 stdin/stdout，按方言解析事件。Reasonix 不必常驻（省资源），也不被 ACP 协议绑死（可换 agent）。

### 事实 3：dbx 内嵌 AI 是"窄域 + 厚安全网"，rs-iot 是"宽域 + 无安全网"，不应内嵌

dbx 内嵌 AI（agent_loop 1608 + agent_tools 2065 + ai.rs 7130 行，~30000 行）能安全运行，**全靠 SQL 是结构化可静态分析语言**——`sql_risk.rs` 用 sqlparser AST 把操作分 ReadOnly/Write/Ddl/Transaction 四档，生产库写操作无条件拒，逐 SQL 确认（`agent_tools.rs:712-741`）。rs-iot 的"分析日志/写 lua 脚本/排障"是开放域，**没有这种安全网**——内嵌 agent 等于 LLM 对工业网关自由发挥。**所以 v1/v2 都外接 Reasonix，不内嵌**（这点 dbx 研究强化了 v1 的结论）。

## 二、v2 核心设计：rs-iot 作为 Agent Host

### 2.1 进程拓扑

```
现场机器（无源码，仅二进制 + 运行时目录）
┌─────────────────────────────────────────────────────────────────────────┐
│  rs-iot :8790（Rust 主进程，唯一对外窗口 + agent host）                  │
│  ┌──────────────────────────────────────────────────────────────┐       │
│  │ axum REST（现有 /api/v1/* 全功能）                            │       │
│  │ + /api/v1/agent/* host 端点(新)                               │       │
│  │ + AgentHost 模块(新,spawn+方言适配+事件中继)                  │       │
│  └────────────────┬────────────────────────────────────┬────────┘       │
│                   │ spawn(按需,空闲回收)                │ MCP stdio      │
│                   ▼                                     ▼                │
│  ┌──────────────────────────────┐        ┌──────────────────────────┐   │
│  │ reasonix run                 │        │ rs-iot --mcp-server(新)  │   │
│  │  --output-format stream-json │        │ rmcp #[tool] 范本(照抄dbx)│   │
│  │  (按需 spawn,非常驻)         │        │ backend=调主进程 REST    │   │
│  │  或 reasonix --acp(可选方言) │        │  loopback :8790          │   │
│  └──────────────────────────────┘        └──────────────────────────┘   │
│                                                                         │
│  前端：所有页面 + 全局浮动 AI 面板（URL/数据感知）                       │
│         ─WS─► /api/v1/agent/* ─► AgentHost spawn+中继                   │
└─────────────────────────────────────────────────────────────────────────┘
```

### 2.2 与 v1 的关键差异

| 维度 | v1（serve 反代） | v2（spawn 驱动） |
|---|---|---|
| Reasonix 部署 | 独立 OS 服务常驻 :8787 | rs-iot 按需 spawn，空闲回收（省内存） |
| rs-iot 职责 | 反代 + 探针（被动） | AgentHost（主动 spawn/驱动/回收） |
| 协议 | HTTP/SSE（serve 固有） | NDJSON stream-json / ACP（按方言适配，可换 agent） |
| agent 可替换性 | 低（绑定 serve） | **高**（方言层抽象，能换 Reasonix/Codex/Grok，照 dbx 范式） |
| 鉴权 | serve token + rs-iot JWT 双层 | rs-iot JWT 单层（agent 不暴露端口） |
| 复杂度 | 低（反代透传） | 中（spawn 生命周期 + 方言解析，但 dbx 有范本） |
| 适合场景 | agent 必须常驻（多值班员高频） | agent 按需用（现场偶发，省资源） |

### 2.3 方言层设计（v2 精髓，照 dbx `CliAgentJsonlDialect`）

rs-iot 的 `AgentHost` 用 trait + 方言枚举适配不同 agent（对标 dbx `ai_cli_agent.rs`）：

```rust
// 借鉴 dbx ai_cli_agent.rs:34-43
pub enum AgentDialect {
    /// reasonix run --output-format stream-json（eventwire NDJSON）
    ReasonixStreamJson,
    /// reasonix --acp（ACP NDJSON JSON-RPC）
    ReasonixAcp,
    /// 未来：其他 agent
}

pub struct AgentProcessSpec {
    pub command: CliAgentCommandSpec,       // dbx ai_cli_agent.rs:30
    pub env: Vec<(String, String)>,
    pub current_dir: Option<PathBuf>,
    pub stdin_prompt: Option<String>,       // 注入 prompt（含页面上下文）
    pub dialect: AgentDialect,
    pub max_turns: u32,                      // dbx agent_loop.rs:24 默认 30
}

pub trait AgentEventSink {
    async fn on_event(&self, event: AgentEvent);  // 借鉴 dbx agent_events.rs:6
}

/// spawn agent → 逐行读 stdout NDJSON → 按方言解析 → 统一 AgentEvent → 推前端
/// 标杆：dbx ai_cli_agent.rs run_cli_jsonl_agent (:1003)
pub async fn run_agent(spec: AgentProcessSpec, sink: &dyn AgentEventSink, cancel: CancellationToken) { ... }
```

统一 `AgentEvent`（对标 dbx `agent_events.rs:6-31`）：`TurnStart / TextDelta / ReasoningDelta / ToolCallStart / ToolCallEnd / TurnEnd / ContextCompacted / AgentEnd / Error`。前端按类型渲染，**与 agent 实现无关**——换 agent 只改方言解析层。

## 三、rs-iot 改动清单（v2，仍遵循"少改核心"）

| 优先级 | 改动 | 位置 | 范本 | 改动量 |
|---|---|---|---|---|
| P0 | **`crates/rs-iot-mcp/`**（MCP server 子 crate） | 新 workspace 成员 | dbx-mcp 整段照抄 | 中（但范本现成） |
| P0 | **`AgentHost` 模块**（spawn + 方言 + 事件中继） | `src/agent/host.rs`（新） | dbx `ai_cli_agent.rs` + `agent_loop.rs` | 中 |
| P0 | **WS `/api/v1/agent/*`**（host 端点） | `src/api/routes_agent.rs`（新） | — | 小 |
| P0 | **浮动面板前端**（抽 Zacp 组件 + 全局显示/URL 感知） | 前端根布局 | Zacp `PermissionModal`/`MessageItem` | 中 |
| P0 | `AGENTS.md` 模板（现场工程师角色） | `scripts/ai/` 部署生成 | — | 小 |
| P1 | `POST /api/v1/lua/script/save`（写脚本） | `routes_lua.rs` | — | 小 |
| P1 | `GET /api/v1/config`（脱敏） | `routes_sys.rs` | — | 小 |
| P1 | **`rs-iot-cli` 子命令**（人机共用辅通道） | `src/cli.rs`（新） | dbx-cli 照抄 | 中（可选） |

**核心数据流（pipeline/lux/采集）仍零改动**——所有新增都是 axum 边 + 新 crate + 前端。

## 四、从 dbx 直接借鉴的设计（三路研究精华）

| 借鉴点 | dbx 源码 | rs-iot 用法 |
|---|---|---|
| **rmcp `#[tool]` + schemars derive** | `dbx-mcp/server.rs:241-242,921-928` | MCP 工具零手写 schema |
| **双 backend trait**（本地直连 core / 远程调 REST） | `dbx-mcp/backend.rs:90-177` | MCP server 调主进程 REST（主进程独占连接池） |
| **运行时权限三档**（read_only/safe_write/high_risk_write） | `dbx-mcp/server.rs:1000-1047` | 改为"写操作风险分类"（普通设定点/设备配置/关键设备禁写） |
| **confirmed-write 一次性提权** | `dbx-mcp/server.rs:989-991`（DBX_MCP_CONFIRMED_WRITE_SQL） | 用户确认单条高危写入后本次放行 |
| **spawn + NDJSON 方言适配** | `dbx-core/ai_cli_agent.rs:34-43,1003` | AgentHost 驱动 Reasonix，可换 agent |
| **结构化 AgentEvent**（TurnStart/ToolCall/Compacted...） | `dbx-core/agent_events.rs:6-31` | 前端渲染 agent 推理过程，与 agent 无关 |
| **流式取消用全局 Notify 注册表** | `dbx-core/ai.rs:28-47` | 简单线程安全，GUI 友好 |
| **多 provider 用 enum+match 不用 trait** | `dbx-core/ai.rs:63,625` | 若 rs-iot 给用户选模型，抄 AiProvider 结构 |
| **CLI 与 MCP 共享 backend trait**（人机同源） | `dbx-cli/main.rs:196-209` | rs-iot-cli 复用 rs-iot-mcp 的 backend |

## 五、v1 vs v2 决策建议

**两条路都成立，选哪个取决于现场使用模式**——这是产品判断，不是技术判断：

| 现场场景 | 推荐 | 理由 |
|---|---|---|
| 多值班员高频用 AI、agent 长期在线 | **v1 serve 反代** | agent 常驻省冷启动；rs-iot 最轻；反代透传简单 |
| 现场偶发用 AI、资源敏感（边缘机器） | **v2 spawn 驱动** | agent 按需起、空闲回收省内存；不常驻端口更安全 |
| 想要 agent 可替换（不绑 Reasonix） | **v2** | 方言层抽象，能接 Codex/Grok 等 |
| 想最快出 MVP | **v1** | 反代改动最小；v2 要写 spawn 生命周期 |

**我的倾向**：先用 v1 出 P0 MVP（反代最快验证产品形态——用户是否真用 AI 面板），若验证通过且发现"agent 常驻浪费资源"或"想换 agent"，再演进到 v2。两者 MCP server 部分**完全相同**（都照抄 dbx-mcp），v1→v2 只换"驱动层"（反代 → spawn host），投资不浪费。

## 六、对元反馈的诚实交代

你授权我"若 ACP 效果更好可不采纳建议"。我重审的结论：

- **ACP 的质量优势（多会话/steer/fs 钩子）在 rs-iot 单机少值班员场景用不满**，且要 rs-iot spawn 持管道（与"少扛事"冲突）。所以 ACP 仍否决——但这不是被建议4 绑架，是优势不兑现。
- **v1（serve 反代）的决策站得住**：rs-iot 最轻、agent 独立、协议松耦合。dbx 研究未推翻它，只是补了 v2 这条"rs-iot 当 host"的并列路线。
- **dbx 的最大贡献**：①消除 MCP 实现风险（rmcp 范本）；②提供 v2 的 spawn host 模型；③强化"不内嵌"结论（窄域+厚安全网才安全，rs-iot 是宽域）。

## 七、风险与未覆盖点（v2 特有）

1. **spawn 生命周期管理**：rs-iot 要实现"按需 spawn + 空闲回收 + 崩溃重启 + 超时强杀"——可借鉴 Zacp `manager.go`（5min tick 回收、cancel 20s 强杀），但 Rust 重写有工作量。
2. **方言解析鲁棒性**：reasonix stream-json 的 eventwire 格式若变，方言层要跟进；建议 v2 P0 先只支持 ReasonixStreamJson 一种，验证后再加 Acp。
3. **stdin 注入页面上下文**：v2 spawn 模式下，"当前页面 URL/数据"要在 spawn 时或 prompt 时注入——dbx 是前端构造 system prompt（`ai.ts:245`），rs-iot 浮动面板同样前端构造后经 WS 传 host 转 stdin。
4. **多并发会话**：v2 每个 prompt 一个 spawn 进程（或复用），并发管理比 v1 复杂——v1 serve 单进程切会话，v2 要 spawn 池。借鉴 dbx/zacp 的并发门闩。
5. **未实测**：reasonix `run --output-format stream-json` 的实际事件流、spawn 冷启动延迟、方言解析边界——P0 落地验证。

---

## 附录：三方案对照总表

| 维度 | v1 serve 反代 | v2 spawn 驱动 | （否决）ACP stdio | （否决）内嵌 dbx 式 |
|---|---|---|---|---|
| Reasonix 形态 | 独立常驻服务 | rs-iot 按需 spawn | rs-iot spawn 持管道 | 不用 Reasonix |
| rs-iot 职责 | 反代+探针（最轻） | AgentHost（中） | 完整 ACP host（重） | 内嵌 30k 行 agent（最重） |
| agent 可换 | 低 | **高**（方言层） | 中（ACP 标准） | N/A |
| 资源占用 | agent 常驻 | **按需回收** | 按需 | 同进程常驻 |
| 实现风险 | 低 | 中（有 dbx 范本） | 中（手写 ACP） | 高（30k 行+无安全网） |
| 安全网 | serve sandbox 配置 | 同 v1 + spawn 隔离 | host fs 钩子 | **无（开放域）** |
| 适合 | 高频多用户 | **偶发/边缘** | 多会话强隔离 | 不适合 rs-iot |
