# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目地图

轻量跨平台工业 IoT 边缘框架（Rust），Go 项目 [Lite_IOT](D:\Go_Codes\Lite_IOT) 的 Rust 重写与增强。内嵌 [lux](E:\github.com\lux) 作为进程内 Redis 兼容数据库（RESP :6390 + Tiered WAL 持久化）。

**数据流（hub 模式 + 双总线）**——这是理解全项目的关键。**两条总线**：数据上行（采集→入库）+ 控制下行（写命令→设备）：

```
── 数据上行总线 ─────────────────────────────────────────────────────────
N × Source(dataSim / opcuaClient / modbusClient / s7Client / sidecarClient[外部]) ──► hub (mpsc 256) ──► pipeline (Processor) ──► lux HSET rtdata:<devid>
   └─ 读出口（都从 lux rtdata 取最新值）：GET /api/v1/rtdatas / MqttPublisher pull / 未来北向 UA Server·Modbus slave·104
── 控制下行总线（CtlRegistry，多源写入口 → 实例 cmd_rx）──────────────────
MQTT <hwid>/command 订阅 ─┐
POST /api/v1/command/send ─┼─► CtlRegistry.try_send(64) ──► 实例 Source::run 的 cmd_rx ──► handle_write（opcua/modbus...）
未来北向 UA Server·DA·Modbus slave·104 写回调 ─┘
管理面：REST :8790 (axum + JWT) ──► InstanceManager（动态启停 Source）/ send_ctl（投递写命令）
```

- **南向采集与北向发布完全解耦**：pipeline 只写 lux rtdata，`MqttPublisher`（`src/plugin/builtin/mqtt_pub.rs`）独立周期从 lux 抽取发布（pull 模式）。lux rtdata 是唯一数据交汇点（single source of truth）。
- **写命令多源统一**：MQTT / REST / 未来北向服务器都是写入口，都经 `CtlRegistry`（`src/host.rs`，`try_send` 非阻塞，channel 满丢弃——对标 Go 版 default）路由到实例 `cmd_rx`。REST 入口 `InstanceManager::send_ctl`（`src/plugin/instance.rs`）封装，实例未注册返回 404。
- **单 Source 启停不影响管道**（共享 hub，pipeline 单消费者；各 Source 独立 `CancellationToken`）。
- **外部插件 bridge（sidecarClient，M7）**：任意语言（C/Go/Python/Java…）的外部进程作为南向 Source 接入——TCP 连 bridge（`config.bridge`，默认 :8781，`enable=false` 默认零影响），acceptor 校验 token（lux `ext_tokens` 的 `hmac_sha256` 比对）后按 inst_id 分发到实例 task；外部 `DataUp`→`Sample`→hub（与内置 Source 同构），写命令经 `cmd_rx`→`Command` 帧→外部→`CommandAck`。**外部=纯运行时**（配置/数据/状态全在 rs-iot，本地仅缓存数据待补传），**lux 不对暴露**（方案 C：外部只连 bridge，认证/授权集中 rs-iot）。`InstConfig.host_kind=External` 标记 + 准入约束（token 只对 External 签发）。协议见 `docs/external-plugin-protocol.md`，Python 样例 `examples/ext-plugin-python/`。
- 架构原则（分层 + 双总线 + 横向扩展边界）见下文「架构原则」节。

**模块职责**（`src/`，按数据流顺序）：

