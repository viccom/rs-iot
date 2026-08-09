# 工业现场场景能力缺口评估

> 结合一个典型工业现场场景，评估 rs-iot 内置插件(2)/外部插件(3)/lua 脚本(4)/REST API(5) 的能力边界，
> 识别需新增的接口或功能，并给出优先级建议。**本文为评估报告 + 建议，不含实现**。
> 事实依据来自代码核实（附 `文件:行号`），可观测性现状详见调研结论。

## 0. 场景

- **设备**：20 台 PLC（5 个厂家，各 N 台）；100 台仪表（串口为主，RS232 + RS485，8 个厂家，协议各不相同但都简单，每台点数 ≤100，采集周期 500ms–3s）；5 套 SCADA（OPC UA / OPC DA / 一家只提供 C# API）
- **链路**：现场通讯链路无法保证不出故障
- **诉求**：① 数据采集能**自诊断、自愈**；② 故障时能**快速定位**（发生时间 / 故障类型 / 故障模块）+ 拿到**故障前一段时间的日志**；③ rs-iot 提供**宿主程序及插件的埋点监测值**；④ rs-iot 可**按需激活通讯报文输出和发送**

## 1. 协议覆盖评估

| 设备类 | 协议 | rs-iot 现状 | 评估 |
|---|---|---|---|
| PLC（5 厂家） | 西门子 S7 | ✅ 内置 `s7Client`（全系列） | 直接覆盖 |
| | Modbus 类（施耐德等） | ✅ 内置 `modbusClient`（TCP/RTU） | 直接覆盖 |
| | 三菱(MC)/欧姆龙(FINS)/AB(EtherNet/IP) | ❌ 无内置 | `luaClient`（串口/简单以太网）或**外部插件**（AB 的 CIP/EtherNet/IP 用 Go/C 库）覆盖 |
| 仪表（8 厂家，串口异协议，简单） | 非标私有协议 | ✅ `luaClient` 主场 | **完美契合**：串口 + 自定义报文 + 一问一答 + 热加载（500ms–3s 周期、<100 点都在 lua 舒适区） |
| SCADA | OPC UA | ✅ 内置 `opcuaClient` | 直接覆盖 |
| | OPC DA | ✅ 外部 Go 样例（huskar-t/opcda） | 覆盖（Windows COM 由外部进程承接） |
| | C# API | ✅ 外部插件路径 | 覆盖（外部 C# 进程封装 API） |

**结论**：协议覆盖的**框架足够**——内置 + lua + 外部插件三条路径能覆盖现场全部协议类型，且都不需要改 rs-iot 核心。这是分层 + 双总线架构的直接收益。**现场缺口不在"框架"，而在"具体协议实现"**（需补的协议主要是 PLC 的三菱/欧姆龙/AB，可用 lua 或外部插件快速补齐）。

### 1.1 多实例运行模型（强项）

现场"同型号 N 台设备"是常态。rs-iot 的多实例模型经代码核实（`instance.rs:77-102`）：

- **一份插件代码 + N 个独立运行实例**：`find_source(app_code)` 拿同一工厂，但每次 `start_source` 调 `create()` 新建 struct 实例，**每实例独立 task / CancellationToken / cmd_rx(64) / 连接 / 配置 / 点表**
- **唯一共享的是 hub**（所有 Sample 汇聚 `mpsc::<Sample>(256)`，pipeline 单消费）；**单实例崩/启停不影响其他**
- 20 台同型号 PLC = 20 个 `inst_id`（同 `app_code`，各自 `app_config` + `devices`）→ 完全支持
- lua 同构：一份脚本可被多实例引用，每实例独立 VM/IO task/`stat`/`luakv`，**实例间并发**；lua **一条通讯链路 = 一个实例**（IO task 独占链路，脚本不碰链路只组报文），但当前是**单设备输出语义**（`set_value`/`collect` 硬绑 `devices[0]`，`api/mod.rs:164-168`），一条链路多设备需扩展单实例多 devid（见 §1.2）

**触发机制**（`lua_client/mod.rs:171-271` select!）：周期触发（`collect`，ticker `period`）+ 事件触发（`on_data` 被动帧 / `on_write` 写命令 / 热加载）；**无显式"条件触发"**（脚本内自定义逻辑实现）。**实例内串行**（一个 mlua VM 非可重入，collect 含 IO 往返期间主循环挂起，其余事件排队），**实例间并发**。

### 1.2 RS485 总线多设备 —— P0 能力缺口（modbus M1 ✅ 已落地；lua M2 待实现）

