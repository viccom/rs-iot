# rs-iot

轻量级**跨平台工业物联网边缘框架**（Rust）。内嵌 [lux](https://github.com/lux-db/lux) 作为内存数据库（对外 Redis 兼容协议 RESP + Tiered WAL 持久化），插件化采集/发布，REST 管理 API（JWT 鉴权），可作为前台进程或系统服务（Windows Service / systemd）部署。

是 Go 项目 [Lite_IOT](https://github.com/) 的 Rust 重写与增强，面向工业物联网边缘网关场景，并预留数据应用分析、数据转换服务的扩展能力。

## 特性

- **lux 嵌入**：单实例同时对外 RESP（`:6390`）+ 进程内零拷贝 `EmbeddedClient`，共享 store/WAL；Tiered 模式每写刷盘，启动自动 snapshot+WAL 恢复。
- **插件分层**：
  - L1 核心协议插件：`inventory` 静态注册，编进单一二进制（零开销）
  - L2 数据转换：内嵌 `rhai` 脚本（feature `transform`，默认开启）
  - L3 WASM 扩展 / L4 进程插件：v2 路线
- **数据流（hub 模式）**：N 个 Source → 共享 mpsc hub → pipeline（Processor）→ fan-out（lux `rtdata:<devid>` 写入 + Sink `mqttPub` 北向发布）。单 Source 启停不影响管道。
- **REST 管理 API**（`:8790`）：JWT 中间件 + 白名单 + `X-Access-Key`；对标 Lite_IOT 路由（sysinfo/plugins/inst/user/group/accesskey/inst 启停）；`apiTest` 静态前端（rust-embed 嵌入）。
- **实例动态启停**：REST `startinst`/`stopinst` 从 lux `instances` 读配置动态启停 Source。
- **服务化**：Windows Service（`windows-service` crate dispatch）/ Linux systemd unit 生成；SCM stop 触发 `CancellationToken` → 优雅关闭（lux SAVE → shutdown）。

## 构建

```bash
cargo build             # debug
cargo build --release   # release（lto + strip）
```

> 依赖 `lux = { path = "../lux" }`（v0.37.0），需 lux 源码在同级 `../lux`。lux 强制带入 tokio[full]/mlua(vendored)/ring/reqwest，首次编译约 1–2 分钟。

## 运行

```bash
# 前台（默认配置：lux :6390、REST :8790、mqtt localhost:1883）
./target/debug/rs-iot

# 指定配置
./target/debug/rs-iot --config config.toml
```

### 验证

```bash
redis-cli -p 6390 PING                       # PONG（lux RESP）
redis-cli -p 6390 HGETALL rtdata:devA        # 实时数据缓存
mosquitto_sub -h localhost -t '+/+/datas'    # 北向 MQTT 发布（需 mosquitto broker）

# REST + JWT
curl -X POST localhost:8790/api/v1/user/login \
  -H 'Content-Type: application/json' -d '{"user_id":"admin","pass":"admin"}'   # 拿 token
curl -H "Authorization: <token>" localhost:8790/api/v1/sysinfo
# 浏览器：http://localhost:8790/apitest
```

## 服务化

```bash
rs-iot install        # Windows: sc create rs-iot ... / Linux: 写 /etc/systemd/system/rs-iot.service
# Windows
sc start rs-iot
sc stop rs-iot
# Linux
sudo systemctl enable --now rs-iot
rs-iot uninstall      # 卸载
```

## 配置（config.toml）

```toml
api_port = 8790
jwt_secret = "qwerty1234567890asdfgh1234567890"

[lux]
port = 6390
bind_host = "127.0.0.1"
data_dir = "./data/lux"
save_interval_secs = 300

[mqtt]
host = "localhost"     # 北向 broker（公网或本地 mosquitto）
port = 1883
```

## 架构

```
┌─────────────────── rs-iot 进程 ───────────────────┐
│  lux 实例 ──┬─ 对外 RESP :6390（redis-cli / 外部） │
│            └─ 进程内 EmbeddedClient（零拷贝）      │
│  Source(dataSim) ─┐                              │
│  Source(OPC UA)* ─┼─► hub(mpsc) ─► pipeline ─► Processor(rhai) │
│                  │                      ├─► lux HSET rtdata:<dev> │
│                  │                      └─► Sink(mqttPub) PUBLISH <hwid>/<dev>/datas │
│  REST :8790 (axum + JWT + apiTest 前端)           │
│  InstanceManager（动态启停 Source）              │
└────────────────────────────────────────────────────┘
（* 真实工业协议为 v2 路线）
```

## 里程碑

- [x] **M1** lux 嵌入 + RESP + Tiered WAL 持久化 + 身份 bootstrap（admin/admin）
- [x] **M2** Source/Sink/Processor 插件（inventory）+ datasim→lux→mqtt 北向 + pipeline
- [x] **M3** REST :8790 + JWT 中间件 + apiTest 前端 + 实例动态启停（hub 模式）
- [x] **M4** 服务化（Windows Service dispatch + systemd unit）+ 优雅关闭（lux SAVE）

## v2 路线

- 真实工业协议：南向 `async-opcua`(client)、`tokio-modbus`；北向 OPC UA server 暴露 lux 数据
- L3 WASM 插件（wasmtime/extism）、L4 进程插件（execd，复用 lux RESP）
- DataFusion 分析视图（SQL/UDF，REST `/api/v1/query`）
- 时序 downsample（lux Stream + 周期聚合）

## 已知限制

- `WIN32_EXIT_CODE 1066`：Windows Service `sc stop` 后进程退出码报告为 1066（ERROR_EXCEPTION_IN_SERVICE），但 lux SAVE 已先执行、数据完整恢复（已验证）。根因为 tokio runtime/lux 关闭时残留 task，后续优化。
- lux 依赖无法 feature 裁剪（tokio[full]/mlua/ring/reqwest 强制链接）。
- RESP 认证为单一密码（非 ACL）；默认 loopback 不启用。

## 许可证

MIT
