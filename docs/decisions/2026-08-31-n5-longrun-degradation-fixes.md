# 2026-08-31 N5 长跑退化修复的执行决策记录

> **状态**：implemented（2026-08-31 裁决并落地；长跑退化修复批）
> **实施**：2026-08-31 裁决并落地（长跑退化 n5 修复批）
> **代码**：见正文各条锚点（采集/管道/告警域）

## D-N5-1｜锁解耦放大既存竞态的自愈加固（采纳）

- **冲突/风险**：`c7c4016`（compaction_gate 临界区缩至 marker）改变了锁时序，放大 reliability 测试 `flush_pipeline_tolerates_concurrent_compaction_replacement_race` 中既存的同进程交接竞态（旧引擎 zombie 压缩线程 vs 新引擎 bootstrap 扫描），裸奔约 40% 失败率。
- **裁决**：随 `c7c4016` 并入三项自愈加固——(i) Drop 对非 OPEN 生命周期也 notify+join 后台线程；(ii) compaction 输出段 id 撞已存在目录时换 id 重试 ≤3 次（撞 id 硬失败会把后续 pass 永久楔死）；(iii) 启动 quarantine 遇并发消失段根跳过（对齐 N1 消失容忍）。120 连续运行全绿验证。
- **理由**：真实 kill -9（跨进程）本无 zombie 写者，竞态仅同进程交接存在；但加固对生产无害且自愈，改动面小。
- **风险**：无已知；测试时长成本略增。

## D-N5-2｜compaction 选窗的 payload 损坏语义放宽（采纳）

- **冲突**：`ca60c8b` 选窗只读 manifest 后，未入选窗口的段若 payload 损坏（manifest 完好）不再使整 pass 失败——旧行为（整级全量加载）会撞上它并 fail。
- **裁决**：接受放宽。窗口内段的损坏校验语义严格保留（`compaction_fails_when_any_source_segment_is_corrupted` 仍绿）；manifest 损坏与目录-级别错配的错误路径逐字保留。
- **理由**：未入选段的损坏本就不应阻塞本次 compaction（下拍选中时仍会报错）；旧行为是全量加载的实现副作用而非设计意图。

## D-N5-3｜tiering 场景残留风险（登记不做）

- **风险**：tiering 配置下 sweep 的 tier-move（rename 热段）与用户线程 `Storage::flush()` 并发时，catalog 构建可能瞬时撞 rename 中段根（非 NotFound，一次 persist 失败下拍自愈）。
- **裁决**：登记不做。rs-iot 场景未开 tiering；如未来启用需先评估此处。

## D-N5-4｜compaction 输出段点预算不暴露公共旋钮（采纳）

- **裁决**：`8bff576` 的输出段点预算（1,048,576 点独立常量）仅 crate 内部字段 + `#[cfg(test)]` 注入，不暴露 StorageBuilder 公共 API。
- **理由**：rs-iot 侧无调参需求，最小面；如后续需要再升级为 builder 旋钮。

## D-N5-5｜WAL 上限缺省值与压测配置的差异澄清（澄清）

- **背景**：剖析时发现 rs-iot `src/config.rs` 的 `wal_size_limit_mb` 缺省 1024，与事故现场 8GB 不符。
- **澄清**：无矛盾——事故 soak 用 `bench/history-100k/config.toml` 显式配 8192；代码缺省值未改。10 万点部署请按 `docs/ops/history-100k-tuning.md` 配置。