| 模块 | 职责 |
|---|---|
| `lib.rs` | 库入口。`run_app_with_shutdown` 编排全链路：lux 启动 → identity bootstrap → hub/InstanceManager/MqttPublisher/pipeline/axum 全部挂到 `CancellationToken` |
| `main.rs` | CLI 入口（run / install / uninstall / service 子命令），clap 解析 |
| `config.rs` | `AppConfig`：toml 加载，缺失字段走 Default（api_port 8780 / lux 6390 / mqtt 5s） |
| `error.rs` | `RsIotError`（thiserror + `#[non_exhaustive]`）+ `TResult<T>` 别名 |
| `types.rs` | 数据模型 + newtype ID（见编码约定） |
| `host.rs` | `HostContext`（lux client + hwid + shutdown + `CtlRegistry` 命令路由表 + `BridgeRegistry` 外部连接分发），注入每个插件 task |
| `plugin/` | `traits.rs`（Source/Sink/Processor 契约）、`registry.rs`（inventory 静态注册）、`instance.rs`（生命周期管理 + `send_ctl` 写命令投递）、`builtin/`（datasim 模拟采集 / opcua_client OPC UA 采集 / modbus_client Modbus TCP/RTU(serial/over-tcp/over-udp) 采集 / s7_client 西门子 S7 全系列采集 / mqtt_pub 北向发布 / **sidecar_client 外部插件 bridge**） |
| `pipeline.rs` | hub 消费者：Processor 转换 → `db::rtdata::write` |
| `transform/` | `passthrough`（默认）/ `rhai`（feature `transform`，默认开启） |
| `db/` | `build_lux_config`、`shutdown_lux`（先 SAVE）、`identity::bootstrap`（幂等建 admin）、`rtdata::write`、`ext_token::issue/verify/revoke`（外部插件 token，hmac_sha256） |
| `api/` | axum 路由 + JWT 中间件（auth.rs）+ 各 routes_*.rs + rust-embed 静态前端 |
| `service/` | Windows Service（`windows-service` dispatch）/ systemd unit 生成 |
| `hwid.rs` | `eiot` 前缀 + sha256(CPU brand + ARCH) 取 8 位，每机稳定 |

**lux Hash 布局**（对标 Lite_IOT `bRedka`，清单见 `src/db/identity.rs:3-11`）：
`users` / `passwords`(bcrypt) / `groups` / `instances` / `devices` / `children` / `accesskey` / `ext_tokens`(外部插件 token: field=instId, value=JSON{token_hash,created_at,enabled}) / `rtdata:<devId>`（field=`<devid>.<tag>`，value=`[ts, localTime, value]` JSON）。

**环境依赖**：
- `lux = { path = "../lux" }` — 需 lux 源码在**同级** `E:\github.com\lux`；修改 lux 会直接影响本项目的编译与行为（EmbeddedClient 无 Debug、`ServerConfig` 无 Builder 等特性已被本项目代码 `expect` 依赖）。
- `D:\Go_Codes\Lite_IOT` — 对标参考实现，新功能常按"Lite_IOT 的 X 模块"定位。
- 端口：lux RESP 6390（避开本机 redis 6379）、REST 8780（config.toml 中 8780 被占用临时改 8790）、MQTT 公共 broker broker.emqx.io:1883（开发验证用）。
- 日志：`tracing` + `tracing-subscriber` + `tracing-appender`（tokio-rs 官方）。终端输出始终开启；`config.toml` 的 `[log].dir` 非空时额外写文件（`<dir>/rs-iot.log.<date>` daily 轮转，non_blocking 专用线程）。级别优先 `RUST_LOG`，缺省 `[log].level`（默认 info）。opcua/modbus 采集/连接/读写均有 `[opcuaClient]`/`[modbusClient]` 前缀日志。

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

**低耦合五机制**：① trait 契约（Source/Sink/Processor，框架依赖抽象）② channel 解耦（hub 256 / cmd 64，异步边界 + 背压）③ lux 作进程内数据总线（RESP 契约，上下游无直接引用）④ `inventory` 静态注册（编译期发现，无注册顺序依赖）⑤ `CancellationToken` + `try_send`（独立启停 + 控制面不阻塞数据面）。

## 常用命令

