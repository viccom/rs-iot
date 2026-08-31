# 历史存储 10 万点容量与可靠性改进实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 消除 2026-08-29 历史模块 10 万点压测暴露的六项根因，使历史链达到 Phase 4 门禁（10 万点/秒 ≥100K pts/s 全量归档、无静默事故路径）。

**Architecture:** 双仓库联动——rs-iot（`E:\Rs_Codes\rs-iot`，编排/观测/告警/lifecycle）与 tsink fork（`E:\Rs_Codes\tsink_fork`，引擎准入/记账/写入路径）。按四个共性缺口组织：①准入预估与记账口径失真（停摆根因）；②高基数 per-row 写入开销（吞吐根因）；③rs-iot fail-soft 与 tsink fail-fast 无协调层（可靠性根因）；④有计数无告警（感知根因）。

**Tech Stack:** Rust（tokio/axum/parking_lot）、嵌入式时序库 tsink（fork 管理）、现有 Q12 告警闭环（`db::alarms` + SSE）、TDD（cargo test，本机务必 `-j 2`）。

---

## 〇、压测背景（结论摘要）

2026-08-29 压测（工件 `bench/history-100k/`，拓扑：1 模型×100 tag×1000 设备 ÷ 100 个 dataSim 实例 period=1s，白名单 100,001 series）：

- 采集/pipeline **满速达标**：1,032 Samples/s ≈ 103K pts/s，hub 无积压；
- 历史链**不达标**：稳态 written ≈7.5K pts/s，**92% 数据 channel 满丢**（dropped 累计 6,500 万+）；
- 功能面全过（series===白名单精确相等、查询/聚合/白名单/死区/limit/优雅关停/WAL kill -9 恢复）；
- 事故路径实锤：admission 停摆重启不恢复、tsink fail_fast 一次 IO 错误引擎自杀、/metrics 高基数 46s 阻塞并拖垮写入。

## 一、根因确认总表（四路代码级审查交叉确认）

| # | 问题 | 根因（置信度） | 关键锚点 |
|---|---|---|---|
| P1 | admission 拒批且重启不恢复 | 预估按 per-head **满 chunk cap** 计费：`sizeof(ActivePartitionHead)+chunk_point_cap×sizeof(ChunkPoint)`≈80.2KiB/head，16384 行批≈**1.34GB 预估**（实际首块 ~2.7KB，虚高 30 倍）；叠加 250ms flush 周期 seal 当前 head → 每批全额计费。拒批阈值 2048MB−1.31GB=**735MB**，而 used 地板（含 mmap/registry，文档说排除、代码计入）846-889MB **随数据单调涨** → 数学上重启必拒、永不恢复（日志数字与公式吻合 <1%，置信度高） | tsink `src/engine/ingest_pipeline/prepare.rs:541-597`（预估公式 :554-560）、`write_buffer.rs:93-132`（250ms seal）、`memory_accounting/context.rs:174-202` 与 `lifecycle/persisted_state/install.rs:36-64`（mmap 入账）；rs-iot `writer.rs:304-323`（整批丢） |
| P2 | writer 高基数排空 7.5K pts/s | 双成分：**(a) admission 停滞**——拒批时 10ms 轮询阻塞满 `write_timeout=30s`（`prepare.rs:318-380`），压测窗口 ~65% 时间在停（P1 的症状）；**(b) per-row 常数开销**——tsink insert_rows 每行 ~25-40 次堆分配+8-10 次锁（identity hash 预扫/resolve 双锁双克隆/lane 三重校验 per-point/分区头规划×2/WAL 按系列成帧）+ 可见性记账 O(series)/分片/批 ≈390 次条目访问/行；rs-iot 侧每行另加 ~11-13 次分配（dedup 键/series entry 键/TsinkRow labels）。25K vs 100K series 吞吐持平佐证常数主导（置信度高） | tsink `shard_routing.rs:4-28`、`series/identity.rs:315-332`、`prepare.rs:450-458`、`apply.rs:269-289`、`write_visibility.rs:117-161`；rs-iot `writer.rs:188,245-248,284-294` |
| P3 | /metrics 46s 阻塞并拖垮写入 | `series_count()` 5s TTL 过期即全量 `list_metrics()`：为一个计数物化 10 万 MetricSeries（30-60 万次锁、50-100 万分配），与 insert_rows 在 3 个**全局单 RwLock 字典**+materialized BTreeSet+summaries 上互卡；Mutex 临界区内执行 → 并发 metrics 串行。**registry 内已有 O(1) `series_count()`（AtomicUsize）未暴露**（置信度高） | tsink `query_exec/metadata_api.rs:12-39`、`core_impl.rs:55-75`（持全局 registry.read() 逐 series decode）、`series/identity.rs:527`（现成 O(1) 计数）；rs-iot `history/engine.rs:213-232`（TTL 缓存临界区内全量 list） |
| P4 | 一次 IO 错误 → 引擎永久僵尸 | `background_fail_fast` 默认 true 且触发后**无复位路径**，所有 worker 退出、读写全灭，rs-iot 侧 engine 三态启动后永不更新（sysstatus 永报 ok）。os error 3（路径找不到）：删除类路径经本机实证对 error 3 免疫（NotFound 容错），候选收敛到 4 处**裸 `?` 无路径上下文**的 IO（首选 `segment/writer.rs:136` 裸 rename），唯一定位失败——需 instrumentation 先行（error 3 部分置信度中） | tsink `engine/config.rs:82`、`storage.rs:1254`（**`with_background_fail_fast` builder 已存在未用**）、`runtime.rs:90-108`；rs-iot `history/engine.rs:94-105`（未调）、`history/mod.rs:160-166`（三态静态） |
| P5 | cardinality 仅启动校准（R4） | limit 是不可变运行时配置、无 set API；但 enforcement **每次写路径把 limit 读进 context** → 改 `AtomicUsize` + 暴露 setter 仅 4 处小改。运维止血：显式配 `cardinality_limit` 大值（置信度高） | tsink `engine/engine.rs:192-206`（RuntimeConfigState）、`core_impl/contexts.rs:276`、`series/identity.rs:350-375`；rs-iot `history/engine.rs:64-73`（启动 auto 校准）、`model/cache.rs:43-47`（refresh_after_write 钩子已存在） |
| P6 | 静默丢弃无告警 | 丢弃是设计（fail-soft 不背压采集，正确），但 11 处丢弃点仅有 metrics 计数，无告警/无 sysstatus 健康态；selfMonitor 17 标签不含 dropped/errors；Q12 告警闭环存在但判定全靠脚本且无内置脚本（置信度高） | rs-iot `self_monitor/mod.rs:135-172`（采集清单）、`api/routes_sys.rs:16-129`（已有 stats 无 health）、`plugin/lua_core/api/alarm.rs:156-210`（fire_event）、`db/alarms` |
| P7 | rtdataWatch ksub 滞后重同步风暴 | lux broadcast 容量仅 1024；Lagged 处理"原地 await 全量 KEYS 重铺"**无退避**，期间不消费通道 → 自放大；epoch+1 使每 SSE 连接走 Full 重读（10 万设备逐个 HGETALL）（置信度高） | rs-iot `api/rtdata_watch.rs:229-295`（Lagged 分支 :262-268 无退避）、`routes_rtdata_subscribe.rs:130-157`；lux `src/pubsub.rs:13`（CHANNEL_CAPACITY=1024，独立上游仓库） |

