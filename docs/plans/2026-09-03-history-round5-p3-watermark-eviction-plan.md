# tsink 第五轮实施计划（R5：P3 水位粗判——消维护拍全量扫）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 把维护拍 sealed 逐出的全 series×chunk_refs 内容比对（S1/S2，两份重复实现）替换为纯水位谓词 `key.sequence <= persisted_chunk_watermarks[series]`，消掉随累积段数线性增长的每拍成本——soak24b 实证的 L0 积压加速（43@3h→1007@27h，+45/h→+190/h）的根因。

**Architecture:** Cut 0 先去重（write_memory.rs 与 budget_enforcement/context.rs 的逐字重复实现合一到 context.rs）零语义变化；Cut 1 换谓词并去掉 `persisted_index.read()` 全程持锁；Cut 2 在唯一水位推进点加边界校验。不新建结构（`persisted_chunk_watermarks` 已存在，flush.rs:562 publish 成功后单点推进、每 series max 单调）。顺带修正语义缺陷：现状内容比对在 compaction 替换后判定失败 → sealed chunk 永久不可逐出（泄漏方向）；水位谓词正确逐出（N8 variant A 测试已锁定该语义）。

**Tech Stack:** Rust（tsink @ `E:\Rs_Codes\tsink_fork`，master 直作，基线 `7f16d59`，lib 576 绿/全量 1312），TDD，复验对照 soak24b 27.2h 曲线。

---

## 〇、勘察结论（执行者必读，勿重新推导）

### 全量扫调用点（改造目标加粗）

| # | 位置 | 语义 | 现状谓词 |
|---|---|---|---|
| **S1** | `context.rs:52-88` + `write_memory.rs:219-255` `find_oldest_evictable_sealed_chunk`（**逐字重复两份**） | 找全局最旧可逐出 sealed chunk | 每 chunk 水位查表后，候选者再对该 series 整条 persisted chunk_refs 线性扫描（`sealed_chunk_is_present_in_persisted_chunks` 六字段比对） |
| **S2** | `context.rs:134-177` `evict_persisted_sealed_chunks` | 全量逐出已持久化 sealed | 同上（retain 清扫） |
| S3/S4 | `write_memory.rs:285-302` to_budget / `budget_enforcement.rs:12-50` | admission/提交后排干 | 复用 S1 循环 |
| S5 | `flush.rs:76-163` 快照收集 | flush 取数据 | **已是水位粗判**（:102 `key.sequence <= persisted_sequence → continue`），不改 |
| S6 | `memory_accounting/context.rs:196-235` 全量记账重算 | observability | **本轮不动**（同曲线第二贡献者，登记后续做增量/采样） |
| S7 | `visibility/cache/summaries.rs:339-372` | 可见性摘要重建 | 已 touched-series 粒度，不动 |

### 关键事实

1. **水位结构已存在**：`persisted_chunk_watermarks: RwLock<HashMap<SeriesId, u64>>`（engine.rs:157），读法 `persisted.get(series_id)`（write_memory.rs:227 / context.rs:60,143）；**唯一推进点** `mark_persisted_chunk_watermarks`（memory_accounting.rs:437-459，每 series max 单调）← flush.rs:562（**publish 成功之后**才调用，失败/回滚不推）。
2. **sequence 全局单调**（engine.rs:158 `next_chunk_sequence` fetch_add；重启从 persisted index 恢复并抬高 install.rs:319-374），sealed 插入严格跟随 sequence 递增——**不存在乱序 seal**，水位 ≤ 判断方向安全。
3. **两份实现的已知小差异**（Cut 0 合一时注意）：write_memory 版有 `has_persisted_lanes` 短路与 `accounting_enabled` 判定位置不同——读两边现状后合并，保持行为等价。
4. **锁收益**：S1/S2 的 `persisted_index.read()` 目前跨 64 shard 全程持有（与查询规划 time_range_planner.rs:36-62 直接竞争）——替换后可整个去掉；`backpressure_lock` 保留（与 admission 并发逐出互斥仍需要）但临界区从"逐 chunk 内容比对"缩到"纯 sequence 比较"。
5. **语义修正论证**：compaction 替换源段后 refs 边界变化，现状内容比对失败 → sealed 永久驻留（内存泄漏方向）；水位谓词正确逐出（数据已在替换输出中持久化）。N8 variant A 测试（persistence_segments.rs:962）**已断言**"watermark advances + sealed evicted"——本轮谓词与其语义一致。

### 必须保持绿的既有测试

`persistence_segments.rs:962`（N8 variant A 水位语义）、`capacity.rs:64/145/185/208`（含 `assert_engine_memory_usage_reconciled` 对账）、`persistence_background.rs:568`（后台逐出断言 :983/:1115）、`admission_control.rs:484/517/624/638/657`（admission relief seal→persist→evict 全链）、`persistence_segments.rs:834-841`（多 shard 持久水位——同名不同义，勿混淆）。

---

## Task 1（Cut 0）: 去重两份逐出实现（零语义变化）

**Files:**
- Modify: `src/engine/core_impl/write_memory.rs:202-302`（删重复，改调 context.rs 实现）
- Modify: `src/engine/maintenance/budget_enforcement/context.rs`（如需可见性调整 pub(crate)/pub(super)）
- Test: 无新测试（行为等价重构，全量门禁即验证）

