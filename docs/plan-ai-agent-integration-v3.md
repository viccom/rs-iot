# AI Agent 集成设计 v3：独立 API Gateway 程序

> 本文是 [`plan-ai-agent-integration.md`](./plan-ai-agent-integration.md)（v1，反代加进 rs-iot）的**演进方案**。
> v1/v2 保留不删。v3 把"少破坏 rs-iot"推到极致——**rs-iot 真正零改动**，新写一个独立 Rust API Gateway 程序，
> 统一反代 rs-iot + reasonix serve（+ 未来其他后端），前端只对接 gateway 一个入口。

## 一、核心思路

rs-iot 是**工业协议网关**（南向设备协议 modbus/opcua/s7 ↔ 北向 lux 数据）。往它里面塞 HTTP 反代、AI 会话管理，
是职责混淆。v3 把"前端入口 + HTTP 聚合 + AI 编排"这一层抽出来，交给一个独立的 **API Gateway 程序**：

- **rs-iot**：回归纯粹——工业数采核心，**零 AI 集成改动**。
- **API Gateway**（新 Rust 程序）：前端唯一入口。无状态纯反代——转发请求到 rs-iot / reasonix serve / 未来其他后端，
  统一鉴权，透传 SSE。
- **rsiot-mcp**（同 v1）：reasonix 的 `[[plugins]]` 子进程，内部调 rs-iot REST 暴露网关工具给 agent。
- **reasonix serve**：OS 服务独立常驻（systemd/Windows service），gateway 不 spawn 不管它启停。

**三个关键决策（用户已定，全部选最简最低风险）**：
1. 采纳 gateway 方案（rs-iot 零改动）
2. reasonix serve 归 **OS 服务管**，gateway 只反代 + 健康检查显示状态（gateway 无状态、无进程管理职责）
3. SSE **透传两条独立流**（gateway 不聚合，前端开两个 EventSource）

这让 gateway 形态极简：**纯无状态 HTTP 反代 + 三层鉴权链**，无进程守护、无 SSE 聚合、无业务逻辑——可靠性极高。

## 二、架构拓扑

```
现场机器（无源码，仅二进制 + 运行时目录）
┌─────────────────────────────────────────────────────────────────────────────┐
│                                                                             │
│  浏览器前端 ──HTTP/SSE──►  API Gateway :8080（新 Rust 程序，无状态纯反代）   │
│  (EventSource×2 + fetch POST)     │                                         │
│                                   │ 三层鉴权链（见 §4）                     │
│                                   ├─反代─► rs-iot :8790（工业数采，零改动） │
│                                   │         └ REST /api/v1/* + SSE /events  │
│                                   │              （log/packet/state）       │
│                                   ├─反代─► reasonix serve :8787（OS 服务）  │
│                                   │         └ SSE /events + POST /submit... │
│                                   │              （agent 流 + 对话/审批）   │
│                                   │              ↑ MCP stdio spawn           │
│                                   │              ┌────────────────────┐     │
│                                   │              │ rsiot-mcp 子进程   │     │
│                                   │              │ (调 rs-iot:8790)   │     │
│                                   │              └────────────────────┘     │
│                                   └─未来─► 其他后端（grafana/时序库/...）   │
│                                                                             │
│  reasonix serve 由 systemd/Windows service 管，gateway 不 spawn 不管启停    │
└─────────────────────────────────────────────────────────────────────────────┘
```

**职责分层（干净）**：
- **rs-iot**：工业协议网关（南向 ↔ lux），零 AI 改动
- **API Gateway**：HTTP BFF（前端入口 + 反代 + 鉴权），无状态
- **reasonix**：AI agent（独立常驻，OS 管生命周期）
- **rsiot-mcp**：agent ↔ rs-iot 工具桥（reasonix 子进程）

## 三、前端入口与路由

前端**只对接 gateway :8080**，不直接访问 rs-iot:8790 或 serve:8787。gateway 路由分三类：

### 3.1 rs-iot 透传（`/api/rsiot/*` → rs-iot:8790）
把 rs-iot 现有全功能 REST + SSE 透传。前端所有工业操作经此。
- `GET /api/rsiot/api/v1/sysstatus` `GET /api/rsiot/api/v1/rtdatas` `POST /api/rsiot/api/v1/command/send` ... → rs-iot `/api/v1/*`（gateway `nest_service("/api/rsiot")` 剥前缀后剩 `/api/v1/...`）
- `GET /api/rsiot/api/v1/events`（SSE）→ rs-iot `/api/v1/events`（log/packet/state 三类）