## 二、全局架构诊断（共性缺口）

1. **容量链路断裂**：pipeline 2.6M → lux 1.65M → **writer 7.5K**（目标 ≥100K）。最薄环节的双重病灶 = 停（P1 准入）+ 慢（P2 开销）。**先解停再提速**——P1 修复后吞吐能恢复多少未知（预估 30-60K），P2 是达标的必经之路。
2. **哲学边界无协调层**：rs-iot fail-soft（旁路不拖垮采集）与 tsink fail-fast（完整性优先自杀）之间缺三样东西：引擎状态上行（sysstatus）、失败语义选择（fail_fast 开关）、丢弃水位告警。P3/P4/P6 同源。
3. **启动期快照 vs 运行期变更**：cardinality（P5）与 engine 三态（P4）都是"启动定格、运行期失联"。`cache::reload` 的 `refresh_after_write` 钩子是现成的联动挂点。
4. **观测本身是负载**：/metrics 高基数 O(series) 计算（P3）与 watcher 全量重铺（P7）在高压下与数据路径争资源——观测面必须 O(1) 化或与数据面隔离。

## 三、里程碑总览与依赖

```
M0 止血包（rs-iot 为主 + tsink 两处小改，解现场事故）
  T1 预估按真实首块计费 ──→ 解除 admission 停摆（P1 主刀）
  T2 关 fail_fast + T3 引擎状态上行/告警 ──→ 消灭僵尸静默（P4）
  T4 series_count O(1) 化 ──→ 消灭观测拖垮写入（P3 主刀）
  T5 flush 管线 IoWithPath instrumentation ──→ P4 error3 取证前置
  T6 内存分量观测导出 ──→ 准入问题可定位
M1 准入与内存治理（tsink）：T7 预算口径对齐文档、T8 admission 减压改 bounded flush、T9 rs-iot 暴露 chunk_points/partition_duration
M2 吞吐主攻：T10 rs-iot 分配削减 → T11 tsink resolve 缓存 → T12 lane 校验批内去重 → T13 可见性记账增量维护 → [决策点 D1] 复测缺口评估 → (可选) T14 多写线程分片
M3 观测/告警/生命周期：T15 watcher 去抖退避、T16 selfMonitor 丢弃标签、T17 sysstatus health 状态化、T18 内建水位告警、T19 cardinality 热校准
M4 复验：T20 bench 全档复测 + 门禁对照 → [决策点 D2] 24h soak
依赖：T1→(M2 全部)、T4→T20（复测前提：观测不再干扰写入）；T2/T3 独立；T7→T9；T19 独立
```

执行规约：**跨 ≥3 commit/≥5 文件，必须 git worktree 隔离**（`git worktree add ../rs-iot-hist100k`）；两仓库改动各自独立 commit、各自跑门禁；每任务 TDD 先红后绿；cargo 命令一律 `-j 2`。

---

## 四、任务清单

### Task 1（M0｜P1 主刀）：admission 预估按真实首块计费 [tsink]

**Files:**
- Modify: `E:\Rs_Codes\tsink_fork\src\engine\ingest_pipeline\prepare.rs:541-597`（预估公式 :554-560）
- Test: `E:\Rs_Codes\tsink_fork\src\engine\ingest_pipeline\prepare.rs`（模块内 #[cfg(test)]）或邻近测试文件

