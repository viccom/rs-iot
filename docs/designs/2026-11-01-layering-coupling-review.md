# rs-iot 分层耦合专项审查与修复（2026-11-01）

> **状态**：implemented（三条类型回边已切 + 分层门禁上线 + 两处越层收口 + 上线 e2e 揪出装配顺序 bug 并修复）
> **日期**：2026-11-01
> **实施**：2026-11-01（本次改动 + 上线复测修复）
> **代码**：`src/types.rs`（InstState/LogEntry）、`src/error.rs`（ModelError）、`src/host.rs`（ArchiveCounters）、`src/plugin/lua_core/manage.rs`（Lua 管理面）、`src/lib.rs`（装配顺序修复）、`scripts/check_layers.py`、`justfile`（`just layers`）
> **测试**：`scripts/check_layers.py`（无环+越层门禁）、`cargo test --lib`（679）、生产 e2e 11 项（见 §7）
> **遗留**：`HostContext`/`AppState` 收窄（受 73 处测试构造点约束）、`db/` 目录拆分（100+ 调用点）、`api` 模块 pub 面收敛

## 1. 一句话结论

数据流的单向性做得好，模块级的**类型归属**做得不好：`architecture.md` 声称的"无环"
不成立——生产段是一个 10 模块强连通分量，全部由 3 条"共享载荷类型定义在消费方
模块"的类型级回边造成。切断后整图变 DAG，并已固化为机械门禁。

## 2. 方法

- 建图：`src/` 全部 `.rs`，按一级模块归并；统计 `crate::X` 引用；排除注释行与
  `#[cfg(test)] mod` 段及纯测试文件（`tests.rs`）。
- 判环：Tarjan 强连通分量（SCC > 1 即有环）。
- 复现：`python3 scripts/check_layers.py`（修复前 exit 1 / 修复后 exit 0）。

## 3. 实测结果（修复前）

- **SCC**：`config, db, error, event_bus, history, host, io_trace, log_ringbuffer, model, plugin`（10 模块一个环）。
- **三条互相引用**（全部是类型级）：

| 回边 | 证据 | 说明 |
|---|---|---|
| `error <-> model` | `error.rs:59` `Model(#[from] model::validate::ModelError)` | 底层错误枚举嵌域错误 |
| `event_bus <-> plugin` | `event_bus.rs:17` `use plugin::instance::InstState` | 总线载荷嵌插件状态 |
| `event_bus <-> log_ringbuffer` | `event_bus.rs:16` `use log_ringbuffer::LogEntry` | 总线载荷嵌日志记录 |

- **切断这三条后 SCC 为空**（整图 DAG），实测分层：
  L0 `error/hwid/process_metrics/run/service/ticker/types`；L1 `config/event_bus/log_cleanup`；
  L2 `db/io_trace/log_ringbuffer`；L3 `history`；L4 `host/model`；L5 `plugin`；
  L6 `api/lib/pipeline/transform`。
- 与旧文档的偏差：`host` 在 L4（持有 `HistoryHandle`，不是底层）；`pipeline` 在 L6（编排者）。

## 4. 修复内容

1. **三条回边**（纯类型搬移 + `pub use` 兼容，零行为变化）：
   - `InstState` → `types.rs`（`plugin::instance` 保留 re-export）
   - `LogEntry` → `types.rs`（`log_ringbuffer` 保留 re-export）
   - `ModelError` → `error.rs`（`model::validate` 保留 re-export）
2. **门禁**：`scripts/check_layers.py` + `just layers`（并入 `just check`）——
   SCC > 1 或命中 FORBIDDEN 越层规则即失败，打印环上边与 file:line。
3. **`plugin -> history` 收口**：`HostContext::archive_counters()` 返回三元快照，
   selfMonitor 不再引用 `history::rows::HistoryStats`。
4. **`api -> plugin::lua_core::*` 收口**：新增 `lua_core::manage` 管理面
   （保留段判定/清单/已装版本/同步/重载），api 只经此入口。

## 5. 未做与理由

| 项 | 理由 |
|---|---|
| `HostContext`(9) / `AppState`(15) 收窄、去 4 个重复字段 | 73 处 `HostContext { … }` 构造点（多数在 e2e），需先抽 `HostContext::for_test`，改动面远大于收益 |
| `db/` 拆分为 store/domain | 100+ 调用点；当前以头注标注三层边界（`src/db/mod.rs`），触碰时再拆 |
| `api` 模块 214 个 `pub` → `pub(crate)` | 对外无消费者、对内无收益，纯噪声改动 |
| `model -> history`（reload 联动） | 已登记于 2026-10-27 优化评估 §3.4；正确修法是订阅者注册，随 effective 显式注入一起做 |

