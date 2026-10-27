# 历史链第二轮治理实施计划（R2：L0 消费提速 + persisted index 驻留有界化 + N7）

> **状态**：superseded
> **取代者**：[2026-08-29-history-100k-improvement-plan.md](2026-08-29-history-100k-improvement-plan.md) §五-F（结论并入总账；本文 Task 3 调参已回退，勿按本文改 tsink 参数）
> **实施**：2026-08-29 ~ 09-03 分批（结论并入 2026-08-29 总账 §五-F；**Task 3（drain 2s→10s + park 1s→250ms）已回退**（62ade1c，实证塌方至 848/s））
> **代码**：tsink fork（过程稿，结论见总账）
> **测试**：tsink fork 侧 `cargo test`；rs-iot 侧 `tests/history_*_e2e.rs`

**Goal:** 消除第 3 轮复验收口判定"部分生效未根治"的两条无界增长——L0 段积压（~29 段/h 单调）与 persisted index 内存驻留（RSS 16.4GB@6h 破线），并把 N7（close 超时）处置掉，使 100K pts/s 长跑可达稳态。

**Architecture:** 三线并进但串行提交：R2-1 改 tsink compaction 常量与旋钮面（根因是"8 进 8 出"——输出段点预算=L0 分片大小导致段数不减少、轮转仲裁偷渡 50% pass）；R2-2 对 persisted index 做 P0（双份 chunk 引用去重）+P1（时间戳索引 LRU 化），复用 `SegmentMapRegistry` 的预算+LRU+按需重建骨架，不动段文件盘格式；R2-3 是 rs-iot 侧配置+文档小项。全部在 tsink `n5-sealed-backlog` 分支续作（rs-iot 侧仅 T8/T9/T10）。

**Tech Stack:** Rust（tsink 嵌入式时序库 fork @ `E:\Rs_Codes\tsink_fork`，path 依赖 `../tsink_fork`），TDD（cargo test，本机一律 `-j 2`），bench 复验用 rs-iot `bench/history-100k/`。

---

## 〇、基线与全局约束（执行者必读）

- **仓库/分支**：`E:\Rs_Codes\tsink_fork`，分支 `n5-sealed-backlog`，基线 `bfdc4b1`（7 commit 领先 master `ce1ad61`，1294 测试全绿）。整个仓库即本 feature 的隔离载体（master 只是合并基线，不直接动）。
- **背景文档**：rs-iot `docs/designs/2026-08-29-history-100k-improvement-plan.md` §五-D/§五-E（第 3 轮判定与数据）；本计划的勘察来源是两份只读勘察报告（2026-09-01，锚点已内联到各 Task）。
- **门禁**：每 Task 独立 commit（中文，仓库风格见 `git log --oneline`）；Task 完成定义 = 新增/修改测试先红后绿 + `cargo fmt` + `cargo clippy --all-features -- -D warnings` + 相关测试模块全绿。
- **硬约束**：本机 cargo 一律 `-j 2`；不动段文件盘格式（chunks.bin/chunk_index.bin/series.bin/postings.bin/manifest.bin 布局不变）；实现者禁改已红测试的断言（期望值随 Task 指令更新除外，须在 commit message 注明）。
- **第 3 轮实测基线（判据对照用）**：100K pts/s 生产（sent 68.1K/s + dropped 31.9K/s），L0 产出 324 段/h，实测消化 270-300 段/h，L0 单调 0→177@6.1h；RSS 单调→16,435MB；L2=1330 段（+218/h）；WAL 锯齿峰值 990MB；errors=0。工件 `rs-iot bench/history-100k/soak_r3_samples.log`。

---

## R2-1 线：L0 消费提速（tsink 侧，Task 1-4）

### 根因（勘察结论，勿重新推导）