**依据**：`ChunkBuilder` 以 64 点块增长（`engine/chunk.rs:55` `ACTIVE_POINT_SNAPSHOT_BLOCK_POINTS=64`，`chunk.rs:323-331` tail `Vec::with_capacity(64)`），实际首块分配 ~2.7KB/head；现公式按 `chunk_point_cap`(2048)×40B 预留 ≈80.2KiB/head，虚高 30 倍。16384 行批预估从 1.34GB → ~45MB，拒批阈值从 735MB → ~2017MB，高于 846-889MB 地板 → **重启即恢复**。防膨胀兜底已由提交后增量记账 + `ingest_pipeline.rs:95-106` 后置强制执行覆盖。

**Step 1: 写红测**（预算受限下大批新 series 应放行）
```rust
// 语义：memory_budget 限制下，单批大量新 series 的预估应为
// batch_rows × (head + 64 × sizeof(ChunkPoint)) 而非满 cap 计费。
// 构造：小预算（如 64MB）+ 单批 16384 行全新 series（每行独立 series）
// 断言：estimate（或 admission 判定）不放行失败/预估值 < 100MB。
// 参考现有 prepare 模块测试的构造方式（读 prepare.rs 现有 #[cfg(test)] 惯例后对齐）。
```
**Step 2: 跑测确认红**（现状按 cap 计费 → 预估 ~1.34GB → 断言失败）
Run: `cargo test -j 2 -p tsink prepare:: -- --nocapture`（在 tsink_fork 仓库根）
**Step 3: 最小实现**：`prepare.rs:554-560` 的 `per_new_partition_head` 常数项由 `chunk_point_cap` 改为 `ACTIVE_POINT_SNAPSHOT_BLOCK_POINTS`（从 `engine/chunk.rs` 引入），公式注释说明"按真实首块计费，膨胀由增量记账后置强制兜底"。
**Step 4: 跑测确认绿** + 全量回归 `cargo test -j 2`（616+ 面必须全绿）
**Step 5: Commit**（tsink 仓库）：`fix(admission): per-head 预估按 64 点首块计费，解除高基数满批虚高 30 倍拒批`

### Task 2（M0｜P4）：关闭后台 fail_fast [rs-iot]

