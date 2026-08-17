# API Gateway 技术选型（v3 P0-B1）

> 本文服务于 [`plan-ai-agent-integration-v3.md`](./plan-ai-agent-integration-v3.md) §八 "Gateway 的技术选型（待 P0 调研）"。
> 结论先行：**选 axum 0.8 + reqwest 手写反代**，印证 v3 §八的倾向并给出决定性证据。
> 所有 API 签名均来自 axum 0.8 官方源 / Cargo 缓存源码 / context7 文档查询（已引证 file:line 或文档链接），不凭记忆。

## 一、执行摘要

**推荐：axum 0.8 + reqwest 手写反代**（v3 §八"倾向 axum + tower 手写"成立）。

**核心理由**（一句话）：gateway 的两件最棘手的事——**两条 SSE 帧透传**与**层3 cookie 注入**——在 axum 栈下有**官方背书的成熟模式**（axum 官方示例 `examples/reqwest-response/src/main.rs` 直接给出 `Body::from_stream(reqwest.bytes_stream())`），且与 rs-iot 已验证的 axum 0.8 / reqwest 0.12 栈**零版本分叉**；Pingora 的 Rust API 在 gateway 的 `/api/gw/*` 自有业务端点上是硬伤（无法挂 axum Router，只能用 `session.respond_write()` 手写 HTTP），轻量库 `axum-reverse-proxy` 在 SSE 鉴权注入上有未知覆盖度。

