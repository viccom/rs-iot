# M3：日志兜底 + 自愈可信 设计

> **阶段**：M3（运行时健壮性路线）。对应 [`scenario-gap-assessment.md`](../../scenario-gap-assessment.md) §8 实施顺序第 1 条「P0（自愈可信 + 日志兜底）」，紧接 RS485 M1（modbus 多 slave ✅）/ M2（lua 多设备 ✅）之后。
> **状态**：设计经 brainstorm 确认（默认落盘 `./logs` / Failed 仅内存可信 / task 退出即 Failed / 方案 A 最小内嵌），待 writing-plans。
> **方案**：A —— 最小内嵌实现，零新依赖，改动集中在 `config.rs` / `lib.rs` / `instance.rs`。

---

## 1. 背景与定位

工业现场诉求（scenario-gap §0 ②「故障定位」+ ①「自诊断自愈」）当前两个 🔴 P0 缺口：

- **日志兜底**：默认不落盘、无保留期、退出丢末尾（§3）—— 故障时无现场日志
- **自愈可信**：`Failed` 状态定义了却从未写入，task 死了仍显 `Running`（§2）—— 自诊断不可信

M3 补这两块「现场可用性底线」。**不做自重启**（§2 标注的「可选增强」，后置）。

---

## 2. 现状（核实到行号）

### 2.1 日志
- `src/config.rs:123-140` —— `LogConfig { dir, level }`，`dir` 默认空 → 开箱仅终端
- `src/lib.rs:48-77` —— `init_tracing`：daily 轮转 `rs-iot.log.YYYY-MM-DD`，**无保留期/大小清理**（长期无限堆积）
- `src/lib.rs:70` —— `std::mem::forget(guard)` → 进程退出不 flush，丢末尾几条（恰是故障现场）
- `config.toml` —— 当前无 `[log]` section（纯 Default，仅终端）

### 2.2 实例自愈
- `src/plugin/instance.rs:22-35` —— `InstState` 5 态含 `Failed`，**全仓从未写入**
- `src/plugin/instance.rs:94-100` —— `start_source` spawn 后立即标 `Running`，task 早退（配置错误 return）仍显 `Running`
- 无 watchdog —— task panic/早退无人感知，需人工 `stop` + `start`
- 状态仅内存 `DashMap`（`instance.rs:50`），进程重启清空

### 2.3 关闭流程（影响 stop 设计）
`src/lib.rs:209-219`：`shutdown.cancel()` → abort pipeline/mqtt/axum/bridge → `mgr.stop_all()` → `db::shutdown_lux()`（先 SAVE）。所有 source 持 `shutdown.child_token()`，`shutdown.cancel()` 即传到各 source。`stop_all` 必须等 task 退出后再 `shutdown_lux`，否则与 lux 写竞争 —— **这是 stop 必须保留「等 join」语义的硬约束**。

---

## 3. 目标 / 非目标

### 目标
1. **日志默认落盘** `./logs` + 按天/大小保留期 + 退出 flush（故障现场可追溯）
2. **task 异常退出真实标 Failed** + 告警日志 + REST 可见（自诊断可信）

### 非目标（后置，不在 M3）
- 自重启（§2「可选增强」）
- Failed 持久化到 lux（重启可见）—— 仅内存可信，进程重启状态自然重置
- 日志 ring buffer / 结构化日志 / 错误聚合限流（§3 P1/P2）
- 统一退避 / 熔断 / 连接状态暴露（§2 P1/P2）
- opcua/modbus/s7 埋点补齐（§4，属 M4）

---

## 4. 详细设计

### 4.1 日志子系统

#### 4.1.1 配置（`src/config.rs`）

`LogConfig` 扩两个字段，`#[serde(default)]` 保持缺失容错：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `dir` | `String` | `"./logs"` | 日志目录（原 `""`）；**显式 `dir=""` 仍可关闭文件层**（保留逃生口） |
| `level` | `String` | `"info"` | 仅当未设 `RUST_LOG` 时作缺省 |
| `max_days` | `u64` | `7` | 按文件名日期删旧，保留最近 N 天 |
| `max_size_mb` | `u64` | `0` | `0`=不限大小；`>0` 时按目录总大小删最旧（双轮转） |

**向后兼容**：现有 config.toml 无 `[log]` → Default → 自动落盘 `./logs`（开箱即落盘，修复 §3 🔴 P0「默认未启用文件日志」）。旧配置若显式写 `dir=""` 则行为不变（仅终端）。

