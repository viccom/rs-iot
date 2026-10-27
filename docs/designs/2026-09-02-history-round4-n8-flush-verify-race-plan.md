# tsink 第四轮实施计划（R4：N8 修复——flush verify 对 compaction 替换的容忍语义）

> **状态**：implemented
> **实施**：2026-08-29 ~ 09-03 分批（结论并入 2026-08-29 总账 §五-I（N8 flush verify 竞态））
> **代码**：tsink fork（`../tsink_fork`）
> **测试**：tsink fork 侧 `cargo test`；rs-iot 侧 `tests/history_*_e2e.rs`

**Goal:** 修复 N8（flush worker 的 `verify_flush_segment_publication` 对"自己刚发布的 L0 根被 compaction 并发替换删除"零容忍→整 pass 失败→死亡螺旋至 WAL 8GB 硬上限），使 100K pts/s 长跑越过 T+90min 复现锚点。

**Architecture:** 单点修复：verify 循环对每个发布根的 `load_segment_index` 错误分流——NotFound → 持 `segment_replacement_gate` 复查确认根已消失（删除只在该锁内发生）→ WARN + 从发布集剔除 + 置 `persisted_index_dirty` 对账 + **不 rollback 不失败**；真损坏仍 rollback+Err。数据安全不变量：被替换删除的 staged 根，其 chunk 持久化责任已转移给替换输出（输出 manifest 在 marker 应用前已校验 `execution.rs:134-141`，且携带源 max wal_highwater `execution.rs:252-256`，WAL 前缀回收安全性不变）。

**Tech Stack:** Rust（tsink @ `E:\Rs_Codes\tsink_fork`，分支 **master**，基线 `6a76597`，lib 573 绿；本轮起直接在 master 上做修复 commit——N8 已在 master，修复即补丁）。

---

## 〇、诊断结论（执行者必读，勿重新推导）

- **报错链**：`runtime.rs:348/375-383`（worker="flush"）→ `runtime.rs:523→531 background_flush_pipeline_once` → `flush.rs:632` → **`flush.rs:350-378 verify_flush_segment_publication`，失败在 `:356 load_segment_index(root)`**，rollback+`return Err` 在 `:359-366` → 错误来自 `loader.rs:646-651 map_required_segment_file_error`（段根 `symlink_metadata` NotFound，code 3）。
- **删除方**：compaction 替换 `execution.rs:144 remove_dir_if_exists(source)`，在 `:421 segment_replacement_guard()` 临界区内（仅持该锁）。
- **竞态窗口**：published root 的磁盘暴露期 = flush 写出（`flush.rs:224`）→ verify 回读（`:356`），期间无互斥。c7c4016 的 known-dirty diff 只覆盖"已进 catalog 的段"，当前 pass 的 staged 根 publish 前对 diff 不可见——对账没机会介入。
- **暴露源**：R3 直通（`05377a8`）是决定性放大器——pass 变快使 L0 被排空至 trigger 以下，flush 在写的分片成为下拍 survey 的"最老候选"；A/B 对照实证（r4b=T1+T2 跑 2h05m 未触发；r5=+R3 于 T+90min 必中）。
- **持久性**：过夜重启实验——完整 WAL 回放恢复后同签名复发（20 条），证明设计缺口非状态损伤。
- **可复用先例**：`is_not_found_error(&err)`（`validation.rs:161`）；`handle_segment_dir_load_error`（`validation.rs:126-159`，NotFound→WARN 跳过/损坏才按 policy 失败）；`load_segment_for_compaction`（`loader.rs:64-77`）同语义。
- **确定性测试注入点**：`invoke_persist_post_publish_hook(&published_segment_roots)`（**`flush.rs:330-331`，cfg(test)**，恰在 stage 完成与 verify `:632` 之间触发）。
- **归因空窗**：compaction 侧删除（drain 内）先于 diff merge（`runtime.rs:323-327`，drain 后）——修复靠置脏后 persisted-refresh worker 的 inventory 扫描兜底（`catalog_refresh.rs:155-163`，该路径已容忍消失），**不重排 compaction 侧时序**（重排会引入反向窗口）。

## Task 1: verify 容忍语义（主修复）

**Files:**
- Modify: `src/engine/lifecycle/flush.rs:350-378`（verify_flush_segment_publication 错误分流）
- Test: `src/engine/tests/persistence_segments.rs`（新增 N8 回归组；注入 hook 已存在于 flush.rs:330）