1. **"8 进 8 出"**：`DEFAULT_OUTPUT_SEGMENT_POINT_BUDGET`=1,048,576 点（`src/engine/compactor.rs:42`）与 L0 分片 `FLUSH_SEGMENT_MAX_POINTS`=1M（`src/engine/lifecycle/flush.rs:210`）相同 → L0→L1 合并 8×1M 产出 ~8 个 1M 段，段数不减少。
2. **轮转偷渡**：L1 永远满足 `DEFAULT_L1_TRIGGER`=4（`compactor.rs:34`）→ `compact_once_with_changes`（`src/engine/compactor/planning.rs:108-121`）每两个 pass 只有一个消化 L0，理论上限直接减半。
3. **节拍形态**：drain 预算只拦"开始下一次 pass"不拦执行（`src/engine/lifecycle/shutdown.rs:85-126`，`COMPACTION_DRAIN_TIME_BUDGET`=2s @ `src/engine/runtime.rs:42`）；单 pass 25-50s 时每拍恰 1 pass + park 1s（`COMPACTION_BACKLOG_PARK_INTERVAL` @ `runtime.rs:50`；两态切换 `runtime.rs:330-334`）。
4. 单线程全串行 + 逐点解码重编码（`execution.rs:494-668`）+ fsync 风暴（~64 fsync/pass，`segment/writer.rs:62-162` + `fs_utils.rs:366-419`）放大单 pass 时长。

### Task 1: 输出段点预算 1M → 8M（消"8 进 8 出"，本轮最高杠杆）

**Files:**
- Modify: `src/engine/compactor.rs:42`（`DEFAULT_OUTPUT_SEGMENT_POINT_BUDGET: u64 = 1_048_576` → `8_388_608`）
- Test: `src/engine/compactor/tests.rs:399`（`compaction_output_point_budget_is_decoupled_from_chunk_point_cap`）、`:458`（`..._keeps_default_chunk_points_behavior`）、`:503`（`compactor_splits_large_output_into_multiple_segments`）——三处期望值按 8× 放大更新；新增一条锁定测试：8 个 1M 点 L0 段一窗合并 → 产出 **1 个** ~8M 点 L1 段（段数 8→1）。

**Step 1: 写红测试**——先更新三处期望值并新增锁定测试（命名参考 `compaction_output_budget_eight_million_reduces_segment_count`），断言输出段数从 ~8 降为 1。
**Step 2: 跑红**——`cargo test -j 2 --lib compactor::tests`，预期新增/更新用例 FAIL（现行 1M 预算下仍产 8 段）。
**Step 3: 最小实现**——改 `compactor.rs:42` 常量。
**Step 4: 跑绿**——同命令全 PASS；顺跑 `cargo test -j 2 --lib` 确认无连带（重点 `compactor/tests.rs:61/114/179/358`、`tests/persistence_background.rs:85/137/194` 不碎；碎则按新语义修期望并在 commit 注明）。
**Step 5: Commit**——`fix(compaction): 输出段点预算 1M→8M——消"8进8出"（L0→L1 段数 8:1 收敛，L1 触发频率÷8，轮转偷渡随消）`

### Task 2: 源窗口 8 → 16（摊薄每 pass 固定开销，须在 Task 1 之后）

**Files:**
- Modify: `src/engine/compactor.rs:35`（`DEFAULT_SOURCE_WINDOW_SEGMENTS: usize = 8` → `16`）
- Test: `src/engine/compactor/tests.rs:61`（`compaction_window_selection_fully_loads_at_most_window_segments`，"恰好吞 8 余 12 产 1"类断言改为 16 段语义）、`:358`（`compactor_limits_source_window_per_pass`）；检查 `tests/persistence_background.rs:232-237`（4 段/窗自洽断言）不受影响。

**Step 1: 写红**——更新窗口断言为 16 段语义 + 新增锁定测试：16 个 L0 段一窗 → 2 个 8M 段输出。
**Step 2: 跑红**——`cargo test -j 2 --lib compactor::tests` FAIL。
**Step 3: 实现**——改 `compactor.rs:35`。
**Step 4: 跑绿**——compactor + persistence_background 模块全绿。
**Step 5: Commit**——`perf(compaction): 源窗口 8→16——单 pass 摊薄 marker/tombstone/巡检固定开销（配合 8M 输出预算）`

### Task 3: drain 预算与积压 park 调整

