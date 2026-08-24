# 历史时序存储（tsink 内嵌）总设计与路线图

> **状态**：accepted（Phase 1a 已实施；Phase 1b 待启动）
> **日期**：2026-08-24
> **决策依据**：[2026-08-24-history-storage-tsink-selection.md](../decisions/2026-08-24-history-storage-tsink-selection.md)（选型与定位，含实测数据）
> **前置已完成**：Phase 1a Tag Registry（**注**：其 tags 表将在设备模型注册表 M-4 退役，由 devreg.overrides 吸收——见 [2026-08-25-device-model-registry-plan.md](2026-08-25-device-model-registry-plan.md) §三处置清单）（[2026-08-24-tag-registry-plan.md](2026-08-24-tag-registry-plan.md)，merge bd8e027）——lux `tags` 表、`TagProps{history,deadband,desc,unit,owner_inst}`、`tag_key()` 单一 id 源、三路径登记（配置声明式 `ChildSpec.props` / pipeline 出口 `note_once` / API 人工）、`GET /api/v1/tags`+`tag/set|batch|delete`。**本文是 ADR 权威版本**（tag-registry 计划附录的 D 表摘引自本文，以本文为准）。

**Goal:** rs-iot 获得有界资源、类型保真、标签属性驱动的嵌入式历史时序存储，支撑单机 10 万点位级轻量边缘 SCADA 定位；订阅读侧升级 KSUB 驱动；供应链自控（viccom/tsink fork）。

**非目标**：不动 pipeline/dispatcher/write_batch/lux rtdata 数据模型（50 万点/秒写侧架构冻结）；不做 HMI 组态/热备冗余/集群（tsink 集群留给中心侧）；不承诺 PromQL range 查询首期可用（fork 优化后再放开）。

---

## 一、三层开关模型（现场裁剪的机制骨架）

```
L1 编译期  Cargo feature: history = ["dep:tsink"]     （默认 off；--no-default-features 裁剪惯例）
L2 部署期  config.toml [history] enabled = false       （编译进了，现场决定开；off 时不初始化引擎、不获取 .tsink.lock、/api/v1/history 返 503）
L3 标签期  TagProps.history = false                    （开了历史，逐标签白名单归档；默认零归档）
```

L3 的批量操作面 = 已交付的 `POST /api/v1/tag/batch`（glob）。归档运行时过滤只认 lux `tags` 表（无 include/exclude 配置项）。

## 二、架构（写/读/订阅三路径）

```
写（旁路，主链路零阻塞）：
  pipeline worker flush_batch（50ms 攒批，类型化 Sample）
    → 查 TagProps 缓存（history=true 过滤 + deadband 逐 series 判定，保 last_archived）
    → 有界 channel（try_send 满即丢+计数）
    → 单写线程（8 MiB 栈，规避 tsink issue #17）聚合大批次 insert_rows
    → tsink Storage（WAL Periodic(500ms) 默认；PerAppend 严格档可选）

读：
  GET /api/v1/history?dev_id=&tag=&start=&end=&agg=&interval=&limit=
    → tsink SeriesSelection + select 快路径（实测 p50 0.08ms）+ Downsample
  （P2 可选：核心库 promql::Evaluator 暴露 /api/v1/query instant——range 待 fork 优化）

订阅升级（rs-iot 自身读侧扩展性，与 tsink 无关）：
  lux KSUB "rtdata:*"（EmbeddedClient::ksubscribe 公开 API，无订阅者零开销）
    → 进程内共享 dirty-set 服务 → subscribe producer 精准读（替代每连接每 tick 全量 HGETALL）
    → SSE 语义/参数完全冻结（snapshot/datachange/interval 不变）；sub_impl="poll" 过渡开关
```

## 三、ADR 全表（Phase 1b+ 实施依据，执行不再议）