### 3.2 reasonix 透传（`/api/agent/*` → serve:8787）
透传 reasonix serve 的对话/审批/inbox，gateway 注入 serve cookie 鉴权（见 §4）。
- `POST /api/agent/submit` `POST /api/agent/approve` `POST /api/agent/cancel` `POST /api/agent/answer` `POST /api/agent/inbox/items` ... → serve 对应端点
- `GET /api/agent/events`（SSE）→ serve `/events`（agent 流：text/reasoning/tool_call/plan/approval）
- `GET /api/agent/status` → serve `/status`（gateway 健康探针用 + 前端显示 agent 在线状态）

### 3.3 gateway 自有（`/api/gw/*`）
gateway 自己的少量端点：
- `POST /api/gw/login` / `GET /api/gw/auth/status`：gateway 自有鉴权（见 §4）
- `GET /api/gw/health`：gateway 汇总健康（rs-iot 在线？serve 在线？MCP 连接？）

### 3.4 SSE 两条独立流（决策3，不聚合）
前端开**两个 EventSource**，各订阅一条：
- `EventSource('/api/rsiot/events')` — rs-iot 实时数据/日志/报文/状态
- `EventSource('/api/agent/events')` — reasonix agent 对话流

gateway 对每条独立透传，不做聚合（最简）。前端按事件来源分发渲染。

## 四、三层鉴权链（gateway 方案的核心复杂度）

```
前端 ──(1) gateway 凭证──► gateway ──(2) rs-iot JWT──► rs-iot :8790
                                   └─(3) serve cookie──► serve :8787
```

**层1 前端↔gateway**：gateway 自有鉴权。最简方案——复用 rs-iot 的 JWT 机制（gateway 与 rs-iot 共享 jwt_secret，前端登录 gateway 拿的 JWT 也能直接调 rs-iot）。或 gateway 独立 session。**推荐前者**：前端一套 JWT 通吃，gateway 转发时把 JWT 透传给 rs-iot（层2 自动满足）。

**层2 gateway↔rs-iot**：透传前端 JWT（`Authorization: Bearer <jwt>`）。rs-iot 现有 `auth_middleware`（`src/api/auth.rs:49-92`）自动验证。**零额外代码**——gateway 转发时保留 Authorization 头即可。

**层3 gateway↔serve**：**伪造 Cookie**（v1 复核已证 serve 只认 cookie `reasonix_token`，不认 Bearer，`auth.go:304-340`）。gateway 持有 serve token（从 serve 启动时 `--token-file` 读，或配置文件指定），转发时加 `Cookie: reasonix_token=<token>`。

**简化关键**：层1 复用 rs-iot JWT → 层2 自动满足 → gateway 实际只需"验 JWT + 注入 serve cookie"两件事。比"gateway 自建完整鉴权体系"简单得多。

## 五、rs-iot 改动（真正零改动确认）

v3 下 rs-iot 的改动清单——**全部为零**（对比 v1 的反代路由 + AppState 字段）：

| v1 改动 | v3 是否还需要 |
|---|---|
| axum 反代路由 `/api/v1/agent/*` | ❌ 移到 gateway |
| AppState 加 `serve_base/serve_token/serve_client` | ❌ 移到 gateway |
| lib.rs 起 serve 子进程 | ❌ serve 归 OS 服务 |
| reqwest 加 stream feature | ❌ 移到 gateway |
| `lua/script/save` + config 脱敏端点 | ✅ 已在 rs-iot 实现（`POST /api/v1/lua/script/save` + `GET /api/v1/config`，gateway 只透传） |

**rs-iot 已补的两个端点**：`POST /api/v1/lua/script/save`（AI 写脚本，路径白名单 `scripts/<name>`）+ `GET /api/v1/config`（脱敏配置，jwt_secret/mqtt 凭据遮蔽）。这两个是**rs-iot 自身能力的补全**（属于工业网关职责），不是 AI 集成专属——gateway 只是透传。已在本分支实现（见 `src/api/routes_lua.rs` + `routes_sys.rs`）。

**核心数据流（pipeline/lux/采集）零改动**——完全符合"少破坏"原则的极致。

## 六、MCP server（同 v1，不变）

rsiot-mcp crate 完全沿用 v1 设计（照抄 dbx-mcp 范本）：
- reasonix `[[plugins]]` spawn `rsiot-mcp` 子进程
- rsiot-mcp 内部用 reqwest 调 **rs-iot :8790** REST（**直接调 rs-iot，不经 gateway**——agent 工具调用不走前端入口）
- rmcp 2.2 + `#[tool]` 宏 + JWT WebBackend
- 工具：`rsiot_list_devices`/`read_tags`/`write_tags`/`get_metrics`... + 写操作 risk 三档

