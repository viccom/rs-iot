# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目地图

轻量跨平台工业 IoT 边缘框架（Rust），Go 项目 Lite_IOT（本机 `D:\Go_Codes\Lite_IOT`，对标参考实现）的 Rust 重写与增强。内嵌 lux 作为进程内 Redis 兼容数据库（RESP :6390 + Tiered WAL 持久化）——lux 为**同级目录** `../lux` 的 path 依赖，新机器需先按 `docs/ops/lux-upstream-sync-strategy.md` §一.5 恢复（clone 官方 + 从私人 fork 拉补丁）。

**Cargo workspace 结构**（2026-08 起，AI Agent 集成项目引入）：根 `rs-iot` crate（主程序，工业数采核心）+ `crates/rsiot-mcp`（reasonix `[[plugins]]` 子进程，MCP 工具→rs-iot REST 翻译层，照 dbx-mcp 范本）+ `crates/rsiot-gateway`（v3 独立 API Gateway，纯反代 rs-iot+reasonix serve，见 `docs/designs/2026-08-11-ai-agent-integration-v3-design.md`）。**三 crate 解耦**：核心数据流（pipeline/lux/采集）只在主 crate，AI 集成全部在子 crate——改子 crate 零影响主 crate（子 crate 均不依赖主 crate，`Cargo.toml` 可验）。改子 crate：`cargo build -p rsiot-mcp` / `-p rsiot-gateway`。

**数据流（hub 模式 + 双总线）**——这是理解全项目的关键。**两条总线**：数据上行（采集→入库）+ 控制下行（写命令→设备）：

```
── 数据上行总线 ─────────────────────────────────────────────────────────
N × Source(dataSim / opcuaClient / modbusClient / s7Client / sidecarClient[外部]) ──► hub (mpsc 512) ──► pipeline dispatcher(按 devid FNV-1a 分片) ──► N worker(Processor + 攒批 50ms + write_batch B 形态 HSET 多对) ──► lux rtdata:<devid>
   └─ 读出口（都从 lux rtdata 取最新值）：GET /api/v1/rtdatas / MqttPublisher pull / 未来北向 UA Server·Modbus slave·104
── 控制下行总线（CtlRegistry，多源写入口 → 实例 cmd_rx）──────────────────
MQTT <hwid>/command 订阅 ─┐
POST /api/v1/command/send ─┼─► CtlRegistry.try_send(64) ──► 实例 Source::run 的 cmd_rx ──► handle_write（opcua/modbus...）
未来北向 UA Server·DA·Modbus slave·104 写回调 ─┘
管理面：REST :8790 (axum + JWT) ──► InstanceManager（动态启停 Source）/ send_ctl（投递写命令）
```

- **南向采集与北向发布完全解耦**：pipeline 只写 lux rtdata，北向 `luaNorth`（mqtt-north 角色，`[mqtt]` 配置自动生成默认实例 mqtt-pub-default）独立周期从 lux 抽取发布（pull 模式）。lux rtdata 是唯一数据交汇点（single source of truth）。
- **写命令多源统一**：MQTT / REST / 未来北向服务器都是写入口，都经 `CtlRegistry`（`src/host.rs`，`try_send` 非阻塞，channel 满丢弃——对标 Go 版 default）路由到实例 `cmd_rx`。REST 入口 `InstanceManager::send_ctl`（`src/plugin/instance.rs`）封装，实例未注册返回 404。
- **单 Source 启停不影响管道**（共享 hub，pipeline dispatcher+N worker 消费者侧 fan-out；各 Source 独立 `CancellationToken`）。worker 数 `[pipeline] workers`（0=auto min(cpu,8)），采集 ticker jitter 错峰（`crate::ticker::phase_ms` FNV-1a 确定性相位，削每秒脉冲）。50万点/秒入库（端到端 bench ~2.6M 点/秒，见 `examples/bench_pipeline_e2e.rs`）。
- **外部插件 bridge（sidecarClient，M7）**：任意语言（C/Go/Python/Java…）的外部进程作为南向 Source 接入——TCP 连 bridge（`config.bridge`，默认 :8781，`enable=false` 默认零影响），acceptor 校验 token（lux `ext_tokens` 的 `hmac_sha256` 比对）后按 inst_id 分发到实例 task；外部 `DataUp`→`Sample`→hub（与内置 Source 同构），写命令经 `cmd_rx`→`Command` 帧→外部→`CommandAck`。**外部=纯运行时**（配置/数据/状态全在 rs-iot，本地仅缓存数据待补传），**lux 不对暴露**（方案 C：外部只连 bridge，认证/授权集中 rs-iot）。`InstConfig.host_kind=External` 标记 + 准入约束（token 只对 External 签发）。协议见 `docs/reference/external-plugin-protocol.md`，Python 样例 `examples/ext-plugin-python/`。
- 架构原则（分层 + 双总线 + 横向扩展边界）见下文「架构原则」节。

