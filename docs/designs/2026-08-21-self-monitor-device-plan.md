# rs-iot 自监控设备（selfMonitor）实施计划

> 日期：2026-08-21
> **状态**：implemented（2026-08-21 于 worktree `feat/self-monitor` 执行完毕，4 task 全落地 + 冒烟全过，已合并 master）
> **实施**：2026-08-21，T1 `42af12a`（hwid 覆盖键）→ T2 `bd02365`（collect 纯函数，**20 只读**——计划原写 15 系笔误，实现补 `healthy` Bool 凑齐四型）→ T3 `8bc8e2e`（Source 落地+注册+删 demo；含冒烟期发现的口径修复：`instance_health` 配置白名单——残留 stat 键如旧版 sim1 被杀进程未落 stopped 不计入健康统计）
> **代码**：`src/config.rs`（`hwid` 覆盖键）+ `src/lib.rs`（删 demo_datasim_config，无条件自启 self_monitor_config）+ `src/plugin/builtin/self_monitor/`（SelfObservation/collect/Source/apply_write——20 只读+3 可写标签一张表）
> **测试**：`cargo test -p rs-iot --features lua,lua-http --lib -j 4` **442/0**（self_monitor 17 用例：标签集精确断言/四型/口径白名单/apply_write 全分支）；clippy `-D warnings` 0；现场冒烟：17 标签四型出数（id==hwid）、watchdog/note 一命令双写回环（中文原样）、report_period 5→2 动态生效（日志+采样确认）、dataSim 实例上线计数联动（1/1/1/running/healthy）、stopinst self 可停（停后标签冻结）
> **遗留**：无（可写标签集后续按需扩展——`apply_write` match 加一臂即可；mem_percent/北向连接状态等候选未做，走扩展点）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 把 rs-iot 自身做成一个常驻设备（devid=`rsiot`）——自身识别/资源/统计变量作为标签周期上报数据面，并提供三个可写标签（watchdog/note/report_period）；同时删除 sim1 demo 设备（lib.rs 空配置分支），hwid 预留配置覆盖入口。

**Architecture:** 新内置插件 `selfMonitor`（Source trait，骨架照 `data_sim`）：tick 周期从 HostContext（lux + metrics）收集指标组 `Sample{devid:"rsiot"}` 推 hub → 既有 pipeline → lux/SSE/前端/北向 MQTT 全链路零新增。编排层**无条件**启动（替换 sim1 demo 分支，不落 lux `instances`——重启恒在、可 stopinst 临时停）。写命令走既有 `command/send` → CtlRegistry 路由（与 modbus/opcua 同机制）。hwid 覆盖走 `AppConfig` 新键。

**Tech Stack:** Rust（tokio select 循环 / inventory 注册 / sysinfo 启动期一次采样）。

---

## 「rs-iot 设备」标签清单（设计核心，文档与代码同源）

设备：`inst_id="self"`（编排层直启），`devid="rsiot"`，`Sample.hwid` 随行（即北向 topic `<hwid>/rsiot/datas`）。

### 只读标签（周期上报，默认 5s；类型覆盖 Float/Int/Bool/Str 四型）

| tag | 类型 | 来源 |
|---|---|---|
| `id` | Str | hwid（config 可覆盖，见 T1） |
| `version` | Str | `env!("CARGO_PKG_VERSION")` |
| `platform` | Str | 启动期一次 sysinfo 采样：`System::name() + os_version() + ARCH`（避免每 tick new_all） |
| `uptime_s` | Int | `ctx.metrics.started` elapsed |
| `cpu_percent` | Float | `process_metrics::sample_process_resource().0` |
| `mem_bytes` | Int | 同上 `.1`（进程内存字节） |
| `healthy` | Bool | 自身健康：无降级且无失败业务实例（inst_degraded==0 && inst_failed==0） |
| `collector_count` | Int | lux `HGETALL instances` 条目数 |
| `device_count` | Int | 遍历 instances 各 `devices` 总数 |
| `tag_count` | Int | 遍历 instances 各 `children` 总数 |
| `inst_running` / `inst_degraded` / `inst_failed` | Int | 遍历 lux `stat:*` 的 `status` 字段计数（**排除 `stat:self`**——口径=业务实例） |
| `alarm_active` | Int | lux `HLEN alarms` |
| `dbsize` | Int | lux `DBSIZE` |
| `pipeline_processed_count` | Int | `ctx.metrics.pipeline_processed_count` |
| `hub_depth_last` | Int | `ctx.metrics.hub_depth_last` |