```bash
cargo build                       # debug
cargo build --release             # release（lto=thin + strip）
cargo test                        # 全量（CI 用 --all-targets）
cargo test <name>                 # 单个测试（如 cargo test hwid）
cargo clippy --all-targets -- -D warnings   # CI 门槛，零警告
cargo fmt --all --check           # 格式检查
cargo doc --no-deps               # 文档（RUSTFLAGS=-D warnings 下零警告）
cargo audit                       # 依赖安全（CI 中 continue-on-error：传递依赖 rustls advisory 已知）

./target/debug/rs-iot --config config.toml   # 前台运行
RUST_LOG=debug ./target/debug/rs-iot         # 日志级别（缺省 info）
rs-iot install / uninstall        # 服务化（Windows: sc create / Linux: systemd）
```

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
- **`#[expect(...)]` 而非 `#[allow(...)]`**：如 `api/mod.rs:9`、`host.rs:62`——lint 不再触发时编译器会提示移除，杜绝永久豁免。`#![allow(dead_code)]` 全局豁免已于 2026-08 治理移除（当前代码零 dead code）；新增代码若短暂存在未用项，用 `#[expect(dead_code)]` + 注释，勿恢复全局 allow。
- **日志**：`tracing`，关键入口函数加 `#[tracing::instrument(skip(...), fields(...))]`；插件 task 用 `[dataSim]`/`[pipeline]`/`[mqtt]` 前缀便于过滤。模块内错误用 `tracing::warn!` 不 panic（如 pipeline 写库失败、mqtt 连接错误均降级继续）。
- **插件扩展**：实现 `Source`/`Sink`/`Processor` trait，在文件底部 `inventory::submit!` 静态注册（`registry.rs` 的 `SourceEntry { name, create }`）；插件配置从 `InstConfig::app_config`（任意 JSON）自行解析（参考 datasim 的 `period` 字段）。
- **异步生命周期**：所有长驻 task 用 `CancellationToken`（`shutdown.child_token()`）退出，`tokio::select!` 中监听；启停 Source 由 `InstanceManager` 统一管（DashMap 状态表 + JoinHandle，stop 3s 超时）。
- **数据流边界**：南向产出只推 `mpsc::Sender<Sample>`（hub）；实例间命令走 `CtlRegistry::send`（非阻塞 `try_send`，channel 满丢弃——对标 Go 版 default 语义，勿改阻塞）。

## 验证规则

