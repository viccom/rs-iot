# M4 埋点体系设计（Instrumentation）

> **状态**：已确认设计，待写实现计划
> **范围**：rs-iot 全埋点体系——统一规范 + 核心框架咽喉 + 内置/外置插件 + REST 暴露
> **前置**：M3（日志兜底 + 自愈可信）已合并 master（commit 4624c12）
> **诉求来源**：`scenario-gap-assessment.md` §4（诉求③：宿主 + 插件埋点监测值）+ §8 实施顺序 #2（P0 埋点补齐）
> **For agentic workers**：本 spec 是 `writing-plans` 的输入，分阶段实施见 §8。

---

## 1. 背景

scenario-gap §4 现状：**仅 `luaClient` 有结构化指标**（`stat:<inst>` JSON + REST `/api/v1/lua/stats`），opcua/modbus/s7/dataSim **零计数器**，核心框架（hub/pipeline/CtlRegistry/mqtt）**全盲**。现场诉求③要"宿主程序及插件的埋点监测值"，当前对 4 个内置插件 + 核心总线完全空白。

前序讨论已确认：核心框架（数据上行 + 控制下行总线的咽喉）比插件更该优先埋点——插件埋的是"某条链路活得好不好"，框架埋的是"整条总线还通不通"。且 `CtlRegistry::send`（`host.rs:45`）`let _ = try_send(msg)` **静默丢弃写命令**，是当前最危险的盲区（控制面不可观测）。

本设计把 lua 已验证的 `stat:<inst>` 模式**抽象为通用规范**，推广到全部内置插件 + 核心框架 + 外置插件，并定义 6 条约束防止"各做各的"重蹈覆辙。

## 2. 目标与非目标

### 目标
1. **统一规范**：一个 `InstanceStat` 结构 + 6 条约束文档，所有实例（内置/外置/框架部件）共用。
2. **核心框架咽喉埋点**：CtlRegistry（堵静默丢命令盲区）+ pipeline + hub 背压 + mqtt + 进程资源，进程级内存 atomic、REST 直读。
3. **插件埋点补齐**：opcua/modbus/s7/datasim 接入；lua 迁移到通用规范；外置 sidecarClient 用 observed stat。
4. **统一 REST 暴露**：`GET /api/v1/inst/stats`（`{state, health}` 合并）+ `/sysstatus` 扩展；废弃 `/api/v1/lua/stats`。
5. **前瞻 Prometheus 命名**：字段命名兼容未来 `/metrics`（counter/gauge 区分、`_count`/`_ms`/`_bytes` 后缀），本次不引入 metrics 依赖。

### 非目标（后置，见 §11）
- `/metrics` Prometheus 文本端点（独立后续 spec，本次只前瞻命名）
- per-slave 明细、报文 trace 开关、日志 ring buffer（scenario-gap §5/§7 P1/P2）
- 自重启、统一退避、熔断（§2 P1/P2，非埋点范畴）
- 外置 bridge 协议扩展（外部进程自报 stat 帧）

## 3. 现状（埋点盲区全景）

数据流咽喉标注（🔴=零埋点盲区）：

```
── 数据上行 ──────────────────────────────────────────────
N×Source ──► [① hub mpsc 256] 🔴 ──► [② pipeline] 🔴 ──► [③ db::rtdata::write] ──► lux
                                                              │
                                              ④ MqttPub 周期从 lux 抽取 🔴 ──► broker
── 控制下行 ──────────────────────────────────────────────
MQTT/REST ──► [⑤ CtlRegistry.send] 🔴🔴 ──► 实例 cmd_rx ──► handle_write
── 生命周期/进程 ─────────────────────────────────────────
            [⑥ InstanceManager] 🟡(仅 InstState)   [⑦ 进程 CPU/mem/uptime] 🟡(仅 dbsize)
```

| 部位 | 现状 | 盲区 |
|---|---|---|
| ① hub channel | 零 | 背压不可见（256 满 Source 静默阻塞） |
| ② pipeline | 零（`pipeline.rs:48` 失败只 warn） | 数据上行吞吐黑盒 |
| ③ db::rtdata::write | 零 | 入库成功率无从量化 |
| ④ MqttPub | 零（固定退避、无 stat） | 北向是否在推数据不可见；且是控制下行入口之一（`mqtt_pub:67` send 返回值忽略） |
| ⑤ CtlRegistry | **零 + 静默丢命令**（`host.rs:45` `let _ = try_send`） | 🔴 最危险：写命令丢弃不可观测 |
| ⑥ InstanceManager | 部分（M3 加 Failed） | 缺 uptime/重启次数 |
| ⑦ 进程资源 | 部分（lux DBSIZE 在 /sysstatus） | sysinfo 已依赖但 CPU/mem/uptime 未用 |

