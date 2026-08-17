# rs-iot 埋点规范与约束（M4）

本文件是所有实例指标（内置插件 / 外置插件 / 核心框架部件）的「宪法」。新增埋点必读。

## 1. 字段分层

- **核心 6 字段**（`InstanceStat`，编译期强制）：`status` / `tick_count` / `fail_count` / `last_activity_ts` / `last_activity_duration_ms` / `last_err`。
- **插件扩展**：走 `extra`（`#[serde(flatten)]` 透传）。加新插件**不改** `InstanceStat` struct。

## 2. 命名对齐 Prometheus（P1 #1 已落地）

- counter（单调增）用 `_total`；耗时 `_ms`；字节 `_bytes`；秒 `_seconds`。
- `status` 受控词表：`running` / `degraded` / `stopped`。
- `/metrics` 端点已上线（P1 #1，commit `1acacb0`），exposition 文本零重命名直接对接 Prometheus/Grafana。

## 3. status 双轨制（不互相覆盖）

- 进程级 `state`（`InstState`：Starting/Running/Stopping/Stopped/Failed）= **死活权威**（supervisor 基于 JoinHandle）。
- 插件级 `status`（InstanceStat）= **health**（running/degraded）。
- REST 合并 `{state, health}`：Failed/Stopped 时 health 保留最后值，**不影响** state 判定。

## 4. 命名空间与 REST 出口

- per-instance → lux key `stat:<inst_id>`（周期 flush）。
- 进程级 → 内存 `AtomicU64`（不落 lux，REST 直读）。
- **REST 出口**（两个，同源 M4 数据）：
  - `GET /api/v1/inst/stats?inst_id=`（JSON，人查出口，`{inst_id, app_code, host_kind, state, health}`）
  - `GET /metrics`（Prometheus exposition 文本，机器出口，免 JWT）—— per-instance 5 family（`rs_iot_instance_info`/`tick_total`/`fail_total`/`last_activity_duration_ms`/`last_activity_ts`）+ 进程级 14 family
- **同批次可观测姊妹端点**（非埋点，同属 P1 可观测面）：`POST/GET/DELETE /api/v1/inst/trace`（报文 hex dump，target `io_trace`）+ `GET /api/v1/logs/recent`（日志 ring buffer，`[log].ring_buffer_capacity` 默认 2000）。详见 [`rest-api.md`](rest-api.md) §4.10。
- `/api/v1/lua/stats`（lua 专属旧端点）**已废弃移除**，统一到 `/api/v1/inst/stats`。

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
