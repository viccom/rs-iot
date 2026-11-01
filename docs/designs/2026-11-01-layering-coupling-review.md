# rs-iot 分层耦合专项审查与修复（2026-11-01）

> **状态**：implemented（三条类型回边已切 + 分层门禁上线 + 两处越层收口）
> **日期**：2026-11-01
> **实施**：2026-11-01（本次改动；提交后回写 commit）
> **代码**：`src/types.rs`（InstState/LogEntry）、`src/error.rs`（ModelError）、`src/host.rs`（ArchiveCounters）、`src/plugin/lua_core/manage.rs`（Lua 管理面）、`scripts/check_layers.py`、`justfile`（`just layers`）
> **测试**：`scripts/check_layers.py`（无环+越层门禁）、`cargo test --lib`、`cargo test --tests --no-default-features --features lua`
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