**lua 唯一范例**：`db::luastat::LuaStat`（status/collect_count/collect_fail_count/last_collect_ts/last_collect_duration_ms/vm_restart_count/script_version/last_err），落 lux `stat:<inst>`，每 collect/reload/启停写一次，REST `/api/v1/lua/stats?inst_id=` 暴露。

## 4. 规范层设计

### 4.1 InstanceStat —— 通用实例指标结构

```rust
/// 通用实例指标（所有实例共用，落 lux key `stat:<inst_id>`）。
///
/// 核心 6 字段编译期强制填全；插件扩展走 `extra`（serde flatten 透传）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstanceStat {
    /// health 维度：`running` / `degraded` / `stopped`。
    /// 死活看进程级 `InstState`（REST 合并为 `{state, health}`），非此字段。
    pub status: String,
    /// counter：主活动计数（collect/publish/process/DataUp 周期数），单调增。
    pub tick_count: u64,
    /// counter：失败计数 → 成功率 = 1 - fail_count/tick_count。
    pub fail_count: u64,
    /// gauge：最近一次活动墙上时间（unix 秒）。
    pub last_activity_ts: i64,
    /// gauge：最近一次活动耗时（毫秒）。
    pub last_activity_duration_ms: u64,
    /// 最近一次失败原因（成功时清空）。
    pub last_err: String,
    /// 插件扩展字段（flatten 透传）。opcua 填 `session_reconnect_count` 等，
    /// lua 填 `vm_restart_count`/`script_version`，框架部件填空 map。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
```

**为何不加 `connected` 为第 7 核心字段**：lua 的 `io_connected` 现后置，opcua/modbus/s7 连接语义各异（session/slave/TCP），强行统一会失真。连接状态作各插件 `extra` 字段，由插件自填合适语义。核心 6 字段足够表达"活跃度 + 成功率 + 最近错误"。

### 4.2 六条规范约束（落 `docs/instrumentation-conventions.md`）

1. **字段分层**：核心 6 字段编译期强制（struct 保证填全）；扩展走 `extra` flatten 透传。加新插件不改共享类型，核心字段不漏。
2. **命名前瞻 Prometheus**：counter 单调增用 `_count`、耗时 `_ms`、字节 `_bytes`；`status` 受控词表（running/degraded/stopped）。未来 `/metrics` 零重命名。
3. **status 双轨制**：进程级 `state`（`InstState`，supervisor 写，死活权威）≠ 插件级 `status`（health，插件自报）。REST 合并 `{state, health}`，**不互相覆盖**——进程级 Failed 时 health 保留最后值但不影响 state 判定。外置插件 health 由 bridge observed 派生（见 §6.4）。
4. **命名空间**：per-instance → lux key `stat:<inst_id>`；进程级 → 内存 `AtomicU64`（不落 lux，REST 直读）。
5. **写入频率**：per-instance 内存持有最新值 + 周期 flush lux（默认每 5s 或每 50 次活动落一次，常量定义，v1 不配置化）；进程级 atomic 不落盘（重启归零合理——进程级指标语义是"本进程实时健康"）。
6. **并发模型**：跨 task 指标用 `AtomicU64`（hub/ctl/pipeline 进程级）；单 task 内用普通字段（插件采集循环串行，`StatTracker` 持有 `&mut self`，无需锁）。

### 4.3 db::stat 通用模块（替代 db::luastat）

把 `src/db/luastat.rs` 泛化为 `src/db/stat.rs`：

```rust
/// 写实例指标（JSON，`SET stat:<inst_id>`）。
pub async fn write(c: &EmbeddedClient, inst_id: &str, stat: &InstanceStat) -> Result<(), RsIotError>
/// 读实例指标（无则 None）。
pub async fn read(c: &EmbeddedClient, inst_id: &str) -> Result<Option<InstanceStat>, RsIotError>
```