**模块职责**：按数据流顺序的完整表（lib/main/config/host/model/plugin/pipeline/history/transform/db/api/service…）
**详见 [docs/architecture.md](docs/architecture.md) §二**（含 2026-10-27 架构审查结论与技术债清单）。

**架构优化评估**：9 项议题与实施顺序见 [docs/designs/2026-10-27-architecture-optimization-review.md](docs/designs/2026-10-27-architecture-optimization-review.md)。

**三层速记**：`api/`（28 个 routes + JWT + SSE）· `plugin/`（Source/Sink 契约 + 11 builtin + lua_core）·
`pipeline.rs`（dispatcher + N worker + 攒批）；数据面与业务域见 `model/`（物模型）· `db/`（lux 封装）· `history/`（旁路）。

**workspace 子 crate**（`crates/`，AI Agent 集成，与主 crate 解耦）：

| crate | 职责 | 现状 |
|---|---|---|
| `crates/rsiot-mcp` | reasonix `[[plugins]]` spawn 的 stdio 子进程，把 MCP 工具调用翻译成 rs-iot loopback REST（照 dbx-mcp 范本：rmcp 2.2 + `#[tool]` 宏 + WebBackend JWT）。`server.rs` 声明 25 工具（含 query percent-encode 输入边界防护），`backend.rs` 调 REST | 25 工具完成（list_devices/read_tags/get_status/list_instances/get_recent_logs/send_device_command/start_instance/stop_instance/save_lua_script/get_config）+ risk 三档框架（CriticalWrite 关键设备禁写 P1 后置）+ query encode 单测；48 单测全绿 |
| `crates/rsiot-gateway` | v3 独立 API Gateway，纯反代 rs-iot:8790 + reasonix serve:8787（`docs/decisions/2026-08-12-gateway-tech-selection.md`：axum 0.8 + reqwest 手写）。`proxy.rs` `forward()` 裸字节流透传（**不用** axum Sse/Event）+ rsiot 凭证转换（query token→Authorization）+ token 防泄漏，`auth.rs` 层1 JWT（复用主 crate jwt_secret） | B1 全完成（SSE 裸透传 + serve cookie 注入 + health 聚合 + rsiot 鉴权转发）；rust-embed 静态前端嵌入 + SPA fallback；37 单测全绿。剩余：OS 服务化 |

子 crate 与主 crate 的耦合边界：gateway 只透传主 crate REST（零改动）；rsiot-mcp 调主 crate REST，主 crate 已补 2 端点（`POST /api/v1/lua/script/save` 路径白名单 + `GET /api/v1/config` 脱敏，属主 crate 自身能力补全，已实现）。