**Files:**
- Modify: `src/engine/runtime.rs:42`（`COMPACTION_DRAIN_TIME_BUDGET`: 2s → 10s）、`src/engine/runtime.rs:50`（`COMPACTION_BACKLOG_PARK_INTERVAL`: 1s → 250ms）
- Test: `tests/persistence_background.rs:194`（`compaction_drain_pass_yields_once_time_budget_is_exhausted`，ZERO 预算语义，应不受影响）；如无对两常量值的锁定测试则补一条文档性断言测试（常量导出可见时）。

**Step 1: 确认现有语义测试**——跑 `cargo test -j 2 --test persistence_background compaction_drain` 应全绿（不动）。
**Step 2: 改常量**——10s 预算让"快 pass 连续多跑"；250ms park 把 `pass+1s` 节拍的死时间压到 ~1-4%。
**Step 3: 跑绿**——persistence_background 全量。
**Step 4: Commit**——`perf(compaction): drain 预算 2s→10s+积压 park 1s→250ms——快 pass 连续消化，节拍死时间压缩`

### Task 4: compaction 旋钮挂 StorageBuilder（现场可调面）

**Files:**
- Modify: `src/engine/config.rs:82`、`:167`（`DEFAULT_COMPACTION_INTERVAL`=5s 硬编码处，`engine.rs:111`）——新增可配置项 `compaction_interval`、`drain_time_budget`、`source_window_segments`、`output_segment_point_budget`（默认=新常量值）
- Modify: `src/engine/construction/resources.rs:106` 附近（builder → Compactor/Runtime 装配管线透传）；`compactor.rs:144-148`（override 仅 `#[cfg(test)]` 的口子放开为生产 builder 参数）
- Test: builder override 生效单测（仿 mmap 预算旋钮 `persisted_mmap_budget_bytes` 的既有测试模式，`src/engine/tests/segment_map_residency.rs:11` `persistent_storage_with_mmap_budget` 为参照）。

**Step 1: 写红**——新测试：builder 设 `source_window_segments(4)` / `output_segment_point_budget(2_000_000)` 后 compaction 行为按覆盖值走。
**Step 2: 跑红**——FAIL（builder 无此方法）。
**Step 3: 实现**——config 字段 + builder 方法 + 装配透传。
**Step 4: 跑绿**——新测试 + compaction 全模块。
**Step 5: Commit**——`feat(config): compaction 四旋钮进 StorageBuilder（interval/drain 预算/窗口/输出点预算）——压测现场可调，rs-iot HistoryConfig 接线登记后续`

---

## R2-2 线：persisted index 驻留有界化（tsink 侧，Task 5-6）

### 根因（勘察结论，勿重新推导）

`84ae44f` 的预算判据**显式排除**持久化索引（`src/engine/lifecycle/persisted_state/install.rs:43-46` 注释原文"persisted index and mmap bytes are EXCLUDED from the budget criterion"，只喂 `persisted_index_used_bytes` 观测）。每 chunk 无上界驻留：chunk 引用**双份**（全局 `chunk_refs` `persisted.rs:131` + 段内 `chunk_refs_by_series` `persisted.rs:125-126`，各 ~56B）；时间戳索引 `HashMap<u64, OnceLock<..>>`（`persisted.rs:132-133`）install 期无条件预插空条目（~100B/chunk 底噪，`install.rs:352-354`），首次触碰构建 anchor 后**永不逐出**（+200-256B/chunk）。关键利好：所有结构要么已落盘（chunk_index.bin/postings.bin 按需 parse 现成，`format.rs:1042/915`）要么有现成重建函数（正是 open 路径在用的）——惰性化不需要补持久化。

### Task 5（P0）: 双份 chunk 引用去重

