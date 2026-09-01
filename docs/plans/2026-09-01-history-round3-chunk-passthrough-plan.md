# tsink 第三轮治理实施计划（R3：chunk 直通——恢复吞吐 + N7 收口）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** compaction 合并对"非重叠 chunk"免解码重编码（直通），把 16M/32M 点单体 pass 从 CPU 主导（解码物化+堆合并+重编码）降为 IO/CRC 主导，消除 writer 饿死占空比——恢复吞吐 ≥60K pts/s 并让 close 在 120s 内排空（N7）。

**Architecture:** 方案 A 最小切：`execution.rs` 的 per-series 循环前插 `try_pass_through_series_chunks` 判定函数，命中则原样发射（header/payload 拷贝、零逐点 clone），未命中回退既有 `stream_merge_series_chunks`（重叠/tombstone/retention/超 cap/空载荷五条件）。写侧零改动——writer 已具备免解码重写能力（`segment/tests.rs:248` 锁定）。方案 B 补统计与观测。方案 C（装载/写出两头免 zstd）登记后续，本轮不做。

**Tech Stack:** Rust（tsink @ `E:\Rs_Codes\tsink_fork`，分支 `n5-sealed-backlog`，基线 `62ade1c`，lib 569 绿），TDD，bench 复验用 rs-iot `bench/history-100k/`（判据对照 r4b/r3 曲线）。

---

## 〇、勘察结论（执行者必读，勿重新推导）

- **现状**：`stream_merge_series_chunks`（`src/engine/compactor/execution.rs:494-581`）逐 chunk 全解码物化（`:636-638`→`ChunkPointCursor`）、k 路堆合并逐点 clone（`:533-574`）、重编码+codec 重选（`:600-634`）。r4b 实测：32M 点（L1→L2）/16M 点（L0→L1）单体 pass 长持 compaction_lock（`runtime.rs:316-317` 门内整个 drain），writer 饿死占空比 = 吞吐 45K vs r3 65K 的主因。
- **写侧地基已存在**：`write_segment_with_wal_highwater`（`src/engine/segment/writer.rs:62-162`）从不解码，只吃 `chunk.encoded_payload` + header；`segment/tests.rs:248` 已锁定"装载后免解码跨 level 重写 + payload 逐字节保真"。level 不在 chunk header（`chunk.rs:38-47`，只在 chunk_index/manifest，writer 用自身 level 重新盖章）——**直通无需改写 header**。
- **Chunk 装载形态**：`encoded_payload` = **已解压**的 codec 载荷（`format.rs:1196/1508-1513`）；写出时 writer 重新 zstd（`format.rs:1450-1469`，level 1）。方案 A 保留这两头 zstd（估 pass 时长仍降 5-10×）。
- **段内序已天然近似**：窗口内 segment_id 升序 × 段内 `(min_ts,max_ts,point_count)` 序（`loader.rs:828-866`）；writer 落盘还会重排序（`format.rs:140-154`）。
- **去重语义**：pass 内只去"相邻 ts+value 全等"精确重复（`execution.rs:555-560`）；真正去重在查询侧（`query_read.rs:258-282` 后值胜出）。**不重叠 ⇒ 无共享 ts ⇒ 直通不引入新同 ts 对 ⇒ 安全**（与窗口外跨层重复同为查询态现状）。

### 回退条件（任一命中走既有解码路径；谓词必须逐字对齐引用锚点）

| # | 条件 | 锚点 |
|---|---|---|
| F1 | series 的 chunk 序列存在 `next.min_ts <= prev.max_ts`（闭区间共享端点即重叠，**与 analysis.rs:62 谓词逐字一致**） | `query_read/analysis.rs:59-72` |
| F2 | `tombstone_ranges` 存在且任一 range 与 `[min_ts,max_ts]` 相交（`start <= max_ts && end > min_ts`，**end 开区间**，对照 `tombstone.rs:76-82`；整段被覆盖的 chunk 可先 `interval_fully_tombstoned` 丢弃再判剩余——本轮简化：只要存在 tombstone_ranges 即整 series 回退） | `tombstone.rs:84-99` |
| F3 | `retention_cutoff.is_some()`（retention 重写路径恒传 Some，`execution.rs:221-233`） | |
| F4 | 任一 `chunk.header.point_count > point_cap` | |
| F5 | 任一 `encoded_payload` 为空（writer `format.rs:158-162` 拒绝） | |

### 不变量（测试锁定）

- I1 输出段点数/段数守恒：直通与旧路径对同一输入产出相同的"点数合计"与切段（预算切段逻辑 `execution.rs:303/331-357` 不变——**注意：直通 chunk 不重切，若单 series 单 chunk 已 > 输出段点预算不会发生（chunk ≤ point_cap « 8M）**；
- I2 直通 pass 零逐点 clone：用既有测试钩 `reset_chunk_clone_count/chunk_clone_count`（`chunk.rs:65-98`）断言；
- I3 重叠场景行为不变：`compacts_overlapping_l0_segments_into_l1`（`compactor/tests.rs:226`）与 `compactor_purges_tombstoned_points`（`:696`）必须保持绿（回退路径锁死）；
- I4 载荷保真：直通输出段重载后 payload 逐字节一致（仿 `segment/tests.rs:248` 断言方式——比较装载后形态而非盘上字节，zstd 非确定性 R7）。

---

## Task 1: 直通快速路径（方案 A，单文件）