- 改 `src/` 任意代码：`cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features` 三项全绿才算完成（与 CI 门槛一致）。
- 改插件/管道/类型：跑对应单测（datasim/hwid/rhai/mqtt_pub/opcua_client 已有），新增逻辑补 `#[cfg(test)]` 模块内单测。
- **opcua_client 端到端**（依赖本机 UA Server 在线 `opc.tcp://127.0.0.1:49320`，CI 不跑）：REST 建实例 `app_code="opcuaClient"` + `app_config{endpoint, security_policy, period, verify_server}` + `devices[].children[].value=NodeId`（如 `ns=2;s=v1`）→ `startinst` → `redis-cli HGETALL rtdata:<devid>` 验证采集；写命令经 `<hwid>/<devid>/command` JSON `{"cmd":"write","data":"{\"node_id\":\"...\",\"value\":123,\"opc_type\":\"Int32\"}"}`。**`opc_type` 在 write 时强烈建议带**：OPC UA server 严格类型匹配（Int32 节点写 Int64 会 BadTypeMismatch），支持 Boolean/SByte/Byte/Int16/UInt16/Int32/UInt32/Int64/UInt64/Float/Double/String（缺省 Float→Double、Int→Int64）。**`verify_server` 默认 false**：跳过 server 证书 hostname/application-uri/有效期校验（仅检查证书被信任+密钥长度），便于用 IP/loopback 连接（server 证书常不含这些 alt name）；生产环境 endpoint 与证书匹配时设 true。`security_policy` 支持 `none` / `Basic256Sha256`（签名并加密；客户端自签证书在 `pki/own/cert.der`，需导入 UA Server 信任列表）。注意 server 端节点须对当前用户开放写权限，否则 BadUserAccessDenied（与 rs-iot 无关）。
- **modbus_client 端到端**（TCP / RTU serial / RTU over TCP-UDP，CI 不跑）：REST 建实例 `app_code="modbusClient"` + `app_config{kind, endpoint|serial_port, slave_id, period, gap_threshold?, max_regs?, reconnect_secs?}` + `devices[].children[].value=地址串` → `startinst` → `redis-cli HGETALL rtdata:<devid>` 验采集。**四种传输**（扁平 `kind`）：`tcp`(默认，`endpoint`) / `rtu_tcp`(`endpoint`，RTU 帧+CRC 走 TCP 无 MBAP 头) / `rtu_udp`(`endpoint`，connected socket+datagram 边界保帧) / `rtu_serial`(`serial_port` + `baud_rate`/`data_bits`/`parity`/`stop_bits`/`flow_control`，缺省 9600/8N1/NONE；tokio-serial，**Linux 编译需 libudev-dev**，CI 已 apt 装)。**地址串** `<func>:<addr>:<type>[:<order>][:<bit>]`：func=`holding`(FC03)/`input`(FC04，只读)；addr=0-based 寄存器号（**不做 40001 自动转换**，PLC tag 换算 holding=tag-40001/input=tag-30001）；type=`bit`/`i16`/`u16`/`i32`/`u32`/`f32`/`f64`；order（i32/u32/f32/f64 可选，默认 ABCD）=`ABCD`/`CDAB`/`BADC`/`DCBA`；bit（bit 类型必填）=0-15。**字节序** 4 种均为对合置换（`perm4[perm4[i]]==i`），单测 round-trip 锁定（f32 12.5=0x41480000 的 ABCD 表示 `[0x4148,0x0000]`）。**批量合并** `gap_threshold` 约束相邻两点**最大单空洞**（非未用总数；需求"读 80 用 50"按最大单空洞调），`max_regs` 约束单区间最大寄存器数（默认 100，协议上限 125，遇 IllegalDataAddress 往下调）。**写命令** 经 `POST /api/v1/command/send`（REST，推荐）或 MQTT `<hwid>/command`，data JSON `{"address":"holding:0:f32:ABCD","value":12.34}`：u16/i16/bit→FC06，i32/u32/f32/f64→FC16，bit 走 RMW（读-改-写，单 task 内原子但非事务，部分失败仅 warn），`input` 只读拒绝。**传输边界**：TCP/RTU(serial/over-tcp/over-udp) 四种统一 `Context`（`rtu::attach_slave` transport 泛型 `AsyncRead+AsyncWrite+Unpin+Send`），采集/写逻辑同构；ASCII 暂不支持（tokio-modbus 无，未来可借 `rmodbus` codec 后置）。实例级 slave_id（多 RTU 从站网关需每 slave 建一实例，共享 endpoint）。RTU over UDP 用 connected socket（datagram 边界天然保帧，无流式粘包）。tokio-modbus 0.17 read/write 返回**双层 Result**（外层 transport `Error` / 内层 `ExceptionCode`），exception 不重连、transport 失败拆 session 重连。满足现场三类需求：① 布尔打包进 FC03/FC04 寄存器按位解析（`bit` 类型）② 混合类型批量读+偏移转换（`merge_regions`）③ 大块读取部分使用（`gap_threshold`/`max_regs`）。
- **s7Client 端到端**（西门子 S7，依赖真实 PLC 或仿真器，CI 不跑）：REST 建实例 `app_code="s7Client"` + `app_config{endpoint, plc_type?, rack?, slot?, period?, timeout_ms?, auto_reconnect?}` + `devices[].children[].value=S7 地址串` + `children[].type=kind`（**强制显式**）→ `startinst` → `redis-cli HGETALL rtdata:<devid>` 验采集。**plc_type**：`s200`/`s200smart`/`s300`/`s400`/`s1200`/`s1500`/`sinumerik828d`（缺省 `s1200`；`rack`/`slot` 缺省走 plc_type 默认——S200/200Smart/1200/1500=(0,1)、S300=(0,2)、S400=(0,3)；端口固定 102）。**地址串**（`DB1.DBD0`/`M10.0`/`I0.0`/`V100`/`Q100`）：tokio-s7 解析器按"位置 + 数字过滤"，**类型前缀 `X/B/W/D/DBX/DBB/DBW/DBD` 纯装饰被忽略**——`DB1.DBD0`/`DB1.DW0`/`DB1.0` 等价，**位宽完全由 kind 决定**，故 `ChildSpec.type` 必填。区首字母仅识别 `I`/`Q`/`M`/`D`/`V`/`T`/`C`（`V`→DB1 固定映射）。位寻址靠点段数（`DB1.X0.0` 3 段 / `M10.5` 2 段，位地址 0-7 必配 `kind:"bool"`，配数值 kind 会读到字节 `M10`）。**kind**：`bool`/`byte`/`i16`/`u16`/`i32`/`u32`/`i64`/`u64`/`f32`/`f64`（大端，S7 原生；小端 `_le` 后置）；`bool` 走 `parse_bit`，其余 `parse_byte(count)`（count=字节数）。**批量 read** 一次 PDU 多点（库自动 batching），`DataItem.return_code != 0xFF` 的项 warn 跳过（不报 Err，对标 modbus exception）。**写命令** 经 REST `POST /api/v1/command/send` 或 MQTT `<hwid>/command`，data JSON `{"address":"DB1.DBD0","value":42.5,"kind":"f32"}`（kind 缺省按 value 推断：bool→Bool/浮点→F64/整数→I64；typed `write_*` 内部处理大端编码）。**v1 边界**：`string`/`wstring` 读写后置（S7 string 有 2 字节头 max_len/actual_len，批量路径与数值不一致；v1.1 单点支持）；counter/timer（`C`/`T` 区）后置；S7Comm Plus（新版加密 S7-1500）不支持（走西门子官方 OPC UA）。**tokio-s7 极年轻**（2 月/18 commit/单作者/无 CI/无集成测试），生产前必实测真机断连恢复 + 长稳；`auto_reconnect` 行为不透明，插件层 `S7Error::is_connection_error()` + drop client 下个 tick 重连兜底（对标 opcuaClient `session=None`）。
- **REST 写命令端点**（`POST /api/v1/command/send`，`routes_command.rs`，默认 JWT 保护）：body `{instance, devid, cmd, data}` → `InstanceManager::send_ctl` → CtlRegistry → 实例 cmd_rx。与 MQTT `<hwid>/command` 同通道，opcua/modbus 写命令均可走 REST（不必依赖 broker）。实例未运行返回 404。
- **集成测试在 `tests/`**：`dataflow.rs`（datasim→hub→pipeline→rtdata 端到端）、`api_flow.rs`（axum oneshot + JWT 鉴权）、`config_load.rs`（配置容错）、`sidecar_e2e.rs`（bridge 完整闭环：Hello/ConfigPush/DataUp→rtdata/写命令→Command→Ack/错误 token 拒绝）。改数据流/REST 鉴权/配置语义/bridge 协议后跑 `cargo test --test <name>` 对应文件。新增集成测试用 `tests/common::start_lux`（**端口 0 由 OS 分配 + 独立临时 data_dir**），勿固定端口、勿写仓库 `./data/`；测试代码同样受 CI `RUSTFLAGS=-D warnings` 约束，保持零警告。
- **sidecarClient 端到端**（CI 内，`tests/sidecar_e2e.rs`，无需真实外部进程——进程内模拟客户端）：起 lux + bridge acceptor（127.0.0.1:0）+ sidecarClient External 实例 + 模拟外部 TCP 客户端 → Hello(token)→HelloAck+ConfigPush→DataUp→验 `HGET rtdata:<devid>`→send_ctl→收 Command→回 Ack；错误 token→HelloAck{ok:false}。**真实多语言验证**用 `examples/ext-plugin-python/`（见其 README）+ `examples/ext-plugin-opcda-go/`（OPC DA，Go + huskar-t/opcda，连 `Matrikon.OPC.Simulation.1`：周期 `SyncRead`→DataUp、Command→`item.Write`，写值按 `GetCanonicalDataType` 自动转换；Windows-only 因 opcda 依赖 COM）。
- 改 lux Hash 读写：`cargo test` 保证编译，但 Hash 结构变更需**端到端验证**（见下）。
- **端到端验证**（改数据流/API 后必做，README 有完整示例）：
  ```bash
  ./target/debug/rs-iot --config config.toml &
  redis-cli -p 6390 PING                    # → PONG
  redis-cli -p 6390 HGETALL rtdata:devA     # 采集→管道→入库链路
  mosquitto_sub -h broker.emqx.io -t '+/devA/datas' -v   # 北向发布链路
  # REST：login 拿 token → 带 Authorization 调各 API（/sysinfo、/rtdatas）
  ```
