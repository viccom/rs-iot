# AI Agent 集成设计：rs-iot × Reasonix

> 给 rs-iot（轻量工业 IoT 边缘框架，Rust）内嵌**一个**轻量、执行效果好的 coding agent，
> 让用户在 rs-iot Web 前端实时和它交流：查网关状态、操作设备、分析日志/报文/数据、编写并运行采集脚本、
> 以及其他受限操作。本文是三路深度研究（Reasonix 集成入口 / Reasonix Desktop 可借鉴功能 /
> [Zacp](https://github.com/helloxz/zacp) ACP 网关架构）收敛后的正式设计，是 P0/P1/P2 实施基准。

**一句话结论**：rs-iot 选定 **Reasonix** 作为唯一 AI agent，扮演**轻量路由代理**角色——
Reasonix 以 `serve` 模式独立常驻（rs-iot 不管其启停），rs-iot 仅做两件轻事：①反代前端 AI 面板的
请求到 Reasonix serve（**SSE 下行 + POST 上行，非 WS**，见第十章复核修正）；②周期健康探针。
rs-iot 暴露**全功能接口**给 Reasonix（MCP 为主 + CLI 为辅，同源），让 agent 像人类现场工程师一样使用网关。
前端是一个**全局浮动面板**（所有页面可显示/隐藏 + URL/数据感知）。
**设计原则：rs-iot 尽量少改动**，不碰核心数据流（pipeline/lux/采集），仅新增代理层 + 浮动面板 + 暴露层。

> **生产级复核已做**（2026-08-11，见第十章）：四个关键点（前端栈 / rmcp 移植 / serve 反代 / MCP spawn 契约）
> 全部用 rs-iot + reasonix + dbx 真实代码验证。两处关键修正：①前端↔rs-iot 从 WS 改 **SSE+POST**（rs-iot 当前零 WS 支持）；
> ②serve 鉴权用**伪造 Cookie** 非 Bearer（serve 只认 cookie/query）。识别 3 个 P0 硬坑（子进程守护/SSE 帧边界/鉴权兼容），
> 均有规避方案。P0 以 **MCP server 先行**（有 dbx 范本、低风险）。

> **AI agent 的角色定位**：取代传统工业现场的**数据采集实施 + 调试 + 脚本开发工程师**——部署设备、配置采集点
> （modbus/opcua/s7 点表）、调试通讯（读报文/排障）、写采集脚本（lua）、分析故障。工具命名与 `AGENTS.md` 提示词
> 一律用现场工程师语义，不用开发者语义。
>
> **部署形态约束**：现场运行 rs-iot 的机器上**没有 rs-iot 源代码**——只有二进制 + 运行时（config.toml/scripts/logs/data）。
> agent 的 `workspace_root` 是运行目录而非源码；MCP/CLI 只操作运行时 API（REST/lux），不依赖源码存在；
> `AGENTS.md` 由部署脚本生成到运行目录。

---

## 一、目标与约束

### 1.1 用户需求（已确认）

- **单 agent**，不要多 agent 网关——轻量优先。
- **综合能力**：既做网关运维分析（只读诊断、读日志/报文/数据），也做脚本/配置开发（写 lua、改配置）。
- **一套系统**：用户只看到 rs-iot 一个前端，AI 面板是其一部分；不割裂成两个 Web 界面。
- **定位**：rs-iot 是"面向 AI Agent 友好的工业数采系统"——集成可靠成品 agent，rs-iot 不自研 agent 框架。

### 1.2 设计原则（用户六条建议，硬约束）

| # | 建议 | 对设计的影响 |
|---|---|---|
| 1 | **rs-iot 尽量少改动**，做 AI 交互窗口与 Reasonix 后端的路由代理 | rs-iot 只加「反代 + 健康探针 + 暴露层 + 浮动面板」，**不碰核心数据流**（pipeline/lux/采集） |
| 2 | AI 面板是**所有页面可显示/隐藏的浮动面板**，能感知当前页面 URL，最好还能获取当前页面数据 | 前端是根布局层全局组件（非独立路由），监听 vue-router + 读 Pinia store 注入上下文 |
| 3 | rs-iot 暴露给 Reasonix 的方式要**评估最优**，结果是暴露**全功能接口**（如同人类工程师用 rs-iot） | MCP 为主 + CLI 为辅，同源多通道（见 §4.3 评估） |
| 4 | Reasonix 启停守护**不让 rs-iot 管理**，rs-iot 只检测其是否启动/正常 | 驱动方式从 ACP stdio（需 rs-iot 持管道）改回 `serve`（独立常驻），rs-iot 做反代 + 探针 |
| 5 | AI agent 角色 = **取代现场采集实施+调试+脚本开发工程师** | `AGENTS.md` 与工具命名用现场工程师语义（点表/报文/排障），非开发者语义 |
| 6 | 现场机器**无 rs-iot 源代码** | workspace_root=运行目录；MCP/CLI 只操作运行时 API；`AGENTS.md` 部署时生成 |

### 1.3 技术约束（源自研究，不可违反）

| 约束 | 来源 | 影响 |
|---|---|---|
| Reasonix 内核在 `internal/`，Go 禁止外部 import | Reasonix `REASONIX.md` + Go 语言规则 | 不能当库嵌入，只能子进程或独立进程 |
| rs-iot 是 Rust，Reasonix 是 Go | 语言异构 | 同进程不可能，只能网络/HTTP 通信 |
| Reasonix 不能作为 MCP server 被调用 | 全仓 grep 无证据 | 反向工具调用只能 rs-iot 做 MCP server |
| rs-iot 现有前端是 rust-embed 静态 SPA | `src/api/frontend.rs` | AI 面板须内嵌，不引入独立前端服务 |
| 建议4 要求 rs-iot 不 spawn Reasonix | 用户需求 | stdio 管道模型不适用，改用 HTTP（serve） |

### 1.4 明确否决的方案（带理由）

| 方案 | 否决理由 |
|---|---|
| **部署 Zacp 当运行时**（`rs-iot→zacp→reasonix` 双层 IPC） | Zacp 核心价值是"多 agent 网关"，单 agent 场景零价值；双 Web 栈（Gin+Vue3 vs axum）并存维护灾难、UI 割裂；多一个故障点 |
| **用 Rig 等 Rust 原生 agent framework** | Rig 是"造 agent 的框架"非成品 agent，要自实现 plan/tool-loop/checkpoint 全套，违背"不自研 agent"定位 |
| **纯 LLM API 自拼** | 用户要的是 agent（带 plan/subagent/工具循环/沙箱），不是裸 API |
| **Codex/Grok 等其他 agent** | Reasonix 已具备 serve + MCP client + plan/subagent/checkpoint/沙箱全套，且研究已透；无切换收益 |
| **ACP stdio 驱动**（曾基于研究推荐，现否决） | 与建议4 冲突——stdio 需 rs-iot 持有 Reasonix 的 stdin/stdout 管道（即 spawn 它），rs-iot 不 spawn 就拿不到管道，除非加 socat 中转层（复杂、违背"少改动"）。改用 serve 独立常驻 + 反代，rs-iot 侧改动骤降 |

---

## 二、架构

### 2.1 进程拓扑（rs-iot 做轻量代理，Reasonix 独立常驻）

```
现场机器（无源码，仅二进制 + 运行时目录）
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│  rs-iot :8790（Rust，唯一对外窗口）           reasonix serve :8787        │
│  ┌──────────────────────────────┐            （独立服务，rs-iot 不管启停 │
│  │ axum REST（现有 /api/v1/*）   │             systemd/Windows service） │
│  │ + /api/v1/agent/* 反代路由(新)│◄──反代────── /submit /events /approve │
│  │ + 健康探针 GET /status (新)   │◄──周期探活── GET /status              │
│  └──────────┬───────────────────┘            ┌──────────────────────┐  │
│             │ MCP stdio spawn（agent 发起）   │ DeepSeek/本地模型    │  │
│             ▼                                 │ plan/subagent/沙箱   │  │
│  ┌──────────────────────┐  薄翻译层           │ 全套 agent 能力      │  │
│  │ rs-iot --mcp-server  │──►调本地 REST──►    │ [[plugins]] 配置指回 │  │
│  │ (新子命令,复用全功能) │   /loopback         │ rs-iot MCP server   │  │
│  └──────────────────────┘                     └──────────────────────┘  │
│  + rs-iot-cli（可选子命令，人机共用，同源）                              │
│                                                                         │
│  前端：所有页面 + 全局浮动 AI 面板（URL/数据感知）                       │
│         ──WS──► /api/v1/agent/* ──反代──► reasonix serve                │
└─────────────────────────────────────────────────────────────────────────┘
```

**关键点**：
- Reasonix **独立常驻**（`reasonix serve --addr 127.0.0.1:8787`），由 OS 服务管理启停，rs-iot 不 spawn 不管它。
- rs-iot 的 `--mcp-server` 子命令是被 Reasonix 经 `[[plugins]]` spawn 的**短命子进程**（每次 Reasonix 启动拉起一个），它只做"把 MCP 工具调用翻译成本地 REST 请求打到 rs-iot 主进程"——纯薄翻译层，不碰核心逻辑，复用现有全功能 REST 接口。
- rs-iot 核心数据流（Source→hub→pipeline→lux）**零改动**。

### 2.2 数据流闭环

| 通道 | 方向 | 协议 | 作用 |
|---|---|---|---|
| **对话通道**（反代） | 前端 → rs-iot → Reasonix | WS（前端↔rs-iot）+ HTTP/SSE（rs-iot↔serve） | 用户对话经 rs-iot 反代到 serve；流式输出原样回传 |
| **工具通道** | Reasonix → rs-iot MCP server → rs-iot REST | MCP stdio（Reasonix↔mcp-server）+ HTTP（mcp-server↔主进程 loopback） | agent 调网关工具，mcp-server 翻译成 REST 调主进程全功能接口 |
| **健康通道** | rs-iot → Reasonix | HTTP `GET /status`（周期探针） | 面板显示 agent 在线/离线/异常 |

**闭环数据流**：
1. 用户在浮动面板输入（带当前页面 URL + 选中设备等上下文）→ rs-iot `/api/v1/agent/submit` → **反代**到 serve `/submit`
2. Reasonix 思考 → 决定调工具 → 经 `[[plugins]]` spawn 的 `rs-iot --mcp-server` → mcp-server 把工具调用翻译成 REST 请求打到 rs-iot 主进程 loopback（复用现有 `/sysstatus`/`/rtdatas`/`/command/send` 等）→ 返回结果
3. Reasonix 流式输出（token + 工具卡片 + 思考 + 计划）→ serve `/events` SSE → rs-iot 反代 → 前端浮动面板渲染
4. 高危操作（写设备/写脚本）→ serve 经权限模式处理（Ask 弹确认 / Auto 放行只读）→ 经反代推前端权限卡片 → 用户批准/拒绝 → 回传 serve

### 2.3 为什么是 serve 反代而非 ACP stdio（决策修订）

研究阶段曾推荐 ACP stdio（多会话/steer/沙箱钩子），但**用户建议4（rs-iot 不管 Reasonix 启停）与 stdio 根本性冲突**——stdio 管道要求 rs-iot 持有 Reasonix 的 stdin/stdout，即必须 spawn 它。改用 serve 后：

| 维度 | ACP stdio（否决） | serve 反代（采用） |
|---|---|---|
| Reasonix 启停 | rs-iot spawn + 持管道 | **OS 服务独立常驻**（满足建议4） |
| rs-iot 侧改动 | 写完整 ACP 客户端（NDJSON RPC + 事件翻译 + 恢复） | **一个反代路由 + 一个探针**（满足建议1 少改动） |
| 多会话 | 天然 | serve `/new`/`/resume` 切换（单进程单活动会话够用） |
| 中途插话 | `session/steer` | serve `/inbox/items` 异步入队（够用） |
| 文件沙箱 | host `fs` 钩子 | serve `[sandbox]` 配置（workspace_root + forbid_read） |
| 丢掉的能力 | — | `session/steer` 实时插话（可接受，工业场景 inbox 异步够用） |

**反代的额外好处**：rs-iot 在反代层能注入鉴权（复用现有 JWT）、记审计日志、做请求改写（如把页面上下文塞进 prompt），是天然的策略层。

---

## 三、Reasonix 集成入口（研究结论摘要）

> 完整证据见各 agent 研究报告；此处只列设计相关结论，带 `file:line` 引证 Reasonix 源码。

### 3.1 Reasonix serve 端点（rs-iot 反代的目标）

Reasonix `serve` 前端把 `control.Controller` 暴露为 HTTP + SSE（`internal/serve/serve.go:1-5,495-532`）。rs-iot 反代要对接的端点：

| serve 端点 | 作用 | rs-iot 反代映射 |
|---|---|---|
| `POST /submit` | 提交一轮对话，输出走 `/events`（`serve.go:708-781`）。繁忙返 409 引导用 `/inbox/items` | `POST /api/v1/agent/submit` |
| `POST /cancel` | 取消当前轮（`serve.go:783-786`） | `POST /api/v1/agent/cancel` |
| `GET /events` | **SSE 流**，`eventwire` NDJSON（token/工具/思考/计划/权限请求，`serve.go:653-702`） | `GET /api/v1/agent/events`（反代 SSE） |
| `POST /approve` | 工具批准/拒绝（`serve.go:788-801`） | `POST /api/v1/agent/approve` |
| `POST /tool-approval-mode` | 切 ask/auto/yolo（`serve.go:1082-1098`） | `POST /api/v1/agent/mode` |
| `POST /inbox/items` | 异步入队（会话繁忙时，`internal/serve/inbox.go`） | `POST /api/v1/agent/inbox` |
| `GET /status` | 综合状态（running/plan/cache/balance，`serve.go:1370-1410`） | rs-iot **健康探针**直连（不经反代） |
| `POST /new` `/resume` `/fork` | 会话生命周期 | `POST /api/v1/agent/session/*` |

**鉴权**：serve 支持 `--auth token|password`（`reasonix.example.toml:267-272`）。rs-iot 反代时在请求头注入 serve token（前端不感知 serve 的存在）。

### 3.2 Reasonix 配置（`reasonix.toml`，部署脚本生成到运行目录）

```toml
default_model = "deepseek"          # 或本地 Ollama provider（数据不出厂，建议6）

[serve]
auth_mode = "token"                 # rs-iot 反代时带 token
token      = "<部署时生成>"          # 或 token-file 避免明文

[[providers]]
name     = "deepseek"
kind     = "openai"
base_url = "https://api.deepseek.com"
models   = ["deepseek-v4-flash", "deepseek-v4-pro"]
api_key_env = "DEEPSEEK_API_KEY"

[[plugins]]                         # rs-iot 暴露的 MCP server，让 Reasonix 调网关工具
name    = "rsiot"
command = "<rs-iot 二进制>"          # 同一个二进制
args    = ["--mcp-server"]           # 子命令模式（薄翻译层）
# MCP server 经 loopback HTTP 调 rs-iot 主进程 :8790 的现有 REST

[sandbox]
workspace_root = "<网关运行目录>"    # 建议6：现场无源码，限定 AI 读写运行目录
allow_write    = ["<运行目录>/scripts"]   # 仅脚本可写
forbid_read    = ["<lux data 目录>", "${HOME}/.ssh"]
bash      = "off"                   # Windows 无 OS 级 bash 沙箱，固定 off
network   = true                    # 允许连模型 API（或本地 Ollama）
```

Provider 配置见 `reasonix.example.toml:62-124`；MCP `[[plugins]]` 见 `:181-192` + 官方样例 `cmd/reasonix-plugin-example/main.go`（NDJSON stdio，350 行，rs-iot `--mcp-server` 照此实现）。

### 3.3 会话与记忆

- **会话存储**：`<session-dir>/<id>.jsonl`（JSONL transcript），serve `--resume` 续跑（`docs/CLI.md:316-318`）。
- **Standing instructions**：`REASONIX.md`/`AGENTS.md` 层级加载——**部署脚本生成 `AGENTS.md` 到运行目录**（建议6 无源码），教 agent 怎么用网关 MCP 工具 + 现场工程师角色定位（建议5）。常驻每轮。
- **Checkpoint/Rewind**：serve 内置文件级快照（write_file/edit_file 执行前），`POST /rewind` 回滚——AI 改 scripts/config 前自动快照，用户可经反代 `/api/v1/agent/rewind` 回滚。

### 3.4 ACP 协议（备选，当前不用）

> 主路径是 serve 反代。若未来需多会话强隔离 / 实时插话 / host fs 沙箱钩子，可切回 ACP stdio（rs-iot 作 host spawn `reasonix --acp`）。ACP 契约留存备查：Reasonix 实现 ACP v1（`docs/ACP.md:9-14`），NDJSON JSON-RPC over stdio，含 `session/new|load|resume|prompt|cancel`、配置轴（collaboration/model/effort/tool_approval）、厂商扩展 `session/steer`（中途插话）/`sessionInbox`（持久队列）/host `fs` 能力钩子。完整证据见研究报告。

---

## 四、rs-iot 侧接口设计

### 4.1 现有接口盘点（覆盖需求 1/2/3 大部分，零改造即可封装为 MCP 工具）

| 需求 | 现有接口 | 证据 |
|---|---|---|
| 查状态 | `GET /sysstatus`（dbsize+实例+ctl/pipeline/hub/mqtt 计数+CPU/mem）、`/sysinfo`、`/inststatus`、`/inst/stats`、`/plugins`、`/metrics` | `src/api/routes_sys.rs:11-63`、`routes_stat.rs`、`routes_metrics/` |
| 读数据 | `GET /rtdatas[?dev_id]` | `src/api/routes_rtdata.rs:20` |
| 读日志/报文 | `GET /logs/recent`、`POST/GET/DELETE /inst/trace`、`GET /events` SSE（log/packet/state） | `routes_logs.rs`、`routes_trace.rs`、`routes_events.rs`、`src/event_bus.rs` |
| 操作网关 | `POST /startinst`、`/stopinst`、`/command/send`、`/inst/set|delete`、`/lua/reload` | `routes_sys.rs:70`、`routes_command.rs`、`routes_inst.rs`、`routes_lua.rs:31` |

### 4.2 新增接口（缺口）—— 遵循"少改动"，仅代理层 + 暴露层 + 面板

| 优先级 | 改动 | 位置 | 说明 | 改动量 |
|---|---|---|---|---|
| P0 | **反代路由组** `/api/v1/agent/*` | `src/api/routes_agent.rs`（新） | axum 反代到 serve :8787（submit/cancel/events/approve/mode/inbox/session/*）；events 用 SSE 流式反代；复用现有 JWT 鉴权 + 审计 | 小（纯转发） |
| P0 | **健康探针** | `src/api/routes_agent.rs` | 周期 `GET serve /status`，`GET /api/v1/agent/health` 返回在线/离线，面板显示 agent 状态 | 小 |
| P0 | **`--mcp-server` 子命令** | `src/main.rs` 分发 + `src/agent/mcp_server.rs`（新） | Reasonix 经 `[[plugins]]` spawn 它；薄翻译层把 MCP 工具调用 → loopback REST 调主进程 :8790 现有接口；NDJSON stdio（参考 `reasonix-plugin-example`） | 中（新子进程，但复用 REST） |
| P0 | **浮动面板前端** | 前端根布局 + `AgentPanel.vue`（新，抽 Zacp 组件） | 全局显示/隐藏 + URL/数据感知（建议2）；对话流/权限卡片/工具卡（抽 Zacp） | 中（前端新增） |
| P0 | `reasonix.toml` + `AGENTS.md` 模板 | `scripts/ai/`（部署生成到运行目录） | 配 serve provider + `[[plugins]]` 指回 rs-iot MCP + 现场工程师角色提示词 | 小 |
| P1 | `POST /api/v1/lua/script/save` | `routes_lua.rs` | **解锁"AI 写脚本并运行"**——当前 `GET /lua/scripts` 只读列表（`routes_lua.rs:55`），无写入口；路径白名单 `scripts/` | 小 |
| P1 | `GET /api/v1/config`（脱敏） | `routes_sys.rs` | 让 AI 读配置（遮蔽 `jwt_secret`/`password`） | 小 |
| P1 | **`rs-iot-cli` 子命令**（可选） | `src/cli.rs`（新） | 人机共用 CLI（建议3 辅通道），同源调 REST；`--json` 结构化输出；现场工程师也可用 | 中（可选，独立于 MCP） |
| P2 | heartbeat 定时巡检 | `src/agent/heartbeat.rs` | 借鉴 Reasonix Desktop 定时引擎；告警自动入 serve inbox 触发 AI 分析 | 中 |

**核心数据流零改动**：上表全部是 axum 路由层 + 子命令 + 前端的新增，不碰 `src/pipeline.rs`/`src/plugin/`/`src/db/` 等核心模块。

### 4.3 暴露方式评估（建议3：暴露全功能接口，如同人类工程师用 rs-iot）

人类工程师用 rs-iot 的方式 = **REST API + Web UI**。AI agent 要"如同工程师一样"，最自然的就是复用同一套 REST。但有三种封装形式，评估如下：

| 维度 | A. 直接 REST（agent 用 bash+curl） | B. MCP server（主推） | C. CLI 子命令（辅推） |
|---|---|---|---|
| Reasonix 原生支持 | 经 bash 工具间接 | **`[[plugins]]` 一行配置原生支持** | 经 bash 工具调用 |
| agent 调用可靠性 | 靠 agent 拼 curl 命令 | **schema 自描述，参数明确，工具级** | 靠 agent 记命令名/参数 |
| 权限粒度 | bash 整体粗粒度 | **`readOnlyHint` 工具级**（查询自动放行/写走 Ask） | bash 整体粗粒度 |
| 输出结构化 | 需 `?format=json` | **天然 JSON** | 需 `--json` flag |
| 人机复用 | 工程师用 REST/UI | agent 专用 | **人 + agent 共用**（建议5 现场工程师工具） |
| 灵活性/可组合 | 一般 | 结构化固定 | **可管道组合**（诊断强） |
| 实现成本 | 零（现有 REST） | 中（mcp-server 子命令） | 中（cli 子命令） |

**结论：B（MCP）为主 + C（CLI）为辅，A 作为底层共用。三者同源（都调 rs-iot REST）。**

- **MCP 主通道**：覆盖日常查询 + 受控写操作。`readOnlyHint` 让"查状态/读数据/读日志"自动放行（agent 体验流畅），"写设备/写脚本"走 Ask 权限卡片（安全）。schema 自描述，agent 不需猜参数。
- **CLI 辅通道**：现场工程师直接用（`rs-iot-cli devices list`、`rs-iot-cli command send ...`），agent 也能经 bash 调用做组合诊断（`rs-iot-cli logs recent | grep ERROR`）。人机共用是关键价值——agent 学会的命令，工程师抄来就能用，反之亦然。
- **REST 底层**：MCP server 和 CLI 都是 REST 的薄封装，复用现有全功能接口，零核心改动。

这正好实现"如同人类工程师使用 rs-iot 一样"——人用 UI/REST/CLI，agent 用 MCP（主）+ CLI（辅），底层同源。

### 4.4 MCP 工具清单（Reasonix 经此调用 rs-iot）

| 工具名 | 类型 | 映射接口 | 备注 |
|---|---|---|---|
| `query_status` | 只读 | `GET /sysstatus` | `readOnlyHint:true`，AI 并行调无需批准 |
| `query_instances` | 只读 | `GET /inststatus` + `/inst/stats` | 同上 |
| `query_rtdata` | 只读 | `GET /rtdatas?dev_id=` | 同上 |
| `get_recent_logs` | 只读 | `GET /logs/recent` | 支持 level/target/limit |
| `get_packet_trace` | 只读 | `POST /inst/trace` 激活 + 读 io_trace | 报文 hex 分析 |
| `read_config` | 只读 | `GET /config`（新） | 服务端脱敏 |
| `start_instance` / `stop_instance` | 写 | `POST /startinst` `/stopinst` | 走 Ask 批准 |
| `send_device_command` | 写 | `POST /command/send` | 走 Ask 批准，高危 |
| `write_and_run_lua_script` | 写 | `POST /lua/script/save`（新）+ `POST /lua/reload` | 路径白名单 `scripts/`，走批准 |
| `set_instance` / `delete_instance` | 写 | `POST /inst/set` `/inst/delete` | 配置采集点，走批准 |

### 4.5 浮动面板的页面感知（建议2）

| 感知维度 | 实现 | 注入方式 |
|---|---|---|
| **当前 URL** | 监听 `vue-router` afterEach | 每次发 prompt 时附"用户正在 /scada 采集页" |
| **当前页面数据** | 读 rs-iot 前端 Pinia store（已选设备 ID、实例列表、活跃告警） | 作为附加上下文："当前选中设备 devA，最近告警：..." |
| **用户操作上下文** | 面板可"采集当前页"按钮，把可见数据快照注入 prompt | 用户主动触发，避免隐私自动抓取 |

这让 agent 像站在工程师身后看同一屏幕的助手，而非割裂工具。agent 因此能说"我看到你在看 devA 的报文，我来分析这帧异常"。

---

## 五、Zacp 借鉴清单（机制，非运行时）

> Zacp 是 ACP host 工程的最佳参考实现。研究产出全程 `file:line` 引证 `G:\AI_Agent\zacp\backend`。
> **只借设计，不部署 Zacp 进程**（理由见 1.3）。

### 5.1 后端机制（P2 增强参考——serve 模式下大多由 Reasonix 自带，rs-iot 按需借鉴）

> 反代模式下，并发/恢复/空闲回收这些 host 机制**主要靠 Reasonix serve 自身能力**（它本就是为长驻服务设计）。
> rs-iot 侧只需健康探针 + 反代。下表是 rs-iot 反代层/面板若需增强时可借鉴的 Zacp 设计（非必须）。

| 借鉴点 | Zacp 源码 | rs-iot 是否需要 |
|---|---|---|
| 3 并发 FIFO 门闩 | `internal/acp/manager/manager.go:196,216-304` | P2：多值班员并发时在反代层加 |
| 会话惰性恢复 | `manager.go:735-814` | serve 自带 `/resume`，rs-iot 反代透传即可 |
| 空闲回收 | `manager.go:496-548` | serve 自带，rs-iot 探针检测后提示面板 |
| cancel 强杀兜底 | `manager.go:1377-1404` | serve 自带 `/cancel` |
| 权限请求 5min 超时 | `internal/ws/bridge.go:42` | serve 自带，反代透传 |
| 事件瘦身 + tool_details 拆分 | `pkg/eventstore/eventstore.go` | P2：若 rs-iot 自存审计日志时借鉴 |
| WS/SSE 协议契约参考 | `internal/ws/protocol.go:6-79` | 反代层事件透传，不重新设计协议 |

### 5.2 前端组件（直接抽 Vue3 组件）

> 决策已定：前端引入 Vue3 + 抽 Zacp 组件。Zacp 前端是独立 Vite SPA（`frontend/`），生产构建同源，组件可抽。

| 组件 | 源码 | 复用性 | 集成方式 |
|---|---|---|---|
| **权限确认卡片** | `frontend/src/components/chat/PermissionModal.vue:1-62` + `stores/session.ts:68-74,957-976` | ⭐⭐⭐⭐ 自包含 | 抽组件 + 类型，POST 提交换 rs-iot 端点（§10.3 已改 SSE+POST） |
| **聊天消息流** | `MessageItem.vue` + `ToolCallCard.vue` + `PlanCard.vue` + `composables/useMessageBlocks.ts` | ⭐⭐⭐⭐ 纯前端 | 依赖 `WsEvent` 类型 + incremark，rs-iot 后端喂数据 |
| **思考流渲染** | `MessageItem.vue:91-113,265-290` | ⭐⭐⭐⭐ | 折叠琥珀面板，流式"思考中…" |
| **滚动跟随** | `composables/useChatScroll.ts:11-63` | ⭐⭐⭐⭐⭐ 纯通用 | 直接抽，防流式渲染跳动 |
| **流式占位转正** | `stores/session.ts:1018-1098` | ⭐⭐⭐⭐ | 防负 id 占位 turn.done 后转正跳变 |
| **类型契约** | `types/ws.ts:9-83` | — | 前后端协议圣经，照此喂数据 |
| **主题 token** | `styles/main.css:18-50` + `config/theme.ts` | ⭐⭐⭐⭐ | 语义色 CSS 变量，rs-iot 前端换肤零成本 |
| **文件编辑器**（可选） | `components/files/FileEditorDrawer.vue:1-591` | ⭐⭐⭐ | CodeMirror toml/shell 高亮，编辑 config.toml/scripts |

**不抽的**（zacp 专属，与 rs-iot 产品形态冲突）：
- `session.ts`（1450 行，深度绑定 ACP↔DB id 模型/草稿/多 agent 切换）——rs-iot 单会话或简化多会话自写。
- `GitPanel`——rs-iot 无 git（除非后续做配置版本化）。
- `agentManage`/多 agent 目录——单 agent 不需要。
- 整套工作区/项目模型——rs-iot 以设备/实例为中心，不是以代码项目为中心。

---

## 六、安全边界

| 风险 | 对策 | 依据 |
|---|---|---|
| AI 误操作设备（写寄存器/启停） | 写类 MCP 工具默认 `Ask` 模式；声明 `readOnlyHint` 的查询自动放行；Reasonix Episode 预算（同操作 3 次失败停） | Reasonix `docs/TOOL_APPROVAL_MODES.md` |
| AI 读到凭据 | `read_config` 工具服务端脱敏（遮 `jwt_secret`/`password`/`mqtt.password`）；Reasonix `[sandbox] forbid_read` 隔离 lux data 目录 | `reasonix.example.toml:146-151` |
| AI 写脚本越权 | `lua/script/save` 路径白名单（仅 `scripts/` 子树），拒绝 `..`/绝对路径；参考 Zacp `resolveInWorkspace` 边界校验 | `zacp backend/internal/service/files.go:117-156` |
| **AI 文件系统逃逸**（重点） | Reasonix `[sandbox] workspace_root` 限定运行目录 + `allow_write` 仅 scripts + `forbid_read` 隔离 data；**建议6 无源码**进一步降低风险（机器上本就只有运行时，无源码可泄露） | `reasonix.example.toml:146-151` |
| serve token 泄露 | serve bind 127.0.0.1（同机）+ `--auth token`；rs-iot 反代注入 token，前端不感知；rs-iot 自身 JWT 守对外入口 | `reasonix.example.toml:267-272` |
| MCP server 越权 | `--mcp-server` 子命令只调 loopback REST，复用 rs-iot 现有鉴权；无独立权限提升面 | — |
| 外部模型数据外泄 | 配本地 Ollama provider（`reasonix.toml` 任意 OpenAI 兼容端点），数据不出厂 | `reasonix.example.toml:98-106` |
| Reasonix 崩溃/挂死 | rs-iot 健康探针检测离线，面板提示；Reasonix 由 OS 服务自愈重启；serve 会话 JSONL 持久可 resume | — |

---

## 七、实施路线图

| 阶段 | 内容 | 验证标准 | 周期 |
|---|---|---|---|
| **P0 最小闭环** | ①axum 反代路由 `/api/v1/agent/*`（submit/events/approve 透传 serve）+ 健康探针；②`reasonix serve` 部署脚本 + `reasonix.toml`/`AGENTS.md` 模板；③最简浮动面板（能对话 + 看流式） | 浮动面板能和 Reasonix 对话，看到流式输出；探针能显示 agent 在线/离线 | 3-5 天 |
| **P1 能力闭环** | ①`--mcp-server` 子命令（5 工具：query_status/rtdatas/send_command/start_stop/get_logs）+ Reasonix `[[plugins]]` 配置；②抽 Zacp 权限卡片 + 工具卡 + 思考流进浮动面板；③`lua/script/save` + config 读取；④浮动面板 URL/数据感知 | 用户在面板让 AI"查设备 X 当前值""把寄存器写成 12.5"走通（含权限确认）；AI 写 lua 脚本热加载跑通；面板感知当前页 | 1-2 周 |
| **P2 工业增强** | ①反代层并发门闩（多值班员）；②MCP 工具补全（trace/set_instance/delete_instance）；③heartbeat 定时巡检（告警自动入 serve inbox 触发分析）；④可选 `rs-iot-cli` 人机共用 | 多值班员并发稳定；"设备 Failed 自动触发 AI 分析"半自动跑通 | 1 周 |
| **P3 按需** | checkpoint/rewind UI、CodeMirror 文件编辑器（编辑 config.toml/scripts）、会话历史持久化到 rs-iot 侧 | 长会话可回溯；配置/脚本 Web 编辑 | 按需 |

**关键**：P0 只动 axum 路由 + 前端 + 部署脚本，**核心数据流（pipeline/lux/采集）全程零改动**——符合建议1。

---

## 八、风险与未覆盖点

1. **DeepSeek API 依赖**：Reasonix 默认 DeepSeek（联网+key）。生产可换本地 Ollama（`reasonix.toml` 任意 OpenAI 兼容端点），质量下降——P0 落地需实测本地模型效果（建议6 现场可能无外网）。
2. **serve 单进程单活动会话**：serve 一个进程同时只一个活动会话，多会话靠 `/new`/`/resume` 切换。多值班员并发若各自要独立会话，P2 在反代层做会话路由 + 并发门闩，或起多 serve 实例（不同端口）。
3. **SSE 反代的连接管理**：axum 反代 serve 的 SSE `/events` 要正确处理长连接 + 断线重连 + keepalive；参考 serve 自身 15s ping（`serve.go:689-697`）。
4. **MCP server 进程模型**：每个 Reasonix 实例经 `[[plugins]]` spawn 一个 `rs-iot --mcp-server` 子进程——该子命令须**无状态可多实例**（只调 loopback REST，无共享可变状态），否则多 Reasonix 实例并发会冲突。
5. **Reasonix serve 是年轻功能**：serve 端点/事件格式可能随版本演进；rs-iot 反代层对未知事件类型要透传不崩溃。
6. **页面数据感知的隐私**（建议2）：自动把当前页数据注入 prompt 可能泄露敏感信息给外部模型——默认只注 URL + 用户主动"采集当前页"，自动注入数据走本地 Ollama 时才开启。
7. **未实测**：反代 SSE 的实际延迟、MCP 工具调用链路 RTT、Reasonix serve 长稳、本地 Ollama 模型对工业场景的编码/分析效果——P0/P1 落地验证。

---

## 九、研究索引（完整证据链）

- **Reasonix serve（主路径）**：端点清单（`internal/serve/serve.go:495-532`）、SSE `/events`（`serve.go:653-702`）、submit/inbox/approve（`serve.go:708-781`/`inbox.go`/`serve.go:788-801`）、status（`serve.go:1370-1410`）、配置（`reasonix.example.toml:267-272`）、supervisor 工件（`internal/cli/cli.go:814-816`）。
- **Reasonix ACP（备选，§3.4）**：`docs/ACP.md`（协议）、`internal/acp/`（实现）、厂商扩展 steer/inbox/fs 钩子。
- **Reasonix MCP client**：`reasonix.example.toml:181-192` + 官方样例 `cmd/reasonix-plugin-example/main.go`（NDJSON stdio，350 行——rs-iot `--mcp-server` 照此实现）。
- **Reasonix 权限模型**：Ask/Auto/Yolo + Episode 预算（`docs/TOOL_APPROVAL_MODES.md`）、强制 fresh approval（config_write/sandbox_escape）。
- **Reasonix Desktop 可借鉴功能**：后台运行时（`desktop/background_runtime.go:297`）、heartbeat 定时引擎（`desktop/heartbeat.go:39`）、Inbox+steer（`desktop/inbox_app.go`）、崩溃恢复（`desktop/crash_pending.go:46`）、devinfo 注入（`desktop/devinfo.go:12`）。
- **Zacp 后端架构**（参考，非运行时）：ACP 桥（`backend/internal/acp/client/client.go`）、manager 并发/恢复/回收（`backend/internal/acp/manager/manager.go`）、WS 协议（`backend/internal/ws/protocol.go`）、持久化瘦身（`pkg/eventstore/`）、文件沙箱边界校验（`backend/internal/service/files.go:117-156`，rs-iot `lua/script/save` 借鉴）。
- **Zacp 前端可复用组件**：权限卡片（`PermissionModal.vue`）、消息流（`MessageItem.vue` + `useMessageBlocks.ts`）、滚动（`useChatScroll.ts`）、主题 token（`styles/main.css`）、文件编辑器（`FileEditorDrawer.vue`）。
- **rs-iot 现状**：见本文 §4.1 接口盘点 + `CLAUDE.md` 项目地图。

---

## 附录：决策演进（为何从 ACP stdio 转向 serve 反代）

1. **研究阶段**（三路深研 Reasonix/Zacp 后端/Zacp 前端）：基于"多会话隔离 + steer 插话 + fs 沙箱钩子 + Zacp host 工程可借鉴"，推荐 ACP stdio 直连。
2. **用户六条建议**：建议4（rs-iot 不管 Reasonix 启停）与 stdio 管道模型根本冲突——stdio 要求 rs-iot spawn 持管道。建议1（少改动）进一步否决"写完整 ACP host"。
3. **转向 serve 反代**：Reasonix 独立常驻 serve，rs-iot 只做反代 + 探针，改动骤降（满足建议1+4）；损失 steer/fs 钩子可接受（inbox 异步够用 + sandbox 配置 + 无源码降风险）。Zacp 价值从"host 工程参考"降为"前端组件来源 + P2 增强参考"。

---

## 十、生产级复核（2026-08-11）+ P0 任务分解

> 本章节是对前九章设计的**生产落地复核**。四个关键点全部用 rs-iot + reasonix + dbx 真实代码验证，
> 找出 2 处关键修正、3 个 P0 硬坑（均有规避方案）。**P0 以 MCP server 先行**（用户决策：低风险有范本）。

### 10.1 复核点1：前端栈（结论修正——前端是快路径，非风险）

**事实**（rs-iot 真实代码）：前端是**纯手写静态 HTML**——`src/api/static/` 下 `index.html`（74 行）+ `scada.html`（1549 行单文件），原生 CSS + 原生 JS + fetch/EventSource，**无框架无构建链**。rust-embed 嵌入（`src/api/frontend.rs:8` `#[folder="src/api/static/"]`）。

**用户决策（2026-08-11）**：前端可大胆重写（AI Agent 重构前端速度极快、质量极高），**前端不是难点、不计入风险**。

**对方案的含义**：§4.2 的"浮动面板前端"和 §5.2 的"引入 Vue3 + 抽 Zacp 组件"**从风险/工作量评估中移除**——前端可整体重构成 Vue3（含浮动 AI 面板 + 抽 Zacp 组件），或保持纯 HTML 加浮动层，两种都可行，由 AI Agent 辅助快速完成。**唯一已验证可复用的技术**：scada.html:939 已用 `EventSource('/api/v1/events?types=log,packet,state')` 消费 SSE，浮动 AI 面板复用同一 EventSource + fetch 模式即可。

### 10.2 复核点2：MCP server 移植（结论——dbx 是成熟范本，低风险）

**事实**（dbx-mcp 真实代码，`G:\AI_Agent\dbx\crates\dbx-mcp`）：

- **MCP 库选型**：用官方 `rmcp = "2.2.0"`（`Cargo.toml:20`），crates.io 真实发布，dbx 实际在用。**不手写 JSON-RPC**。
- **工具声明**：`#[tool_router]` + `#[tool]` + `#[tool_handler]` 三宏（`server.rs:241,921`），`schemars::JsonSchema` derive 自动生成 inputSchema（**零手写 schema**）。
- **入口**：`main.rs` 21 行（`serve(rmcp::transport::stdio())`），照抄即模板。
- **依赖零冲突**：rmcp 依赖（tokio/serde/thiserror 2/schemars 1.x）全部与 rs-iot 已有版本兼容（`Cargo.lock` 核对），唯一新增 schemars 1.x。
- **rs-iot 现有 reqwest 0.12.28（rustls+json）与 dbx WebBackend 完全一致**。

**rs-iot 移植要点**（agent 给了可编译级骨架）：
- **crate 结构**：`crates/rsiot-mcp/`（独立 crate，对标 dbx `crates/dbx-mcp`），`main.rs`(20行) + `server.rs`(450-550行) + `backend.rs`(250-350行) + `lib.rs`(8行)，**砍掉** dbx 的 session.rs/transport.rs/LocalBackend/sql_risk.rs/mongo/redis（rs-iot 用不上）。
- **鉴权改写**（dbx cookie → rs-iot JWT）：WebBackend 用预签发 JWT token（`Authorization: Bearer`），不需要 dbx 的 cookie session 流程，**比 dbx 版更简单**。
- **权限三档必须 rs-iot 自己写**（不能抄 dbx sql_risk）：但 rs-iot 风险绑定**工具名**（`rsiot_write_tag` 本身就是 write），不需 dbx 那样 parse SQL，**6 层校验砍到 3 层**（read_only policy / 操作类型 risk / 生产环境保护）。
- **不用** rmcp 的 readOnlyHint 静态注解（dbx 教训：运行时分类更灵活）。

**真实工作量**：**约 900-1100 行，11-14 人天**（含主进程端点适配）。三个隐藏复杂度：schemars 版本对齐（用 `rmcp::schemars` re-export 避免双版本）、写操作 risk 校验（rs-iot 独有，~150-200 行）、主进程 REST 端点可能要补。

### 10.3 复核点3：serve 反代（结论——可行但有 3 个 P0 硬坑）

**事实**（reasonix serve + rs-iot axum 真实代码）：

**关键修正1：放弃 WS，全 SSE+POST**。rs-iot **当前零 WS 支持**（`Cargo.toml:56` 只有 `macros` feature，源码无 tungstenite 用法）。reasonix serve 本身就是 **SSE 下行（`/events`）+ POST 上行（`/submit`/`/approve`/`/answer`/`/inbox/items`）**，已验证能跑交互式审批。rs-iot 做**协议透传**（`serve.go:497-531`），前端用 EventSource + fetch POST，比"WS↔SSE 桥接"简单一个数量级，且 SSE 自动重连 WS 没有。

**关键修正2：serve 鉴权用伪造 Cookie 非 Bearer**。serve token 模式**只认 cookie `reasonix_token` 或 `?token=` query，不认 `Authorization: Bearer`**（`auth.go:304-340`）。rs-iot 反代出站请求必须加 `Cookie: reasonix_token=<token>`。

**3 个 P0 硬坑（必须正面解决，均有规避方案）**：

| 坑 | 风险 | 规避方案 | 证据 |
|---|---|---|---|
| **① serve 子进程生命周期** | 子进程崩溃/僵死 → 反代全 502 | rs-iot 用 `tokio::process::Command` 起 serve（`--port-file/--token-file/--pid-file`）+ 定期 `GET /status` 健康检查 + pid 监控 + 自动重启 | `cli.go:814-816`、`serve.go:1370` |
| **② SSE 帧边界切割** | reqwest `bytes_stream()` 按 TCP 包分块，裸透传前端收到半个 JSON | 反代层累积 buffer 到 `\n\n` 才 yield 一个 Event（用 `tokio_util::codec` 或手写状态机）；丢弃 serve 的 `: ping`/`: connected` 注释帧，靠 axum `KeepAlive(15s)` 自保活 | `serve.go:681-700`、`Cargo.toml:92`（reqwest 缺 stream feature） |
| **③ 鉴权兼容** | serve 不认 Bearer，反代 401 | 出站请求统一加 `Cookie: reasonix_token=<token>`；token 从 `--token-file` 读，存 AppState | `auth.go:304-340` |

**其他坑（P1/P2）**：serve 单进程单活动 session，并发 prompt 第二个 409 → 透传 `/inbox/items` 排队（`serve.go:766`、`inbox.go`，**不自己造队列**）；所有 POST 必须 `Content-Type: application/json` 否则 415（`serve.go:549-562` csrfGuard）；serve SSE 无 `event:` 头（kind 在 JSON 内，`eventwire/wire.go:16`），前端用 `onmessage` 解析；reqwest 需加 `stream` feature。

**真实工作量**：反代层**约 400 行，5-7 人天**（起 serve 60 行 + SSE 反代 80 行 + POST 反代 8×30 行 + 路由/配置/测试）。比"从零写 WS↔SSE 桥接"少 3-5 倍。

### 10.4 复核点4：MCP spawn 契约（结论——清晰，低风险）

rs-iot 的 `rsiot-mcp` 二进制作为 reasonix `[[plugins]]` 的 stdio 子进程被 spawn：reasonix 读 `reasonix.toml` 的 `[[plugins]] command="rsiot-mcp" args=[]`，spawn 后经 stdio NDJSON 通信，rsiot-mcp 内部用 reqwest 调 rs-iot 主进程 loopback REST（:8790，JWT 鉴权）。**rsiot-mcp 无状态可多实例**（多个 Reasonix 各 spawn 一个不冲突）。无 spawn 竞态（reasonix 管理子进程生命周期）。

### 10.5 P0 任务分解（MCP server 先行）

> 用户决策（2026-08-11）：P0 从 **MCP server 先行**切入。两个 crate（rsiot-mcp + 反代层）相互独立，MCP 先行可独立验证"agent 调网关工具"走通，不依赖反代层。

**P0-A：rsiot-mcp crate（11-14 人天，低风险有范本）**

| 步骤 | 内容 | 验证标准 | 估时 |
|---|---|---|---|
| A1 | 建 `crates/rsiot-mcp/` workspace 成员，Cargo.toml（rmcp 2.2 transport-io + reqwest 0.12 rustls+json + schemars 用 rmcp re-export） | `cargo build -p rsiot-mcp` 通过，依赖零冲突 | 0.5d |
| A2 | `main.rs`(20行) + `lib.rs`(8行)，照抄 dbx 砍掉 legacy fallback | 二进制能起，stdio transport 握手 | 0.3d |
| A3 | `server.rs`：先抄 1 个工具 `rsiot_list_devices`（`#[tool]`+`Parameters<T>`+`ServerHandler`）跑通宏链路，再扩到 5-8 个工具 | rmcp 客户端 `list_tools`/`call_tool` 成功 | 2.5d |
| A4 | 写操作 risk 三档校验（rs-iot 独有，~150-200 行，工具名→risk 映射 + 生产环境保护） | read_only 挡写、关键设备禁写单测过 | 1.5d |
| A5 | `backend.rs`：`RsiotBackend` trait + WebBackend JWT 版（预签发 token，Authorization Bearer + `x-rsiot-mcp-request` 头 + 401 重试） | FakeBackend 单测 + 真 REST 调通 | 2d |
| A6 | 主进程 REST 端点适配（盘点现有端点，补 `rsiot_list_devices`/`read_tags`/`write_tags` 等缺的） | MCP 工具调主进程端到端通 | 1.5-3d |
| A7 | `tests/protocol.rs`：tokio::io::duplex + rmcp client 端到端测 3-5 个工具 | 集成测试绿 | 1d |
| A8 | reasonix.toml 配 `[[plugins]] rsiot` + 端到端联调（reasonix spawn rsiot-mcp，agent 调工具） | agent 经 MCP 读到设备数据 | 1.5-2d |

**P0-B：反代层（5-7 人天，中风险有 3 个 P0 坑）**——MCP 走通后启动

| 步骤 | 内容 | 验证标准 | 估时 |
|---|---|---|---|
| B1 | Cargo.toml 加 reqwest `stream` feature + AppState 加 `serve_base/serve_token/serve_client` | 编译通过 | 0.3d |
| B2 | lib.rs 起 reasonix serve 子进程（`--addr 127.0.0.1:0 --auth token --port-file --token-file --pid-file`）+ 读文件 + 健康检查 + 自动重启 | serve 起停可控，AppState 拿到 port/token | 1.5d |
| B3 | POST 反代（submit/cancel/approve/answer/inbox/items）：强制 Content-Type+Cookie，透传状态码 | 单 POST 端到端通 | 1.5d |
| B4 | SSE 反代（`/events`）：bytes_stream + **帧边界切割**（`\n\n` 累积）+ 丢弃 `:` 注释帧 + axum KeepAlive | 前端 EventSource 收到完整 JSON 帧 | 2d |
| B5 | 路由注册 `build_router`（**不**加 PUBLIC_PATHS，JWT 自动保护）+ 错误透传（409/415/413） | JWT 鉴权 + 409 透传正确 | 0.5d |
| B6 | 测试：mock serve + 帧边界用例（帧切两半）+ 长流 keep_alive + 子进程重启 | 测试绿 | 1d |

**P0-C：前端浮动面板（AI 重构快路径，不计入硬工期）**

P0-A/B 后端跑通，前端用 AI Agent 重构（引入 Vue3 + 抽 Zacp 聊天/权限组件，或纯 HTML 浮动层），EventSource + fetch POST 对接 `/api/v1/agent/*` 反代路由。这是快路径，与后端解耦。

### 10.6 验证标准（P0 整体闭环）

- **MCP 闭环**：reasonix agent 能经 rsiot-mcp 读到 rs-iot 设备数据（`rsiot_list_devices`/`read_tags`），写操作触发权限询问
- **反代闭环**：前端浮动面板能和 reasonix 对话，看到流式输出（SSE），权限确认走 POST
- **零核心破坏**：`cargo test --all-targets --all-features` 全绿（现有测试不受影响），pipeline/lux/采集零改动
- **3 个 P0 坑均解决**：serve 崩溃自动恢复、SSE 帧边界正确、cookie 鉴权生效

### 10.7 复核后的风险评估

| 维度 | 复核前评估 | 复核后评估 | 变化 |
|---|---|---|---|
| MCP server | 中风险（"要手写 NDJSON"） | **低风险**（rmcp 范本成熟，依赖零冲突） | ↓ 大幅降低，印证"dbx rmcp 是非常大利好" |
| serve 反代 | 低风险（"反代透传简单"） | **中风险**（3 个 P0 坑，但均有规避方案） | ↑ 风险被如实暴露，但可控 |
| 前端 | 中工作量（隐含 Vue 引入难） | **快路径**（AI 重构，不计硬工期） | ↓ 移出风险盘 |
| 整体落地性 | "大活，工作量不小" | **可落地、低风险**（两个独立 crate 都有范本/规避） | 确认可行 |

**结论**：v1 方案经生产级复核，**可落地、低风险**。dbx rmcp 范本把 MCP server 从"未知风险"降为"翻译工作"；serve 反代的 3 个 P0 坑全部识别且有规避方案；前端是 AI 重构快路径。总工期约 **16-21 人天（后端，不含前端 AI 重构）**，分 P0-A（MCP）→ P0-B（反代）→ P0-C（前端）三段，每段可独立验证。