| # | 决策 | 要点/依据 |
|---|---|---|
| D1 | 形态=内嵌核心库 | 决策文档 §2.2 实测对比（写 2.2×/读快 1-4 个数量级/类型保真/零监听面） |
| D2 | 依赖=同级 path 依赖 `../tsink_fork`（fork 本地克隆 `E:\Rs_Codes\tsink_fork`，remote=github.com/viccom/tsink），pin 基线 tag | 对齐 lux path 先例与 CI 双 checkout 模式；实施前先打基线 tag |
| D3 | 写路径=flush_batch 单点 fan-out → 有界 channel → 单写线程（8 MiB 栈） | tsink 并发实测：单写×2000 点批 182K pts/s 最优；50 并发小批 39K 最差（WAL 锁护航）——"少写者大批次"是该引擎的正确用法 |
| D4 | 类型化写，取 pipeline 内存 `SampleValue` | 不走 lux 读回（那边已字符串化）；映射 Float→F64/Int→I64/Bool→Bool/Str→String；同 series 混型被 tsink 拒绝（有测试），写入侧自律 |
| D5 | 映射：metric=tag（sanitize），labels=[devid,hwid]，时间戳 ms（`ts*1000`） | 与 rtdata field（tag_key）同源；ms 为高频源留余量 |
| D6 | 资源全部显式有界：memory_limit（默认 256MiB）/cardinality_limit（按 tags 表 history=true 计数×1.2 自动校准，可覆盖）/retention（默认 30d）/wal_size_limit | tsink 默认全无上限（u64::MAX）——必须显式配置；预算超限行为=准入背压（延迟尖刺而非 OOM，实测引擎内存从不越预算） |
| D7 | 读 API=类型化 `/api/v1/history`（select 快路径） | PromQL range 有每步 ~50ms 结构性成本，不进首期承诺 |
| D8 | 订阅读侧=KSUB dirty-set（见 §二） | poll-diff 是万点级设计；10 万点位×N 订阅者读放大不可接受；KSUB 是 lux 原生能力 |
| D9 | 设备删除不级联删历史（工业惯例） | 提供显式清理端点；tag 删除语义同（Tag Registry 已实现） |
| D10 | 关停序列：停 pipeline 写入 → history close() 超时 15s 放弃（WAL 回放兜底）→ lux SAVE | close 排空实测 8-13s；WAL 就是为此设计的 |
| D11 | **元数据唯一来源（宪法级）**：设备/标签元数据唯一出生地=设备模型(devmodels)+设备注册表(devreg)，采集实现只消费不创建；tags 表被 devreg.overrides 吸收（M-4 退役）；历史归档消费 `effective()` = 模板默认∪覆盖 | 决策与四期实施计划：[2026-08-25-device-model-registry](../decisions/2026-08-25-device-model-registry.md) + [plan](2026-08-25-device-model-registry-plan.md)；Phase 1b 的白名单/死区来源随之从 tags 表切换为 effective() 接口（M-1 起稳定） |

## 四、tsink 语义陷阱（集成必读，实测发现）

1. **`.tsink.lock` 进程锁**：一个数据目录永远只允许一个进程持有（1s 获取超时）。内嵌 rs-iot 与 tsink-server 不能共享目录；形态切换=离线交接。L2 enabled=false 时不初始化引擎即不获锁。
2. **未来时间戳有界可见性**：超出墙钟 +15 分钟（`DEFAULT_FUTURE_SKEW_ALLOWANCE`，不可配置）的时间戳被持久化但暂不可查询（防坏时钟设计）。设备时钟漂移大的现场需在写入侧钳制或知情接受"数据迟到"。
3. **PromQL range 每步成本**：~50ms/步线性（step=15s 21 步=1072ms；instant 1.6ms、series 元数据 1ms 均快）——fork 优化第一候选（单次扫描+逐步插值）。
4. **Windows debug 构建主线程栈溢出**（上游 issue #17，1MiB 栈）：D3 的 8MiB 线程同时是规避手段；本机 debug 复现失败（疑 rustc 版本相关）。
5. 内存记账为引擎内部口径；进程 WS 会更高（mmap 页缓存+线程栈+批缓冲），监控看 WS 而预算看引擎口径。

## 五、Phase 路线图与验收门禁