**Files:**
- Modify: `src/engine/state/persisted.rs:125-126`（删 `PersistedSegmentState.chunk_refs_by_series` 字段）
- Modify: `src/engine/lifecycle/persisted_state/install.rs:286`、`:326-329`（不再构建段内副本；全局 `chunk_refs` 构建 `:347-351` 保留）
- Modify: `src/engine/query_exec/time_range_planner.rs:45`（fallback `chunk_refs_by_series.keys`）、`:59`（`.get(series)`）——改从全局 `chunk_refs.get(series)` 按 `segment_slot` 过滤（`PersistedChunkRef.segment_slot` 字段判等）
- Modify: `src/engine/maintenance/memory_accounting.rs:355-431`（删段内分量记账，`persisted_index_used_bytes` 口径随之更新）
- Test: `src/engine/query_exec/time_range_planner.rs:212`、`:256` 剪枝单测（行为等价：改动前后时间范围剪枝选出的 (series, chunk) 集合一致）；新增 residency 断言测试：段内副本结构不再存在（编译期即锁定）+ `persisted_index_used_bytes` 较去重前约降（可用固定拓扑对比记账值）。

**Step 1: 写红**——先写行为等价测试（剪枝结果不变）并保留；再写记账对比测试（去重后每 chunk 记账 ≤ 原值-56B 量级，用固定小拓扑）。
**Step 2: 跑红**——记账对比 FAIL（现值含双份）。
**Step 3: 实现**——删字段 + 改 planner 过滤 + 改记账。
**Step 4: 跑绿**——`cargo test -j 2 --lib`（重点 time_range_planner/query_exec/persistence_recovery 全绿；`metadata_postings.rs:114` 用全局 `chunk_refs.len()` 不受影响）。
**Step 5: Commit**——`fix(persisted): 段内 chunk_refs_by_series 副本删除——全局/段内双份引用去重（每 chunk ~56B 无上界项归零，剪枝改按 segment_slot 过滤全局 refs）`

**已知取舍（commit message 注明）**：planner 由"段内 O(段内 series) 直取"变为"全局 per-series refs 线性过滤"，series 的 refs 随段数增长（bench 曲线下 ~10² 量级）可接受；段级局部化的正解是 P3（登记后续，前置=维护拍全量扫改水位粗判，本轮不动 `write_memory.rs:229` / `budget_enforcement/context.rs:62,145`）。

### Task 6（P1）: 时间戳索引 LRU 化

**Files:**
- Modify: `src/engine/state/persisted.rs:132-133`（`chunk_timestamp_indexes: HashMap<u64, OnceLock<TimestampSearchIndex>>` → 预算化缓存：clock+字节预算，条目含 `segment_slot` 供段替换时定向逐出；骨架抄 `src/engine/state/segment_maps.rs:86-89`（touch）与 `:236-268`（enforce_budget，含保底 1 条驻留））
- Modify: `src/engine/lifecycle/persisted_state/install.rs:352-354`（删 install 期无条件预插空条目）、`:408-414`（段替换逐出改按缓存条目 `segment_slot` 匹配清除，缓存有界故 O(缓存大小)）
- Modify: `src/engine/core_impl.rs:236-275`（`persisted_chunk_timestamp_search_index`：miss → 走既有 `build_timestamp_search_index_from_payload` 重建后入缓存；记账口 `:267-270` 保留）
- Test: 新增 `src/engine/tests/` 下 residency 测试（仿 `segment_map_residency.rs:69`（`..._collapses_to_freshest_map_and_remaps_transparently`）/`:99`（`..._bounds_resident_bytes_across_restart_reload`）模式）：① 小预算下多 chunk 触碰 → 驻留字节数有界且最旧条目被逐出；② 被逐出后再次可见性查询透明重建、结果一致；③ compaction 替换源段后缓存条目清除（仿 `:167` `compaction_replacement_releases_source_segment_maps`）。既有 `persistence_recovery.rs:1988` 懒构建断言必须保持绿。

**Step 1: 写红**——三条 residency 测试（预算逐出/透明重建/替换清除）。
**Step 2: 跑红**——FAIL（现 OnceLock 永不逐出）。
**Step 3: 实现**——缓存结构 + install/remove 改造 + core_impl 重建接入。预算默认 128MiB，builder 旋钮 `timestamp_index_cache_budget_bytes`（仿 mmap 预算装配 `construction/assembly.rs:114`）。
**Step 4: 跑绿**——新测试 + persistence_recovery/persistence_background 全绿。
**Step 5: Commit**——`fix(persisted): 时间戳索引 LRU 化——OnceLock 永生条目改预算缓存(默认128MiB+旋钮)，install 预插空条目底噪归零，miss 透明重建`