key 仍是 `stat:<inst>`（与 lua 现有数据兼容）。`LuaStat` 删除，lua 改用 `InstanceStat`（`vm_restart_count`/`script_version` 迁入 `extra`）。

## 5. 核心框架咽喉埋点

### 5.1 ProcessMetrics —— 进程级指标单例

```rust
/// 进程级指标（Arc，注入 HostContext；REST /sysstatus 直读 atomic）。
pub struct ProcessMetrics {
    /// 进程启动时刻（REST 算 uptime，不参与序列化）。
    pub started: std::time::Instant,
    // 控制下行（CtlRegistry）
    pub ctl_send_count: AtomicU64,
    pub ctl_dropped_count: AtomicU64,      // channel 满丢
    pub ctl_not_found_count: AtomicU64,    // 实例未注册
    // 数据上行（pipeline）
    pub pipeline_processed_count: AtomicU64,
    pub pipeline_write_fail_count: AtomicU64,
    // hub 背压（gauge）
    pub hub_depth_last: AtomicU64,
    pub hub_depth_max: AtomicU64,
    // 北向（mqtt）
    pub mqtt_publish_count: AtomicU64,
    pub mqtt_publish_fail_count: AtomicU64,
    pub mqtt_connected: AtomicBool,
}
```

**注入方式**：`HostContext` 加字段 `metrics: Arc<ProcessMetrics>`（与 `lux`/`ctl_registry` 同级），所有 task（pipeline/mqtt/插件/CtlRegistry）共享。`AppState`（REST）也持有同一 `Arc`，handler 直读 atomic。

**CPU/mem 不进 ProcessMetrics**：sysinfo 采样有开销，放 `/sysstatus` handler 现采（低频 REST），不常驻 task。

### 5.2 CtlRegistry 改造（堵最危险盲区）

`host.rs:41` `send()` 当前 `let _ = try_send(msg)` 完全静默。改造为分类计数 + warn，**返回值语义不变**（调用方 mqtt_pub/routes_command 零改动）：

```rust
pub fn send(&self, inst: &str, msg: CtlMessage) -> bool {
    self.metrics.ctl_send_count.fetch_add(1, Ordering::Relaxed);
    if let Some(tx) = self.map.get(inst) {
        match tx.value().try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.ctl_dropped_count.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("[ctl] {inst} channel full, command dropped");
                true // 保持原语义：找到实例（满丢对标 Go default）
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.ctl_dropped_count.fetch_add(1, Ordering::Relaxed);
                true // map 还在但 channel 关了（实例收不到），保持原返 true 语义
            }
        }
    } else {
        self.metrics.ctl_not_found_count.fetch_add(1, Ordering::Relaxed);
        false
    }
}
```

`CtlRegistry` 加字段 `metrics: Arc<ProcessMetrics>`（构造时注入）。**规则 10 大声失败**：丢弃事实被计数 + warn，不再静默。

### 5.3 pipeline / hub 背压 / mqtt

- **pipeline**（`pipeline.rs:44`）：`db::rtdata::write` 成功 → `pipeline_processed_count.fetch_add(1)`；失败 → `pipeline_write_fail_count.fetch_add(1)`（现有 warn 保留）。
- **hub 背压**：tokio mpsc `len()` 仅 Receiver 端可读（`hub_rx` 在 pipeline task）。pipeline 每轮 `recv()` 后采样 `hub_rx.len()`，更新 `hub_depth_last`（store）+ `hub_depth_max`（fetch_max）。`hub_depth_max→256` 即 Source 阻塞告警。
- **mqtt_pub**（`mqtt_pub/mod.rs`）：`publish_rtdata` 每个 topic publish 成败 → `mqtt_publish_count`/`mqtt_publish_fail_count`；`connection.poll()` 收到 ConnAck → `mqtt_connected=true`，连接错误 → `mqtt_connected=false`（现有 sleep 2s 退避保留）。

### 5.4 进程资源（/sysstatus 现采）

`/sysstatus` handler 用 sysinfo crate（已是依赖）现采 CPU/mem；uptime 从 `ProcessMetrics.started` 算。采样失败降级返 0/null，不 500。

## 6. 插件埋点

### 6.1 StatTracker —— 规范落地的统一接入机制

把"持有 InstanceStat + 周期 flush + tick 计数"封进 helper，插件只调几个方法（DRY，避免每插件各写 flush 逻辑）：