**前端工程**（`frontend/`，M1 起）：Vue3 + Vite + TypeScript SPA（工业监控界面 + AI 面板）。`src/views/`：Login 登录 / Overview 总览 / Telemetry 实时遥测（+ 历史趋势面板：HistoryQuery raw+六聚合/手写 SVG min-max 抽稀曲线/503 文案归一，消费 `GET /api/v1/history`；dev_id 随选中设备 chip 预填、tag 挂实时点位 datalist；实时点位表「操作」列行级「趋势」按钮预填直达（ad-hoc trend 惯例，RtdataPanel emit history 事件）——2026-08-26 自模型页迁入，历史查询属监控关注点，模型页回归纯元数据管理；纯函数 `frontend/src/lib/historyQuery.ts`）/ Collectors 采集实例 / **DeviceModels 设备模型注册表（四面板：设备模型/设备注册表/标签总览 effective/发现队列）** / Diagnostics 诊断 / Control 控制 / Lua 脚本；复用组件 `src/components/monitor/`（九面板：RtdataPanel/CollectorList/CommandPanel/LogStream/PacketTrace/AlarmPanel/SecretPanel/SystemStatus/InstanceForm）+ `src/components/devmeta/`（ModelForm/DeviceForm 表单 + HistoryQuery 历史查询面板）；基建 `frontend/src/api/client.ts` fetch 封装（自动带 JWT、401 区分已登录态 token 失效 vs 登录中业务 401）、`src/stores/`（Pinia：auth/monitor/agent/devreg/lua/secrets/workspace）、`frontend/src/router.ts`（history 模式 + 守卫）。**构建产物 `dist/` 经 rust-embed 嵌入 `crates/rsiot-gateway`**（gateway `static_assets.rs` 服务静态资源 + SPA fallback），`dist/` 不入库（`.gitignore`）。dev：`cd frontend && npm run dev`（Vite :5173，proxy `/api` → gateway :8080 同源无 CORS）；build：`npm run build`（产 `dist/` 后重编 gateway 才嵌入新前端）；单测基线 **16 文件 203 测**（`npm run test -- --run`，vitest；2026-10-27 实测）。

**存储布局与环境依赖**：lux Hash 表清单（users/passwords/devmodels/devreg/rtdata:*/alarms/discoveries…）、
path 依赖（`../lux` / `../tsink_fork`）、端口与日志配置——**详见 [docs/architecture.md](docs/architecture.md) §三**。

## 架构原则

rs-iot 是**工业协议网关**：南向 N 种设备协议 × 北向 M 种对接协议。分层 + 双总线把 N×M 网状耦合**降维成 N+M**（每个协议只对接总线，不互相适配）。这是生存约束——N、M 都会持续增长，网状结构不可维护。

**横向扩展点**（加一个协议只动边界，不碰核心）：

| 加一个… | 要做的事 | 不碰 |
|---|---|---|
| 南向协议 | `builtin/xxx.rs` 实现 Source + `inventory::submit!` + `mod.rs` 加一行 | hub/pipeline/lux/北向/其他南向 |
| 外部南向插件 | 外部进程任意语言实现，经 sidecarClient bridge 接入（`host_kind=external` + token，协议见 docs） | bridge 协议/内置 Source/核心总线 |
| 北向发布 | pull/push 插件从 lux 取数据 | 南向/pipeline |
| 写入口 | 新代码调 `CtlRegistry::send` / `InstanceManager::send_ctl`（MQTT/REST/服务器回调） | 南向 `cmd_rx` 逻辑、其他写入口 |
| Processor | `transform/xxx.rs` 实现 Processor trait | 南向/北向 |
| REST 端点 | `routes_xxx.rs` + `build_router` 注册一行 | 数据面/控制面 |
| MCP 工具（给 AI agent） | `crates/rsiot-mcp/src/server.rs` 加 `#[tool]` 方法 + 映射主 crate REST | 主 crate 核心数据流/其他工具 |
| gateway 路由 | `crates/rsiot-gateway` 加路由（不动主 crate） | 主 crate（gateway 视角零改动）|

**低耦合五机制**：① trait 契约（Source/Sink/Processor，框架依赖抽象）② channel 解耦（hub 512 / cmd 64，异步边界 + 背压）③ lux 作进程内数据总线（RESP 契约，上下游无直接引用）④ `inventory` 静态注册（编译期发现，无注册顺序依赖）⑤ `CancellationToken` + `try_send`（独立启停 + 控制面不阻塞数据面）。

## 常用命令

> ⚠️ **本机禁页面文件**：所有 cargo 命令务必加 `-j 2` 限并行，防 rustc 编译大型 crate 撑爆内存（曾致 Everything 服务被挤崩溃）。详见 `docs/ops/lux-upstream-sync-strategy.md` §八。CI 环境不限。