**Files:**
- Modify: `src/engine/compactor/execution.rs:311-346`（`rewrite_segments` per-series 循环前插判定）
- Test: `src/engine/compactor/tests.rs`（新增直通测试组）

**Step 1: 写红测试**（新增 4 条）：
1. `compaction_passthrough_non_overlapping_chunks_without_decoding`：两段各含同 series 的时间不重叠 chunk（段 A [0,100]、段 B [101,200]），合并后断言：输出 chunk 数 = 输入 chunk 数（不重切不合并）、点数守恒、**`chunk_clone_count()` 为 0 或仅 header/payload 级（若钩计的是 Chunk::clone 则断言远小于逐点路径）**——先读 `chunk.rs:65-98` 钩的真实计数语义再定断言值、重载输出段 payload 与源逐字节一致。
2. `compaction_passthrough_falls_back_on_time_overlap`：同 series 两 chunk 共享一个 ts（[0,100]/[100,200]）→ 断言走解码合并（输出 1 个去重有序 chunk，同 `:226` 语义）。
3. `compaction_passthrough_falls_back_on_tombstones`：存在 tombstone_ranges → 行为与旧路径一致（点被清）。
4. `compaction_passthrough_falls_back_when_retention_cutoff_set`：retention 路径行为不变。
**Step 2: 跑红**——`cargo test -j 2 -p tsink --lib compaction_passthrough` FAIL（现行总是解码：测试 1 的 clone 计数/payload 断言、测试 2-4 因判定函数不存在编译错也算红）。
**Step 3: 实现**：新增 `try_pass_through_series_chunks(series_id, chunks, point_cap, tombstone_ranges, retention_cutoff, emit_chunk) -> bool`——按 §〇 回退表判定；命中则按序 `emit_chunk(Chunk { header: 原样 clone, points: Vec::new(), encoded_payload: 原样 clone, wal_highwater: default })`；`rewrite_segments` 循环改为"先试直通，false 才走 stream_merge"。计数口径：`emitted_points/chunks` 等额累计（R6）。
**Step 4: 跑绿**——新增 4 条 + compactor 全模块 + `persistence_background` 全绿；`:226/:696` 原样绿。
**Step 5: Commit**——`perf(compaction): 非重叠 chunk 直通——免解码物化/堆合并/重编码（pass 由 CPU 主导降为 IO/CRC 主导；重叠/tombstone/retention/超cap/空载荷五条件回退既有路径；r4b 实证 32M 点单体 pass 长持 compaction_lock 致 writer 吞吐 45K vs 65K）`

## Task 2: 直通统计与观测（方案 B，小改）

**Files:**
- Modify: `src/engine/compactor.rs:79-90`（`CompactionRunStats` 加 `passthrough_chunks/passthrough_points`）、`src/engine/compactor/execution.rs`（累计）、观测透传点（rg `CompactionRunStats` 找 drain/metrics 消费处）。
- Test: 直通测试 1 补断言 stats 计数 >0；回退测试断言 =0。

**Step 1-5:** 红→实现→绿→fmt/clippy→commit：`feat(compaction): 直通统计 passthrough_chunks/points 进 CompactionRunStats——bench 观测直通命中率`

## Task 3: 全量门禁

`cargo fmt --check` + `cargo clippy --all-features -- -D warnings` + `cargo test -j 2`（全 target；上轮全量 1305+，本轮新增后应 1309+）。全绿才进 Task 4。

## Task 4: 复验 bench（2h，主会话编排）

流程同 r4b（清洁目录→起服→provision→2h05m 采样器→优雅停服）。**判据**：
1. 吞吐：written 均值 **≥60K/s**（r4b 45.1K / r3 同窗 62-83K）且无 <20K 持续窗口；
2. N7：close 在 120s 内完成（日志无"close 超时放弃"）；
3. 保持：L0 有界（≤~20 振荡）、RSS 平台（≤~6GB）、WAL <1GB、errors=0；
4. 直通命中率（若观测面可达）。
**未达 1/2**：候选后手 = 窗口 16→8（单常量）再试；仍不达 → 方案 C（两头免 zstd）升级立项。

## Task 5: 合并推送 + 24h soak 启动（判据达成后）

tsink 分支（14+R3 commit）merge master → push origin+metme → rs-iot 侧重建 → 24h soak（门禁 ⑦ 最终判定）启动方案沿第 3 轮模式（独立载体 + 有界采样器 + 巡检 cron）。

---

## 登记后续（本轮不做）

- 方案 C：装载侧 raw 存储形态通路（`chunk_payload_from_record` raw 变体，`format.rs:1515-1569`）+ 写出侧 raw 分支——两头免 zstd，逼近纯盘速；
- compaction_lock 范围收缩/维护门公平调度（结构正解，直通生效后优先级降低）；
- rs-iot HistoryConfig 接线 tsink 新旋钮（含 `timestamp_index_cache_budget_bytes`）；tsink drain-then-close（N7 代码正解）；P2-P4 惰性化系列。

## 风险与回滚

- R1 重叠判定差一（`<=` vs `<`）：谓词逐字复制 `analysis.rs:62`，边界测试锁 `[0,100]/[100,200]`；
- R2 tombstone end 开区间：对照 `tombstone.rs:76-82`；本轮简化为"存在即回退"最保守；
- R5 拷贝内存：header+payload 各一次 memcpy（远小于逐点 clone），clone 钩断言锁定；
- R6 统计口径：等额计数，测试锚定；
- 单 Task 可 revert；直通 false 时行为与旧路径完全一致（回退矩阵测试锁死）。