```rust
/// 实例指标追踪器（插件主采集 task 内持有，单 task 串行，&mut self 无需锁）。
pub struct StatTracker {
    stat: InstanceStat,
    inst_id: String,
    lux: EmbeddedClient,
}
impl StatTracker {
    pub fn new(inst_id: String, lux: EmbeddedClient) -> Self { /* status="running", 计数 0 */ }
    /// 活动成功：tick_count++，更新 last_activity_*，清 last_err，status=running。
    pub fn tick_ok(&mut self, duration_ms: u64) { /* 连续失败恢复时 status=running */ }
    /// 活动失败：fail_count++，last_err=err；连续失败达阈值 → status=degraded。
    pub fn tick_fail(&mut self, err: &str) { /* 阈值常量 DEGRADED_AFTER_FAILS=3 */ }
    /// 写扩展字段（opcua session_reconnect_count / lua vm_restart_count 等）。
    pub fn extra_insert(&mut self, k: &str, v: serde_json::Value) { self.stat.extra.insert(k.into(), v); }
    /// flush 到 lux stat:<inst>（失败仅 warn，不返 Err 不阻塞采集）。
    pub async fn flush(&self) { let _ = db::stat::write(&self.lux, &self.inst_id, &self.stat).await; }
    /// 是否到周期 flush 点（tick_count % FLUSH_EVERY_TICKS == 0）。
    pub fn should_flush(&self) -> bool { self.stat.tick_count % FLUSH_EVERY_TICKS == 0 }
}
```

常量：`FLUSH_EVERY_TICKS: u64 = 50`、`FLUSH_INTERVAL_SECS: u64 = 5`、`DEGRADED_AFTER_FAILS: u32 = 3`（v1 常量，不配置化）。

**插件 task 接入模式**（select! 加 flush ticker）：

```rust
let mut tracker = StatTracker::new(inst_id.clone(), ctx.lux.clone());
let mut flush_ticker = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
loop {
    tokio::select! {
        _ = token.cancelled() => { tracker.flush().await; break; }
        _ = flush_ticker.tick() => { tracker.flush().await; }
        res = collect_once() => {
            match res {
                Ok(d) => { tracker.tick_ok(d); /* push sample */ }
                Err(e) => { tracker.tick_fail(&e.to_string()); }
            }
            if tracker.should_flush() { tracker.flush().await; }
        }
    }
}
```

### 6.2 lua 迁移（范例 → 通用样板）

`lua_client/mod.rs` 的 `LuaStat` 局部变量改为 `StatTracker`：
- collect 成功 → `tracker.tick_ok(duration_ms)`；失败 → `tracker.tick_fail(err)`
- VM 重建 → `tracker.extra_insert("vm_restart_count", n)`
- 脚本版本 → `tracker.extra_insert("script_version", v)`
- 退出 → `tracker.flush()`（status 写 stopped 由最后 flush 前设 `tracker.stat.status="stopped"`）

**行为变化**：lua 从"每 collect 写 lux"改为"周期 flush（≤5s 滞后）"。REST 查到的 stat 最多滞后 5s，运维无感，但省 opcua/modbus 大点表时的 lux 热点。

### 6.3 内置插件接入

| 插件 | tick 语义 | extra 扩展 |
|---|---|---|
| **datasim** | 模拟采集周期 | `{}`（最简，作接入样板测试） |
| **opcua** | 订阅回调汇聚后 | `session_reconnect_count`、`subscription_alive`(bool) |
| **modbus** | 轮询周期 | `exception_count`（FC exception 累计） |
| **s7** | 轮询周期 | `reconnect_count` |

埋点位置：各插件主采集 task 内，**采集结果处理点**（协议库回调/响应汇聚后）调 `tick_ok`/`tick_fail`。单 task 串行持有 `StatTracker`。

### 6.4 外置插件（sidecarClient，observed stat）

外部进程不写 lux（方案 C）。rs-iot 侧 bridge task **观察**指标，用 StatTracker：

| 字段 | observed 来源 |
|---|---|
| `tick_count` | bridge 收到并转发的 `DataUp` 帧数 |
| `last_activity_ts` | 最后 `DataUp` 时间 |
| `fail_count` | bridge 协议解析错误/帧错误累计 |
| `status` | `connected && 最近有 DataUp` → running，否则 degraded |
| extra `connected` | TCP 连接状态 |
| extra `last_heartbeat_ts` | 最后心跳时间 |
| extra `reconnect_count` | bridge 侧重连次数 |