```bash
cargo build -j 2                       # debug（主 crate）
cargo build -j 2 -p rsiot-mcp          # 只编 MCP 子 crate（AI Agent 集成）
cargo build -j 2 -p rsiot-gateway      # 只编 gateway 子 crate
cargo build -j 2 --release             # release（lto=thin + strip）
cargo test -j 2                        # 全量（CI 用 --all-targets）
cargo test -j 2 <name>                 # 单个测试（如 cargo test hwid）
cargo clippy -j 2 --workspace --all-targets -- -D warnings   # 全 workspace（含两个子 crate）
cargo fmt --all --check                # 格式检查
cargo doc -j 2 --no-deps               # 文档（RUSTFLAGS=-D warnings 下零警告）
cargo audit                            # 依赖安全（CI 中 continue-on-error：传递依赖 rustls advisory 已知）

./target/debug/rs-iot --config config.toml   # 前台运行
RUST_LOG=debug ./target/debug/rs-iot         # 日志级别（缺省 info）
rs-iot install / uninstall        # 服务化（Windows: sc create / Linux: systemd）

./target/debug/rsiot-mcp          # MCP server（stdio，被 reasonix spawn）
RSIOT_BASE=http://127.0.0.1:8790 RSIOT_JWT=<token> ./target/debug/rsiot-mcp   # MCP 带配置

./target/debug/rsiot-gateway gateway.toml      # gateway 前台运行
RSIOT_GW_CONFIG=gateway.toml ./target/debug/rsiot-gateway   # gateway 配置走环境变量

cd frontend && npm run dev       # 前端 dev（Vite :5173 HMR，proxy /api → gateway :8080）
cd frontend && npm run build     # 前端 build（产 dist/，需重编 gateway -p rsiot-gateway 才嵌入新前端）
cd frontend && npm run test      # 前端单测（vitest，加 -- --run 单次跑完退出）
```

### 前端开发流程（frontend/）

两种开发模式，按场景选：

- **dev 模式（改前端代码调试）**：`cd frontend && npm run dev` 起 Vite :5173（HMR 热替换）；另起 gateway（:8080）+ rs-iot + reasonix serve 三件套。浏览器访问 `http://localhost:5173`，`/api` 经 vite proxy 到 gateway（同源无 CORS）。改 Vue/TS 代码即时热替换，无需重编 gateway。**注意**：dev 走 vite proxy，prod 走 gateway rust-embed 直连——两者代理路径不同，SSE 行为可能有差异（prod 已验证实时，dev 若缓冲见 `vite.config.ts` 注释）。
- **prod 模式（验真实嵌入）**：`cd frontend && npm run build` 产 `dist/` → `cargo build -j 2 -p rsiot-gateway` 重编嵌入 → 起三件套，浏览器访问 gateway :8080。**验前端改动必须走此模式**（dev 模式验不到 rust-embed 嵌入 + SPA fallback）。

**rust-embed 重建陷阱**（必读）：`crates/rsiot-gateway/src/static_assets.rs` 用 `rust-embed` 在编译期把 `frontend/dist/` 嵌入二进制。改 `frontend/dist/` 后 **rust-embed 不会自动感知**（它按 cargo 的 crate 重编触发，不监听 dist 文件变化）——必须 `touch crates/rsiot-gateway/src/static_assets.rs`（或改任意 gateway .rs 文件）强制重编 gateway，否则跑的是旧前端。症状：`npm run build` 成功但浏览器看不到改动。一行修复：`touch crates/rsiot-gateway/src/static_assets.rs && cargo build -j 2 -p rsiot-gateway`。

**前端验证规则**（改 frontend/ 后）：
- `cd frontend && npm run build`（vue-tsc 类型检查 + vite 构建，TS 类型错误会 fail）
- `cd frontend && npm run test -- --run`（vitest 单测，16 文件 203 测，2026-10-27 实测）
- 验嵌入：`touch crates/rsiot-gateway/src/static_assets.rs && cargo build -j 2 -p rsiot-gateway`（强制重嵌入）+ 起三件套浏览器验
- 前端改动**不碰**主 crate / rsiot-mcp（硬约束：gateway 是唯一对接点，经 REST 反代 + rust-embed 静态资源）