#### 4.1.2 flush（`src/lib.rs` `init_tracing`）

签名改为返回 guard，不再 `forget`：

```rust
/// 返回 `WorkerGuard`：调用方持有到 shutdown 末尾 drop，触发 non_blocking
/// worker drain 完剩余日志，保证故障末尾落盘。`None` = 仅终端（dir 空 或 建目录失败）。
pub fn init_tracing(cfg: Option<&config::LogConfig>) -> Option<tracing_appender::non_blocking::WorkerGuard>
```

- dir 非空且 `create_dir_all` 成功 → 建 daily appender + `non_blocking` → 返回 `Some(guard)`（**不 `forget`**）
- dir 空 或 建目录失败 → 文件层不启用，返回 `None`（建目录失败仍 `eprintln`，与现状一致）

**持有方**：`init_tracing` 的调用入口（`src/main.rs` Run 分支、`src/service/windows.rs` service_main）改为 `let _guard = init_tracing(...)` 绑局部变量，持有到该入口返回（此时 shutdown 流程已结束、`tracing::info!("rs-iot stopped")` 已入队）→ drop guard → worker drain 剩余日志 → 故障末尾落盘。install/uninstall 调 `init_tracing(None)` 返回 `None`，无需持有。**不改 `run_app` / `run_app_with_shutdown` 签名**（guard 不传入，由 main/service 入口持有；guard 在入口作用域存活覆盖整个运行期 + shutdown）。

#### 4.1.3 保留期清理（`src/lib.rs`）

抽**纯函数**（核心可测单元，无 async 无全局状态）：

```rust
/// 清理 `dir` 下早于 max_days 的 `rs-iot.log.YYYY-MM-DD` 文件；
/// max_size_mb>0 时再按总大小删最旧直到低于阈值。返回删除文件数。
/// 单文件 IO/解析失败仅 warn 跳过，不中断。
fn purge_old_logs(dir: &str, max_days: u64, max_size_mb: u64) -> TResult<usize>
```

步骤：
1. 读 dir，过滤文件名匹配 `rs-iot.log.YYYY-MM-DD` 的文件，解析日期（解析失败跳过）
2. 删日期早于 `今天 - max_days` 的（max_days=0 表示不按天删）
3. 若 `max_size_mb > 0`：统计剩余文件总大小，超 `max_size_mb * 1MB` 则按日期最旧优先删，直到低于阈值
4. 返回删除数；单文件删除失败 `warn!` 跳过继续

**cleanup task**（`shutdown.child_token()` 绑定退出）：
- 启动时立即跑一次 `purge_old_logs`（清上次遗留）
- 之后每 1h 跑一次（`tokio::select!` 监听 cancel / interval）
- 失败仅 `warn!`，不阻塞主流程

### 4.2 自愈子系统（`src/plugin/instance.rs`）

#### 4.2.1 supervisor 机制

`InstanceHandle` 改为持有 **supervisor task 的 handle**（supervisor 内部唯一 await source join，无竞态）：

```rust
struct InstanceHandle {
    token: CancellationToken,
    sup_join: JoinHandle<()>,   // 原 join（source task）→ 改为 supervisor task 的 handle
}
```

`start_source` / `start_sink` 在 spawn source task 后，**再 spawn 一个 supervisor task**：

```rust
// （原有）spawn source task，拿到 source_join
let source_join = tokio::spawn(async move { source.run(...).await; tracing::info!("[instance] source {id} exited"); });

// （新增）supervisor：唯一 await source_join，按 token 判定最终状态
let states_sup = self.states.clone();
let token_sup  = token.clone();
let id_sup     = id_key.clone();
let sup_join = tokio::spawn(async move {
    let _ = source_join.await;                 // 等 source 结束（panic 也以 JoinError 返回，被吞）
    if !token_sup.is_cancelled() {
        // 非主动 stop（token 未 cancel）= 异常退出 → 标 Failed
        states_sup.insert(id_sup.clone(), InstState::Failed);
        tracing::error!("[instance] {id_sup} task exited unexpectedly → Failed");
    }
    // token 已 cancel = 主动 stop，stop() 已标 Stopping→Stopped，不干预
});

self.states.insert(id_key.clone(), InstState::Running);
self.handles.insert(id_key, InstanceHandle { token, join: sup_join });
```

`stop()` 改为 await `sup_join`（替代原 await source join），3s 超时语义不变：

