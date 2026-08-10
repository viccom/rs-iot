# rs-iot

轻量级**跨平台工业物联网边缘框架**（Rust）。内嵌 [lux](https://github.com/lux-db/lux) 作为内存数据库（对外 Redis 兼容协议 RESP + Tiered WAL 持久化），插件化采集/发布，REST 管理 API（JWT 鉴权），可作为前台进程或系统服务（Windows Service / systemd）部署。

是 Go 项目 [Lite_IOT](https://github.com/) 的 Rust 重写与增强，面向工业物联网边缘网关场景，并预留数据应用分析、数据转换服务的扩展能力。

## 特性

- **lux 嵌入**：单实例同时对外 RESP（`:6390`）+ 进程内零拷贝 `EmbeddedClient`，共享 store/WAL；Tiered 模式每写刷盘，启动自动 snapshot+WAL 恢复。
- **插件分层**：
  - L1 核心协议插件：`inventory` 静态注册，编进单一二进制（零开销）。已实现：`dataSim`（模拟）/ `opcuaClient`（OPC UA client）/ `modbusClient`（Modbus TCP / RTU serial / RTU over TCP-UDP）/ `s7Client`（西门子 S7-200/200Smart/300/400/1200/1500/Sinumerik828D）/ `luaClient`（Lua 5.4 动态脚本，覆盖长尾/自定义协议，串口+tcp+udp，热加载）
  - L2 数据转换：内嵌 `rhai` 脚本（feature `transform`，默认开启）
  - **外部南向插件（sidecarClient bridge，M7）**：任意语言（C/Go/Python/Java…）外部进程经 TCP bridge 接入，token 鉴权；外部=纯运行时（配置/数据在 rs-iot，本地仅缓存补传），lux 不对暴露。协议见 `docs/external-plugin-protocol.md`；样例：`examples/ext-plugin-python/`（Python）、`examples/ext-plugin-opcda-go/`（OPC DA，Go + opcda，连 Matrikon DA Server）
  - L3 WASM 扩展：v2 路线
- **数据流（hub + 双总线）**：数据上行 `N × Source → hub(mpsc) → pipeline(Processor) → lux rtdata`；控制下行 `写入口 → CtlRegistry → 实例 cmd_rx`。南向采集与北向发布完全解耦（lux rtdata 单一交汇点）；单 Source 启停不影响管道。
- **北向发布 + 写入口多源**：`MqttPublisher` 独立常驻周期从 lux 抽取发布（pull 模式）；写命令入口：MQTT `<hwid>/command` 订阅 + REST `POST /api/v1/command/send`（经 CtlRegistry 路由到实例，opcua/modbus 即时下行）。
- **Feature 裁剪**：`opcua`/`modbus`/`s7`/`lua` 为可选 Cargo feature（默认全开），现场可 `--no-default-features --features "lua,modbus"` 按需裁剪（减体积/省系统依赖，如不跑 Modbus RTU 的 Linux 机可不装 libudev-dev）。详见 `docs/plan-feature-gating-protocols.md`。
- **REST 管理 API**（仓库 `config.toml` 为 `:8790`，代码默认 `:8780`）：JWT 中间件 + 白名单 + `X-Access-Key`；对标 Lite_IOT 路由（sysinfo/plugins/inst/user/group/accesskey/inst 启停 + `/rtdatas` 实时数据查询 + `/command/send` 写命令）；`apiTest` 静态前端（rust-embed 嵌入）。
- **实例动态启停**：REST `startinst`/`stopinst` 从 lux `instances` 读配置动态启停 Source。
- **服务化**：Windows Service（`windows-service` crate dispatch）/ Linux systemd unit 生成；SCM stop 触发 `CancellationToken` → 优雅关闭（lux SAVE → shutdown）。
- **文件日志（M3）**：`tracing-appender` daily 轮转，`[log].dir` 默认 `./logs`（开箱即落盘）+ `max_days`/`max_size_mb` 保留期清理（按天/按总大小删旧，跳过当天活动日志）+ 退出 `WorkerGuard` flush（不再 `mem::forget`，保故障末尾日志）。
- **可观测性（M4 + P1 三子系统）**：统一埋点（`InstanceStat` + `StatTracker`，全部内置插件 + sidecar + mqttPub）+ `GET /metrics`（Prometheus 文本，免 JWT，进程级 + per-instance 指标）+ `POST/GET/DELETE /api/v1/inst/trace`（per-instance 报文 hex dump 按需激活，lua + modbus 4 传输）+ `GET /api/v1/logs/recent`（日志 ring buffer 快照，内存 2000 条/实例）+ `GET /api/v1/inst/stats`（per-instance JSON 指标，替代废弃的 `/api/v1/lua/stats`）。详见 `docs/rest-api.md` §4.10。
- **实例自愈可信（M3）**：实例 task 异常退出（panic/配置错误早退）由 supervisor 真实标 `Failed` + `error!` 落盘；REST `inststatus` 可见，`startinst` 重拉。
- **L1→L2 生产基线**：`missing_docs`/clippy/rustfmt 基线、统一 `RsIotError`（thiserror + `#[non_exhaustive]`）错误规范、newtype ID（`Hwid`/`InstId`/`UserId`/`PluginName`）、`tracing` 日志（`#[instrument]` + TraceLayer）、GitHub Actions CI。规范依据见 `rust-architecture-by-code-size.md`。

## 构建

```bash
cargo build             # debug
cargo build --release   # release（lto + strip）
```

> 依赖 `lux = { path = "../lux" }`（v0.37.0），需 lux 源码在同级 `../lux`。lux 强制带入 tokio[full]/mlua(vendored)/ring/reqwest，首次编译约 1–2 分钟。

## 运行

```bash
# 前台。不带 --config 时用代码默认（lux :6390、REST :8780、mqtt localhost）；
# 带仓库 config.toml 时：lux :6390、REST :8790、mqtt broker.emqx.io:1883
./target/debug/rs-iot
./target/debug/rs-iot --config config.toml
```

### 端到端验证（公共 broker，已实测通过）

```bash
redis-cli -p 6390 PING                       # PONG（lux RESP）
redis-cli -p 6390 HGETALL rtdata:devB        # 实时数据缓存（采集→管道→入库）

# 北向 MQTT 周期发布（pull 模式：MqttPublisher 每 5s 从 lux 抽取发布）
mosquitto_sub -h broker.emqx.io -t '+/devB/datas' -v

# REST + JWT
curl -X POST localhost:8790/api/v1/user/login \
  -H 'Content-Type: application/json' -d '{"user_id":"admin","pass":"admin"}'   # 拿 token
curl -H "Authorization: <token>" localhost:8790/api/v1/sysinfo
curl -H "Authorization: <token>" localhost:8790/api/v1/rtdatas   # 实时数据查询（全部/单设备）
# REST 写命令（opcua/modbus 写命令均可走 REST，不必依赖 MQTT broker；经 CtlRegistry 路由到实例）
curl -X POST localhost:8790/api/v1/command/send \
  -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"instance":"mb1","devid":"devA","cmd":"write","data":"{\"address\":\"holding:0:f32:ABCD\",\"value\":12.5}"}'

# 可观测性（M4 + P1）
curl -s localhost:8790/metrics | head                          # Prometheus 端点（免 JWT，对接 Grafana）
curl -H "Authorization: <token>" "localhost:8790/api/v1/inst/stats?inst_id=mb1"   # per-instance 指标 JSON
curl -X POST -H "Authorization: <token>" -H 'Content-Type: application/json' \
  -d '{"inst_id":"mb1","duration_secs":60}' localhost:8790/api/v1/inst/trace       # 激活报文 trace
curl -H "Authorization: <token>" "localhost:8790/api/v1/logs/recent?level=warn&limit=20"  # 最近日志快照
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
host = "broker.emqx.io"       # 公共 MQTT broker（匿名；生产换私有 broker + credentials）
port = 1883
publish_interval_secs = 5     # 北向 pull 发布周期（秒）

[bridge]
enable = false                # 外部插件 bridge（sidecarClient）；默认关，现有部署零影响
bind_host = "127.0.0.1"       # 同机；跨机部署改 "0.0.0.0"（token 强制校验）
port = 8781
heartbeat_timeout_secs = 15
accept_hello_timeout_secs = 10

# 日志（M3，默认即可：落盘 ./logs，保留 7 天，daily 轮转，退出自动 flush）
[log]
dir = "./logs"                # 默认 "./logs"；置 "" 关闭文件层（仅终端）
level = "info"                # RUST_LOG 优先
max_days = 7                  # 按文件名日期保留最近 N 天（0 = 不按天删）
max_size_mb = 0               # 0=不限；>0 时按目录总大小删最旧
ring_buffer_capacity = 2000   # 日志 ring buffer 容量（GET /api/v1/logs/recent 读；含报文 trace 衔接）
```

## 架构

两条总线：数据上行（采集→入库）+ 控制下行（写命令→设备）。南向 N 种协议 × 北向 M 种对接，经双总线降维成 N+M（每个协议只对接总线，不互相适配）。

```
┌─────────────────────── rs-iot 进程 ───────────────────────┐
│  lux 实例 ──┬─ 对外 RESP :6390（redis-cli / 外部）        │
│            └─ 进程内 EmbeddedClient（零拷贝）             │
│                                                          │
│  ── 数据上行总线 ──────────────────────────────────────── │
│  Source(dataSim)    ─┐                                   │
│  Source(opcuaClient) └─► hub(mpsc 256) ─► pipeline ─► Processor(rhai) │
│  Source(modbusClient)┘                                 └─► lux HSET rtdata:<dev> │
│                                                          │
│  ── 北向读出口（都从 lux rtdata 取最新值）─────────────── │
│  MqttPublisher（常驻）── 周期 KEYS rtdata:* → HGETALL → PUBLISH <hwid>/<dev>/datas │
│  GET /api/v1/rtdatas                                      │
│                                                          │
│  ── 控制下行总线（CtlRegistry，多源写入口 → 实例 cmd_rx）─ │
│  MQTT <hwid>/command 订阅 ─┐                             │
│  POST /api/v1/command/send ─┴─► CtlRegistry ─► handle_write（opcua/modbus…）│
│  （未来：北向 UA Server·Modbus slave·104 写回调）         │
│                                                          │
│  管理面：REST :8790 (axum + JWT + apiTest) ─► InstanceManager（动态启停）│
│  可观测面（M4+P1）：GET /metrics（Prometheus，免 JWT）               │
│                    GET /api/v1/inst/stats（per-instance JSON 指标）   │
│                    POST/GET/DELETE /api/v1/inst/trace（报文 hex 按需激活）│
│                    GET /api/v1/logs/recent（日志 ring buffer 快照）   │
└──────────────────────────────────────────────────────────┘
```

**外部插件 bridge（sidecarClient，M7）**——任意语言外部进程作为南向 Source 接入：

```
外部进程(C/Go/Python/...) ──TCP──► bridge acceptor(:8781, token 鉴权)
                                        │ 按 inst_id 分发到实例 task
                                        ▼
                              sidecarClient task（每实例一个）
                                        │ DataUp → Sample → hub → pipeline → rtdata（与内置同构）
                                        │ 写命令 ← CtlRegistry ← MQTT/REST → Command 帧 → 外部
                                        ▼
                              lux（loopback，外部永不触碰）
```

外部=纯运行时（配置/数据/状态全在 rs-iot，本地仅缓存数据待补传）；几百实例并发，每实例独立 task；双向重连容错（外部指数退避 + 缓存补传，bridge 断连感知 + 失联期间写命令丢弃）。

## CI

GitHub Actions（`.github/workflows/ci.yml`，push/PR 触发）：

- `fmt`：`cargo fmt --all --check`
- `clippy`：`cargo clippy --all-targets -- -D warnings`
- `test`：矩阵 `ubuntu-latest` + `windows-latest`
- `doc`：`cargo doc --no-deps`（`RUSTFLAGS=-D warnings`）
- `audit`：`cargo audit`（信息性 `continue-on-error`：传递依赖 rustls TLS 链有已知 advisory，rs-iot 用明文 MQTT 实际风险低，升级依赖链后改严格）

依赖 `lux = { path = "../lux" }`，CI 需同 org 下 lux 仓库一并 checkout（`viccom/lux`，按实际调整）。GitHub 仓库尚未创建，ci.yml 按标准格式编写，push 后实测。

## 里程碑

- [x] **M1** lux 嵌入 + RESP + Tiered WAL 持久化 + 身份 bootstrap（admin/admin）
- [x] **M2** Source/Sink/Processor 插件（inventory）+ datasim→lux→mqtt 北向 + pipeline
- [x] **M3** REST :8790 + JWT 中间件 + apiTest 前端 + 实例动态启停（hub 模式）
- [x] **M4** 服务化（Windows Service dispatch + systemd unit）+ 优雅关闭（lux SAVE）
- [x] **L1→L2 治理（STEP 1-10）**：lint/文档基线 + 统一错误规范（thiserror/`#[non_exhaustive]`）+ newtype ID + 日志规范（`#[instrument]`/TraceLayer）+ mqtt pull 化（MqttPublisher 周期从 lux 抽取）+ `/rtdatas` 路由 + 公共 broker 配置 + CI + 端到端验证（公共 broker 实测通过）
- [x] **M5** opcuaClient 南向插件（async-opcua，none/Basic256Sha256，读写 UA Server）+ 文件日志（tracing-appender daily 轮转）
- [x] **M6** modbusClient 南向插件（tokio-modbus，批量读 + 位解析 + 4 种字节序 + 混合类型偏移）+ REST 写命令端点 `/api/v1/command/send`（CtlRegistry 多源写总线，opcua/modbus 即时下行）
- [x] **M7** 外部插件 bridge（sidecarClient）：任意语言外部进程经 TCP 接入（token 鉴权，方案 C 不暴露 lux）；sidecarClient Source + acceptor + BridgeRegistry + 7 帧协议（Hello/ConfigPush/DataUp/Command/Ack/Heartbeat）+ 双向重连容错 + ext_tokens hmac；Python 样例 + 端到端集成测试
- [x] **M8** modbusClient RTU 扩展（tokio-modbus `rtu` + tokio-serial）：扁平 `kind` 支持 `rtu_serial` / `rtu_tcp` / `rtu_udp`（RTU over TCP/UDP 经 `rtu::attach_slave` 统一 `Context`，采集/写逻辑同构；UDP 自写 `UdpIo` adapter 保 datagram 边界），串口参数缺省 9600/8N1/NONE；ASCII 后置（未来可借 `rmodbus` codec）
- [x] **M9** s7Client 西门子 S7 南向插件（tokio-s7 0.1，纯 Rust async，ISO-on-TCP :102）：批量 read + 大端自解码，覆盖 S7-200/200Smart/300/400/1200/1500/Sinumerik828D；`kind` 显式映射 typed 位宽（tokio-s7 地址前缀装饰陷阱）；写命令 typed `write_*`；string/wstring 后置
- [x] **M10** luaClient 动态脚本南向插件（mlua 0.12，Lua 5.4 vendored）：`collect`/`on_data`/`on_write` 回调 + ctx API（L1 通讯/L2 数据/L3 联动/L4 kv/L5 http/L6 工具）+ 串口/tcp/udp 三传输 + 沙箱护栏（库裁剪+指令 hook+内存限额）+ 文件热加载（notify 去抖，失败回退）+ `stat:<inst>` 运行指标（collect 计数/耗时/degraded）。详见 `docs/lua-script-guide.md`
- [x] **M11** Feature 裁剪：`opcua`/`modbus`/`s7` 改为可选 Cargo feature（默认全开），CI minimal job 守护裁剪组合可编译 + 优雅降级（`find_source` 未命中返 "source plugin not found"）。详见 `docs/plan-feature-gating-protocols.md`

## 里程碑（续）：现场可用性 + 可观测性（2026-08-08 ~ 2026-08-10）

> 以下编号独立于上方 M1-M11（功能里程碑），是「现场可用性 + 可观测性」主线，三日 58 commit 落地。审查报告见 `docs/development-audit-2026-08-08-to-2026-08-10.md`，场景缺口基线见 `docs/scenario-gap-assessment.md`。

- [x] **RS485-M1** modbus 多 slave 轮询：`slave_id` 下沉设备级（`DeviceSpec.slave_id`）+ `app_config.poll_strategy`（`round_robin` 默认 / `sequential`）+ `request_timeout_secs`（per-request 超时跳过，单 slave 无响应故障隔离不拖垮整轮）；向后兼容旧单 slave 配置。一条 RS485 总线一个实例管多从站
- [x] **lua-M2** lua 单实例多设备输出：`set_value(devid, tag, v)` 3 参（原硬绑 `devices[0]`）+ `set_values` 批量 + `collect` 嵌套返回 `{[devid]={tag=v}}` 自动判定 → 一条链路多 devid 各产一个 Sample
- [x] **M3** 日志兜底 + 自愈可信：`[log].dir` 默认 `./logs`（开箱落盘）+ `max_days`/`max_size_mb` 保留期清理 + 退出 `WorkerGuard` flush（修 `mem::forget` 丢末尾日志盲区）；实例 supervisor（`spawn_supervisor`）真实标 `Failed`（修 task 死了仍显 Running 盲区）
- [x] **M4** 统一埋点：`InstanceStat` 替代 lua 专属 `luastat` + `StatTracker` 接入 helper + `ProcessMetrics` 进程级 + `CtlRegistry`/pipeline/hub 咽喉计数；统一 `GET /api/v1/inst/stats`（替代废弃 `/api/v1/lua/stats`）；全部内置插件（dataSim/opcua/modbus/s7/lua/sidecar）+ mqttPub 接入。约定见 `docs/instrumentation-conventions.md`
- [x] **P1 #1** `/metrics` Prometheus 端点（免 JWT，手写 exposition 文本，进程级 14 + per-instance 5 metric family，对接 Grafana）
- [x] **P1 #2** 报文 trace（阶段 1）：`POST/GET/DELETE /api/v1/inst/trace` per-instance 按需激活收发字节 hex dump；lua（io_engine direct dump 三点）+ modbus 4 传输（`TraceIo<T>` 拦截层）；opcua/s7/sidecar 后置。**按设计只进内存 ring buffer（2000 条/实例，不落盘）**
- [x] **P1 #3** 日志 ring buffer：`RingBufferLayer` 截获最近 N 条 + `GET /api/v1/logs/recent?level=&target=&limit=`；独立 filter `info,io_trace=trace` 衔接报文 trace（`?target=io_trace` 拉报文 hex）
- [x] **端到端测试** `tests/e2e_modbus_full_e2e.rs`：T1 modbus RTU serial（物理 COM2↔COM3 com0com + 仿真器，`#[ignore]` CI 跳过）/ T2 TCP / T3 lua 多设备 / T4 P1 三子系统；现场补充 RTU over UDP 采集 + 写命令（FC05/FC06）+ 三路径报文 trace 验证

## v2 路线

- 更多南向协议：Modbus RTU / OPC DA client / IEC 60870-5-104 master / DNP3 / BACnet …
- 北向服务器插件（既读出口又写入口）：OPC UA Server / OPC DA Server / Modbus slave / IEC 60870-5-104 Server —— 都经 CtlRegistry 写总线 + lux rtdata 读
- L3 WASM 插件（wasmtime/extism）
- 外部插件 bridge 增强：TLS（rustls）、配置变更自动 notify、历史时序存储、迟到数据 ts 单调过滤、北向外部插件（app_type=North + sidecarClient）、批量导入 API（几百实例运维）
- DataFusion 分析视图（SQL/UDF，REST `/api/v1/query`）
- 时序 downsample（lux Stream + 周期聚合）
- v2 push sink：Sink trait 已保留，供推送式北向插件使用

## 已知限制

- `WIN32_EXIT_CODE 1066`：Windows Service `sc stop` 后进程退出码报告为 1066（ERROR_EXCEPTION_IN_SERVICE），但 lux SAVE 已先执行、数据完整恢复（已验证）。根因为 tokio runtime/lux 关闭时残留 task，后续优化。
- lux 依赖无法 feature 裁剪（tokio[full]/mlua/ring/reqwest 强制链接）。
- RESP 认证为单一密码（非 ACL）；默认 loopback 不启用。
- 依赖安全：传递依赖 rustls TLS 链有已知 advisory（`cargo audit`），rs-iot 用明文 MQTT 实际风险低，升级 lux/rumqttc 依赖链后清理。

## 许可证

MIT