- 已知失败不视为通过：Windows Service `sc stop` 后退出码 1066（ERROR_EXCEPTION_IN_SERVICE）是已知限制，但 lux SAVE 已先执行、数据完整——验证时确认数据可恢复而非进程退出码。

## 常见流程

- **新增 API 路由**：在 `src/api/` 加 `routes_*.rs` handler → `api/mod.rs` `build_router` 注册路由 → 需要免鉴权则在 `auth.rs` `PUBLIC_PATHS` 加白名单（默认全部要 JWT 或 `X-Access-Key`）→ DTO 与 state 从 `AppState`（lux client + mgr + hwid + jwt_secret）取。
- **新增 lux Hash 表/字段**：先更新 `db/identity.rs:3-11` 的清单注释 → 读写逻辑放对应模块（用户→routes_user、实例→routes_inst、rtdata→`db/rtdata.rs`）→ 初始数据在 `identity::bootstrap` 幂等写入（用 `HEXISTS` 判断，不覆盖已有记录）。
- **新增南向/北向插件**：`plugin/builtin/` 新文件实现 trait + `inventory::submit!` 注册 → `types::InstConfig` 的 `app_code` 字符串匹配注册表名 → 默认数据流靠 `lib.rs` 的 demo config（`try_start_from_lux` 读到 `instances` 表 autoStart 实例则优先拉起）。
- **新增外部南向插件**（任意语言）：外部进程实现 bridge 协议（`docs/external-plugin-protocol.md`）→ rs-iot 侧 REST 建实例 `app_code="sidecarClient"` + `host_kind="external"` + `app_config{heartbeat_timeout_secs?}` + devices（`routes_inst::set` 自动签发 token 返回明文一次）→ `startinst`（注册 slot 等外部接入）→ 外部进程连 bridge 报到。无需改 rs-iot 代码（外部协议地址语义由外部自解，rs-iot 透传 children）。
- **新增写命令入口**：调 `InstanceManager::send_ctl(inst, CtlMessage)`（经 `CtlRegistry` 路由到实例 `cmd_rx`，未注册返回 `false`）。现有入口：MQTT `<hwid>/command`（`mqtt_pub.rs` 订阅 + `serde_json::from_slice::<CtlMessage>`）、REST `POST /api/v1/command/send`（`routes_command.rs`）。写命令 `data` 字段格式由各南向插件 `handle_write` 自定（opcua `{node_id,value,opc_type}`、modbus `{address,value}`）。
- **新增 config 字段**：`config.rs` 对应 struct 加 `#[serde(default)]` 字段 + `Default` 实现（缺失字段容错是既有约定）。
- **新增二进制行为**：`main.rs` 子命令分发 → 逻辑在 `lib.rs`（前台）或 `service/`（服务态）——前台与服务态共用 `run_app_with_shutdown`，shutdown 来源不同（ctrl_c vs SCM/sigterm）。