> **设计原则**（核实自代码 + 设计意图）：**一条通讯链路 = 一个实例**。RS485 总线上挂多台设备，由**一个实例独占该串口**统一管理——链路（open/重连/收发）由宿主封装，脚本/插件只管报文与设备逻辑。**不应**为总线上每台设备各建实例（串口 OS 独占，第二个实例 open 同一口必失败）。
>
> **链路边界**：一个 lua 实例**只持有一条链路**（`io_engine` 单 backend，`lua_client/mod.rs:93-105` 三传输三选一），不会同时打开多个物理接口或多个 TCP 连接。这里的"多设备"**仅指同一条链路上的多台从站**（RS485/RS485-like 总线：同协议、同串口参数、不同从站地址）。**不同厂商协议 / 不同串口参数 / 不同物理接口 → 必然不同实例**（不同脚本，或同脚本多实例配不同 `serial_port`/`endpoint`）。

**lua 分层（已正确，无需改）**：链路由 IO task 独占封装（`io_engine/mod.rs:74-100`），脚本经 `ctx:request(frame)` 收发字节、**不碰链路**，只组报文帧（`modbus-like` 示例的 MBAP 帧含 `uid`/slave_id，`modbus-like/main.lua:24`）。因此脚本**技术上可在 `collect()` 里循环组不同 uid 帧轮询一条总线上的多台从站**，宿主独占串口不冲突。

**真正卡点（单实例多设备输出能力待补）**：
- `luaClient`：`set_value(tag,v)` 与 `collect()` 都**硬绑 `inst.devices[0]`**（`api/mod.rs:164-168`，注释"多设备扩展后置"；`mod.rs:146-152,205`）。注意 `get_value(devid,tag)` **有** devid 参数、`set_value` **没有**——这条链路轮询回的多台设备数据无处分发（只能归一个 devid）。最小扩展：`set_value` 加 devid 参数（对齐 `get_value`），或 `collect` 返回 `{[devid]={tag=v}}` 多设备结构
- `modbusClient`（内置，自管链路 `Option<Context>`）：**单实例单 `slave_id`**——RS485 多从站需扩"单实例多 slave 轮询"（一个 Context 独占串口，轮询 `set_slave` 不同 id + 各自点表）

**结论**：缺口**不是**"串口共享层"、**也不是**"拆多实例"，而是 **lua 单实例的多 devid 输出 + modbus 单实例的多 slave 轮询能力待补**——让一个实例能管好一条链路上的多台设备。这正是 RS485 现场"一条总线一个实例"能否落地的关键。

**对照（范例）**：`opcuaClient`/`s7Client` **已实现**单实例多设备多点位（一个 UA session / PLC 连接批量读 `devices[]` × `children[]`）——单实例多设备是非总线协议的刚需且已落地，是"单实例多设备"的现成范例；lua/modbus 的 RS485 多从站扩展应对齐这个能力模型。

**不改代码的临时 workaround**：
1. lua `collect()` 内手动轮询多从站（组不同 uid 帧），结果归 `devices[0]` 单 devid（tag 加从站前缀区分，牺牲"每台设备独立 devid"）
2. 每条 RS485 总线配 RS485↔以太网网关（如 Moxa NPort），多从站变 TCP endpoint，每从站一实例走 TCP（绕过串口独占，每从站独立 devid）

**建议**：**P0** — ✅ `modbusClient` 单实例多 slave 轮询**已落地（M1）**；⬜ `luaClient` 单实例多设备批量（`collect` 嵌套返回 + `set_values`）待 M2。完整设计见 [`superpowers/specs/2026-08-08-rs485-multi-device-design.md`](superpowers/specs/2026-08-08-rs485-multi-device-design.md)，M1 实现计划见 [`superpowers/plans/2026-08-08-modbus-multi-slave.md`](superpowers/plans/2026-08-08-modbus-multi-slave.md)。

#### 1.2.1 modbus 多 slave 采集策略（设计输入）

**前提**（设计硬约束）：同一条 RS485 总线上的 slave **串口参数必须完全一致**（波特率/数据位/校验/停止位），各 slave 仅 unit id 不同；点表可同可异。故**串口参数留实例级**（总线共享），**slave_id 下沉到设备级**。

**两种轮询策略**——半双工 RS485 物理上"一次一对请求-响应"，两种都是串行轮询，仅顺序组织不同：