| Phase | 内容 | 状态/门禁 |
|---|---|---|
| 1a | Tag Registry（标签/设备元数据统一存储地基） | **已实施**（bd8e027；508 单测+6 e2e；A/B bench 出口登记开销 -8%，≥50 万门槛 4.5× 余量；优化项登记：per-worker 本地 seen+代际失效、布隆预过滤） |
| 1b | 历史写路径：L1/L2/L3 三层开关骨架（feature/配置/门禁 e2e）→ tsink path 依赖接线 → props+deadband 过滤 → 单写线程 → e2e（白名单外零写入/死区生效/kill 恢复零丢失/主链路劣化 <5%） | 待启动；另立 plan 引用本文 ADR |
| **M-1~M-4** | **设备模型注册表（物模型）**：devmodels/devreg/discoveries 三表 + tags 退役 + lua/sidecar v2 + 前端三页——元数据唯一来源原则落地，属业务/元数据面重构（数据面冻结） | **accepted 待启动**；[决策](../decisions/2026-08-25-device-model-registry.md) + [实施计划](2026-08-25-device-model-registry-plan.md)（四期 TDD）。**与 1b 的关系**：1b 消费 `effective()`（M-1 T8 稳定后任意点可切入 1b；若 1b 先行，其 props 消费接口预留从 tags 表切换为 effective() 的单点替换） |
| 2 | `/api/v1/history` 读路径 + KSUB dirty-set 订阅升级（双 impl 过渡，行为等价 e2e） | 待启动；可与 1b 部分并行 |
| 3 | 治理运维：`rs_iot_history_*` metrics（archived_tags/deadband 削减率/写错误/内存水位）+ 关停序列 D10 + 前端标签管理页 | 待启动 |
| 4 | 10 万点位 PoC 大压测 + ≥24h soak：10 万 series 混合周期（1s/3s/5s）、100%/20% 两档归档、写+订阅+读三线同场 | 门禁：主链路劣化 <5%；历史写 ≥100K pts/s（100% 归档档，本机）；soak 内存无单调增长/磁盘 ≈8-9B/点±20%；kill -9 零丢失；deadband 实测削减率 ≥50%（缓变负载）；series 数 === history=true 计数 |
| 5 | 演进：PromQL instant 端点 → range（等 fork 优化）→ rollup+北向增量 → 中心侧 tsink-server 聚合拓扑（边缘内嵌/中心独立的分场景形态，PromRW/LP 上送+断连缓存） | backlog |

**磁盘治理数学**（10 万点位全量 1Hz）：~8-9 B/点 ≈ 70GB/天，30 天 ≈ 2TB（不可接受）→ 20% 归档+deadband(1%) ≈ 3-7GB/天，30 天 100-200GB（1TB SSD 从容）。这就是 L3 白名单默认关 + deadband 的定量化理由。

## 六、风险登记

| 风险 | 缓解 | 回退 |
|---|---|---|
| 引擎进程内失败（记账失真/死锁） | 旁路 fail-soft+有界 channel+写超时；1b 故障注入测试 | `--no-default-features` 去 history 回现状 |
| 边缘硬件吞吐临界（开发机 182K，4 核折算 60-90K） | 大批次+Periodic WAL+归档过滤 | 降归档比例/点位规模，不降主链路 |
| tsink 0.10 数据格式演进 | fork tag 基线+数据目录版本标注+迁移工具（backlog） | 目录隔离，历史只读保留 |
| KSUB 改造回归 | 双 impl 过渡开关+行为等价 e2e | `sub_impl="poll"` 一键切回 |
| 订阅读侧 10 万级未验证 | Phase 4 三线同场压测 | — |

## 七、fork backlog（viccom/tsink，按优先级）

1. PromQL range 求值优化（每步 ~50ms → 单次扫描+插值）
2. Windows debug 引导路径栈深 root-cause（issue #17）
3. 数据格式跨版本迁移工具
4. 72h soak 基线（压实堆积/WAL 膨胀/mmap 稳态）
5. （lux 侧联动，非 fork）KSUB field 级事件增强（现 key 级，HSET 后仍需 HGETALL 该设备）

## 附录：实测基线（2026-08-24，Ryzen 9 5900HX/63GB/NVMe/Win10；真实边缘盒按比例折减，相对关系可迁移）

**tsink 内嵌（tsink-bench，重启零丢失全轮验证，磁盘 7.7-8.9 B/点）**：单写×2000 点批 **182,292 pts/s**（p50 9.0ms）/ 4 写×500 点 143,598 / 16 写×200 点 83,028 / 50 写×200 点 38,404（p50 264ms，WAL 锁护航）/ 50 写 PerAppend 24,802（p50 410ms）/ mixed 50 写+读：读 p50 0.08ms；进程 WS 峰：单写 ~390MB，50 客户端 ~1.0GB；CPU 全程 ≤13%（16 核占比）；close 排空 8-13s。
**tsink-server HTTP（tsink-http-bench）**：LP 写 82,540 pts/s（p50 22ms）；PromQL instant 1.6ms / range 21 步 1072ms / 6 步 267ms；二进制 17.4MB。
**rs-iot 管道（既有）**：端到端 bench mean 2.25-2.45M pts/s（A/B 3×3 交替）；lux 写 1.65M；设计门槛 500K。
**测试可信度**：tsink 核心库 509 单测本机全过；README 示例 debug 跑通；上游 CI 双 OS+性能回归门禁。
**工件位置**：`E:\Rs_Codes\tsink`（源码）、`E:\Rs_Codes\tsink-bench`（run_*.log+采样 CSV）、`E:\Rs_Codes\tsink-http-bench`（h1-h3.log）。