语义：rs-iot 观察到的外部活跃度（非外部自报）。外置实例 `health` 不再 null，但外部内部细节仍拿不到（除非未来扩展 bridge 协议加 stat 帧，后置）。

## 7. REST 端点

| 端点 | 行为 |
|---|---|
| `GET /api/v1/inst/stats?inst_id=<inst>` | `{inst_id, app_code, host_kind, state, health}`：`app_code`/`host_kind` 来自 InstanceManager 内存存的 `InstConfig`（实例分类，见实现要点）；`state`=进程级 `InstState`（`InstanceManager::status()`，死活权威）；`health`=lux `stat:<inst>` JSON（核心+extra flatten），无则 `null` |
| `GET /api/v1/inst/stats`（不带参） | 全实例数组 `[{inst_id, app_code, host_kind, state, health}, ...]`；前端按 `app_code=luaClient`→lua 脚本 / `host_kind=external`→外置 / 其余→内置 自行分类 |
| `GET /api/v1/sysstatus`（扩展） | 现 `{status, dbsize, instances}` + `metrics:{ctl_send_count, ctl_dropped_count, ctl_not_found_count, pipeline_processed_count, pipeline_write_fail_count, hub:{depth_last, depth_max}, mqtt:{publish_count, publish_fail_count, connected}}` + `process:{uptime_secs, cpu_pct, mem_bytes}` |
| ~~`GET /api/v1/lua/stats`~~ | **废弃**（移除路由 + `routes_lua::stats` handler；`reload`/`scripts` 保留） |

`/inst/stats` 实现要点：`state` 从 `mgr.status()` 取（进程级，权威）；`health` 从 lux `stat:<inst>` 读（`db::stat::read`）；`app_code`/`host_kind` 从 `mgr` 内存取。合并呈现，进程级 Failed 时 health 仍返最后值（不掩盖）。

**实例分类数据源**：`InstanceManager` 加内存表 `configs: DashMap<String, InstConfig>`，`start_source`/`start_sink` 时 insert（`inst_id` → `InstConfig`），`stop` 时保留（配置态；状态走 Stopped，重新 start 覆盖）。`/inst/stats` handler 从 `configs` 取 `app_code`/`host_kind` 注入响应顶层——零 lux 往返、与运行态状态同源。**分类不进 `InstanceStat`**（运行态健康指标保持纯粹，分类是配置态）。

**JWT 保护**：新端点默认 JWT（与现有 routes 一致），不加 `PUBLIC_PATHS`。

**破坏性变更**：废弃 `/api/v1/lua/stats`——现有调该端点的客户端会 404，需改调 `/api/v1/inst/stats?inst_id=`。spec 明确标记，CLAUDE.md「常见流程」更新。

## 8. 分阶段实施（纵向分层，每阶段独立可交付/可测试/可合并）

| Phase | 内容 | 验收 |
|---|---|---|
| **1 规范地基** | `InstanceStat` struct + `db::stat` 通用模块（替 luastat，删 `LuaStat`）+ `StatTracker` + `docs/instrumentation-conventions.md`（6 约束）+ 统一 `/inst/stats`（{state,health}）+ 废弃 `/lua/stats` + **lua 迁移** | lua REST 输出兼容（字段平铺）；统一端点对 lua 返 {state,health}；`/lua/stats` 404 |
| **2 核心咽喉** | `ProcessMetrics` struct + 注入 `HostContext`/`AppState` + **CtlRegistry 改造**（计数+warn，返回语义不变）+ pipeline 计数 + hub 深度 gauge + `/sysstatus` 扩展 | dropped_count 可观测；/sysstatus 含 metrics；CtlRegistry 现有调用方测试不破坏 |
| **3 内置插件** | opcua/modbus/s7/datasim 套 StatTracker（核心+extra） | 各插件 /inst/stats 返真实指标（tick_count 随采集增长） |
| **4 外置+资源** | sidecarClient observed stat + mqtt_pub 计数/connected + 进程资源（sysinfo CPU/mem/uptime） | 外置实例 health 非 null；mqtt 指标可见；/sysstatus 含 process 资源 |