**v3 与 v1 的 MCP 部分 100% 相同**——这是好消息，P0-A 投资完全保留。

## 七、P0 坑（从 v1 继承，搬到 gateway，全都有规避）

v1 复核的 3 个 P0 坑在 v3 下搬到 gateway 处理，规避方案不变：

| 坑 | v3 位置 | 规避方案 |
|---|---|---|
| **① serve 生命周期** | **OS 服务管（决策2）**，gateway 只健康检查显示状态 | systemd/Windows service 自动重启；gateway `GET /api/agent/status` 探测，离线时前端提示。**P0 坑①从代码职责降为运维职责，最稳** |
| **② SSE 帧边界切割** | gateway **不做帧切割，裸字节流透传** | `Body::from_stream(reqwest.bytes_stream())` 原样吐字节，前端 EventSource 自解 `\n\n`。**不用** axum `Sse`/`Event`（`Event::data` 按 `\n` 拆行插 `data:` 前缀，破坏 serve 无 `event:` 头的裸 NDJSON）。两条上游自带心跳（rs-iot KeepAlive + serve `: ping`），gateway 不自加 KeepAlive。详见 `plan-gateway-tech-selection.md §四悬念2` + gateway `proxy.rs` |
| **③ 鉴权兼容** | gateway 注入 serve cookie | gateway 转发 serve 请求时加 `Cookie: reasonix_token=<token>` |

**额外坑（gateway 特有）**：
- **gateway 是新单点**：gateway 挂=前端全挂。但 gateway 逻辑极简（纯反代+鉴权，无状态无业务），可靠性容易保证。OS 服务管理 gateway 本身。
- **三层鉴权链断点**：任一层凭证过期/失效，整链断。需明确错误返回（401 时标注哪层失败）。

## 八、Gateway 的技术选型（待 P0 调研）

gateway 是独立 Rust 程序，技术选型三选一（P0 启动前建议派 agent 调研）：

| 选项 | 优势 | 劣势 |
|---|---|---|
| **Pingora**（Cloudflare Rust 网关框架） | 生产级，反代+LB+健康检查内置 | 学习曲线，可能过重 |
| **axum + tower 手写**（复用 rs-iot 已验证栈） | 最定制，无新框架风险 | 反代/健康检查/SSE 帧边界全手写 |
| **axum-reverse-proxy 等轻量库** | 反代现成 | SSE 帧边界/鉴权注入可能要自己补 |

**倾向**：axum + tower 手写——gateway 逻辑足够简单（纯反代+鉴权+两条 SSE），手写可控性最高，且复用 rs-iot 已验证的 axum 0.8 栈。Pingora 留作"若手写发现反代/健康检查复杂度超预期"的退路。

## 九、P0 任务分解（gateway + MCP 并行）

> 两个组件（gateway + rsiot-mcp）相互独立，可并行。MCP 同 v1 P0-A，gateway 是 v3 新增。

**P0-A：rsiot-mcp crate（同 v1，11-14 人天，低风险）**——见 v1 §10.5，此处不重复。

**P0-B：API Gateway 程序（v3 新增，估 8-12 人天）**

| 步骤 | 内容 | 验证标准 | 估时 |
|---|---|---|---|
| B1 | 调研技术选型（Pingora vs axum 手写 vs 轻量库），定方案 + 建 gateway 工程 Cargo.toml | 编译通过，技术选型有据 | 1d |
| B2 | 鉴权层：复用 rs-iot JWT（共享 jwt_secret）+ gateway 登录端点 + JWT 验证中间件 | 前端登录 gateway 拿 JWT | 1.5d |
| B3 | rs-iot 透传路由 `/api/rsiot/*`：REST 透传（Authorization 头保留）+ SSE `/events` 透传（帧边界+KeepAlive） | 前端经 gateway 读到 rs-iot 数据/事件 | 2d |
| B4 | serve 透传路由 `/api/agent/*`：POST 透传（强制 Content-Type + 注入 serve cookie）+ SSE `/events` 透传（帧边界，无 event: 头处理）+ 健康检查 `/status` | 前端经 gateway 与 reasonix 对话 + 见流式输出 | 2.5d |
| B5 | gateway 自有端点：`/api/gw/login` `/auth/status` `/health`（汇总 rs-iot+serve+MCP 健康） | 健康聚合正确 | 1d |
| B6 | 配置（rs-iot/serve 地址 + serve token 来源 + jwt_secret）+ OS 服务化（systemd unit / Windows service） | gateway 作为 OS 服务启停 | 1d |
| B7 | 测试：mock rs-iot + mock serve，帧边界/鉴权链/健康检查/断线重连用例 | 测试绿 | 1.5d |