| 策略 | 顺序 | 强项 | 弱点 |
|---|---|---|---|
| **1 轮转 round-robin**（建议默认） | 每轮对每个 slave 各发 1 条报文（各取点表下一区间），多轮采完 | 所有设备齐头并进、刷新均匀；单 slave 故障天然隔离 | 单 slave 多点分多轮采，点间时间差大 |
| **2 逐设备 sequential** | 一个 slave 所有报文采完再换下一个 | 单设备多点时间一致（V/A/P 同时刻算功率） | slave 间有延迟；慢 slave 靠超时跳过才不拖后续 |

**关键约束（两种都要守）**：① **per-request 超时 + 跳过**（单 slave 无响应不拖垮整轮——RS485 鲁棒性底线）；② period 内轮不完则跳过剩余（`MissedTickBehavior::Skip`）；③ 半双工串行，不并发。

**配置模型方向**：`slave_id` 从 `app_config` 下沉到 `DeviceSpec`（每设备一个 unit id + 各自 children，点表同异随意）；串口参数仍 `app_config` 实例级；`app_config` 加 `poll_strategy`（默认 `round_robin`，可 `sequential`）；向后兼容旧 `app_config.slave_id` 单 slave 配置。

## 2. 自诊断 / 自愈评估（诉求①，重点）

### 现状
- 各插件有**基础重连**，但策略异构、无统一框架：
  - `opcuaClient`：库级 `session_retry_limit(3)`；读失败拆 session 下 tick 重连（`opcua_client/mod.rs:240,387-395`）
  - `modbusClient`：固定 `sleep(reconnect_secs)`（默认 2s，**非退避**，`modbus_client/mod.rs:226-230`）
  - `s7Client`：**唯一**用 `is_connection_error()` 区分连接错误（`s7_client/mod.rs:448-450`），库 `auto_reconnect`
  - `luaClient`：靠采集周期天然限速，**无 reconnect_secs/无退避**（`lua_client/mod.rs:138-143`）
  - `mqttPub`：固定 sleep 2s（`mqtt_pub/mod.rs:71-74`）
- 实例状态 5 态枚举（`instance.rs:24-35`），但 **`Failed` 定义了却从未写入**——插件 `run()` 早退（如配置错误 return）仍被标 `Running`（`instance.rs:99`），外部观察"在运行"实际 task 已死
- 实例 task **panic/早退不会自重启**，也不会标 Failed，需人工 stop+start

### 缺口
- 🔴 **Failed 状态不真实**（task 死了仍显 Running）——直接破坏"自诊断"可信度
- 🔴 **无实例 watchdog**（panic/早退不自愈）
- 🟠 无统一退避/熔断/重试上限（modbus/mqtt 固定间隔，opcua 仅库级上限）
- 🟠 连接状态（`io_connected`/`last_io_err`）不对外暴露（`luastat.rs:6` 注释承认 v1 后置）

### 建议
| 优先级 | 项 | 说明 |
|---|---|---|
| **P0** | Failed 状态真实写入 | `start_source` spawn 后监控 JoinHandle，task 结束（非正常 stop）→ 标 Failed；修 `instance.rs` |
| **P0** | 实例 watchdog（可选自重启） | Failed 实例按策略自动 stop+start（带退避 + 上限），或至少 REST 可见 + 告警 |
| P1 | 统一连接状态暴露 | 各插件 task 维护 `connected: bool` + 写入 `stat:<inst>`，REST 可查 |
| P1 | 统一退避策略 | modbus/mqtt 固定 sleep → 指数退避 + jitter；新增 `reconnect_max_secs` |
| P2 | 熔断 | 连续失败 N 次进入熔断（暂停采集 + 告警），避免无效报文风暴 |

## 3. 故障定位评估（诉求②：时间/类型/模块 + 故障前日志）

### 现状
- ✅ **模块前缀齐全**：`[opcuaClient]`/`[modbusClient]`/`[s7Client]`/`[luaClient]`/`[sidecarClient]`/`[pipeline]`/`[mqtt]` 等，grep 可定位模块
- ✅ tracing 带时间戳 + daily 文件轮转（`lib.rs:48-77`）
- ✅ RUST_LOG 级别可控

