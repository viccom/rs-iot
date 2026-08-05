# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目地图

轻量跨平台工业 IoT 边缘框架（Rust），Go 项目 [Lite_IOT](D:\Go_Codes\Lite_IOT) 的 Rust 重写与增强。内嵌 [lux](E:\github.com\lux) 作为进程内 Redis 兼容数据库（RESP :6390 + Tiered WAL 持久化）。

**数据流（hub 模式，pull 化）**——这是理解全项目的关键：

```
N × Source(dataSim...) ──► hub (mpsc 256) ──► pipeline (Processor) ──► lux HSET rtdata:<devid>
MqttPublisher（常驻独立 task）── 每 publish_interval_secs 周期 KEYS rtdata:* → HGETALL → PUBLISH <hwid>/<devid>/datas
REST :8790 (axum + JWT) ──► InstanceManager（动态启停 Source）
```

- 南向采集与北向发布**完全解耦**：pipeline 只写 lux rtdata，`MqttPublisher`（`src/plugin/builtin/mqtt_pub.rs`）独立周期从 lux 抽取发布（pull 模式，STEP 5 起不再走 pipeline fan-out）。Sink trait 已保留但无内置实现，v2 才启用 push 式北向。
- 单 Source 启停不影响管道（共享 hub，pipeline 单消费者）。

**模块职责**（`src/`，按数据流顺序）：

| 模块 | 职责 |
|---|---|
| `lib.rs` | 库入口。`run_app_with_shutdown` 编排全链路：lux 启动 → identity bootstrap → hub/InstanceManager/MqttPublisher/pipeline/axum 全部挂到 `CancellationToken` |
| `main.rs` | CLI 入口（run / install / uninstall / service 子命令），clap 解析 |
| `config.rs` | `AppConfig`：toml 加载，缺失字段走 Default（api_port 8780 / lux 6390 / mqtt 5s） |
| `error.rs` | `RsIotError`（thiserror + `#[non_exhaustive]`）+ `TResult<T>` 别名 |
| `types.rs` | 数据模型 + newtype ID（见编码约定） |
| `host.rs` | `HostContext`（lux client + hwid + shutdown + `CtlRegistry` 命令路由表），注入每个插件 task |
| `plugin/` | `traits.rs`（Source/Sink/Processor 契约）、`registry.rs`（inventory 静态注册）、`instance.rs`（生命周期管理）、`builtin/`（datasim / mqtt_pub） |
| `pipeline.rs` | hub 消费者：Processor 转换 → `db::rtdata::write` |
| `transform/` | `passthrough`（默认）/ `rhai`（feature `transform`，默认开启） |
| `db/` | `build_lux_config`、`shutdown_lux`（先 SAVE）、`identity::bootstrap`（幂等建 admin）、`rtdata::write` |
| `api/` | axum 路由 + JWT 中间件（auth.rs）+ 各 routes_*.rs + rust-embed 静态前端 |
| `service/` | Windows Service（`windows-service` dispatch）/ systemd unit 生成 |
| `hwid.rs` | `eiot` 前缀 + sha256(CPU brand + ARCH) 取 8 位，每机稳定 |

**lux Hash 布局**（对标 Lite_IOT `bRedka`，清单见 `src/db/identity.rs:3-11`）：
`users` / `passwords`(bcrypt) / `groups` / `instances` / `devices` / `children` / `accesskey` / `rtdata:<devId>`（field=`<devid>.<tag>`，value=`[ts, localTime, value]` JSON）。

**环境依赖**：
- `lux = { path = "../lux" }` — 需 lux 源码在**同级** `E:\github.com\lux`；修改 lux 会直接影响本项目的编译与行为（EmbeddedClient 无 Debug、`ServerConfig` 无 Builder 等特性已被本项目代码 `expect` 依赖）。
- `D:\Go_Codes\Lite_IOT` — 对标参考实现，新功能常按"Lite_IOT 的 X 模块"定位。
- 端口：lux RESP 6390（避开本机 redis 6379）、REST 8780（config.toml 中 8780 被占用临时改 8790）、MQTT 公共 broker broker.emqx.io:1883（开发验证用）。

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
- 改插件/管道/类型：跑对应单测（datasim/hwid/rhai/mqtt_pub 已有），新增逻辑补 `#[cfg(test)]` 模块内单测。
- **集成测试在 `tests/`**：`dataflow.rs`（datasim→hub→pipeline→rtdata 端到端）、`api_flow.rs`（axum oneshot + JWT 鉴权）、`config_load.rs`（配置容错）。改数据流/REST 鉴权/配置语义后跑 `cargo test --test <name>` 对应文件。新增集成测试用 `tests/common::start_lux`（**端口 0 由 OS 分配 + 独立临时 data_dir**），勿固定端口、勿写仓库 `./data/`；测试代码同样受 CI `RUSTFLAGS=-D warnings` 约束，保持零警告。
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
- **新增 config 字段**：`config.rs` 对应 struct 加 `#[serde(default)]` 字段 + `Default` 实现（缺失字段容错是既有约定）。
- **新增二进制行为**：`main.rs` 子命令分发 → 逻辑在 `lib.rs`（前台）或 `service/`（服务态）——前台与服务态共用 `run_app_with_shutdown`，shutdown 来源不同（ctrl_c vs SCM/sigterm）。

## 安全边界

- **`data/`、`target/`、`*.log` 已被 .gitignore 忽略，不要提交**；`data/` 含 lux 快照 + WAL，删除即丢数据。
- **`jwt_secret` 与 `MqttConfig::username/password` 不得写真实凭据进仓库**：config.toml 中 jwt_secret 是开发默认值（与 Lite_IOT 对齐），生产部署必须换；mqtt 公共 broker 仅开发验证用。
- **lux 只 bind loopback**（`db/mod.rs` 中 `bind_host` 默认 127.0.0.1 + `require_auth=false`）。若要远程暴露 RESP，必须先开 `password` + `require_auth`——这是认证边界，改动需显式确认。
- **`db::shutdown_lux` 必须先 SAVE 再 `shutdown_and_wait`**：lux 的 shutdown 默认不落盘（Memory 模式丢数据、Tiered 模式 WAL 不截断）。Tiered 下 `save_interval_secs` 也不能为 0（WAL 无限增长）。改 lux 交互代码时必须守住这两条。
- 输出信息：`hwid`（每机唯一标识）可在日志/API 中正常出现；不输出 bcrypt 哈希、JWT token 明文、lux `passwords` 表内容。

## 交付格式

每项任务完成时按此格式汇报：

1. **改动总结**：改了什么模块、为什么（一句话/条）。
2. **验证命令**：实际跑过的命令及其结果（fmt/clippy/test 输出；端到端验证的具体数据观察）。
3. **风险与未覆盖点**：已知限制（1066 退出码、cargo audit advisory、lux 无法 feature 裁剪）、未跑通的场景、需人工确认的配置（jwt_secret 更换、私有 broker 接入）。