**P0-C：前端（AI 重构快路径）**——对接 gateway :8080（两个 EventSource + fetch POST），AI 辅助快速完成，不计硬工期。

**P0-D：reasonix serve 部署（OS 服务化）**——写 systemd unit / Windows service 配置 + reasonix.toml 模板，0.5d。

### 验证标准（P0 整体闭环）
- **MCP 闭环**：reasonix 经 rsiot-mcp 读到 rs-iot 设备数据（不经 gateway，直连 rs-iot:8790）
- **gateway 闭环**：前端经 gateway 同时看到 rs-iot 实时事件 + reasonix 对话流（两条 SSE）
- **rs-iot 零改动**：`cargo test --all-targets --all-features` rs-iot 全绿，pipeline/lux/采集零改动
- **3 个 P0 坑均解决**：serve 归 OS 服务自愈、两条 SSE 帧边界正确、serve cookie 鉴权生效

## 十、v1 vs v3 对比（决策参考）

| 维度 | v1（反代加进 rs-iot） | v3（独立 gateway） |
|---|---|---|
| rs-iot 改动 | 反代路由+AppState（~400行） | **真正零改动** |
| 新组件 | rsiot-mcp | **gateway 程序** + rsiot-mcp |
| 职责清晰 | rs-iot 混了反代 | rs-iot 纯数采 / gateway 纯编排 |
| 前端入口 | rs-iot:8790（工业API+agent混杂） | gateway:8080（单一） |
| SSE 反代点 | 1（reasonix） | 2（reasonix + rs-iot） |
| 鉴权 | rs-iot JWT 自动保护（简单） | gateway 复用 JWT + 注入 serve cookie（中） |
| 新单点 | 无 | gateway（逻辑简单易可靠） |
| serve 生命周期 | rs-iot spawn 守护（P0 坑①代码职责） | OS 服务管（P0 坑①运维职责，更稳） |
| 未来扩展 | 每加反代改 rs-iot | gateway 加路由（不动 rs-iot） |
| 后端工作量 | rsiot-mcp(11-14d) + 反代(5-7d) = 16-21d | rsiot-mcp(11-14d) + gateway(8-12d) = 19-26d |

**v3 比 v1 多约 3-5 人天**（gateway 比内嵌反代稍大），换来：rs-iot 真正零改动 + 职责分层干净 + 未来扩展不动 rs-iot + serve 归 OS 服务更稳。

## 十一、风险与未覆盖点（v3 特有）

1. **gateway 是新单点**：可靠性靠"逻辑简单 + OS 服务化"保证。若 gateway 挂，前端全挂（rs-iot 本身仍在跑，agent 经 MCP 仍能用，但前端入口断）。
2. **三层鉴权链复杂度**：比 v1 多一层。复用 rs-iot JWT 简化了层1+层2，但层3（serve cookie）仍需 gateway 处理。需明确 401 错误标注哪层失败。
3. **两条 SSE 帧边界**：gateway 要写两份帧边界代码（rs-iot 的有 event: 头、serve 的无），略有差异。可抽象成通用 SSE 转发工具函数复用。
4. **gateway 技术选型未定**：P0-B1 需调研 Pingora/axum 手写/轻量库，选错会返工。
5. **gateway 部署**：现场机器多一个二进制 + OS 服务配置，部署链路多一步（但 rs-iot 零改动抵消了这部分）。
6. **未实测**：gateway 并发两条 SSE 的连接管理、serve token 轮换、gateway 自身长稳——P0 落地验证。

---

## 附录：三方案并存对照

| 方案 | 核心 | rs-iot 改动 | 文档 |
|---|---|---|---|
| **v1** | 反代加进 rs-iot（serve 独立常驻） | 反代路由+AppState（~400行） | `plan-ai-agent-integration.md` |
| **v2** | rs-iot spawn 驱动 agent（按需 spawn） | AgentHost 模块（中） | `plan-ai-agent-integration-v2.md` |
| **v3** | **独立 API Gateway 程序**（本文） | **真正零改动** | `plan-ai-agent-integration-v3.md` |

三者 MCP server 部分**完全相同**（照抄 dbx-mcp），差异只在"agent 如何接入"这一层。最终选哪个待 P0 验证产品形态后定——v3 在"rs-iot 零改动 + 职责分层"上最优，代价是多一个 gateway 程序。