### 缺口
- 🔴 **`config.toml` 默认未启用文件日志层**（`[log].dir` 缺省空，开箱仅终端）——现场不配就无落盘日志
- 🟠 **无"故障前 N 条日志"环形缓冲**：诉求"拿到故障前一段时间的日志"只能去翻 daily 文件，无内存 ring buffer 可快照
- 🟠 **WorkerGuard 被 `std::mem::forget`**（`lib.rs:70`）——进程异常退出时不 flush，丢最后几条日志（恰是故障现场）
- 🟠 **daily 轮转不删旧文件**——长期运行无限堆积，无大小轮转/保留期
- 🟡 错误无聚合/限流——持续断连时相同 warn 每周期重复刷屏，淹没关键信息

### 建议
| 优先级 | 项 | 说明 |
|---|---|---|
| **P0** | 默认启用文件日志 + 保留期 | `[log].dir` 默认 `./logs`；加 `max_days`/`max_size` 按大小+时间双轮转删旧 |
| **P0** | 退出前 flush 日志 | 不再 `forget` WorkerGuard，shutdown 时显式 drop（保故障末尾日志） |
| P1 | 内存 ring buffer + REST 快照 | 保留最近 N 条（按实例/级别），`GET /api/v1/logs/recent` 取故障前快照 |
| P1 | 错误聚合/限流 | 相同错误去重或降频（如前 N 次详细、之后采样） |
| P2 | 结构化日志（JSON） | 便于 ELK 采集，按 inst/level/module 字段过滤 |

## 4. 埋点监测评估（诉求③：宿主 + 插件埋点值）

### 现状
- ✅ **仅 `luaClient` 有结构化指标**：`stat:<inst>` JSON 含 `status`/`collect_count`/`collect_fail_count`/`last_collect_ts`/`last_collect_duration_ms`/`vm_restart_count`/`script_version`/`last_err`（`luastat.rs:12-30`），REST `GET /api/v1/lua/stats?inst_id=` 暴露（`routes_lua.rs:27`）
- ✅ lux `DBSIZE` 在 `/sysstatus` 暴露（粗略宿主指标）
- ❌ **opcua/modbus/s7/dataSim 零计数器**——无采集次数/成功率/延迟/连接状态/错误计数，只散落 `warn!` 文本
- ❌ 无 `/metrics`/Prometheus/OpenMetrics 端点（`Cargo.toml` 无 metrics crate）
- ❌ 无 CPU/内存/uptime/hub 队列水位

### 缺口
- 🔴 **4 个内置南向插件无任何埋点**——诉求"插件埋点监测值"对它们完全空白
- 🔴 无 `/metrics`（无法对接 Prometheus/Grafana）
- 🟠 无进程资源指标（CPU/mem/uptime）
- 🟠 hub 队列（256 硬编码，`lib.rs:128`）水位不暴露——满了背压但外部看不到

### 建议
| 优先级 | 项 | 说明 |
|---|---|---|
| **P0** | opcua/modbus/s7 补 per-instance 指标 | 复用 lua 的 `stat:<inst>` 模式：采集计数/失败计数/最后采集 ts+耗时/连接状态/last_err；REST 统一 `GET /api/v1/inst/stats?inst_id=`（不限 luaClient） |
| P1 | `/metrics`（Prometheus 文本格式） | 进程级 + per-instance gauge/counter，对接现有监控栈 |
| P1 | 进程资源指标 | CPU/mem/uptime 进 `/sysstatus`（sysinfo crate 已是依赖） |
| P2 | hub/pipeline 队列水位 | 暴露 hub channel 填充度，背压可视化 |

## 5. 报文输出评估（诉求④：按需激活报文输出和发送）

### 现状
- ❌ **南向插件零 debug/trace**：opcua/modbus/s7 全仓搜 `tracing::debug!`/`tracing::trace!` 在插件内零命中（仅 `lua_client/api/mod.rs:337` 一处脚本日志桥）
- ❌ 无原始报文/字节/帧 dump 开关
- ❌ 无 per-instance 报文 trace 的运行时按需激活
- 🟡 抓包只能靠 `RUST_LOG=tokio_modbus=trace` 之类底层库日志（未配置、未验证），且全局生效、无法按实例、无法只抓一段

### 缺口
- 🔴 **完全缺失**——诉求"按需激活通讯报文输出和发送"无任何框架支持

### 建议
| 优先级 | 项 | 说明 |
|---|---|---|
| **P1** | per-instance 报文 trace 开关 | REST `POST /api/v1/inst/trace?inst_id=&duration=60s` → 该实例收发字节 hex dump 进日志（或专用 ring buffer）→ 到时自动关。按实例隔离，不影响其它实例 |
| P1 | hex dump 工具 | 统一 `frame_hex(bytes)` 工具函数（modbus/opcua/s7/lua 复用） |
| P2 | trace 帧 REST 拉取 | trace 内容存 ring buffer，`GET /api/v1/inst/trace?inst_id=` 拉取，前端可视化 |