**为何 Phase 1 含 lua 迁移**：lua 是唯一有埋点的插件，是通用规范的"活样板"。Phase 1 不带 lua 迁移就没有可验证的实例（其他插件 Phase 3 才接入），规范无法验收。

## 9. 测试策略

- **InstanceStat**：serialize/deserialize roundtrip（核心 6 字段 + extra flatten 透传）；extra 未知字段不丢。
- **db::stat**：write/read roundtrip、read 缺失返 None、实例间隔离（沿用 `luastat.rs` 现有测试模式，迁移）。
- **StatTracker**：`tick_ok`/`tick_fail` 计数正确、连续失败达 `DEGRADED_AFTER_FAILS` 转 degraded、恢复转 running、`should_flush` 边界（tick%50）、`flush` 写 lux。
- **CtlRegistry**：send 分类计数（Ok→send_count、Full→dropped_count、未注册→not_found_count）+ **返回值语义不变**（Full 返 true、未注册返 false）。
- **ProcessMetrics**：atomic fetch_add/fetch_max 正确；`/sysstatus` JSON 含 metrics 结构。
- **统一 REST 端点**：`{state,health}` 合并、health=null（无 stat 实例）、全实例列表、JWT 保护（沿用 `api_flow.rs` 模式）；进程级 Failed 时 health 仍返最后值。
- **外置 observed**：bridge 收 DataUp → tick_count++、失联 → status degraded（扩 `sidecar_e2e.rs`）。
- **/inst/stats 分类注入**：响应含 `app_code`/`host_kind`（从 InstanceManager `configs` 取）；lua/opcua/sidecarClient 实例分类字段正确。

所有新测试受 CI `RUSTFLAGS=-D warnings` 约束，保持零警告。

## 10. 错误处理

- **stat flush 失败**（lux 写）：`StatTracker::flush` 仅 warn，不返 Err、不阻塞采集（对标 pipeline 写失败降级）。
- **ProcessMetrics 计数**：`fetch_add`/`fetch_max` 用 `Ordering::Relaxed`（性能，计数无需严格序）。
- **sysinfo 采样失败**：`/sysstatus` handler 降级返 0/null，不 500。
- **`/inst/stats` 读 lux 失败**：health 返 null + warn，state 仍返（进程级不依赖 lux）。
- **废弃端点**：`/lua/stats` 移除后返 404（axum 默认），不特殊处理。

## 11. 简化（YAGNI，明确后置）

| 项 | 后置原因 |
|---|---|
| `/metrics` Prometheus 文本端点 | 独立后续 spec；本次只前瞻命名（§4.2 约束 2） |
| per-slave 明细 / 报文 trace / 日志 ring buffer | scenario-gap §5/§7 P1/P2 |
| 自重启 / 统一退避 / 熔断 | §2 P1/P2，非埋点范畴 |
| 外置 bridge 协议扩展（外部自报 stat 帧） | 当前 observed stat 已满足"外部活跃度"可见性 |
| StatTracker flush 周期配置化 | v1 常量（5s/50），现场无调参诉求前不进 config |
| 连接状态作核心字段 | 各插件连接语义各异，作 extra 更合适（§4.1） |

## 12. 验收标准

1. **规范落地**：`docs/instrumentation-conventions.md` 存在，6 约束可逐条对照代码；`InstanceStat`/`db::stat`/`StatTracker` 公开 API 完整 doc。
2. **CtlRegistry 盲区消除**：`/sysstatus` 返 `ctl_dropped_count`/`ctl_not_found_count`；故意填满 cmd channel 后 send，dropped_count 增长 + 日志见 warn。
3. **全实例有 stat + 分类**：`/inst/stats`（不带参）对 opcua/modbus/s7/lua/datasim/sidecarClient 实例均返非 null health + 正确 `app_code`/`host_kind`（datasim/sidecar 需起实例）。
4. **进程级可见**：`/sysstatus` 含 pipeline/hub/mqtt/process 指标；采集运行时 processed_count 增长、hub_depth_max 反映负载。
5. **lua 迁移无回归**：lua 实例 collect 正常，`/inst/stats?inst_id=<lua>` 返 `{state:running, health:{status, tick_count, ..., vm_restart_count, script_version}}`。
6. **端点废弃生效**：`/api/v1/lua/stats` 返 404；`reload`/`scripts` 不受影响。
7. **CI 全绿**：`cargo fmt --all --check` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo test --all-targets --all-features` 三项通过。