### Feature 裁剪（内置协议可选）

默认编译面（2026-08-26 用户裁决，见 decisions/2026-08-26-default-compile-surface.md）：
**默认 = `lua` + `lua-http` + `opcua` + `modbus`**；`s7`/`eip`/`transform`/`history` **出默认**，需显式 `--features`。
未编译某协议时建该 `app_code` 实例返回 "source plugin not found"（优雅降级，非崩溃）。现场可按需裁剪——减二进制体积、省系统依赖（如不跑 Modbus RTU 的 Linux 机器可不装 `libudev-dev`）：

```bash
cargo build --no-default-features --features lua                     # 最小现场（仅 lua 脚本采集）
cargo build --no-default-features --features "lua,modbus,opcua"      # 选装部分协议
cargo build --features "s7,eip"                                      # 加装西门子/AB（出默认，需显式）
```

关掉某协议后，配置该 `app_code` 建实例会返回 `"config error: source plugin not found"`（`find_source` 优雅降级，非崩溃）。`tokio-serial` 因 lua 串口 IO 依赖而**保持无条件**（不随 `modbus` gate）。CI `minimal` job 守护裁剪组合可编译 + 优雅降级测试（`tests/feature_gate_e2e.rs`）。详见 `docs/decisions/2026-08-07-feature-gating-protocols.md`。注意：`eip`（rust-ethernet-ip）要求 **rustc ≥ 1.96**（workspace MSRV 随之提升）。

## Skill 优先使用（Rust 开发）

处理 Rust 相关问题前，先调用对应 skill 加载规范，再动手：

| 场景 | Skill | 说明 |
|---|---|---|
| **写/改/审任何 Rust 代码** | `rust-best-practices` | Apollo GraphQL 最佳实践手册（9 章：borrowing/clippy/性能/错误处理/测试/泛型/类型状态/注释/Send-Sync）。动手写代码前调用 |
| **编译错误**：所有权/借用/生命周期（E0382/E0597/E0502/E0106 等，"value moved"、"does not live long enough"） | `m01-ownership` | CRITICAL 级——报错后先调用，再改 |
| **错误处理决策**：`?`/unwrap/expect/panic、Result vs Option、thiserror/anyhow 分工 | `m06-error-handling` | CRITICAL 级——涉及错误传播方式时先调用 |
| **Rust 问题路由/对比/最佳实践**：tokio vs、crate 选型、"怎么用/为什么/区别" | `rust-router` | 任何 Rust 疑问先路由到此，它会分派到对应章节 |
| **代码审查/PR review/质量检查** | `code-review-skill` | 含专门 Rust review 指南（reference/rust.md：所有权/借用、unsafe 审查、异步取消安全性、错误处理） |
| **通用质量流程**（系统级，SessionStart 已注入）：写 feature/bugfix 前 TDD（`superpowers:test-driven-development`）、遇 bug 先系统化调试（`superpowers:systematic-debugging`）、声称完成前验证（`superpowers:verification-before-completion`） | superpowers 系列 | 与上面 Rust 专属 skill 叠加使用，不冲突 |

优先级：**编译错误/错误处理先走 m01/m06 → Rust 写作规范走 rust-best-practices → 审查走 code-review-skill**。这些 skill 与本文件"编码约定/验证规则"冲突时，以本文件（项目实际约束）为准。

## 编码约定