### 可写标签（`POST /api/v1/command/send`，body `{"instance":"self","devid":"rsiot","cmd":"write","data":"{...}"}`；写后随下一上报周期在 rtdata 可见）

| tag | 类型 | 语义 |
|---|---|---|
| `watchdog` | Int | 写任意值→下周期回读（写通路自检，呼应 e2e 写回环方法论） |
| `note` | Str | 现场备注（这台网关是谁/在哪/干什么） |
| `report_period` | Int 秒 | 改自身上报周期（下周期生效，钳制 [1,3600]） |

不可写说明：日志级别运行时不可调（EnvFilter 无 reload handle，超本特性范围）；实例启停是 REST 动作不做成标签。

### 扩展点（用户要求"后期标签随时可加"）

- 只读：`collect()` 纯函数集中一张表（见 T2），加标签=加一行 + 本文档表格登记一行
- 可写：`handle_write` 的 match 加一臂 + 文档表格登记

---

## 已核实的机制契约（子代理不必重查）

- `HostContext`（src/host.rs:140）：`lux: EmbeddedClient`、`hwid`、`metrics: Arc<ProcessMetrics>`、`shutdown`、`ctl_registry`——selfMonitor 全部数据可从 lux+metrics 取，**无需改 HostContext**
- `ProcessMetrics`（src/process_metrics.rs:11）：`started: Instant` + 一组 `AtomicU64`（pipeline_processed_count / hub_depth_last 等，`load(Relaxed)` 读）
- `sample_process_resource() -> (f32 cpu, u64 mem_bytes)`（process_metrics.rs:155，公共函数）
- `Sample`/`SampleValue`（types.rs:114）：`{hwid, devid, values: BTreeMap<String,SampleValue>, timestamp}`；`SampleValue::{Float,Int,Bool,Str}`
- `CtlMessage`：`{instance, devid, cmd, data: String}`——`data` 是字符串化 JSON（modbus `handle_write(&session,&m.data)` 同款；解析失败 warn 不 panic）
- 插件注册：`inventory::submit!{ crate::plugin::registry::SourceEntry { name, create } }`（data_sim/mod.rs:138 同款）；`mod plugins` 里 `pub mod` 一行（builtin 模块树，找 `data_sim` 的声明位置照加）
- Source trait 签名（data_sim/mod.rs:25）：`async fn run(&self, ctx: HostContext, inst: InstConfig, tx: mpsc::Sender<Sample>, cmd_rx: mpsc::Receiver<CtlMessage>, token: CancellationToken)`
- ticker 设施：`crate::plugin::ticker::phase_ms(inst_id, period)`（jitter 相位）；`tokio::time::interval_at` + `MissedTickBehavior::Skip`
- `StatTracker`（crate::plugin::stat_tracker）：`new(inst_id, lux)` + tick_ok/flush/set_status——照 data_sim 用法（健康状态进 `stat:self`）
- demo 分支位置：src/lib.rs:286-292（`try_start_from_lux(&mgr,&client).await == 0` → `start_source(demo_datasim_config())`）；`demo_datasim_config()` 在 lib.rs:436
- lux 查询：`EmbeddedClient` 是 RESP 风格（`execute_value("DBSIZE",&[])` / HGETALL 用法照 db/ 层封装——**优先复用 `src/db/` 既有封装**：stat 层有 list/遍历封装则用之，alarms::list 已有；instances hgetall 在 routes_inst 有局部 fn，可搬纯查询到 db 层或插件内直接 RESP，取最少重复的路径）
- `start_source` 走 `InstanceManager`（mgr.start_source(config)）——self 实例不写 lux `instances`（demo 同款：runtime-only，roster 自动并入 `inst_id+state` 条目）