**Step 1: 写红**——三条测试（诊断报告第六节变体）：
1. `flush_pipeline_tolerates_published_root_replaced_before_verify`（变体A）：hook 删首个发布根 + 同时向 `pending_persisted_segment_diff` merge 记录该根 removed → 现状 `flush_pipeline_once()` 返回 Err(NotFound)（红）；修复后 Ok + 水位推进 + sealed evict + select 点数不丢 + WARN 含跳过根。
2. `flush_pipeline_marks_dirty_when_replacement_precedes_diff_merge`（变体B）：hook 只删根不 merge diff → 修复后 Ok 且 `persist_index_dirty` 终态 true（对账排队）。
3. `flush_pipeline_still_fails_when_verify_finds_corruption`（变体C）：hook 删根内 manifest.bin 留目录 → 修复后仍 Err（DataCorruption，不得被容忍语义吞掉）。
**Step 2: 跑红**——`cargo test -j 2 -p tsink --lib flush_pipeline_tolerates|marks_dirty|still_fails` 记录红（1/2 号 FAIL 于 Err(NotFound)；3 号现状即 Err 但断言的是"修复后仍 Err"，作为防退化护栏与 1/2 同批提交）。
**Step 3: 实现**——verify 循环内：
```text
match load_segment_index(root) {
    Err(err) if is_not_found_error(&err) => {
        // 复查必须在替换锁内：删除只发生在 segment_replacement_guard 临界区
        let truly_gone = { let _g = segment_replacement_guard(); !root.exists() };
        if truly_gone {
            warn!(root = ?root, "published root replaced by compaction before verify; \
                   persistence responsibility transferred to replacement outputs");
            skipped.push(root.clone());          // 不进 recovery metadata/install 集
            publish_ctx_replacement_happened.store(true, Release);  // 复用 stage-3 的 known-dirty
                                                                   // apply/置脏路径（flush.rs:397 同款）
            continue;
        }
        // 锁下仍在 = 极端交错（删除发生在复查后）→ 重读一次，仍 NotFound 才按损坏处理
        return retry_or_fail(...);
    }
    Err(err) => { /* 现状 rollback + return Err */ }
    Ok(idx) => { /* 现状 */ }
}
```
跳过根从 `published_segment_roots` 集合剔除后流入后续 stage（recovery metadata `:633` 天然不含）。具体变量名/结构以现场代码为准（先读 flush.rs:320-380 现状再动手）。
**Step 4: 跑绿**——三条新测试 + `cargo test -j 2 -p tsink --lib` 全量 576+ 绿（重点 persistence_segments/persistence_background/persistence_recovery 不碎）。
**Step 5: 门禁**——cargo fmt + `cargo clippy --all-features -- -D warnings` 零警告。
**Step 6: Commit**——`fix(flush): N8——verify 对 compaction 并发替换容忍：NotFound 持替换锁复查后跳过该发布根（持久化责任已转移给替换输出,水位/evict/WAL 回收照常）+置脏对账；真损坏仍 rollback；R3 直通使 L0 排空致在写分片入选 compaction 窗,T+90min 必中(soak24/r5 双实证,过夜重启复发=设计缺口)`

## Task 2: 观测计数（小项）

`flush` counters 加 `replaced_published_roots_total`（rs-iot 侧 metrics 接线登记后续）；测试 1 补断言计数 ≥1。commit：`feat(flush): N8 观测——replaced_published_roots_total 计数进 flush counters`。

## Task 3: 全量门禁

`cargo fmt --check` + `cargo clippy --all-features -- -D warnings` + `cargo test -j 2`（全 target，基线 1309+3 新增）。

## Task 4: 双重复验（主会话编排）

- **4a 保留现场恢复验证**（最硬的端到端证据）：在 N8 故障数据目录（`bench/history-100k/data/history`，含打满 8GB WAL 与 20+ NotFound 现场）上用修复后二进制启动——预期：回放完成就绪后 **flush worker 不再 NotFound**、written 恢复、WAL 前缀回收重启回落、errors 不再增长。跑 30 分钟收证后停服清场。
- **4b 全新 2h bench**（越过 T+90min 锚点）：判据——T+120min 无 flush worker ERROR、written ≥60K、WAL 有界、L0 有界、errors=0；close 行为记录。

## Task 5: 合并推送 + soak 第 4 次

修复 commit 推送 master（metme+origin）；rs-iot 重建；24h soak 第 4 次启动（门禁 ⑦）。rss 界定/N7/丢占比的观察目标沿用 §五-G/H。

---

## 登记后续（不因 N8 修复消项）

- **N9（2026-09-02 4a 实测新登记）**：回放"已打满 8GB WAL"时 commit 非线性冲 38.27GB 致系统虚拟内存耗尽死亡（System 事件 2004 实锤，启动 52s 内；对照：昨夜旧二进制回放 7.4GB WAL 峰值仅 ~13.7GB——非线性放大待查）。触发条件=N8 造成的 WAL 打满，N8 修复后正常运行不会进入该场景——列纵深防御项；故障数据目录现场保留供排查。
- P3 水位粗判消维护拍全量扫 / 方案 C 两头免 zstd / N7 close 排空改进 / rs-iot HistoryConfig 接 tsink 旋钮与 flush 计数 / 大 WAL 回放提速（恢复时长∝WAL 体量，与 N9 同族）。

## 风险与回滚

- 容忍语义过宽（把真损坏当 NotFound 吞掉）：变体 C 护栏 + `is_not_found_error` 判据限定 Os code 3/NotFound kind；复查持替换锁且 `root.exists()` 双重确认。
- 复查后仍存在的极端交错：重读一次仍 NotFound 才进损坏路径（不无限重试）。
- 跳过根若实际未被替换（误判）：持替换锁复查排除了该可能（删除只在该锁内）。
- 单 commit 可 revert；修复不影响慢路径（compaction 不替换时行为与现状逐字节一致）。