```rust
// stop(): cancel token + 标 Stopping → await sup_join（3s 超时）→ 标 Stopped
h.token.cancel();
match tokio::time::timeout(Duration::from_secs(3), h.sup_join).await {
    Ok(Ok(())) => {}
    Ok(Err(e)) => tracing::warn!("[instance] {inst_id} sup join error: {e}"),
    Err(_)     => tracing::warn!("[instance] {inst_id} stop timeout (3s)"),
}
self.states.insert(inst_id.to_string(), InstState::Stopped);
```

> supervisor 不需要独立 shutdown token —— 它依赖 `source_join` 完成而自然退出；source 因 instance token（shutdown 的 child）cancel 而退出 → supervisor 随之结束。关闭流程 `stop_all` await `sup_join` 等 supervisor 完成（含 source 退出）→ 再 `shutdown_lux`，不引入 lux 写竞争。

#### 4.2.2 Failed 判定矩阵

| source task 结束方式 | token 状态 | supervisor 判定 | 终态 |
|---|---|---|---|
| 正常运行中被 `stop()` / `stop_all` | cancelled（stop 已 cancel） | 不干预 | `Stopped`（stop 标） |
| `run()` 早退（配置错误 return） | 未 cancel | 标 Failed + `error!` | `Failed` |
| `run()` panic | 未 cancel | `JoinError` 被吞 → 标 Failed + `error!` | `Failed` |
| `run()` 自然完成（未 cancel） | 未 cancel | 标 Failed + `error!` | `Failed` |

核心：**「token 是否 cancelled」是「正常停 vs 异常死」的唯一判据**。主动 stop 路径必先 cancel token，故不会误判。

#### 4.2.3 Failed 后处置（不自重启边界）

- 标 `Failed`（内存 DashMap）+ `tracing::error!` 告警
- `/api/v1/inststatus` 天然可见（`status()` 读 DashMap，`Failed` 自动返回）
- 运维可调 `startinst`（REST）重新拉起 → `start_source` 覆盖状态 `Failed → Starting → Running`
- **不自重启**（§2「可选增强」，后置）

---

## 5. 错误处理

遵循项目「模块内错误 `warn!` 不 panic」约定（CLAUDE.md 编码约定）：

- **cleanup**（`purge_old_logs` / cleanup task）：IO 错误、文件名日期解析失败、单文件删除失败 —— 一律 `warn!` 跳过，不中断清理、不 panic、不阻塞主流程
- **init_tracing 建目录失败**：`eprintln` + 文件层不启用（guard=None，仅终端，与现状一致）
- **supervisor**：source panic 的 `JoinError` 被 `let _ = source_join.await` 吞（不传播），转标 Failed + `error!`；supervisor 自身不 panic
- **stop 超时**（`sup_join` 3s 未完成）：`warn!` + 标 `Stopped`（与现状语义一致）；token 已 cancel，task 后续自行退出

---

## 6. 测试策略

### 6.1 日志

- **`config.rs` 单测**：`LogConfig::default()` 断言 `dir=="./logs"` / `max_days==7` / `max_size_mb==0` —— 锁默认值防回归
- **`lib.rs` `purge_old_logs` 单测**：tempdir 造多个日期文件（含非日志文件、错误文件名），验证：
  - 早于 max_days 的被删、近期的保留
  - max_days=0 不按天删
  - max_size_mb>0 时按总大小删最旧
  - 非日志文件 / 解析失败文件名不被误删
  - 返回删除数正确
- **`init_tracing` 单测**：dir 非空 → 返回 `Some(guard)`；dir 空 → `None`（guard drop 行为由 `tracing-appender` 保证，不重复测）

### 6.2 自愈（`instance.rs` `#[cfg(test)]`）

用 **mock source + `#[cfg(test)] inventory::submit(SourceEntry { ... })`** 注册测试专用插件名（test build 独有，release 不存在；registry `find_source` 编译期静态注册，运行时无法注入，故走 inventory）。mock source：

- **早退 source**（`run()` 立即 return）→ `start_source` 后短暂 await → 断言 `status()["id"]==Failed`
- **panic source**（`run()` 里 panic）→ 断言 `Failed`（JoinError 路径）
- **正常 start → stop**：`start` → `stop` → 断言 `Stopped`（不误判 Failed）
- **token cancel 退出**（`run()` 内 `select!` 监听 token，cancel 后 return）→ 断言 `Stopped`