---

### Task 0: worktree

```bash
cd /e/Rs_Codes/rs-iot && git worktree add ../rs-iot-self -b feat/self-monitor
```

### Task 1: hwid 配置覆盖入口（TDD）

**Files:** `src/config.rs`（AppConfig 增 `hwid` 键）+ `src/lib.rs`（覆盖逻辑）+ `docs/reference/rest-api.md`（config 行提一句）

1. 失败测试（config.rs tests）：默认反序列化 `hwid` 为空串；`hwid = "custom-id"` 显式覆盖解析成功
2. 实现：`AppConfig` 增 `#[serde(default)] pub hwid: String`（文档注释：空=按硬件自动生成 `hwid::hardware_id()`；非空=部署期指定（多网关平台侧统一编址/规避同型号 CPU 撞号））
3. lib.rs `run_app`：`let hwid = if cfg.hwid.is_empty() { hwid::hardware_id() } else { cfg.hwid.clone() }`（types::Hwid::from）
4. rest-api.md `/api/v1/config` 要点补一句含 hwid 覆盖键
5. `cargo test -p rs-iot --features lua,lua-http --lib config -j 4` 绿 → commit `feat(config): hwid 部署期覆盖键——空缺省自动生成，非空指定（多网关唯一性预留）`

### Task 2: collect 纯函数 + 单测（TDD 核心）

**Files:** Create `src/plugin/builtin/self_monitor/mod.rs`（本任务只建模块+纯函数+tests，不实现 Source）+ builtin 模块树声明

1. 设计输入结构（避免纯函数直接耦合 lux 异步——异步查询在 T3 组装，纯函数只做汇总）：

```rust
/// 一次 tick 的原始观测（T3 从 lux/metrics/静态信息组装）。
pub(crate) struct SelfObservation {
    pub hwid: String, pub version: String, pub platform: String,
    pub uptime_s: u64, pub cpu: f32, pub mem_bytes: u64,
    pub collector_count: usize, pub device_count: usize, pub tag_count: usize,
    pub inst_running: usize, pub inst_degraded: usize, pub inst_failed: usize,
    pub alarm_active: u64, pub dbsize: i64,
    pub pipeline_processed: u64, pub hub_depth: u64,
    /// 可写标签当前值（watchdog/note 由 T3 状态区传入）。
    pub watchdog: Option<i64>, pub note: Option<String>,
}
pub(crate) fn collect(obs: &SelfObservation) -> BTreeMap<String, SampleValue>
```

2. 失败测试（≥5 用例）：四型齐备（platform/id 是 Str、cpu Float、计数 Int）；watchdog/note None 时不插键；全部键存在性（用固定期望键集 diff——**这张表就是标签清单的执行态**，加标签先改这里）；数值直传不丢精度；空实例统计（collector_count=0 时各计数 0）
3. 实现纯函数（一行一标签，BTreeMap 插入）
4. 测试绿 → commit `feat(self-monitor): 标签汇总纯函数 collect——20 只读标签四型覆盖 + 口径单测`

### Task 3: SelfMonitor Source + 注册 + 替换 demo

**Files:** `src/plugin/builtin/self_monitor/mod.rs`（补 Source impl）+ `src/lib.rs`（删 demo 分支/demo_datasim_config，改无条件自启）

1. `impl Source for SelfMonitor`（骨架照 data_sim）：
   - 常量：`INST_ID="self"`、`DEVID="rsiot"`、默认 period 5s（`app_config.period` 可覆盖——编排层自启用缺省）
   - 可写状态区：`watchdog: Option<i64>`、`note: Option<String>`、`report_period: u64`（Mutex 或 select 循环内可变局部——cmd 分支与 tick 分支同任务内，局部 mut 即可）
   - 静态信息 OnceLock：`platform`（`System::new_all()` 一次采样 name+os_version+ARCH）、version 编译期宏
   - tick：lux 异步查询组 `SelfObservation`（instances HGETALL 统计、`stat:*` 遍历计 status 排除 `stat:self`、HLEN alarms、DBSIZE）→ `collect()` → `Sample{hwid: ctx.hwid, devid:"rsiot", ...}` → tx.send；StatTracker tick_ok
   - `cmd_rx` 分支：`serde_json::from_str(&m.data)` → match tag："watchdog"（as_i64）、"note"（as_str trim，≤256 字符）、"report_period"（as_u64 钳 [1,3600] 并重建 ticker）；未知 tag/解析失败 warn（可感知，照 opcua handle_write 风格）
   - select 三臂：token / cmd_rx / ticker（照 data_sim + flush_ticker）