## 6. 验证

- `python3 scripts/check_layers.py`：修复前 `exit 1`（打印 10 模块环 + 6 处越层），
  修复后 `exit 0`（22 模块 / 62 条生产依赖边，无环、无越层）。
- `cargo fmt --all --check`：通过。
- `cargo clippy --all-targets -- -D warnings`（默认档）、
  `--no-default-features --features lua`（最小档）、`--workspace`：均通过。
- `cargo test --lib`：679 passed / 0 failed。
- `cargo test --tests --no-default-features --features lua -- --test-threads=4 --no-fail-fast`：
  全部测试二进制通过（611 lib-under-minimal + 各 e2e 目标，0 failed）。
- **顺手修掉 `events_e2e` 的 lux 临时目录撞名 flaky**：`tests/common::test_data_dir`
  原用 pid+纳秒命名，时钟分辨率不足时同 binary 内并发测试会撞同一目录
  （`Resource temporarily unavailable`）。加进程内原子序号兜底后，`--test-threads=10`
  连跑 3 次 10/10 通过。
- `just check`（fmt + docs + layers + clippy 三档 + 单测 + 全量 e2e + s7/eip 档）：**全过**。
  期间 `instance_failed_e2e` 出现过一次并发时序失败（该套件依赖 supervisor 判定窗口，
  重跑与后续全量均 5/5 通过），未复现，属既有时间敏感用例，未在本次范围内处理。
- `cargo test --features "s7,eip" --test s7_e2e --test eip_client_e2e --test feature_gate_e2e`：通过。
- `python3 scripts/check_docs.py`：OK（72 个 md）。

## 7. 上线 e2e 与装配顺序 bug（2026-11-01 追加）

分层重构上线（重编 release + warden 重启 dev-rs-iot）后的生产 e2e 对照**揪出一个
既有 bug**——正是本审查登记的「装配顺序契约只存在于注释」的直接后果：

**现象**：`/sysstatus` 直读 `HistoryStats` 的 `written` 正常增长（855→2197→2854），
而 selfMonitor 同源三计数 `rsiot.history_written_total` 恒 0（采集时间戳新鲜）。

**根因**（`src/lib.rs` 装配顺序）：`InstanceManager::new(ctx.clone(), hub_tx)` 在
`ctx.history = parts.handle` **之前**构造——mgr 把这份 `history=None` 的副本 clone
给每个插件 task，所以**所有插件**的 history 槽从未接通（T16 上线以来即如此；旧代码
`ctx.history.as_ref().map(...)` 同样得 None→0，非本次重构引入）。pipeline 拿到的
是注入后的 ctx（sysstatus/metrics 正常），两条读径分叉即本次对照的来源。

**修复**：把「ensure_sys_model_registered → cache::reload → discovery::load →
history init → ctx.history 注入」整段挪到 `InstanceManager::new` 之前（mgr 构造
点即「插件面 ctx 定格点」，注释已写明后续加能力必须在此之前注入）。该段只依赖
lux client 与 effective，前移无依赖冲突；默认北向/自动启动实例本就在 mgr 之后。

**复测**（重启后生产 e2e 11 项全过）：`sysstatus written=381` ↔
`selfMonitor written_total=370`（采集时刻差几秒，同源同步），dropped/errors 双 0；
11 项 = inststatus/plugins/实时数据新鲜度/三计数/history 读/KPI/engine 状态/SSE/
lua stdlib 门面/lua scripts。ModelError 文案（`model error: …`，HTTP 400）重启后
复测不变。启动日志：优雅停机（writer 排空→close→锁释放）→ 重启 → 北向/管线/REST
就绪，顺序调整无副作用。

**教训**：单测里 observe_light 用注入了假 HistoryHandle 的 HostContext 验证过
wiring（所以单测全绿），但**生产装配点给的是另一份 ctx**——模块图看不见这类
「同一抽象、两处装配」的分叉。收敛方向即技术债 4（HostContext/AppState 单一
装配源）；在机械化之前，跨装配点的共享可变注入必须有对照断言。
