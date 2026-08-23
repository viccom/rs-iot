# startinst 唯一启动守卫（修双启动幽灵任务）实施计划

> 日期：2026-08-23
> **状态**：implemented（2026-08-23 执行完毕，TDD 红→绿，真机验证 409/速率单份/重启放行）
> **实施**：T1 重现测试（manager 级 Err + 幽灵速率锁 + REST 409，两测先红）→ T2 修复
> （错误变体 + `claim_starting` 原子认领 + 409 映射）→ clippy 2 处顺手修
> （`is_none_or`/`is_ok()`）。
> **代码**：`src/error.rs`（`InstanceAlreadyRunning` 变体）+ `src/plugin/instance.rs`
> （`claim_starting` free fn——`DashMap::entry` 检查-置 Starting 原子完成，锁不跨
> await；start_source/start_sink 入口共用）+ `src/api/routes_sys.rs`（守卫错误 →
> 409 CONFLICT，其余错误仍 500）+ `tests/startinst_guard_e2e.rs`（2 用例）。
> **测试**：`cargo test -j 2` **572/0**（新增 2：二次启动 Err+4.5s 采样 2..=6 幽灵
> 回归锁 + stop 后重启放行；REST tower oneshot 200→409）；clippy `--all-targets` 0
> warning；真机端到端（release + 真实 HTTP）：首启 200、二次 409（报文
> `instance sim-1 already active`）、拒后 12s 采样增量 6（修复前翻倍 ~10+）、
> stopinst→startinst 200/200。
> **遗留**：stop 3s 超时路径旧任务短暂双跑（见下"边界与残留风险"，既有边界留
> 告警人工介入）；前端可在 409 时提示"实例已在运行"（UI 措辞优化，另行小改）。
> **背景**：CPU 优化验证期间发现（现场 exe 与 master 均存在）：对**已运行**实例再次
> `POST /api/v1/startinst` 返回 ok 并静默创建第二个同 ID 任务——旧任务无人 cancel、
> 注册表条目被新任务顶替，旧任务变"幽灵"：双份采样/写库/CPU，且 `/metrics`
> `instance_info` 只显示一行，监控不可见。实测 12s 样本产出 7→10 翻倍。
> 同源的 MQTT client-id 现象（同机双实例互踢）**不是 bug**：hwid 同机相同是部署期
> 设计，多实例测试应走 hwid 启动参数覆盖（`hwid` 配置键），不在本计划范围。

**Goal:** `start_source`/`start_sink` 增加原子唯一启动守卫：同 inst_id 处于
Starting/Running/Stopping → 拒绝（新错误变体 + REST 409）；Stopped/Failed 放行
（保留 ed76dc2「stop 后可重启」语义）。编排层 R5 单通道锁（含 Stopped 不复活）
不受影响，形成双保险。

**Architecture:** 守卫放 `InstanceManager::start_source/start_sink` 入口（保护
REST/编排/未来北向全部调用方），用 `DashMap::entry` 原子检查-置 Starting（锁
作用域不跨 await，防并发 startinst 竞态）；`routes_sys::startinst` 把
`RsIotError::InstanceAlreadyRunning` 映射 409 CONFLICT（其余错误仍 500）。

**Tech Stack:** Rust（dashmap entry API / thiserror 新变体——`RsIotError` 已
`#[non_exhaustive]`，加变体不破坏下游）。

---

## T1 重现测试（先红后绿）

`tests/startinst_guard_e2e.rs`（照 `lua_stat_e2e`/`api_flow` harness）：

- **manager 级**：start_source(dataSim period=1) → Ok；同 id 二次 start_source →
  必须 `Err(InstanceAlreadyRunning)`；hub 采样速率 4.5s 内 2..=6 条（幽灵任务会
  翻倍到 ~7-10，速率断言锁死回归）；stop → start → Ok（重启语义保留）。
- **REST 级**（tower oneshot）：inst/set 持久化 + startinst → 200；再次
  startinst → 409（现行为 200，先红）。

## T2 修复实现

1. `src/error.rs`：`RsIotError::InstanceAlreadyRunning(String)`
   （`#[error("instance {0} already active (starting/running/stopping)")]`）。
2. `src/plugin/instance.rs`：start_source/start_sink 的裸
   `states.insert(Starting)` 改 entry 原子认领——Occupied 且活跃态（Starting/
   Running/Stopping）→ Err；否则置 Starting 继续（Stopped/Failed/无条目放行）。
3. `src/api/routes_sys.rs::startinst`：`InstanceAlreadyRunning` → 409。

## 边界与残留风险

- stop 3s 超时后旧任务可能仍在跑（stop 仍置 Stopped → restart 放行 → 理论上仍可
  短暂双跑）——既有边界，守卫已消除常见路径，超时路径留告警日志人工介入。
- supervisor 标 Failed 后 restart 放行（自愈预期语义，不变）。
- `start_default_north` R5 预检含 Stopped（编排不复活运维停掉的北向），与守卫
  语义不同属有意差异（R5 更严），互不冲突。