- **注释用中文**（代码与文档一致），doc 注释必须完整——`missing_docs` 是 warn，库公开项都要有 `//!`/`///`。
- **错误处理**：库公开 API 统一返回 `Result<_, RsIotError>`（`TResult`），新错误来源在 `error.rs` 加 `#[from]` 变体；`#[non_exhaustive]` 保证新增变体不破坏下游 match。binary 层（main/run/service）才用 anyhow 聚合。
- **newtype ID**：`Hwid`/`InstId`/`UserId`/`PluginName` 隔离不同语义的字符串 ID，在 `types.rs` 用 `impl_string_newtype!` 宏统一实现 `From`/`AsRef`/`Display`；新 ID 直接套用该宏。
- **`#[expect(...)]` 而非 `#[allow(...)]`**：如 `api/mod.rs:9`、`host.rs:62`——lint 不再触发时编译器会提示移除，杜绝永久豁免。`#![allow(dead_code)]` 全局豁免已于 2026-08 治理移除（存量 lua_core 有 16 处 `#[allow(dead_code)]`（预留未接线，待接线后转 expect））；新增代码若短暂存在未用项，用 `#[expect(dead_code)]` + 注释，勿恢复全局 allow。
- **日志**：`tracing`，关键入口函数加 `#[tracing::instrument(skip(...), fields(...))]`；插件 task 用 `[dataSim]`/`[pipeline]`/`[mqtt]` 前缀便于过滤。模块内错误用 `tracing::warn!` 不 panic（如 pipeline 写库失败、mqtt 连接错误均降级继续）。
- **插件扩展**：实现 `Source`/`Sink`/`Processor` trait，在文件底部 `inventory::submit!` 静态注册（`registry.rs` 的 `SourceEntry { name, create }`）；插件配置从 `InstConfig::app_config`（任意 JSON）自行解析（参考 datasim 的 `period` 字段）。
- **异步生命周期**：所有长驻 task 用 `CancellationToken`（`shutdown.child_token()`）退出，`tokio::select!` 中监听；启停 Source 由 `InstanceManager` 统一管（DashMap 状态表 + JoinHandle，stop 3s 超时）。
- **数据流边界**：南向产出推**宿主注入的 `SampleSender`**（`Deref` 到 `mpsc::Sender<Sample>`，转发前登记 dev2inst 归属；**禁止自建裸 hub sender**）；实例间命令走 `CtlRegistry::send`（非阻塞 `try_send`，channel 满丢弃——对标 Go 版 default 语义，勿改阻塞）。
- **channel 容量原则**：数据面发送端可 `await` → 有界（hub 512、worker 256）；控制面 `try_send`
  → 有界且满丢计数（CtlRegistry 64）；仅 notify 回调线程允许 unbounded 并须在注释写明理由
  （唯一现存例外：`hotload.rs` 事件管道）。
- **进程级单例（OnceLock）登记制**：新增 `OnceLock` 单例须在 `docs/architecture.md` §四登记，
  并说明测试间是否需复位（现有 5 处：model/cache、model/discovery、history/engine、
  db/secrets、process_metrics）。

## 验证规则

**一键门禁**：`just check`（= fmt + docs + clippy 三档 + 单测/裁剪档 e2e；本机实测全绿）。
分步：`just fmt` / `just docs` / `just lint` / `just test` / `just check-s7-eip`。

- **不要用 `--all-features` 当日常门禁**：`history` 依赖 `../tsink_fork` 真实仓库，stub 环境不可编译。
- 按改动面选精准验证集（插件/管道/历史/协议真机/REST 端点/数据流端到端/前端 rust-embed 陷阱）：
  **完整清单见 [docs/guides/testing-and-verification.md](docs/guides/testing-and-verification.md)**。
- 已知豁免（不视为失败）：Windows Service `sc stop` 退出码 1066；`lua_north_e2e` 生命周期用例
  需本地 MQTT broker（无则显式 SKIP）。
- 真机 e2e 环境：仓库根 `FIELD_E2E_LOCAL.md`（gitignore，不入库）。

## 常见流程