**Files:**
- Modify: `E:\Rs_Codes\rs-iot\src\history\engine.rs:94-105`（build_storage builder 链）
- Test: `E:\Rs_Codes\rs-iot\tests\`（新增或扩展现有 history e2e）

**Step 1: 红测**：e2e 断言 builder 配置面——新增单测（engine.rs 模块测试）检查 `build_storage` 产物的 runtime 配置含 `background_fail_fast=false`（若 builder 不便内省，改为测试语义：注入一个必失败的 worker 场景过于重——则退为配置断言测试，或以 tsink `with_background_fail_fast` 文档测试为据做集成冒烟）。
**Step 2: 确认红** → **Step 3**: builder 链加一行 `.with_background_fail_fast(false)`，注释引 `runtime.rs:100-104`（Continue 降级语义）与 `tests/reliability.rs:687`（fail-fast-off 重试语义已有背书）。
**Step 4**: `cargo test -j 2 --features history` 全绿 → **Step 5**: Commit：`feat(history): 后台 worker 失败降级续跑（关 fail_fast，单次 IO 错误不再引擎自杀）`

### Task 3（M0｜P4/P6）：引擎运行态上行 + 僵尸告警 [rs-iot]

**Files:**
- Modify: `src/history/mod.rs:160-166`（三态静态 → 可变运行态，加 `shutting_down`/`degraded`）
- Modify: `src/history/writer.rs:304-349`（`Closed` 错误桶**首增**时 `error!` 一次性日志 + `db::alarms::fire` 系统告警事件；节流复用 writer.rs:36-38 模式）
- Modify: `src/api/routes_sys.rs:29-93`（sysstatus history 块输出运行态）
- Test: writer 单测（错误桶→告警触发）+ sysstatus handler 测试

**Step 1-5**: 红测（Closed 桶从 0→1 触发告警、sysstatus 反映 shutting_down）→ 实现 → 绿 → commit `feat(history): 引擎僵尸态上行 sysstatus + Closed 桶首增告警`。注意 `AppState.history_engine_state` 消费方（前端）同步。

### Task 4（M0｜P3 主刀）：series 指标 O(1) 化 [tsink+rs-iot]

**Files:**
- Modify: `E:\Rs_Codes\tsink_fork\src\storage.rs`（Storage trait 加 `fn series_count(&self) -> u64`，默认实现走现有 `series/identity.rs:527` 的 AtomicUsize 计数；engine 后端 override）
- Modify: `E:\Rs_Codes\rs-iot\src\history\engine.rs:213-232`（`series_count()` 直调新 trait 方法，删除 list_metrics 物化与 TTL 缓存——O(1) 无需缓存；HELP 文本注明"registry 口径（近似）"）
- Test: tsink trait 测试（计数与建 series 数一致）；rs-iot 现有 telemetry 测试改造

**红测**：tsink 侧 `series_count()` 在 N 个 series 建立后 == N；rs-iot 侧 metrics 渲染不再调用 list_metrics（可用计数断言或耗时断言）。
**Commit**（两仓库各一）：tsink `feat(storage): 暴露 O(1) series_count`；rs-iot `perf(history): series 指标 O(1) 化，消灭高基数 46s metrics 阻塞`。

### Task 5（M0｜P4 取证）：flush 管线 IO 带路径上下文 [tsink]

**Files:**
- Modify: `src/engine/segment/writer.rs:136`（裸 `fs::rename` → 包装 `IoWithPath`，或换已有 `rename_path_with_retry`/`rename_and_sync_parents`）
- Modify: `src/engine/fs_utils.rs:318-349`（write_tmp_and_sync 各 IO 包路径）、`src/engine/wal/segments.rs:216-243,553`（read_dir 不容错处）、`segment/writer.rs:158-168`
- Modify: `src/engine/runtime.rs:448-461`（错误日志 `%error` → `{:?}` 带 chain）
- Test: 现有 reliability 测试面回归

**验收**：人为注入路径错误（临时目录删除）的测试可见含路径的错误串。Commit：`fix(engine): flush/WAL 管线 IO 错误携带路径与链路（os error 3 取证前置）`

### Task 6（M0｜P1 观测）：内存分量与准入缺口导出 [tsink+rs-iot]

**Files:**
- Modify: tsink `src/engine/maintenance/memory_accounting.rs:195-224`（`MemoryObservabilitySnapshot` 已有分量，确保可从 StorageHandle 取）
- Modify: rs-iot `src/history/engine.rs`（telemetry 快照加分量）、`src/api/routes_metrics/render.rs:439-446`（新增 `rs_iot_history_memory_{registry,index,mmap,active}_bytes` family + admission `last_estimate_bytes`/`shortfall_bytes`）
- Test: render.rs 现有渲染测试扩面

Commit：`feat(metrics): 历史内存分量与 admission 预估导出（下次事故一眼定位虚高 vs 真满）`

### Task 7（M1｜P1 治本）：预算口径对齐文档 [tsink]

**Files:**
- Modify: `src/engine/maintenance/memory_accounting/context.rs:174-202`（registry/metadata/persisted index 出 used_bytes，移入 observability 分量）
- Modify: `src/engine/lifecycle/persisted_state/install.rs:36-64,316`（mmap 同上）
- Modify: `docs/performance-tuning.md`（预算表同步：口径="active+sealed"，registry/mmap 走 RSS）
- Test: 记账单测（建 series/装段后 used 只含 active+sealed；分量字段非零）

**效果**：地板 846-889MB → ~100MB，预算语义回归，10 万 series 内存数学有解（active 稳态 ≈4.1GB@2048cap 或 1GB@512cap）。
Commit：`fix(accounting): used 口径对齐文档——registry/index/mmap 出预算入观测`

### Task 8（M1）：admission 减压触发 bounded flush [tsink]

**Files:**
- Modify: `src/engine/runtime.rs:130-139`（admission 唤醒的 `BackgroundEligibleOnly` → `BackgroundBounded`，使 30s 等待可能等来真实释放——现结构对"head 重开"预估项无效）
- Test: reliability/准入测试（预算压力下减压路径真的 seal+persist）

Commit：`fix(admission): 减压唤醒 bounded flush，等待不再注定徒劳`

### Task 9（M1）：rs-iot 暴露 chunk_points / partition_duration [rs-iot]

**Files:**
- Modify: `src/config.rs` HistoryConfig（加 `chunk_points: usize`（0=默认 2048）、`partition_secs: u64`（0=默认 1h），`validate()` 边界）
- Modify: `src/history/engine.rs:94-105`（builder 条件调用 `with_chunk_points`/`with_partition_duration`，tsink `storage.rs:1153/1175` 已有）
- Modify: `docs/rest-api.md` 或 ops 调参指南（10 万 series 建议：chunk_points≤512 → active 稳态 ~1GB）
- Test: config roundtrip + builder 传递断言

Commit：`feat(history): 暴露 chunk_points/partition_duration 调参面`

### Task 10（M2）：rs-iot writer 分配削减 [rs-iot]

**Files:**
- Modify: `src/history/writer.rs:188`（push_dedup 键与 :245-248 series entry 键**共用一次构造**：dedup HashSet<(String,String)> 与 series HashMap 同键型，一次 clone 两用，砍 2 分配/行）
- Modify: `src/history/writer.rs:284-294`（TsinkRow labels：`Vec::with_capacity(2)` 复用；hwid 同批不变可缓存 String clone——注意跨行安全）
- Test: writer.rs:383-1272 现有 13 个语义测试全绿（行为等价重构）+ 新增性质测试（键构造次数，可选）

预期：砍 ~9 次分配/行，单独 5-15% 提升。Commit：`perf(history): writer 每行分配削减（dedup/series 键共用+labels 复用）`

### Task 11（M2）：tsink resolve 批内/调用方缓存 [tsink]

**Files:**
- Modify: `src/engine/ingest_pipeline/resolve.rs:117-133`（insert_rows 入口对批内行建 `(metric,canonical_labels)→series_id` 局部 HashMap 去重——rs-iot 场景批内 series 重复率低但跨批 100% 重复；进一步可加 storage 级 LRU 解析缓存，命中跳过 `identity.rs:315-332` 的双锁双 hash 双克隆）
- Modify: `src/engine/label.rs:72-118`（`marshal_metric_name` 免分配化：复用 buffer/预计算 identity hash——`shard_routing.rs:7` 预扫也用）
- Test: resolve 语义测试（命中/未命中/淘汰路径）+ 并发正确性

Commit：`perf(engine): series 解析批内去重+LRU 缓存，绕开每行字典双锁`

### Task 12（M2）：lane 校验批内按 series 去重 [tsink]

**Files:**
- Modify: `src/engine/ingest_pipeline/prepare.rs:450-458`（`validate_series_lane_compatible` per-point 三重锁查 → 批内按 series_id 去重后每 series 一次）
- Test: lane 混型拒绝测试回归（语义不变）

Commit：`perf(engine): lane 三重校验批内去重（每 series 一次锁查）`

### Task 13（M2）：可见性记账增量维护 [tsink]

**Files:**
- Modify: `src/engine/core_impl/write_visibility.rs:117-161,301-341`（before/after 全量估算 → 增量 delta 维护，消灭 O(series)/分片/批 ≈390 次条目访问/行；merge 阶段 recency Mutex + 3 全局写锁的串行化点一并评估）
- Test: 记账一致性测试（增量 vs 全量对拍）

Commit：`perf(engine): 可见性内存记账增量维护`

### [决策点 D1] M2 中期复测

Run T20 的 bench（M0+M1+T10-T13 完成后）：若 written ≥100K pts/s 且 dropped≈0 → T14 跳过；否则评估缺口决定是否上多写线程分片（**注意**：必须按 series hash 分片保死区/乱序单写者语义，`build_storage` 补 `with_max_writers`，tsink 全局锁是并行度上限——预期 1.3-1.7×，架构改动中大风险，单独设计文档再启动）。

### Task 15（M3）：rtdataWatch Lagged 去抖+排空+退避 [rs-iot]

**Files:**
- Modify: `src/api/rtdata_watch.rs:229-295`（Lagged 分支改造：① 循环 `try_recv` 先排空积压；② 重铺加最小间隔（≥5s）+ 指数退避；③ backfill `tokio::spawn` 出去不占消费循环）
- Test: watcher 单测（连续 Lagged 不触发连环重铺；退避窗口内丢事件仍靠 epoch+1 兜底）

Commit：`fix(rtdataWatch): Lagged 先排空+重铺退避，切断自放大风暴`

### Task 16（M3）：selfMonitor 丢弃/错误标签 [rs-iot]

**Files:**
- Modify: `src/process_metrics.rs` + `src/plugin/builtin/self_monitor/mod.rs:63-105,135-172`（加 `history_dropped_total/history_errors_total/history_written_total` 快照；注意 `readonly_tag_set_exact` 单测双处同步——`_sys_rsiot` 模型 tags 与 collect 清单）
- Test: self_monitor 标签集合精确断言更新

Commit：`feat(selfMonitor): 历史丢弃/写入标签进 rtdata（上层平台可图表化）`

### Task 17（M3）：sysstatus history.health 状态化 [rs-iot]

**Files:**
- Modify: `src/api/routes_sys.rs`（`history.health: "ok"|"degrading"|"dropping"`：按 dropped 速率/占比阈值判档）+ `self_monitor` 的 `healthy` 纳入该位
- Test: 三档判定单测

Commit：`feat(sysstatus): history 健康三态（ok/degrading/dropping）`

### Task 18（M3）：内建丢弃水位告警 [rs-iot]

**Files:**
- Modify: `src/history/rows.rs`（fan_out 侧计数已有）+ 新增内建判定（pipeline worker 上下文周期判断 dropped 速率超阈 → `tracing::error!` + `db::alarms::fire` 系统告警 + event_bus SSE 事件；节流防风暴复用 alarm.rs 频控思路；阈值先常量后配置）
- Test: 告警触发/恢复/节流单测

Commit：`feat(alarm): 历史丢弃水位内建告警（不依赖现场脚本）`

### Task 19（M3｜P5）：cardinality 热校准（只放大）[tsink+rs-iot]

**Files:**
- Modify: tsink `src/engine/engine.rs:192-206`（`cardinality_limit: usize` → `AtomicUsize`）、`construction/assembly.rs:137`（初始化）、`core_impl/contexts.rs:276`（load 读 atomic）、`src/storage.rs`（新 `set_cardinality_limit(usize)`，trait 面暴露）
- Modify: rs-iot `src/model/cache.rs:43-47`（`refresh_after_write` 尾部：重算 `auto_cardinality`，**仅当大于当前值**时调 setter——只放大不缩小，白名单缩小由 L3 过滤天然生效）
- Test: tsink 热更新并发写正确性；rs-iot e2e（运行期扩白名单后写入不再 CardinalityLimitExceeded——**用压测首启场景做回归**）

Commit：tsink `feat(storage): cardinality_limit 运行期热更新`；rs-iot `fix(history): 白名单扩张后 cardinality 只放大重校准（R4 收口）`

### Task 20（M4）：bench 全档复测 + 门禁对照 [rs-iot]

**Step 1**: worktree 里 `cargo build -j 2 --release --features history`（tsink fork 同步拉最新）
**Step 2**: 清 `bench/history-100k/data/`，按既有工件重跑全流程（首启→provision→重启→**monitor 15s 间隔**→10 分钟→verify→停服→重启持久化→kill -9）
**Step 3**: 门禁对照（Phase 4 六门禁）：①pipeline 劣化<5%（对比 off 基线，bench_pipeline_e2e）②历史写 ≥100K pts/s 100% 归档 ③kill -9 零丢失 ④磁盘 8-9B/点 ±20%（**数据需 seal 后测**：跑后等 partition 轮转或手动触发 compaction 再 du）⑤deadband 削减 ≥50%（加 deadband 对照组规模）⑥series===白名单数
**Step 4**: 结果回填本文档 §五 + `docs/decisions.md` 登记
**Step 5**: Commit：`test(bench): 10 万点复测——门禁 N/6（附证据）`

### [决策点 D2] 24h soak：门禁全过且用户要求时启动（磁盘预算按 §二数学预留，retention 调 1 天）

## 五、复验记录（2026-08-30 T20 全档复测回填，worktree 双仓 22 commit）

| 门禁 | 目标 | 实测 | 判定 |
|---|---|---|---|
| ① pipeline 劣化 | <5% | hub_depth=0 全程、Samples 满速（~1032/s），未跑 off/on A/B（间接证据） | ✅（间接） |
| ② 历史写吞吐/归档 | ≥100K pts/s / 100% | **稳态 95-101K pts/s、稳态段 dropped 冻结（≈100% 归档）**；全程平均 90.8K（含启动爬坡）、全程归档 88% | ⚠️ 边缘达标（writer_threads=4） |
| ③ kill -9 丢失 | 0 | **数据零丢失**（同窗口重启前后 516/516 精确一致）；但首次重启 init_failed 需二次重启自愈（见新发现 N1） | ⚠️ 数据面过/可用性面新缺陷 |
| ④ 磁盘 B/点 | 8-9 ±20% | 16.21 B/点（10 分钟窗口未 seal 值，首轮 84.6 大幅改善） | ⏳ 未判定（需 seal 后复测） |
| ⑤ deadband 削减 | ≥50% | chk_dead 566/566 全剔除（100% 削减） | ✅ |
| ⑥ series 精确 | ===白名单 | 100001 精确相等 | ✅ |

**总吞吐提升：修复前 7.5K → 稳态 ~100K pts/s（13.3×）；errors 22.9 万行 → 0；/metrics 46s → 0.32s。**

### 新发现（T20 期间，超出 T1-T19 范围，登记为后续任务）

- **N1｜kill -9 后首次重启 init_failed（os error 3）**：✅ **已修复**（2026-08-30 N1 专项，tsink commits 000a94a+bdce995，分支并入 master 9a292e1）。根因：compaction 替换先删源段目录，registry catalog 校验链读段根遇 PATH_NOT_FOUND 裸 Io 上抛——运行期 flush worker 报错与恢复期 init_failed 同一机制。修复：catalog 构建对段根消失跳过+WARN（侧车仅校验缓存，跳过保守安全）；open/hydrate/flush 全链 IO 带 IoWithPath。T21 实弹验证：kill -9 → **一次重启成功** + 数据零丢失。
- **N2｜T20 后段 dropped 5.5M→7.43M**：归因升级为 N4（见下），同一机制。
- **N3｜T20 期间 flush worker 再报错一次**：N1 根因覆盖（catalog 校验链），已随 N1 修复。
- **N4｜WAL 高水位推进跟不上高吞吐写入**：✅ **已修复**（2026-08-30 N4 专项，tsink commit 5e6885c）。根因：WAL 空间回收唯一路径是全有或全无的 `reset_if_current_highwater_at_most`（flush 快照后零新 append 才回收——持续写入下条件永不满足），与 catalog/seal/N1 无关。修复：新增 `reclaim_covered_prefix` 按 checkpoint 前缀删整段 + flush 管线挂接 + 回放终点回收。rs-iot 100K pts/s 实测：WAL 50-170MB 锯齿有界（修复前 4 分钟涨满 2GB）、169.8→46.5MB 回收实弹执行、wal_disk 拒批归零、kill -9 后 **25 秒 ready**（此前 2.5GB WAL 回放 4.5 分钟）、数据 38/38 零丢失。残留边界（未修，登记）：blob lane 启用但永无 blob 数据的部署重启仍全量回放（floor=0 保守口径）。

## 五-B、T21 终局复测（2026-08-30，N1 修复 + writer_threads=4 + chunk_points=512 + wal_size_limit_mb=8192）

| 门禁 | 目标 | T20 实测 | T21 实测 | 判定 |
|---|---|---|---|---|
| ② 写吞吐/归档 | ≥100K / 100% | 稳态 95-101K / 全程 88% | **稳态 96-104K，全程 written 54.4M≈sent，窗口内 dropped 零增长（4.21M 全部集中于启动 15 秒爬坡）；verify 8 PASS（dup_ts 归零）** | ✅ |
| ③ kill -9 | 0 丢失 + 恢复 | 数据 0 丢失但需二启 | **数据 41/41 精确零丢失（last_ts 逐秒一致）+ 一次重启成功**（N1 修复实弹验证：段目录消失跳过 WARN ×2 未炸） | ✅ |
| ④ 磁盘 B/点 | 8-9 ±20% | 16.2（未 seal） | **段数据口径 9.6 B/点**（525MB/54.4M 点，kill 前单进程生命周期口径；WAL 未截断部分不计） | ✅ |
| ⑥ series 精确 | ===白名单 | 100001 | 100001 | ✅ |

**结论：门禁 ②③④⑥ 达成；Phase 4 十万点容量目标（10 分钟窗口版）收官。遗留 N4（WAL 推进）为长跑 soak 前置必修项；D2 24h soak 待 N4 修复后决策。**
观察项：RSS 稳态 2.6-3.4GB（预算口径 used 仅 38MB——T7 口径分离后 registry/mmap/identity cache 走 RSS 属预期，10 万 series 部署需按此预留内存）；WAL 回放 2.5GB 耗时 ~4.5 分钟（长 WAL 的恢复时间成本）。

## 五-C、D2 24h soak 结果（2026-08-31，**失败于 8.2h——长跑退化 N5 族，未通过**）

soak 配置同 T21 终局（writer_threads=4 + chunk_points=512 + wal 8GB）。2026-08-30 21:36 启动，2026-08-31 04:46 WAL 触顶 8GB，04:46-05:45 间进程静默死亡（无 panic/无 stopped 日志/无 Windows Application Error/无 WER——死亡方式未决）。

**数据曲线**（591 磁盘样本 + 28 metrics 样本）：
- `memory_used`（active+sealed）**单调堆积**：299MB → 470MB(3.1h) → 983MB(5.1h) → 1378MB(6.9h)——persist/compaction 吞吐跟不上 seal 速率，sealed 积压；
- RSS 同步涨至 **13.76GB**（01:45 实测点；机器 63.4GB 物理内存，空闲 28.5GB，非系统 OOM）；
- 丢弃率非单调恶化：5.8%→35%→46%（3.1h 峰值）→31%（后期 written 回升至 91.6K/s）——吞吐随 compaction 阶段波动；
- WAL 前缀回收工作 7.17h（89 次回落）→ 04:41 停滞（回收依赖段持久化，persist 停则回收停）→ 5 分钟涨满 8GB → wal_disk 拒批；
- 段数据 9.1GB/10.1h（261KB/s 均速）；errors=0、series=100001 全程正确。

**N5 族登记（长跑退化，专项排查）**：
- **N5a（核心）**：sealed chunk 积压——persist/compaction 处理速率 < seal 速率（100K pts/s × chunk_points=512），积压单调涨，是内存/WAL/死亡三连锁的第一因；10 分钟窗口测不出；
- N5b：WAL 回收依赖段持久化，持久化停滞则回收连锁停滞（5 分钟涨满 8GB 的放大器）；
- N5c：进程静默死亡方式未决（候选：大分配失败 fastfail abort / 外部终止；OS 无取证）。

**判定**：Phase 4 门禁 ⑦（24h soak）**未通过**；T21 十分钟窗口的全部结论不受影响（容量/正确性/恢复已独立验证）。soak 复跑前置 = N5a 修复。工件：soak_samples.log / soak_metrics.log / logs（现场冻结保留）。

## 五-D、N5 修复与两轮复验（2026-08-31，**N5 三连锁消除获证；N5c 结案=环境 commit 耗尽；新发现 N6/mmap 并已修复**）

**N5 修复**（tsink `n5-sealed-backlog` 分支 5 commit，1283 全绿）：`121d967` 轮转反饥饿 / `ca60c8b` 选窗只加载窗口段（5.8GB→10MB）/ `c7c4016` 锁临界区缩至 marker（persist 阻塞 5.03s→0.04s，死亡链切断，顺带修复三个既存交接竞态 120 连跑验证）/ `45e6d26` L0 分片（1M 点/段，durable 全分片后推进）/ `8bff576` 输出段预算解耦。执行决策记录 `docs/decisions/2026-08-31-n5-longrun-degradation-fixes.md`。

**复验第 1 轮**（tmux 载体）：2h49m 时服务+采样器随 tmux server 崩溃双亡（日志止于正常 WAL 回收 INFO）；死前指标全绿。
**复验第 2 轮**（独立进程载体）：**7h25m 全程三连锁消除获证**（L1 峰值 8 vs 事故 4640；memory_used 稳定 341-370MB vs 事故单调涨 1378MB；L2 稳步压实；errors=0）——后同样静默死亡。

**N5c 结案（铁证）**：Windows System 事件 2004 三次记录 rs-iot commit 25.7/27.4/29.2GB——**段文件映射驻留随段数据单调涨 → 系统 commit limit 耗尽（WSL vmmem 竞争 12-14GB）→ Windows 终止最大 commit 进程**。三次死亡时刻与事件精确吻合。非代码缺陷，但暴露资源治理缺口。**真相修正**：Windows 上 tsink 段 "mmap" 实为堆驻留（`Box<[u8]>` 读入，绕开 Windows 映射锁目录）——commit 涨即全部活跃段字节堆驻留。

**新发现并修复**：
- **N6（L0 消费瓶颈）**：复验 2 全程 L0 单调涨 5→580（~130 段/h）——L0→L1 消费（8 段/5s 节拍）跟不上分片后产出；传导致 WAL 水位抬升（710MB 上探）。修复 `bfdc4b1`：一拍 drain 消化到水位 + 2s 时间预算让出 + 积压自适应 park（5s→1s）+ 每 lane 独立预算防饿死；理论消化 4800 段/h vs 产出 324 段/h（14× 余量），预期 L0 稳态 0-4 振荡。
- **mmap 驻留治理（N5c 资源面根治）**：修复 `84ae44f`：`SegmentMapRegistry` 全局预算（默认 4GiB，builder 可配）+ LRU 淘汰 + 按需 remap（读路径透明）+ 替换/重建同临界区释放——commit/RSS 主分量有界。rs-iot HistoryConfig 接线留后续小任务。

tsink 分支累计 7 commit（1294 全绿）。**第 3 轮复验启动前置就绪**（目标：L0 稳态个位数 + RSS/commit 有界平台 + 更长存活）。

## 五-E、第 3 轮复验结果（2026-09-01，6h07m 提前收口——N6/mmap 均部分生效未根治，第二轮治理立项）

载体：独立 Hidden 进程（PowerShell Start-Process），T0=00:30:45，06:31 优雅停服（手动）。**提前收口原因**：① 每小时监控 cron 实际处于 paused 态（10:30 自动收口不会触发，放任则无限跑）；② 5h25m 已触发双预警（L0 破百单调 + RSS 破 15GB 线），结论已明，继续跑边际价值低且有再次 commit 耗尽风险。

**四目标判定**：
- **N6（L0 有界）✗ 部分生效未根治**：L0 全程单调 0→177（均值 ~29 段/h，晚窗 05:55 129 → 06:30 177 约 +48/h）；修复前 130 段/h 至 580@8h——速率降至约 1/4~1/2 但仍无界，外推 ~7h 破 200 判定线。根因：2s drain 预算内实际窗合并数不足（实测消化 ~270-300 段/h vs 产出 324 段/h）。
- **mmap 驻留治理（RSS 有界）✗ 部分生效未根治**：RSS 单调涨至峰值 16,435MB（6.1h；~5.4h 破 15GB 告警线）；RSS/段数据比值 3.5→1.7 持续降（payload 预算工作），但 persisted index 常驻结构（chunk refs/时间戳索引/postings，每段一份；L2=1330，+218 段/h）不在治理范围，叠加后仍无界。
- **WAL 锯齿回归 ✓**：峰值 990MB 锯齿有界（<8192MB 上限的 12%；复验 2 上探 710MB，形态均为锯齿非触顶）。
- **进程存活 ✓（6h 窗口）**：16.4GB RSS 下无 System 2004；但 6h 手动停未覆盖复验 2 的 7h25m 死亡窗口，存活结论部分成立。

**正确性全绿**：errors=0 全程、series=100001、L1 峰值 ≤8（N5 修复保持）、memory_used 290→539MB 稳定、守恒闭合（生产 100K/s = sent 68.1K/s + dropped 31.9K/s，channel 满丢为既定背压行为，同 T21b）。

**新登记 N7（优雅关停在大积压规模下退化）**：CTRL_BREAK 停服时 `[history] close 超时(30s)放弃,WAL 回放兜底(部分 writer 线程脱落)`——数据安全（WAL 有界 990MB）但下次启动需回放而变慢；T21b 短窗关停仅 80ms，规模放大后 close_timeout_secs=30 不足。

**第二轮治理立项（tsink `n5-sealed-backlog` 分支续作，接 §五-D 7 commit 之后）**：
- **R2-1 L0 再提速**：drain 预算/选窗合并数提升（窗口 16/32 或预算加长），稳态消化需 >324 段/h 且有余量（当前实际仅 ~270-300）；
- **R2-2 persisted index 驻留有界化**：chunk refs/时间戳索引/postings 每段一份的常驻内存纳入 SegmentMapRegistry 同款预算治理或惰性化；
- **R2-3 N7 收尾（小项）**：close_timeout 调大或 drain-then-close 改进。

工件：`soak_r3_samples.log` / `soak_r3_metrics.log`（归档改名）/ `r3_metrics_final.txt`（停服前 /metrics 快照）/ `logs/rs-iot.log.2026-08-31`（UTC 命名，含关停尾证）。

## 六、风险与回滚

- **T1 单公式改动**：理论风险是"防 head 无限膨胀的预算防线弱化"——由增量记账+提交后强制执行兜底（`ingest_pipeline.rs:95-106`）；若复测出现真 OOM，回滚公式或 chunk_points 调小双保险。
- **T2 关 fail_fast**：持续 IO 故障被掩盖（数据滞留 WAL/内存）——T3 告警 + T6 分量观测是必要配套，**T2/T3 必须同批交付**；副作用：启动维护转同步（bootstrap 稍慢）。
- **T7 口径变更**：现有用户按旧口径调的预算会"变松"——release note 必须写明口径变更与换算。
- **T19 热校准**：并发写正确性依赖 atomic 语义，tsink 侧需并发测试；只放大策略避免缩放震荡。
- **两仓库联动**：tsink 改动先合（含测试），rs-iot 再 pin 新 commit；bench 复测用两仓库 HEAD 组合。
- **回滚单元**：每 Task 独立 commit，单 Task 可 revert；M0 内 T1/T2 是行为突变点，revert 即恢复旧行为。

## 七、执行注意事项

1. 本机 cargo 一律 `-j 2`（禁页面文件，CLAUDE.md:93）；tsink 仓库测试面 616+，rs-iot 带 `--features history` 的面 ~818。
2. 长任务跨 ≥3 commit → **worktree 隔离**（全局 AGENTS.md 硬性规则）。
3. 每 Task 走 TDD：红→绿证据在 commit message 或 PR 描述留痕；实现者禁改已红测试断言。
4. 涉及 tsink 的 Task（1/4/5/7/8/11/12/13/19）先读对应锚点源码再动，本计划锚点来自 2026-08-29 只读审查，行号可能有 ±10 漂移。
5. M0 完成后即可安排一次现场止血部署（不发 M2），配合运维指南：`cardinality_limit` 显式配置 + `chunk_points=512` + 预算 ≥2GB。