---

## 收尾线（Task 7-10）

### Task 7: tsink 全量门禁

**Step 1:** `cargo fmt`；`cargo clippy --all-features -- -D warnings`（零警告）。
**Step 2:** `cargo test -j 2`（预期 1294+新增全绿；有任何红即回修，不带病合并）。
**Step 3:** Commit（如有 fmt 修整）；更新 rs-iot 侧无依赖说明——rs-iot 用 path 依赖 `../tsink_fork`，分支工作区即生效，无需 pin。

### Task 8: R2-3 N7 处置（rs-iot 侧，配置+文档）

**Files:**
- Modify: `bench/history-100k/config.toml:44`（`close_timeout_secs = 30` → `120`）
- Modify: `docs/ops/history-100k-tuning.md`（补一节：大积压规模/长跑部署 close_timeout 建议 ≥120s；引用 r3 证据——6h 积压下 30s 超时退 WAL 回放，恢复变慢；tsink 侧 drain-then-close 改进登记为后续小项）
- Commit: `docs(ops): N7 处置——bench close_timeout 30→120s + 调参指南补长跑关停建议`

### Task 9: 复验 bench 短窗（2h，主会话编排，不派子代理）

流程：`cargo build --release --features history -j 2`（rs-iot）→ 清 bench data 目录 → `start.sh` 独立载体起服 → provision → soak_sample.sh 2h → 收口。**判据**（对照 r3 同刻，`soak_r3_samples.log` 同 T+时刻值）：
1. L0 有界：全程不单调爬，稳态 ≤ 两位数振荡（r3 同刻 ~60+ 且单调）；
2. RSS：2h 点 ≤ r3 同刻值 ×0.6，且增速显著放缓（P0+P1 生效；严格有界要 P2/P3，非本轮判据）；
3. 摄入不反噬：written ≥60K pts/s（r3 为 67.8K），errors=0；
4. WAL 锯齿 <1GB；优雅关停在 120s 内完成（N7 验证）。

### Task 10: 合并/推送/24h soak 决策点（留用户裁决）

tsink 分支 7+6=13 commit → merge master → push origin(github)+metme 双远程；24h soak（门禁 ⑦ 最终判定）是否立即启动由用户定。

---

## 登记的后续项（本轮不做）

- P2 段级规划结构惰性化（time_bucket_postings/series_time_summaries）/ P3 chunk_refs 段级惰性（前置：`write_memory.rs:229`、`budget_enforcement/context.rs:62,145` 维护拍全量扫改水位粗判）/ P4 merged_postings 按 key 惰性并集；
- 合并 chunk 直通（非重叠 series 免解码重编码，`execution.rs:494-581`）、并行 compaction（lane/窗口级 worker pool）；
- L1→L2 输出预算按级别放大（当前 4×8M 进 → 4×8M 出，仅 ~11 pass/h，低优先）；
- rs-iot HistoryConfig 暴露 Task 4 新旋钮 + mmap 预算旋钮（e2e 配套）；
- tsink drain-then-close（N7 代码侧正解）。

## 风险与回滚

- **T1 输出段 8M 点**（payload ~64MB/段）：mmap 驻留预算 4GiB 下常驻 ~64 段，LRU 可逐出；若 bench 出现查询延迟恶化，回滚常量即可（单行）。
- **T2 窗口 16**：单 pass 内存/时长 ×2（全量载荷加载+逐点解码）；与 T1 正交，可独立回滚。
- **T3 预算 10s**：compaction_lock 持有变长，可能拉长 catalog refresh 全量扫描等待（锁序 maintenance > compaction > segment replacement）；bench 判据 3 若不达标先回退此项。
- **T5/T6**：不动盘格式、不动查询语义（行为等价测试锁定）；T6 缓存 miss 重建走已验证路径（open 路径同款），锁语义变化（OnceLock→RwLock）需并发测试覆盖——`cargo test -j 2` 全量含既有 1294 面。
- 每 Task 独立 commit，单 Task 可 revert。