- **新增 API 路由**：在 `src/api/` 加 `routes_*.rs` handler → `api/mod.rs` `build_router` 注册路由 → 需要免鉴权则在 `auth.rs` `PUBLIC_PATHS` 加白名单（默认全部要 JWT 或 `X-Access-Key`）→ DTO 与 state 从 `AppState`（lux client + mgr + hwid + jwt_secret）取。
- **新增 lux Hash 表/字段**：先更新 `db/identity.rs:3-20` 的清单注释 → 读写逻辑放对应模块（用户→routes_user、实例→routes_inst、rtdata→`db/rtdata.rs`）→ 初始数据在 `identity::bootstrap` 幂等写入（用 `HEXISTS` 判断，不覆盖已有记录）。
- **新增南向/北向插件**：`plugin/builtin/` 新文件实现 trait + `inventory::submit!` 注册 → `types::InstConfig` 的 `app_code` 字符串匹配注册表名 → 默认数据流靠 `lib.rs` 的 demo config（`try_start_from_lux` 读到 `instances` 表 autoStart 实例则优先拉起）。
- **新增外部南向插件**（任意语言）：外部进程实现 bridge 协议（`docs/reference/external-plugin-protocol.md`）→ rs-iot 侧 REST 建实例 `app_code="sidecarClient"` + `host_kind="external"` + `app_config{heartbeat_timeout_secs?}` + devices（`routes_inst::set` 自动签发 token 返回明文一次）→ `startinst`（注册 slot 等外部接入）→ 外部进程连 bridge 报到。无需改 rs-iot 代码（外部协议地址语义由外部自解，rs-iot 透传 children）。
- **新增写命令入口**：调 `InstanceManager::send_ctl(inst, CtlMessage)`（经 `CtlRegistry` 路由到实例 `cmd_rx`，未注册返回 `false`）。现有入口：MQTT `<hwid>/command`（阶段 B 起经 luaNorth 兼容脚本 `down()` → `ctx:send_command`；进程内订阅路由在 `lua_north/`）、REST `POST /api/v1/command/send`（`routes_command.rs`）。写命令 `data` 字段格式由各南向插件 `handle_write` 自定（opcua `{node_id,value,opc_type}`、modbus `{address,value}`）。
- **新增 config 字段**：`config.rs` 对应 struct 加 `#[serde(default)]` 字段 + `Default` 实现（缺失字段容错是既有约定）。
- **新增二进制行为**：`main.rs` 子命令分发 → 逻辑在 `lib.rs`（前台）或 `service/`（服务态）——前台与服务态共用 `run_app_with_shutdown`，shutdown 来源不同（ctrl_c vs SCM/sigterm）。
- **实例自愈可信（M3）**：`start_source`/`start_sink` spawn source/sink task 后，再 spawn 一个 supervisor task（`spawn_supervisor`）唯一 await 其 join，task 退出且 `token` 未 cancel → 标 `InstState::Failed` + `error!`（覆盖 panic/配置错误早退/异常 return）；主动 `stop` 走 Stopping→Stopped（token cancel）不误判。状态仅内存（进程重启重置）。不自重启（后置）。`GET /api/v1/inststatus` 返回 Failed 实例；运维 `startinst` 重拉（Failed→Starting→Running）。**注意**：对 Failed 实例调 `stopinst` 会把状态改写为 Stopped（stop 总以 Stopped 收尾，操作员 stop 视为确认）；Failed 的 `error!` 日志已落盘，状态查询窗口在 stop 前可见。
- **新增/修改插件埋点**：遵循 `docs/conventions/instrumentation-conventions.md` 6 约束。内置插件用 `StatTracker`（select! 加 flush ticker + tick_ok/fail）；进程级部件用 `ProcessMetrics`（atomic，/sysstatus 直读）。实例指标统一查 `GET /api/v1/inst/stats`（`{inst_id, app_code, host_kind, state, health}`，废弃的 `/api/v1/lua/stats` 已移除）。进程级 + per-instance 指标另经 `GET /metrics`（Prometheus exposition 文本，免 JWT）对接监控栈——同一 M4 数据源的机器出口（vs `/sysstatus`/`/inst/stats` 的 JSON 人查出口）。
- **lua 脚本链路感知**：luaClient 通讯链路由宿主 IO task 管（open/connect/收发/重连/trace，脚本不碰），脚本只经 `ctx:request(frame, timeout)` 组帧/解帧。`ctx:transport()` 返回底层链路类型（`"tcp"`/`"serial"`/`"udp"`，与 `app_config.kind` 一致），脚本据此选帧格式（serial→PDU+CRC16、tcp→MBAP 头）。当前三种传输都是 client/主站侧；**TCP/UDP Server 不支持**（与请求-响应模型冲突，后置，详见 `docs/guides/lua-script-guide.md`「链路角色」）。一个实例一条链路；同链路多从站由脚本 `collect()` 循环轮询（M2 多设备输出）。

## 文档管理

完整规范见 `docs/conventions/documentation.md`（本文档与它冲突时以规范为准）。要点：