## 安全边界

- **`data/`、`target/`、`*.log` 已被 .gitignore 忽略，不要提交**；`data/` 含 lux 快照 + WAL，删除即丢数据。
- **`jwt_secret` 与 `MqttConfig::username/password` 不得写真实凭据进仓库**：config.toml 中 jwt_secret 是开发默认值（与 Lite_IOT 对齐），生产部署必须换；mqtt 公共 broker 仅开发验证用。
- **lux 只 bind loopback**（`db/mod.rs` 中 `bind_host` 默认 127.0.0.1 + `require_auth=false`）。若要远程暴露 RESP，必须先开 `password` + `require_auth`——这是认证边界，改动需显式确认。
- **`db::shutdown_lux` 必须先 SAVE 再 `shutdown_and_wait`**：lux 的 shutdown 默认不落盘（Memory 模式丢数据、Tiered 模式 WAL 不截断）。Tiered 下 `save_interval_secs` 也不能为 0（WAL 无限增长）。改 lux 交互代码时必须守住这两条。
- **外部插件 bridge 安全**：bridge `bind_host` 默认 `127.0.0.1`（同机零暴露），跨机部署显式改 `0.0.0.0` 且 token 强制（acceptor 每连接校验，不可关）。外部插件 token 明文仅建实例时返回一次（存 `hmac_sha256` hash 于 `ext_tokens`），删实例自动清除——不得写真实 token 进 config.toml/日志。lux 永不对外部暴露（方案 C），外部只连 bridge。PoC 不上 TLS（局域网信任），跨机敏感场景 TLS 后置。
- 输出信息：`hwid`（每机唯一标识）可在日志/API 中正常出现；不输出 bcrypt 哈希、JWT token 明文、lux `passwords` 表内容。

## 交付格式

每项任务完成时按此格式汇报：

1. **改动总结**：改了什么模块、为什么（一句话/条）。
2. **验证命令**：实际跑过的命令及其结果（fmt/clippy/test 输出；端到端验证的具体数据观察）。
3. **风险与未覆盖点**：已知限制（1066 退出码、cargo audit advisory、lux 无法 feature 裁剪）、未跑通的场景、需人工确认的配置（jwt_secret 更换、私有 broker 接入）。