**最大发现**（详见 §四）：**axum 的 `Sse` 类型 + `Event` 构造器是 SSE 透传的反模式**——`Event::data()` 会把 `\n` 拆成多行 `data:` 字段（[axum `axum/src/response/sse.rs` write_buf 实现](https://github.com/tokio-rs/axum/blob/main/axum/axum/src/response/sse.rs)，memchr2 找 `\n`/`\r` 后插 `data: ` 前缀），强行包装 serve 的无 `event:` 头裸 NDJSON 会破坏前端 `onmessage` 解析。**正解是绕过 `axum::response::sse`，直接用 `Response::builder().body(Body::from_stream(...))`**——这是 axum 官方示例钦定的模式，且**天然不需要帧边界状态机**（透传字节流即可，前端 EventSource 自解 `\n\n`）。

## 二、需求复述（从 v3 提炼的 gateway 硬需求）

| # | 硬需求 | 来源 | 验收 |
|---|---|---|---|
| H1 | **REST 反代**：`/api/rsiot/*` → rs-iot:8790 `/api/v1/*`（gateway `nest_service` 剥 `/api/rsiot` 前缀，前端写完整 `/api/rsiot/api/v1/...`） | v3 §3.1 | 前端 `GET /api/rsiot/api/v1/sysstatus` 命中 rs-iot `/api/v1/sysstatus` |
| H2 | **REST 反代**：`/api/agent/*` → serve:8787（POST submit/approve/cancel/answer/inbox） | v3 §3.2 | 前端 POST 经 gateway 到达 serve |
| H3 | **SSE 透传 ×2**：`/api/rsiot/events`（rs-iot，有 `event:` 头）+ `/api/agent/events`（serve，**无 `event:` 头**，kind 在 JSON body 内） | v3 §3.4 + §七坑② | 两条独立流，前端两个 EventSource 各收完整帧 |
| H4 | **层1 鉴权**：gateway 自有 `POST /api/gw/login`（复用 rs-iot `jwt_secret`）+ 中间件验 JWT | v3 §4 层1 | 前端登录 gateway 拿 JWT，无 JWT 调 `/api/*` 返 401 |
| H5 | **层2 透传**：转发 rs-iot 时保留 `Authorization: Bearer <jwt>` | v3 §4 层2 | rs-iot `auth_middleware`（[rs-iot `src/api/auth.rs:49-92`](../src/api/auth.rs)）直读，零额外代码 |
| H6 | **层3 cookie 注入**：转发 serve 时**伪造** `Cookie: reasonix_token=<token>`（serve 只认 cookie 不认 Bearer，证据 v1 §10.3 `auth.go:304-340`） | v3 §4 层3 | serve 端鉴权通过，前端不感知 token |
| H7 | **gateway 自有业务端点**：`GET /api/gw/health`（聚合 rs-iot + serve + MCP 探针）、`GET /api/gw/auth/status` | v3 §3.3 | 非"反代"，是 gateway 自己的 handler |
| H8 | **OS 服务化**：systemd unit / Windows service | v3 §九 B6 | gateway 作为 OS 服务启停 |
| H9 | **强制 Content-Type**：serve POST 必须带 `Content-Type: application/json` 否则 415（serve csrfGuard，v1 §10.3 `serve.go:549-562`） | v1 §10.3 其他坑 | gateway 转发 POST 时保头 |

**非需求**（v3 明确排除）：不聚合 SSE（两条独立流）、不 spawn serve（OS 服务管）、不存业务状态（无状态纯反代）。

## 三、三选项评估表

每格带证据（库版本 + 文档链接 / file:line）。打分 1-5（5 优）。

### 选项 A：Pingora 0.8（Cloudflare Rust 网关框架）

| 维度 | 评分 | 证据 |
|---|---|---|
| 反代（路径重写 / upstream 选择） | 4 | `ProxyHttp::upstream_peer` 按 `session.req_header().uri.path()` 选 peer（[Pingora `docs/user_guide/modify_filter.md` Route Traffic 示例](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/modify_filter.md)，`if path.starts_with("/family/")`）。但路径**重写**（剥 `/api/rsiot` 前缀）需在 `upstream_request_filter` 改 `RequestHeader` 的 uri，比 axum `axum::extract::Path` 重写多一层。 |
| SSE 透传 | 5 | Pingora 设计目标就是长连接 L7 反代，连接池 + HTTP/1.1 keep-alive 原生；Cloudflare 生产 SSE 量级验证。**理论最强项**。 |
| 鉴权注入（层3 cookie） | 3 | `upstream_request_filter(&self, session, upstream_request: &mut RequestHeader, ctx)` 是注入 header 的钩子（[Pingora `docs/user_guide/rate_limiter.md`](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/rate_limiter.md) 示例 `upstream_request.insert_header("Host", ...)`）。可行但 `RequestHeader` API 比 axum `HeaderMap` 陌生。 |
| 鉴权中间件（层1 JWT） | 3 | `request_filter(&self, session, ctx) -> Result<bool>`：返 `true` 短路（[Pingora `docs/user_guide/modify_filter.md` Return Custom Error Pages](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/modify_filter.md)，`session.respond_error(403)`）。JWT 验证逻辑要自己写 `jsonwebtoken` 调用，可移植 rs-iot `verify_token`。 |
| **gateway 自有业务端点（H7）** | **1（硬伤）** | **Pingora 是反代框架不是通用 web 框架**。`/api/gw/health` 这类非反代端点只能塞进 `request_filter` 用 `session.respond_write(status, headers, body)` 手写 HTTP 响应（见 [Pingora rate_limiter 示例](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/rate_limiter.md) 的 429 分支）。**无法挂 axum Router**（Service trait 与 Handler 模型不兼容，需自写适配层）。聚合 health 要并发探 rs-iot + serve + MCP，写 `respond_write` 手搓 JSON 是反人类。 |
| 学习曲线 | 2 | `ProxyHttp` trait 生命周期（request_filter → upstream_peer → upstream_request_filter → response_filter → body_filter，[Pingora `docs/user_guide/internals.md` mermaid 图](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/internals.md)）+ `Session` API + 自己的 `Server`/`http_proxy_service`/`background_service` 模型，与 axum/tower 心智模型完全不同。 |
| 与 rs-iot 栈一致性 | 1 | rs-iot 用 axum 0.8 + tower 0.5 + hyper 1.x。Pingora 自带 hyper + 自己的 service 抽象，**双框架并存**（gateway 内还要挂 axum 处理 `/api/gw/*`，或在 Pingora `request_filter` 里手写 JSON 响应）。 |
| 生产成熟度 | 5 | Cloudflare 量级生产，零停机升级、连接池、TLS、健康检查内置。 |

**Pingora 致命伤**：H7（gateway 自有 `/api/gw/*` 业务端点）无干净落点，要么双框架（Pingora 反代 + axum 挂业务端点，两套 server），要么在 `request_filter` 里手写 HTTP。v3 §二架构图把 `POST /api/gw/login` `GET /api/gw/health` 列为一等公民，Pingora 适配成本被低估。

### 选项 B：axum 0.8 + reqwest 手写（推荐）

| 维度 | 评分 | 证据 |
|---|---|---|
| 反代（路径重写） | 4 | `axum::extract::Request` 拿 path，字符串替换剥前缀，`reqwest::RequestBuilder::send()` 转发。`tower::Service` / `axum::middleware::from_fn` 包成中间件。代码量小，无新框架。 |
| SSE 透传 | 5 | **axum 官方示例钦定模式**：[axum `examples/reqwest-response/src/main.rs`](https://github.com/tokio-rs/axum/blob/main/examples/reqwest-response/src/main.rs) ——`response_builder.body(Body::from_stream(reqwest_response.bytes_stream()))`。`Body::from_stream` 签名要 `Stream<Item = Result<Bytes, E>> where E: Into<BoxError>`（[axum `axum-core/src/body.rs` StreamBody::poll_frame](https://github.com/tokio-rs/axum/blob/main/axum-core/src/body.rs)），reqwest `bytes_stream()` 产 `Result<Bytes, reqwest::Error>`，reqwest::Error 实现 `std::error::Error + Send + Sync`，**无缝匹配**。 |
| 鉴权注入（层3 cookie） | 5 | reqwest `RequestBuilder::header(COOKIE, ...)` 一行注入。出站请求构造完全可控。 |
| 鉴权中间件（层1 JWT） | 5 | `axum::middleware::from_fn_with_state` + 复用 rs-iot `verify_token`（[rs-iot `src/api/auth.rs:38-46`](../src/api/auth.rs)），与 rs-iot `auth_middleware`（[rs-iot `src/api/auth.rs:49-92`](../src/api/auth.rs)）几乎逐行可抄。 |
| gateway 自有业务端点（H7） | 5 | axum 本来就是 web 框架，`/api/gw/health` 就是个普通 handler，`tokio::join!` 并发探 rs-iot/serve/MCP 返 JSON，天然顺手。 |
| 学习曲线 | 5 | rs-iot 已用 axum 0.8（[rs-iot `Cargo.toml:56`](../Cargo.toml)），团队已验证栈，零新增心智。 |
| 与 rs-iot 栈一致性 | 5 | axum 0.8 / reqwest 0.12 / tower 0.5 / hyper 1.x 与 rs-iot 同版本（gateway 复用同一栈，见 [rs-iot `Cargo.toml:56-58,92`](../Cargo.toml)）。注意 rs-iot 的 reqwest 是 `optional + lua-http`，gateway 独立 Cargo.toml 要自己拉 reqwest **并加 `stream` feature**（v1 §10.3 已标此坑，[rs-iot `Cargo.toml:92`](../Cargo.toml) 当前缺 stream feature）。 |
| 生产成熟度 | 4 | axum + hyper + reqwest 是 Rust async web 第一栈，生产广泛。反代长稳需自测（连接管理 / 背压 / 客户端断线探测），但比 Pingora 的可控性反而更高（自己掌握每个字节）。 |

### 选项 C：axum-reverse-proxy 等轻量库

| 维度 | 评分 | 证据 |
|---|---|---|
| 反代 | 4 | `axum-reverse-proxy` crate 提供现成 `ReverseProxy::new(prefix, upstream)`，开箱即用。 |
| SSE 透传 | 3（未知） | 底层也是 axum + reqwest，理论透传。但 crate 是否禁用响应缓冲、是否保 chunked 边界，**文档未明**（crates.io 页面 WebFetch 失败，无可靠源）。需 P0-B1 实测。 |
| 鉴权注入（层3 cookie） | 2（未知） | 是否暴露"转发前改 header"钩子**不明**。若不暴露，要么 fork 要么绕过库直接用底层 reqwest（退化为选项 B）。 |
| 鉴权中间件（层1 JWT） | 4 | 可叠 `axum::middleware` 在 ReverseProxy 之前，这一层独立。 |
| gateway 自有业务端点 | 5 | axum Router `nest` 或 `merge` 即可。 |
| 学习曲线 | 4 | API 小，但"库不够用时退路"的学习成本反而比直接手写高（要学库的抽象 + 退路）。 |
| 与 rs-iot 栈一致性 | 4 | 同 axum 栈，但多一个第三方依赖（小维护者 crate）。 |
| 生产成熟度 | 2 | 社区维护（非 axum 官方），维护活跃度 / SSE 边界 bug 修复速度无保障。v3 §九 B1 验证标准"编译通过 + 技术选型有据"——选轻量库的"据"反而最弱。 |

**轻量库致命伤**：层3 cookie 注入 / SSE 帧透传这两个 gateway 核心需求的覆盖度是黑盒，选它等于把风险押在第三方小 crate 上，而 gateway 逻辑本就极简（v3 §一"纯无状态 HTTP 反代"）——手写比依赖库更短更可控。

## 四、核心悬念澄清（决定性章节）

### 悬念 1（最关键）：axum `Sse` 能否透传 serve 的无 `event:` 头裸 SSE 帧？

**结论：不能用 axum `Sse` 类型，必须裸字节流透传。**

**证据链**：

1. **axum `Sse::new(stream)` 要求 `stream: Stream<Item = Result<Event, E>>`**（[axum `src/response/sse.rs` SseBody::poll_frame](https://github.com/tokio-rs/axum/blob/main/src/response/sse.rs)）。每个 item 必须是构造好的 `Event` 对象。

2. **`Event::data()` 会破坏裸 NDJSON**：`EventDataWriter::write_buf` 用 `memchr2_iter(b'\n', b'\r')` 找换行，**每个 `\n`/`\r` 后插一个 `data: ` 前缀**（[axum `axum/src/response/sse.rs:442-469`](https://github.com/tokio-rs/axum/blob/main/axum/axum/src/response/sse.rs)）。测试 `write_data_writer_str` 证明 `"moon star\nsun"` → `b"data: moon star\ndata: sunset"`。**多行 JSON（pretty-printed）会被拆碎**。

3. **`Event` 不调 `.event()` 时输出无 `event:` 头**——看似能用？但 `Event` 仍强制走 `data: <payload>\n\n` 包装。serve 的裸 NDJSON 是 `<json>\n\n`（无 `data:` 前缀），用 `Event::data(json)` 包装后前端收到的字节是 `data: <json>\n\n`，**前端 EventSource 的 `onmessage` 默认期望 `data:` 字段，但 serve 前端（reasonix 自带）按裸 NDJSON 解析**——若前端代码用 `eventSource.addEventListener('message', e => JSON.parse(e.data))` 尚可，但 reasonix 前端可能直接按裸 JSON 流解析（`eventwire/wire.go:16` 证实 serve 不写 `event:` 头，kind 在 JSON 内）。**注入 `data:` 前缀会破坏 reasonix 前端的裸 JSON 解析**。

4. **rs-iot 自己的 SSE（[rs-iot `src/api/routes_events.rs:61-68`](../src/api/routes_events.rs)）用 `Event::default().event(kind).json_data(payload)`** —— 有 `event:` 头，前端按 `addEventListener(kind)` 分发。这条流经 gateway 透传时，若用 `Event` 包装会丢 `event:` 头语义（除非重新解析）。

**正解：绕过 axum `Sse`，裸字节流透传**（§五给出骨架代码）。

### 悬念 2：裸字节流透传方案是否成立？

**结论：完全成立，且有 axum 官方示例背书。**

**证据**：

1. **axum 官方示例 `examples/reqwest-response/src/main.rs`**（context7 查证原文）：
   ```rust
   async fn stream_reqwest_response(State(client): State<Client>) -> Response {
       let reqwest_response = client.get("http://127.0.0.1:3000/stream").send().await?;
       let mut response_builder = Response::builder().status(reqwest_response.status());
       *response_builder.headers_mut().unwrap() = reqwest_response.headers().clone();
       response_builder
           .body(Body::from_stream(reqwest_response.bytes_stream()))
           .unwrap()
   }
   ```
   这就是 SSE 反代的官方钦定骨架。

2. **`Body::from_stream` 签名**（[axum `axum-core/src/body.rs` StreamBody](https://github.com/tokio-rs/axum/blob/main/axum-core/src/body.rs)）：接受 `Stream<Item = Result<impl Into<Bytes>, E>> where E: Into<BoxError>`。reqwest `bytes_stream()` 产 `impl Stream<Item = Result<Bytes, reqwest::Error>>`，`reqwest::Error: std::error::Error + Send + Sync + 'static` 满足 `Into<BoxError>`。**零适配**。

3. **是否需要 `\n\n` 帧边界状态机？**——**不需要**。这是 v1/v3 文档（[v3 §七坑②](./plan-ai-agent-integration-v3.md)）的**最大认知修正**：
   - **若 gateway 只透传不解析**：字节流原样转发，前端 EventSource 自解 `\n\n` 边界。gateway 只是个字节管道。**无需帧边界代码**。
   - **v1/v3 之所以认为要帧边界**，是因为 v1 设想"在反代层丢弃 `: ping` 注释帧 + axum KeepAlive 自保活"——这两件事都是**可选优化**，不是透传必需。
   - **真正要做的是 KeepAlive**：gateway 到前端若中间有 nginx 等代理，长无数据连接会被掐。但若 gateway 直接面向前端（v3 §二架构图，前端直连 gateway:8080），**只要上游（rs-iot/serve）自己在发心跳/KeepAlive，gateway 透传字节即可**。
     - rs-iot SSE 自带 `KeepAlive::default()`（[rs-iot `src/api/routes_events.rs:68`](../src/api/routes_events.rs)，默认 15s `:\n\n` 注释帧）。
     - serve 心跳见 v1 §10.3 `: ping` / `: connected` 注释帧。
     - **两条上游都自带心跳，gateway 透传即可，无需自加 KeepAlive**。

4. **裸透传的"丢弃 `: ping`"决策**：可选。若前端 EventSource 收到 `: ping` 注释帧会自动忽略（SSE 规范，注释帧不触发 `onmessage`）。**透传更安全**（不破坏上游心跳语义）。

**修正后的 §七坑②**：gateway SSE 反代**无帧边界代码**，只裸字节流透传。v1/v3 估时（B3 2d / B4 2.5d 含 SSE 部分）可下调约 0.5-1d。

### 悬念 3：Pingora Rust API 对 BFF 反代的适配度

**结论：H7（gateway 自有业务端点）是硬伤，否决。**

**证据**：

1. **Pingora `ProxyHttp` trait 的 `request_filter` 返回 `Result<bool>`**，`true` 表示短路并自己写响应（[Pingora `docs/user_guide/modify_filter.md` Return Custom Error Pages](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/modify_filter.md)，`session.respond_error(403).await; return Ok(true)`）。

2. **`/api/gw/health` 聚合探针**需并发 fetch rs-iot + serve + MCP，返结构化 JSON。在 Pingora 里要：
   - 在 `request_filter` 里 match path == "/api/gw/health"
   - 并发 `reqwest::get` 三个上游
   - 用 `session.respond_write(200, &[("Content-Type","application/json")], &json_bytes).await` 手写响应
   - 这比 axum handler `async fn health() -> Json<...>` 重 5-10 倍代码。

3. **无法挂 axum Router**：Pingora 的 `Service` trait 与 axum `Handler` 模型不兼容（Pingora 自带 hyper + 自己的 `Session` 类型）。要么双 server（Pingora 反代 :8080 + axum 业务端点 :8081，前端要连两个端口，破坏 v3 §三"前端只对接 gateway :8080"），要么在 Pingora `request_filter` 里手写所有业务端点。

4. **层1 JWT 鉴权**可行（`request_filter` 调 `jsonwebtoken`），层3 cookie 注入可行（`upstream_request_filter` 改 `RequestHeader`），SSE 透传强项——但 H7 把 Pingora 拖垮。

**Pingora 适用场景**：纯反代 + LB + 零业务端点（如 Cloudflare 边缘节点）。v3 gateway 有 `/api/gw/*` 业务端点，不匹配。

## 五、推荐方案 + 理由

### 决策

**选 B：axum 0.8 + reqwest 手写反代。**

印证 v3 §八"倾向 axum + tower 手写"的判断。Pingora 留作"若手写发现反代/健康检查复杂度超预期"的退路（实际不会触发，见上）。

### 理由（按 v3 §六规则6"暴露冲突不折中"）

| 冲突点 | 选择 | 否决项 | 理由 |
|---|---|---|---|
| H7 业务端点 | axum Router 原生 handler | Pingora `request_filter` + `respond_write` | health 聚合探针写 JSON，handler 比 respond_write 简单 10 倍 |
| SSE 透传 | 裸字节流 `Body::from_stream` | axum `Sse` + `Event` 包装 | serve 无 `event:` 头裸 NDJSON，`Event::data` 拆 `\n` 破坏前端解析（§四悬念1） |
| 帧边界状态机 | **不做**（透传字节流） | tokio_util::codec 状态机 | 上游（rs-iot/serve）自带心跳，gateway 只做字节管道，前端 EventSource 自解 `\n\n`（§四悬念2） |
| reqwest feature | gateway 独立 Cargo.toml 加 `stream` feature | 复用 rs-iot 的 reqwest（无 stream feature） | rs-iot reqwest 是 `optional + lua-http` 无 stream（[rs-iot `Cargo.toml:92`](../Cargo.toml)），gateway 独立依赖 |
| 层1鉴权 | 复用 rs-iot `verify_token` + `jwt_secret` | gateway 独立签发 | v3 §4 层1 推荐"复用"，前端一套 JWT 通吃 |
| 库依赖 | 仅 axum/tower/reqwest/tower-http（与 rs-iot 同版本） | axum-reverse-proxy 第三方 crate | gateway 逻辑极简（v3 §一），手写 < 200 行，第三方库风险 > 收益 |

### gateway Cargo.toml 骨架（P0-B1 直接用）

```toml
[package]
name = "rsiot-gateway"
version = "0.1.0"
edition = "2021"

[dependencies]
# 与 rs-iot 同版本栈（[rs-iot Cargo.toml:56-58]）
axum = { version = "0.8", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["trace", "cors"] }
# gateway 专属：stream feature 必加（v1 §10.3 坑）
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream"] }
# 复用 rs-iot JWT
jsonwebtoken = { version = "10", features = ["rust_crypto"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
bytes = "1"
futures-util = "0.3"
# OS 服务化（同 rs-iot service 模块）
[target.'cfg(windows)'.dependencies]
windows-service = "0.7"
```

## 六、关键代码模式（推荐方案骨架）

### 模式 1：SSE 裸字节流透传（最棘手，含决定性修正）

**不需要帧边界状态机**（§四悬念2 已证）。两条 SSE 流共用一个通用转发函数，差异仅在上游 URL + 层3 cookie。

```rust
//! src/proxy.rs — 通用反代：REST + SSE 共用，SSE 零特殊处理。
//!
//! 关键：用 `Body::from_stream` 裸透传 reqwest 字节流，
//! 绕过 axum `Sse`/`Event`（serve 无 event: 头裸 NDJSON，Event::data 会拆 \n 破坏前端解析，
//! 见 docs/plan-gateway-tech-selection.md §四悬念1）。

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use std::sync::Arc;

/// gateway 共享状态。
pub struct GwState {
    /// reqwest 连接池（长连接复用，SSE 与 REST 共用）。
    pub client: reqwest::Client,
    /// rs-iot 基址，如 "http://127.0.0.1:8790"。
    pub rsiot_base: String,
    /// serve 基址，如 "http://127.0.0.1:8787"。
    pub serve_base: String,
    /// 层3：serve token（伪造 Cookie 注入）。
    pub serve_token: Arc<String>,
    /// 层1：jwt_secret（复用 rs-iot，验前端 JWT）。
    pub jwt_secret: Arc<String>,
}

/// 通用透传：剥 `prefix` 后拼 `upstream_base + 剩余 path + query`，转发。
///
/// `inject_cookie`：层3 hook，Some(token) 则注入 `Cookie: reasonix_token=<token>`。
/// REST 与 SSE 走同一路径——SSE 上游返 `text/event-stream`，headers clone 保留 Content-Type，
/// `Body::from_stream(bytes_stream())` 原样吐字节，前端 EventSource 自解 `\n\n`。
///
/// 不做帧边界状态机（上游 rs-iot/serve 自带心跳，gateway 是字节管道，
/// 见 docs/plan-gateway-tech-selection.md §四悬念2）。
pub async fn forward(
    State(st): State<Arc<GwState>>,
    req: Request,
    upstream_base: &str,
    strip_prefix: &str,
    inject_cookie: Option<&str>,
) -> Response {
    // 1. 构造上游 URL：剥 prefix，保 query。
    let (parts, body) = req.into_parts();
    let path_and_query = parts.uri.path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let remaining = path_and_query
        .strip_prefix(strip_prefix)
        .unwrap_or(path_and_query);
    let upstream_url = format!("{upstream_base}{remaining}");

    // 2. 构造出站请求：保 method/headers/body，按需注入 cookie。
    let mut req_builder = st.client
        .request(parts.method, &upstream_url)
        .header(HeaderName::from_static("x-forwarded-for"), "gateway");
    // 透传原 headers（Authorization 层2 自动满足）。
    for (name, value) in parts.headers.iter() {
        req_builder = req_builder.header(name, value);
    }
    // 层3：serve cookie 注入。
    if let Some(token) = inject_cookie {
        req_builder = req_builder.header(
            reqwest::header::COOKIE,
            HeaderValue::from_str(&format!("reasonix_token={token}"))
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );
    }
    let upstream_req = req_builder.body(reqwest::Body::wrap_stream(body_into_byte_stream(body)));

    // 3. 发送。
    let upstream_resp = match st.client.execute(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, %upstream_url, "upstream request failed");
            return (StatusCode::BAD_GATEWAY, "upstream unreachable").into_response();
        }
    };

    // 4. 透传响应：status + headers + 流式 body。
    //    SSE 上游 Content-Type: text/event-stream 经 headers clone 原样保留。
    //    Body::from_stream(bytes_stream()) 是 axum 官方钦定模式
    //    （examples/reqwest-response/src/main.rs）。
    let status = upstream_resp.status();
    let mut resp_builder = Response::builder().status(status);
    *resp_builder.headers_mut().unwrap() = upstream_resp.headers().clone();
    resp_builder
        .body(Body::from_stream(upstream_resp.bytes_stream()))
        .unwrap_or_else(|e| {
            tracing::error!(%e, "build response failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "gateway error").into_response()
        })
}

/// axum Body → reqwest Body（请求体流转，POST submit 等用）。
///
/// axum 0.8 `Body: Stream<Item = Result<Frame<Bytes>, BoxError>>`，
/// reqwest `Body::wrap_stream` 接 `Stream<Item = Result<Bytes, E>>`，
/// 需把 Frame 拆出 Bytes。SSE 是 GET 无 body，POST 有 body 走这条。
async fn body_into_byte_stream(_body: axum::body::Body) -> impl bytes::Bytes {
    // 实现略：用 Body::into_data_stream() 转 Stream<Result<Bytes, _>>，
    // 再 reqwest::Body::wrap_stream(stream)。
    // 见 axum examples/stream-to-file 的 into_data_stream 用法。
    unimplemented!("P0-B1 实测：axum::body::Body::into_data_stream → reqwest::Body::wrap_stream")
}
```

**路由挂载**（`src/main.rs`）：

```rust
let app = axum::Router::new()
    // H1: rs-iot REST + SSE 透传（层2 Authorization 自动透传，无 cookie 注入）
    .nest_service("/api/rsiot", {
        let st = state.clone();
        axum::routing::any(move |req: Request| {
            let st = st.clone();
            async move {
                forward(State(st), req, &state.rsiot_base, "/api/rsiot", None).await
            }
        })
    })
    // H2: serve REST + SSE 透传（层3 注入 cookie）
    .nest_service("/api/agent", {
        let st = state.clone();
        let token = state.serve_token.clone();
        axum::routing::any(move |req: Request| {
            let st = st.clone();
            let token = token.clone();
            async move {
                forward(State(st), req, &state.serve_base, "/api/agent", Some(&token)).await
            }
        })
    })
    // H7: gateway 自有业务端点（Pingora 的硬伤，axum 原生）
    .route("/api/gw/login", axum::routing::post(gw_login))
    .route("/api/gw/auth/status", axum::routing::get(gw_auth_status))
    .route("/api/gw/health", axum::routing::get(gw_health))
    // 层1 JWT 中间件（复用 rs-iot verify_token，PUBLIC_PATHS 放行 login）
    .layer(axum::middleware::from_fn_with_state(state.clone(), auth_middleware));
```

### 模式 2：层3 serve cookie 注入

上方 `forward()` 的 `inject_cookie` 参数已是完整实现。token 来源（P0-B1 选其一）：

```rust
//! src/config.rs — token 来源两选一。

#[derive(serde::Deserialize)]
pub struct GwConfig {
    pub rsiot_base: String,        // "http://127.0.0.1:8790"
    pub serve_base: String,        // "http://127.0.0.1:8787"
    pub jwt_secret: String,        // 与 rs-iot config.toml [api] jwt_secret 同值
    pub listen: String,            // "0.0.0.0:8080"
    /// 方案 A（推荐）：直接配置文件指定 token（与 serve 启动 --token 一致）。
    pub serve_token: String,
    /// 方案 B：从 serve --token-file 路径读（token 轮换时 gateway 重读，需 watch）。
    /// pub serve_token_file: String,
}
```

**方案 A（配置直填）**：最简，serve 与 gateway 共享一份 config 片段。token 轮换需重启 gateway。
**方案 B（token-file 读）**：serve 启动 `--token-file /path/to/token`，gateway 启动时 `std::fs::read_to_string` + 可选 `notify` watch 热更新。比 A 多一个依赖（`notify`）。

**P0-B1 建议方案 A**，token 轮换频率低（现场设备，非公网），轮换时 OS 重启 gateway 成本可接受。

### 模式 3：gateway 自有 health 聚合（H7，Pingora 的硬伤在此）

```rust
//! src/routes_gw.rs — gateway 自有端点，纯 axum handler。

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use crate::proxy::GwState;

#[derive(Serialize)]
pub struct HealthResp {
    pub rsiot: &'static str,   // "online" / "offline"
    pub serve: &'static str,
    pub mcp: &'static str,     // MCP 探针：rsiot-mcp 是否被 reasonix spawn（难探，P0-B1 后置）
}

/// GET /api/gw/health：并发探 rs-iot + serve。
///
/// MCP 连接状态难从 gateway 探（gateway 不 spawn rsiot-mcp，是 reasonix spawn），
/// P0 先返 "unknown"，P1 经 serve /status 间接判断（serve 知道自己的 plugins 是否连上）。
pub async fn gw_health(State(st): State<Arc<GwState>>) -> Json<HealthResp> {
    let (rsiot, serve) = tokio::join!(
        probe_upstream(&st.client, &format!("{}/api/v1/sysstatus", st.rsiot_base)),
        probe_upstream(&st.client, &format!("{}/status", st.serve_base)),
    );
    Json(HealthResp {
        rsiot: if rsiot { "online" } else { "offline" },
        serve: if serve { "online" } else { "offline" },
        mcp: "unknown",  // P0 占位，P1 从 serve /status 间接探
    })
}

async fn probe_upstream(client: &reqwest::Client, url: &str) -> bool {
    match client.get(url).timeout(std::time::Duration::from_secs(2)).send().await {
        Ok(r) => r.status().is_success() || r.status().is_client_error(), // 401 也算在线
        Err(_) => false,
    }
}
```

## 七、风险与验证建议

### P0-B1 最小验证（先跑通再扩）

**B1-1（决定性，先做）**：SSE 裸透传 demo。起两个 mock 上游（axum 在 :9001 发有 `event:` 头的 SSE，:9002 发无 `event:` 头裸 NDJSON），gateway 透传，浏览器开两个 EventSource 验证：
- rs-iot 风格流（有 `event:` 头）：前端 `addEventListener(kind)` 收到。
- serve 风格流（无 `event:` 头裸 NDJSON）：前端 `onmessage` 收到，JSON.parse 成功。
- **若 serve 风格流前端收不到/解析错——回到 §四悬念1 复核**（理论上不会，字节透传不破坏）。

**B1-2**：层3 cookie 注入验。mock serve 检查请求头有 `Cookie: reasonix_token=test`，无则 401。gateway 转发后 serve 返 200。

**B1-3**：health 聚合。mock rs-iot 离线，gateway `/api/gw/health` 返 `{"rsiot":"offline","serve":"online","mcp":"unknown"}`。

### 风险登记

| 风险 | 等级 | 规避 |
|---|---|---|
| reqwest 默认 timeout 截断 SSE 长连接 | 高 | gateway 构造 `reqwest::Client` 时**不设全局 timeout**（或 `.timeout(None)` + `.connect_timeout(3s)` 仅限连接阶段），SSE 靠上游心跳保活 |
| 客户端断线后 gateway 仍从上游拉流（资源泄漏） | 中 | axum 0.8 `Body::from_stream` 的 drop 会传播取消（reqwest future drop 中断上游连接），P0-B7 断线用例验证 |
| 中间代理（nginx）缓冲 SSE | 中 | gateway 自加响应头 `X-Accel-Buffering: no`（`forward()` headers clone 后追加）。v3 架构前端直连 gateway:8080，此风险仅在 gateway 前再套 nginx 时触发 |
| serve token 轮换 gateway 不感知 | 低 | 方案 A 重启 gateway；P1 改方案 B（notify watch token-file） |
| 层1 JWT 复用 rs-iot jwt_secret，rs-iot 改 secret 后 gateway 不同步 | 中 | config 共享同一份（部署时 `cp` 或符号链接），P0-B6 配置文档约束 |
| gateway 是新单点（v3 §九风险1） | 中 | OS 服务化 + gateway 逻辑极简（< 500 行）保证可靠性 |
| `Body::wrap_stream` 请求体转发的 Frame→Bytes 适配 | 低 | axum 0.8 `Body::into_data_stream()` 直接给 `Stream<Result<Bytes,_>>`（axum examples/stream-to-file 原文），`reqwest::Body::wrap_stream` 接它。P0-B1 编译验证 |

## 八、与 rs-iot 主进程的耦合边界澄清（诚实标注"零改动"张力）

v3 §五"rs-iot 真正零改动"是**对 gateway 而言**（gateway 只透传，rs-iot 不知道 gateway 存在）。但 v3 同时要 rsiot-mcp 经 rs-iot REST 暴露工具给 agent——**rs-iot REST 现状能否喂饱 MCP 工具，是独立的另一回事**。诚实盘点：

### rs-iot REST 现状（[rs-iot `src/api/mod.rs:96-151`](../src/api/mod.rs)）

| MCP 工具需求（v1 §10.5） | rs-iot 现有端点 | 缺口 |
|---|---|---|
| `rsiot_list_devices` | `GET /api/v1/devices` ✓ + `GET /api/v1/device/get` ✓ | 无 |
| `read_tags`（读实时值） | `GET /api/v1/rtdatas` ✓ | 无 |
| `write_tags`（写设备） | `POST /api/v1/command/send` ✓ | 无 |
| `get_metrics` | `GET /metrics` ✓（免 JWT，但 MCP 直连 rs-iot 自带 JWT 更稳走 `/api/v1/sysstatus`） | 无 |
| 列实例 / 实例状态 | `GET /api/v1/insts` ✓ + `GET /api/v1/inststatus` ✓ + `GET /api/v1/inst/stats` ✓ | 无 |
| 列采集点（children/tag） | `GET /api/v1/device/get` 返 children ✓ | 无（device.get 含 children 列表） |
| **读 lua 脚本内容** | `GET /api/v1/lua/scripts` ✓（列表） | △ 是否返回脚本**内容**还是仅文件名，待 P0-A 验 routes_lua.rs 实际返回结构 |
| **写/改 lua 脚本** | ❌ 只有 `POST /api/v1/lua/reload`（重载，非保存） | **缺口**：v3 §五已标"POST /api/v1/lua/script/save 仍在 rs-iot，属 rs-iot 自身能力补全"。MCP `save_script` 工具依赖此端点 |
| **读配置（脱敏）** | ❌ 无 `/api/v1/config` | **缺口**：v3 §五已标"GET /api/v1/config 脱敏仍在 rs-iot"。MCP `get_config` 工具依赖 |
| 报文 trace | `POST /api/v1/inst/trace` ✓ + `GET /api/v1/logs/recent?target=io_trace` ✓ | 无 |
| 日志查询 | `GET /api/v1/logs/recent` ✓ | 无 |

### 结论（诚实）

- **gateway 维度**：rs-iot **真正零改动**成立。gateway 只透传 `/api/v1/*`，不要求 rs-iot 新端点。
- **MCP 维度**：rs-iot 有 **2 个端点缺口**（`POST /lua/script/save` + `GET /config`），是 rs-iot 自身能力补全（工业网关职责，非 AI 集成专属）。**是否补由 rs-iot 产品决定**，与 gateway 选型无关。
- **P0 边界**：gateway（本报告）与 rsiot-mcp（v1 P0-A）解耦。gateway P0 可独立交付（前端经 gateway 看到 rs-iot + serve SSE），MCP 的 rs-iot 端点缺口不影响 gateway 闭环。

**修正 v3 §五表述**：建议从"rs-iot 真正零改动"细化为"**gateway 视角 rs-iot 零改动；MCP 视角 rs-iot 有 2 个端点待补（save_script / get_config），属 rs-iot 自身能力**"。这更诚实，避免"P0 后才发现 MCP 要改 rs-iot"的预期落差。

---

## 附录：决策树（P0-B1 启动入口）

```
gateway 选型？
├─ H7 gateway 自有业务端点（health/login）是否一等公民？
│   ├─ 是（v3 §3.3 列为一等公民）──► 否决 Pingora（request_filter 手写 JSON 反人类）
│   └─ 否（纯反代无业务端点）──► Pingora 可选（但 v3 不此形态）
├─ SSE 透传是否要解析/聚合？
│   ├─ 否（裸透传）──► axum Body::from_stream（官方钦定，无帧边界代码）
│   └─ 是（聚合/改帧）──► 需帧边界状态机（v3 不需要，决策3 两条独立流）
├─ 层3 cookie 注入是否可控？
│   ├─ axum + reqwest ──► RequestBuilder.header() 一行（完全可控）
│   └─ axum-reverse-proxy ──► 钩子覆盖度不明（黑盒）
└─ 结论：axum 0.8 + reqwest 手写（选项 B）
```

## 引证索引

- v3 需求源：[docs/plan-ai-agent-integration-v3.md](./plan-ai-agent-integration-v3.md) §三/四/七/八
- v1 复核证据：[docs/plan-ai-agent-integration.md](./plan-ai-agent-integration.md) §10.3（serve cookie/SSE 帧/csrfGuard）
- axum SSE 反模式证据：[tokio-rs/axum `axum/src/response/sse.rs` EventDataWriter::write_buf](https://github.com/tokio-rs/axum/blob/main/axum/axum/src/response/sse.rs)（memchr2 拆 \n）
- axum 裸透传官方示例：[tokio-rs/axum `examples/reqwest-response/src/main.rs`](https://github.com/tokio-rs/axum/blob/main/examples/reqwest-response/src/main.rs)（`Body::from_stream(bytes_stream())`）
- axum StreamBody 签名：[tokio-rs/axum `axum-core/src/body.rs`](https://github.com/tokio-rs/axum/blob/main/axum-core/src/body.rs)
- Pingora request_filter / upstream_request_filter：[cloudflare/pingora `docs/user_guide/modify_filter.md` + `rate_limiter.md`](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/modify_filter.md)
- Pingora 请求生命周期：[cloudflare/pingora `docs/user_guide/internals.md`](https://github.com/cloudflare/pingora/blob/main/docs/user_guide/internals.md)
- rs-iot SSE 实现对端：[rs-iot `src/api/routes_events.rs:49-69`](../src/api/routes_events.rs)
- rs-iot auth 中间件（层2 复用）：[rs-iot `src/api/auth.rs:38-92`](../src/api/auth.rs)
- rs-iot REST 路由清单：[rs-iot `src/api/mod.rs:96-151`](../src/api/mod.rs)
- rs-iot 依赖栈：[rs-iot `Cargo.toml:56-58,92`](../Cargo.toml)
- dbx 帧边界可借鉴实现：[dbx `crates/dbx-core/src/ai.rs:732-778`](file:///G:/AI_Agent/dbx/crates/dbx-core/src/ai.rs)（`drain_next_stream_line` + `stream_data_payload`，已处理无 event: 头裸 NDJSON，740-742 行 `if line.starts_with('{')`）