## 6. 缺口总结与优先级

| 诉求 | 现状评级 | 关键缺口 | 建议优先级 |
|---|---|---|---|
| ① 自诊断自愈 | 🟠 弱 | Failed 不真实 / 无 watchdog / 无统一退避 | **P0** Failed+watchdog；P1 退避+连接状态；P2 熔断 |
| ② 故障定位 | 🟡 中（日志好，但默认不落盘 + 无 ring buffer） | 文件日志默认关 / 退出丢末尾 / 无故障前快照 | **P0** 默认文件+保留期+flush；P1 ring buffer+聚合 |
| ③ 埋点监测 | 🔴 仅 lua | 4 内置插件零计数 / 无 /metrics | **P0** 内置插件补 stat；P1 /metrics+资源 |
| ④ 报文输出 | 🔴 无 | 无 dump 开关 / 无按需激活 | **P1** per-instance trace 开关 |
| 协议覆盖 | ✅ 框架足够 | 具体协议（三菱/欧姆龙/AB） | 用 lua/外部补，不改核心 |
| **RS485 总线多设备** | 🟡 modbus ✅ M1 / lua 待 M2 | modbus 多 slave 已落地（device 级 slave_id + poll_strategy + 超时故障隔离）；lua `set_value`/`collect` 绑 devices[0] 待 M2 | **P0** lua 单实例多 devid（M2）；modbus 已完成（见 §1.2） |

## 7. 对任务 2/3/4/5 的影响（回应"是否需新增接口或功能"）

| 任务 | 是否需新增 | 具体内容 |
|---|---|---|
| **2 内置插件指南** | 是（文档待代码先行） | 需补"运行指标暴露（stat 模式）/ 报文 trace / 自愈策略"章节；但这些依赖 P0/P1 代码实现，指南需等代码落地后补 |
| **3 外部插件** | 部分 | 外部进程是独立进程，自带诊断（不依赖 rs-iot 埋点）；但 rs-iot 侧应把外部实例的连接状态/心跳纳入统一 `stat`（当前 sidecarClient 无 stat） |
| **4 lua 脚本** | 否（已是范例） | luaClient 的 `stat:<inst>` 是其它插件的范例，指南已覆盖；外部脚本通过 `ctx:log_*` 自行埋点 |
| **5 REST API** | 是（多处） | 需补：统一 `GET /api/v1/inst/stats`（不限 lua）、`GET /api/v1/metrics`（Prometheus）、`POST /api/v1/inst/trace`（报文 dump 开关）、`GET /api/v1/logs/recent`（故障前日志）、`GET /healthz`。详见 [API 完备性评估](api-completeness-assessment.md) |

## 8. 建议的实施顺序（如后续决定实现）

0. **P0（RS485 现场可用性，最优先）**：单实例支持"一条链路多设备"——`luaClient` 单实例多 devid 输出（`set_value` 加 devid 参数 / `collect` 多设备）+ `modbusClient` 单实例多 slave 轮询。设计是"一条链路一个实例"，但当前单实例只能管单设备/单 slave，须扩展（见 §1.2）。不改代码的临时方案：lua 脚本内手动轮询（结果归单 devid）或 RS485↔以太网网关。
1. **P0（自愈可信 + 日志兜底）**：Failed 状态真实写入 + 实例 watchdog；默认文件日志 + 保留期 + 退出 flush。这俩是"现场可用性"底线。
2. **P0（埋点补齐）**：opcua/modbus/s7 补 per-instance stat + 统一 REST 端点。把 lua 的范例推广到全部内置插件。
3. **P1（可观测体系）**：`/metrics` ✅（2026-08-09 `0d4eb22`，免认证 + 手写 Prometheus 文本 + M4 数据源，spec `superpowers/specs/2026-08-09-metrics-prometheus-design.md`）+ 进程资源指标 ✅（M4 `/sysstatus`）+ 报文 trace 开关 ⬜（子系统 2，独立 spec）+ 日志 ring buffer ⬜（子系统 3，独立 spec）。
4. **P1（自愈增强）**：统一退避 + 连接状态暴露。
5. **P2**：熔断、结构化日志、trace 帧可视化、队列水位。

> 以上为评估建议，是否实现、实现哪些，由项目决策。本文档作为需求与优先级依据。
