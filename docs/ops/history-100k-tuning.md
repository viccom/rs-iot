# 历史存储部署调参指南（10 万点/秒量级）

> **状态**：reference（10 万点位调优实测记录，长期参考）

> 基于 2026-08-29/30 压测与修复（T1-T21、N1-N4）的实测结论。适用 `--features history` 构建。
> 实测基线：100,001 series（1000 设备 × 100 tag + 1 对照）、100K pts/s 持续写入、10 分钟窗口（T21b）。

## 一、推荐配置（10 万点/秒量级模板）

```toml
[history]
enabled = true
data_dir = "<独立数据目录>"
memory_limit_mb = 2048          # 预算口径见 §二，不再需要按总内存调大
cardinality_limit = 0           # 0=自动（白名单 ×1.2）；白名单运行期扩张会自动只放大重校准（T19）
retention_days = 1              # 按业务需要；磁盘量见 §四
wal_sync_ms = 500               # Periodic 档：kill -9 最多丢 500ms 内未 sync 尾部
wal_size_limit_mb = 8192        # ≥2× 稳态 WAL 水位（N4 修复后 WAL 有界锯齿，见 §五）
channel_capacity = 262144       # 4 writer 下每分片 65536
batch_max_rows = 16384
close_timeout_secs = 120         # 大积压规模长跑建议 ≥120（见 §五"优雅关停"）；短窗/小部署 30 够
writer_threads = 4              # T14：series 亲和分片；默认 1，10 万点建议 4
chunk_points = 512              # T9/T21：加速 seal，降低 active 内存与预估
partition_secs = 0              # 默认 1h，一般无需调
```

关键实测对比（同负载 100K pts/s）：

| 配置 | 稳态 written | 丢失 | 说明 |
|---|---|---|---|
| 全默认（单 writer、chunk 2048） | 7.5K pts/s | 92% | 修复前基线 |
| 修复后默认（单 writer） | 62-74K | ~30% | T1-T13 效果 |
| `writer_threads=4` | 95-104K | 稳态≈0 | T14 |
| `writer_threads=4 + chunk_points=512` | 96-104K | 稳态 0（仅启动 ~15s 爬坡丢） | T21 终局 |

## 二、内存规划（重点：预算口径 ≠ 进程内存）

`memory_limit_mb` 的预算口径是 **active head + sealed 未刷盘**（T7 修正，对齐文档）；实测 10 万 series 下 used 仅 ~38MB——**不要按总内存调这个值**。

真正要预留的是**进程 RSS**：实测稳态 **2.6-3.4GB**（registry 10 万 series 字典、mmap 段映射、identity 缓存等"预算外"分量，`rs_iot_history_memory_*_bytes` family 可逐项观测）。部署建议：**10 万 series 预留 4GB RSS**。

降 RSS 的手段（按效果排序）：`chunk_points` 调小（256 可再降 active，代价 chunk 更碎）、白名单瘦身（registry 随 series 数线性）、retention 缩短（mmap 随段数据量）。

## 三、观测与判读

- **sysstatus**：`history.health` 三态（`ok` / `degrading`：dropped 累计占比 >0.5% 或 errors>0 / `dropping`：>5%；两阈值为 `[history].health_degrading_ratio`/`health_dropping_ratio` 默认值，可配——小部署可收紧 degrading 提前降档预警，Task B/#4）+ `degraded`（引擎 Closed 锁存，配一次性 ERROR 日志与 `rsiot.history_closed` error 级内建告警）。
- **selfMonitor**（devid=rsiot，每 5s 入 rtdata）：`history_dropped_total / history_errors_total / history_written_total` 可直接图表化。
- **/metrics 关键 family**：`rs_iot_history_*`（sent/written/dropped/errors 分桶/series/内存 5 分量/admission 预估与缺口）；tsink 侧 `prefix_reclaims_total` 持续增长、`prefix_reclaim_errors_total` 恒 0 为 WAL 回收健康。
- **内建告警**（T18）：dropped 速率 >10/s 且窗口占比 >1% 时 fire `rsiot.history_drop`（warn 级，同活动期去抖，人工确认清除后仍超阈会重新告警）。阈值为 `[history].alarm_drop_rate_per_sec`/`alarm_drop_ratio` 默认值，可配（Task B/#4）——小部署（50/100 pts/s 级）建议 `alarm_drop_rate_per_sec` 收到 5 以下，否则丢一半也够不着速率门槛。
- **admission 拒批判读**：`rs_iot_history_admission_last_estimate_bytes` 大而 `memory_active_sealed_bytes` 小 = 预估虚高（理论已修，若再现报障）；active_sealed/registry 逼近预算 = 真用满。

## 四、容量数学

- **磁盘（段数据）**：实测 **9.6 B/点**（T21，chunk_points=512 口径）→ 10 万点/秒 ≈ **8.3GB/天**、~250GB/月（retention 按需治理）。
- **WAL**：N4 修复后有界锯齿（实测 50-170MB @100K pts/s），`wal_size_limit_mb` 给到稳态峰值 10×（8GB）即高枕；触顶会 wal_disk 桶拒批（fail-soft 丢批），health 会转 degrading。
- **cardinality**：自动档 = 白名单数 ×1.2；运行期扩白名单自动放大（T19），**缩小不缩限**（白名单缩减由 fan_out 过滤天然生效）。显式配置 `cardinality_limit>0` 时不参与热校准（配置主权）。

## 五、行为特征与已知边界

- **冷启动爬坡**：启动后 ~15 秒内 writer 排空未满速，此间有少量 dropped（T21b 实测 4.2M，全程占比 ~7%）；属冷启动特征，稳态零丢。接受不了再立项优化（identity cache 预热）。
- **kill -9 恢复**：N1/N4 修复后**一次重启成功**，恢复时间 ≈ WAL 稳态水位回放（实测 ~25 秒 @百MB 级 WAL）；数据零丢失（Periodic 500ms 档最多丢强杀前 500ms 尾部）。
- **优雅关停**：close 排空有看门狗超时（`close_timeout_secs`，默认 30s）。短窗健康引擎实测 80ms 完成；但**大积压规模长跑会超时退化**——第 3 轮复验（N7，6h @100K pts/s）30s 超时后放弃等待、部分 writer 线程脱落，退 WAL 回放兜底（数据安全，代价是下次启动多回放 ~1GB WAL、恢复变慢）。**长跑/大积压部署建议 `close_timeout_secs ≥ 120`**；tsink 侧 drain-then-close 改进登记为后续小项。
- **已知边界**（低风险登记）：blob lane 启用但永无 blob 数据的部署，重启仍全量回放（tsink floor=0 保守口径）；`/metrics` 在 10 万 series 下 0.3s（T4 O(1) 化后正常）。
- **部署顺序**：建模（devmodel/devreg/inst）与启动顺序已无约束（T19 热校准兜底）；历史数据目录需独占（.tsink.lock 进程锁）。

## 六、相关文档

- 实施计划与门禁对照：`docs/plans/2026-08-29-history-100k-improvement-plan.md`
- 配置键全清单：`docs/reference/rest-api.md` §4.1
- tsink 引擎侧调参：`../tsink_fork/docs/performance-tuning.md`
- 压测工件（脚本+指标）：`bench/history-100k/`