2. `inventory::submit!` 注册 `"selfMonitor"`；模块树声明
3. lib.rs：删 `demo_datasim_config()` 与空配置分支 → **无条件** `mgr.start_source(self_monitor_config()).await`（构造 InstConfig：inst_id "self"、app_code "selfMonitor"、devices 留空——tag 由代码生成不由配置驱动；日志 `"started self-monitor instance (devid=rsiot)"`）；dataSim 插件本身**保留**（用户仍可手建 dataSim 实例）
4. 单测：self_monitor_config 形态；cmd 解析各臂（watchdog/note/period 钳制/坏 JSON/未知 tag）——解析逻辑提纯函数测
5. `cargo test -p rs-iot --features lua,lua-http --lib -j 4` 全绿 + `cargo clippy -j 4 --all-features -- -D warnings`（注意仓内 clippy 惯例）0 警告 → commit `feat(self-monitor): selfMonitor 内置插件——rsiot 设备 20 只读+3 可写标签；删 sim1 demo 改无条件自启`

### Task 4: 收口回归 + 现场冒烟（主会话亲自执行）

1. `cargo test -p rs-iot --features lua,lua-http -j 4` 全量 + `cargo fmt --check`
2. 冒烟（复用 F:\rsiot-field\work\e2e-lua-d，config 无自启实例=原 demo 场景）：
   - 新构建 rs-iot.exe 起进程 → `python resp.py HGETALL rtdata:rsiot`：20 只读标签齐、`id`==启动日志 hwid、四型正确（cpu 浮点/note 字符串）
   - `command/send` watchdog=7 → 下一周期 `rsiot.watchdog=7`；note 写中文 → 回读；report_period 5→2 → 时间戳间隔变短
   - 建一个 modbus 实例 → `collector_count`/`device_count`/`tag_count` 变化；`stopinst {"inst_id":"self"}` → inststatus stopped、rtdata:rsiot 停更
   - 清理：实例删、进程杀、端口释放
3. 前端零改动验证：Telemetry 通用面板自动出现 rsiot 设备（不实测浏览器，SSE/rtdatas 机制既有——登记冒烟记录即可）

### Task 5: 文档收口

1. 本计划头部实施块回填（代码/测试/遗留）
2. rest-api.md §4.7 写命令补自设备示例（instance:"self" 三可写标签形态）+ `/api/v1/plugins` 将自动多出 selfMonitor（无需改文）
3. `docs/ops/releases/2026-08-21-self-monitor.md`（照既有 release note 惯例：变更/标签表/迁移注意——sim1/devA 不再出现属行为变更）
4. `python scripts/check_docs.py` → commit docs
5. 合并 master（用户确认）→ worktree 清理

## 验收清单

- [ ] hwid config 覆盖键生效（空=自动；非空=指定），测试覆盖
- [ ] rtdata:rsiot 20 只读标签四型齐备、口径排除 self
- [ ] 三可写标签命令回环（watchdog/note/report_period）
- [ ] sim1/devA 永不再出现；selfMonitor 无条件自启、可 stopinst
- [ ] cargo test 全绿 + clippy 0 + fmt 过；现场冒烟记录在案
- [ ] 文档三处（计划实施块/rest-api 示例/release note）

## 显式不做

- 日志级别运行时可调（EnvFilter 无 reload，另行立项）
- mem_percent（需系统总内存另查；mem_bytes 已覆盖需求，后续加标签走扩展点）
- 前端专属面板（通用 Telemetry/CommandPanel 已可见可操作）