### 6.3 集成测试（可选 `tests/instance_failed_e2e.rs`）

真实 lux + `InstanceManager` + mock source 早退 → `GET /api/v1/inststatus` 返回 `Failed`。验证整链（supervisor → DashMap → REST DTO）。

### 6.4 回归
- 现有 `tests/dataflow.rs`（datasim→hub→pipeline→rtdata）保持绿 —— supervisor 不改数据流
- 现有 `tests/api_flow.rs` / `sidecar_e2e.rs` 保持绿 —— stop 语义不变

---

## 7. 改动清单

| 文件 | 改动 |
|---|---|
| `src/config.rs` | `LogConfig` 加 `max_days` / `max_size_mb` + `Default` 更新 |
| `src/lib.rs` | `init_tracing` 返回 `Option<WorkerGuard>`（不 forget）；新增 `purge_old_logs` 纯函数 + cleanup task（`shutdown.child_token()` 绑定） |
| `src/main.rs` | Run 分支 `let _guard = init_tracing(Some(&cfg.log))` 持有（install/uninstall 的 `init_tracing(None)` 返回 None，无需持有） |
| `src/service/windows.rs` | service_main `let _guard = init_tracing(Some(&cfg.log))` 持有 |
| `src/plugin/instance.rs` | `InstanceHandle.join` → `sup_join`；`start_source`/`start_sink` 增 supervisor spawn；`stop` 改 await `sup_join`；Failed 标记逻辑；`#[cfg(test)]` mock source + 单测 |
| `config.toml` | 可选加注释版 `[log]` 示例（说明默认已落盘 `./logs` + 保留期） |
| `CLAUDE.md` | 更新「日志」段（dir 默认 `./logs` / max_days / cleanup）+ 「实例状态」段（Failed 现真实写入 + watchdog） |
| 测试 | config 默认值单测 / `purge_old_logs` 纯函数单测 / supervisor mock 单测（+ 可选集成测） |
| 文档 | 本 spec + 后续 writing-plans 产物 |

---

## 8. 简化与风险

- **日志保留期仅按文件名日期 + 总大小**：不做按行数/压缩/归档外移。`max_size_mb` 默认 0（仅按天），现场按需开。
- **cleanup 周期固定 1h**：不暴露为配置（YAGNI）；启动立即跑一次兜底上次遗留。
- **Failed 仅内存**：进程重启状态重置（不持久化到 lux）—— 符合 M3 边界，持久化属后续告警/审计。
- **supervisor 每实例一个 task**：N 实例 N supervisor，开销可忽略（await join，无轮询）。
- **stop 超时后 supervisor 泄漏**：若 source 因 bug 不退，stop 3s 超时放弃等 `sup_join`，supervisor 挂在 await 直到 source 最终退出 —— 与现状 stop 超时后 join handle 泄漏同等，可接受。
- **guard 持有依赖调用方**：所有入口（run / service）都必须接 guard，漏接则该入口不 flush —— 以单测/代码审查守住。
- **tracing-appender 无原生保留期**：自建 `purge_old_logs`（约 50 行）+ cleanup task，已评估优于引外部 rolling crate（多不成熟/不维护，违项目少依赖倾向）。

---

## 9. 配置示例（`config.toml`，可选）

```toml
# 日志（默认即可：落盘 ./logs，保留 7 天，daily 轮转，退出自动 flush）
[log]
dir = "./logs"        # 默认 "./logs"；置 "" 关闭文件层（仅终端）
level = "info"        # RUST_LOG 优先
max_days = 7          # 按文件名日期保留最近 N 天
max_size_mb = 0       # 0=不限；>0 时按目录总大小删最旧
```

---

## 10. 验收标准

1. `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets --all-features` 三项全绿（CI 门槛）
2. 无 `[log]` 的 config.toml 启动后 `./logs/rs-iot.log.YYYY-MM-DD` 存在且有日志写入
3. ctrl_c / service stop 后，最后几条日志（含 "rs-iot stopped"）已落盘（不丢末尾）
4. `./logs` 下早于 max_days 的日志文件被清理
5. 构造一个 run() 早退的实例 → `/api/v1/inststatus` 返回该实例 `Failed`（而非 `Running`）
6. 正常 `stopinst` → `/api/v1/inststatus` 返回 `Stopped`（不误判 Failed）
7. 现有集成测（dataflow / api_flow / sidecar_e2e）保持绿