- **六类分域**：`docs/guides/`（怎么用/怎么开发）、`docs/reference/`（API/协议是什么）、`docs/conventions/`（必须遵守的约束）、`docs/decisions/`（ADR：为什么选 X）、`docs/designs/`（设计定稿）、`docs/ops/`（部署/现场/维护）。新文档先选域再落笔。
- **命名**：活文档（指南/参考/规范/运维）用语义名 kebab-case；一次性文档（designs/decisions）用 `YYYY-MM-DD-<topic>[-design].md` 日期前缀。
- **头部元数据与实施块**：新文档必须有状态块；designs/decisions 用完整块（状态/日期/实施 commit/代码/测试/遗留）——`implemented`/`partially-implemented` 必填实施与代码字段，部分实施必列遗留。
- **索引义务**：新增/移动/归档文档必须同步更新 `docs/README.md`（主题链路视图 + 分域索引）与本文件（项目地图）中的引用。
- **校验**：改文档后跑 `just docs`（= `python3 scripts/check_docs.py`）。校验项：状态块/实施块/索引对齐/链接有效性/
  **正文引用的 `src|tests|crates` 路径必须存在**/`docs/plans/` 不得复活；红了按报错修。
- **取代回写**：架构级替换（模块搬迁/机制换型）后旧文档必须回写 `superseded` + `**取代者**` 指针，禁止旧文档与新事实并存。
- **归档**：过时文档 `git rm`（git 历史即归档），提交注明 `chore(docs): 归档…`；**归档/移动前必须清理全部引用**（CLAUDE.md、README.md、docs/README.md、源码 `//!`、examples、Cargo.toml 注释）。

## 安全边界

- **`data/`、`target/`、`logs/`、`*.log`、`pki/` 已被 .gitignore 忽略，不要提交**；**`gateway.toml` / `gateway.toml.bak` / `config.toml` 含 jwt_secret 与 serve_token，禁止提交真实值**；`data/` 含 lux 快照 + WAL，删除即丢数据。
- **`jwt_secret` 与 `MqttConfig::username/password` 不得写真实凭据进仓库**：config.toml 中 jwt_secret 是开发默认值（与 Lite_IOT 对齐），生产部署必须换；mqtt 公共 broker 仅开发验证用。
- **lux 只 bind loopback**（`db/mod.rs` 中 `bind_host` 默认 127.0.0.1 + `require_auth=false`）。若要远程暴露 RESP，必须设 `[lux] password`（`require_auth` 由代码按 password 有无自动派生，无配置项）——这是认证边界，改动需显式确认。
- **`db::shutdown_lux` 必须先 SAVE 再 `shutdown_and_wait`**：lux 的 shutdown 默认不落盘（Memory 模式丢数据、Tiered 模式 WAL 不截断）。Tiered 下 `save_interval_secs` 也不能为 0（WAL 无限增长）。改 lux 交互代码时必须守住这两条。
- **外部插件 bridge 安全**：bridge `bind_host` 默认 `127.0.0.1`（同机零暴露），跨机部署显式改 `0.0.0.0` 且 token 强制（acceptor 每连接校验，不可关）。外部插件 token 明文仅建实例时返回一次（存 `hmac_sha256` hash 于 `ext_tokens`），删实例自动清除——不得写真实 token 进 config.toml/日志。lux 永不对外部暴露（方案 C），外部只连 bridge。PoC 不上 TLS（局域网信任），跨机敏感场景 TLS 后置。
- 输出信息：`hwid`（每机唯一标识）可在日志/API 中正常出现；不输出 bcrypt 哈希、JWT token 明文、lux `passwords` 表内容。

## 交付格式

每项任务完成时按此格式汇报：

1. **改动总结**：改了什么模块、为什么（一句话/条）。
2. **验证命令**：实际跑过的命令及其结果（fmt/clippy/test 输出；端到端验证的具体数据观察）。
3. **风险与未覆盖点**：已知限制（1066 退出码、cargo audit advisory、lux 无法 feature 裁剪）、未跑通的场景、需人工确认的配置（jwt_secret 更换、私有 broker 接入）。
