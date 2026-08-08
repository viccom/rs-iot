# rs-iot 埋点规范与约束（M4）

本文件是所有实例指标（内置插件 / 外置插件 / 核心框架部件）的「宪法」。新增埋点必读。

## 1. 字段分层

- **核心 6 字段**（`InstanceStat`，编译期强制）：`status` / `tick_count` / `fail_count` / `last_activity_ts` / `last_activity_duration_ms` / `last_err`。
- **插件扩展**：走 `extra`（`#[serde(flatten)]` 透传）。加新插件**不改** `InstanceStat` struct。

## 2. 命名前瞻 Prometheus

- counter（单调增）用 `_count`；耗时 `_ms`；字节 `_bytes`。
- `status` 受控词表：`running` / `degraded` / `stopped`。
- 未来 `/metrics` 端点零重命名。

## 3. status 双轨制（不互相覆盖）

- 进程级 `state`（`InstState`：Starting/Running/Stopping/Stopped/Failed）= **死活权威**（supervisor 基于 JoinHandle）。
- 插件级 `status`（InstanceStat）= **health**（running/degraded）。
- REST 合并 `{state, health}`：Failed/Stopped 时 health 保留最后值，**不影响** state 判定。

## 4. 命名空间

- per-instance → lux key `stat:<inst_id>`（周期 flush）。
- 进程级 → 内存 `AtomicU64`（不落 lux，REST 直读）。

## 5. 写入频率

- per-instance：内存持有 + 周期 flush（默认每 5s 或每 50 次活动；常量见 `stat_tracker.rs`）。
- 进程级：atomic 不落盘（重启归零合理）。

## 6. 并发模型

- 跨 task 指标（hub/ctl/pipeline 进程级）→ `AtomicU64`。
- 单 task 内（插件采集循环）→ 普通字段（`StatTracker` 持有 `&mut self`，无需锁）。

## 特例：lua 可配 degraded 阈值

lua 保留自己的 `consecutive_fail` + `cfg.fail_threshold`（默认 10）判定 degraded，用 `StatTracker::set_status` 覆盖 StatTracker 内部固定阈值（DEGRADED_AFTER_FAILS=3）。其余插件用 StatTracker 默认阈值。

## 接入清单

| 实例类型 | 接入方式 | tick 语义 |
|---|---|---|
| 内置插件（opcua/modbus/s7/datasim） | `StatTracker` | 采集周期成功/失败 |
| lua | `StatTracker`（已迁移） | collect 周期 |
| 外置（sidecarClient） | `StatTracker`（observed） | 收到 DataUp |
| 核心框架（pipeline/hub/ctl/mqtt） | `ProcessMetrics` atomic | 各自咽喉 |