**Step 1:** 读两份实现，列差异表（`has_persisted_lanes` 短路 / `accounting_enabled` 位置）；把等价逻辑合并到 context.rs（admission 路径 prepare.rs:343 与 post-commit 路径统一指向它）。
**Step 2:** `cargo test -j 2 -p tsink --lib` 全量 576 绿（重点 capacity/admission_control/persistence_background）。
**Step 3:** fmt + clippy 零警告。
**Step 4:** Commit：`refactor(evict): 去重 find_oldest_evictable/evict_persisted 双份实现——admission 与 budget_enforcement 路径统一指向 context.rs（行为等价;为水位谓词替换铺路）`

## Task 2（Cut 1 核心）: 纯水位谓词替换

**Files:**
- Modify: `src/engine/maintenance/budget_enforcement/context.rs:17-32`（`sealed_chunk_is_present_in_persisted_chunks` 改为水位比较或被内联取代）与 `:52-88`、`:134-177`（三处谓词点）
- Modify: 推进点不变（flush.rs:562 / memory_accounting.rs:437-459）
- Test: `src/engine/tests/persistence_segments.rs` 新增 2 条锁定测试

**Step 1: 写红**：
1. `sealed_eviction_survives_compaction_boundary_rewrite`：构造"flush 持久化 sealed → compaction 合并该段（refs 边界改写，六字段不再全等）→ 逐出仍发生（水位语义）"——现状内容比对下该场景 sealed 永驻（红=断言逐出失败）。
2. `publish_rollback_keeps_sealed_until_watermark_advances`：注入 publish 失败（可用 cfg(test) hook 或极小磁盘错误注入，先读 flush.rs:539-556 现状找注入点）→ 水位不进 → sealed 不逐出。
**Step 2: 跑红**——`cargo test -j 2 -p tsink --lib sealed_eviction 2>&1 | grep -E 'test result|FAILED'`（测试 1 FAIL 于现状内容比对；测试 2 视现状可能已绿，作为护栏同批提交并注明）。
**Step 3: 实现**——三处谓词点换 `key.sequence <= persisted_chunk_watermarks.get(series).copied().unwrap_or(0)`；删 `persisted_index.read()` 获取（S1/S2 全程）；内容比对函数降级 `#[cfg(test)]`（供测试交叉校验）。在谓词处加不变量注释：*"evictable ⇔ sequence ≤ watermark（publish 成功后单点推进）; compaction 替换不回退水位——数据已在替换输出持久化"*。
**Step 4: 跑绿**——新增 2 条 + 全量 578+ 绿（N8 variant A / capacity 对账 / admission relief 全链是重点盯防）。
**Step 5:** fmt + clippy。
**Step 6:** Commit：`perf(evict): P3 水位粗判——sealed 逐出谓词由 per-chunk 内容比对(随累积 refs 线性,compaction 替换后永久不可逐出的泄漏方向)改为纯水位 sequence≤watermark(已存在,publish 成功单点推进);去掉 S1/S2 的 persisted_index 全程读锁(与查询规划解竞争)——soak24b L0 积压加速(43→1007,+45/h→+190/h)根因消除`

## Task 3（Cut 2）: 水位推进边界校验

**Files:**
- Modify: `src/engine/maintenance/memory_accounting.rs:437-459`（`mark_persisted_chunk_watermarks` 加 debug_assert：推进值 ≤ 本 pass 已安装 refs 的 max sequence——防未来出现"未验证 publish 就推水位"路径导致过度逐出丢数据）
- Test: 现有 N8 variant A 已覆盖语义；debug_assert 由全量 debug 测试面执行。

**Step 1:** 实现校验（读 mark 与其调用方 flush.rs:539-563 现状，取"本 pass max_sequence"传入比对；注意不引入新锁竞争——校验值在 mark 内部可从参数推导）。
**Step 2:** 全量绿 + fmt/clippy。
**Step 3:** Commit：`fix(evict): 水位推进边界校验——debug_assert 推进值≤已安装 refs max sequence(防未验证 publish 推水位的过度逐出;数据安全护栏)`

## Task 4: 全量门禁

`cargo fmt --check` + `cargo clippy --all-features -- -D warnings` + `cargo test -j 2`（全 target，基线 1312+2 新增 = 1314）。

## Task 5: 复验 bench + soak 第 5 次（主会话编排）

- **5a 复验 6h bench**（对照 soak24b 前段）：判据——L0 斜率显著回落（soak24b 3h=43/6h=178；修复目标 ≤1/4 即 6h ≤45 且非单调加速）、吞吐 ≥70K 均值、errors=0、RSS/WAL 与 r6 同界；
- **5b soak 第 5 次**（门禁 ⑦ 第 5 次尝试）：24h+，判据沿用 §五-J 清单 + L0 斜率全程 ≤10/h；
- 达标后合并推送双远程 + §五-K 回填。

---

## 登记后续（本轮不做）

S6 记账全量重算增量/采样化（同曲线第二贡献者）/ 方案 C 两头免 zstd / N7 close 排空 / N9 满 WAL 回放根因 / 上游 PR 回馈评估。

## 风险与回滚

- **过度逐出丢数据**（高损害低概率）：水位只在 flush.rs:562 publish 成功后单点推进（现状即如此，本轮不改推进点）+ Task 3 debug_assert 护栏 + N8 variant A 语义锁。
- **Cut 0 合一漏掉 admission 路径差异**：Step 1 的差异表先行；全量门禁（admission_control 5 条 relief 测试）兜底。
- **记账对账漂移**：capacity.rs `assert_engine_memory_usage_reconciled` 全绿门禁。
- 每 Task 独立 commit 可 revert；Cut 1 与 Cut 2 分开提交便于二分。
