# 决策：历史时序存储选型 tsink（内嵌形态）与单机 10 万点位级定位

> **状态**：accepted
> **日期**：2026-08-24
> **决策**：rs-iot 的历史时序存储选用 **tsink 核心库（内嵌形态，fork 自管）**，不选独立 tsink-server、不选外部时序库、暂不启用 lux 原生时序；产品定位目标为**单机 10 万点位级轻量边缘 SCADA/遥测平台**（纯 WEB + 功能组件独立二进制）。总设计见 [2026-08-24-history-storage-master-plan.md](../designs/2026-08-24-history-storage-master-plan.md)；前置地基 Tag Registry 已实施（[2026-08-24-tag-registry-plan.md](../designs/2026-08-24-tag-registry-plan.md)，merge bd8e027）。

## 1. 背景与需求

rs-iot 是纯最新值缓存模型（lux `rtdata:<devid>` HSET 覆盖语义），**零历史存储**——v2 路线图（README）预留"历史时序存储"。需求定调：

- 边缘单机形态：单二进制单进程（rs-iot 身份核心），新增存储不得破坏"单服务"部署叙事
- 资源有界：边缘盒子内存/磁盘受控，超限须背压而非 OOM
- 类型保真：`SampleValue{Float,Int,Bool,Str}` 全类型入历史（rtdata 缓存已字符串化是既有缺陷，历史侧不得延续）
- Windows 一等公民（现场 Windows 边缘机）+ 主流多平台（Linux x64/ARM64 为中心侧演进预留）
- 容量目标：10 万点位、秒级采集（最坏 100K pts/s 持续入流）

## 2. 候选与评估结论（2026-08-24 实测，环境：Ryzen 9 5900HX 8C16T/63GB/NVMe/Win10，数字为上限参考）

### 2.1 tsink 是什么

Rust 轻量时序库 v0.10.2（MIT，~17 万行：核心 80k + server 85k），WAL（分段/三段高水位/损坏段隔离）+ LSM 式分层压实 + 自适应编码（delta-of-delta/Gorilla XOR/zstd）+ mmap 读 + 内存预算准入背压 + 基数上限 + 热温冷分层 + rollup。CI 双 OS 门禁 + BPP/性能回归门禁。多平台发布矩阵：Linux x64/ARM64、Windows x64/ARM64、macOS ARM64。

**供应链风险与对策**：上游 cantrepro/tsink 提交历史被整体重写（155 commit 挤在 4 秒内）、单一维护者、疑似重度 AI 辅助开发——**已 fork 至 viccom/tsink 自管**，升级节奏/格式迁移/缺陷修复自主可控。

### 2.2 形态对比（内嵌核心库 vs 独立 tsink-server），同负载实测

| 维度 | 内嵌核心库 | 独立 tsink-server |
|---|---|---|
| 写吞吐（1 写者×2000 点批） | **182K pts/s，p50 9ms** | 82.5K pts/s，p50 22ms（HTTP+LP 解析） |
| 原生读（select API 5min 窗口） | **p50 0.08ms** | 无对应端点 |
| PromQL instant / range | ~1.6ms / 同引擎 | 1.6ms / **~50ms 每步**（结构性成本，随步数线性） |
| 类型保真 | **全类型**（F64/I64/U64/Bool/String/Bytes/Histogram） | 查询面 PromQL/remote-read 均以 float 为中心 |
| 二进制/内存 | 5.1 MiB(strip) / WS 峰 ~390MB | 17.4MB / ~630MB |
| 新增监听/运维面 | **零** | +1 端口 +1 服务 +认证 +监控 |
| 崩溃隔离 | 弱（进程内，靠旁路+预算+fail-soft 兜底） | 强 |

**结论**：边缘侧选内嵌（性能/类型/单进程叙事全面占优；崩溃隔离劣势被"历史是旁路功能"的定位消解）；独立 server 的正确位置是**未来中心侧聚合**（多进程共享/Grafana 直连/独立扩缩）——两者不互斥，是分场景拓扑。

### 2.3 为什么不是 lux 原生时序 / 外部 TSDB

- lux 时序（TSADD/TSRANGE/聚合/retention）零新增依赖，但压实成熟度/内存治理/基数控制未经同等检验，且 rs-iot 需要的"正经历史库"能力（分层压实+预算准入+rollup）缺口大——列为备选与对照基线，PoC 若证明 lux 够用可降级替换（数据目录隔离，切换成本可控）
- TDengine/InfluxDB/IoTDB/TimescaleDB 等均独立进程形态，破坏单二进制叙事，边缘资源开销大——业界对标（IoTDB 树形建模/TDengine 超级表/PI exception-compression）用于设计借鉴而非选型

## 3. 定位：单机 10 万点位级轻量 SCADA

容量推演（最坏 100K pts/s 持续入流）：管道 2.6M 实测（基线 500K 设计门槛，26×余量）、lux 写 1.65M（16×）、**tsink 内嵌单写大批次 182K（1.8×，最薄环节）**。三项条件性短板与对策（详见总设计 §6 风险表）：历史链 PoC 验证 + 选择性归档（Tag Registry `history` 白名单，业界同构：Ignition per-tag History Enabled / PI 死区两级过滤）+ 读侧 KSUB 升级（poll-diff 是万点级设计）。SCADA 能力边界诚实声明：轻量分段（无 HMI 组态编辑器/热备冗余/认证驱动），对标水处理/小型能源站/产线级场景。

## 4. 关联文档

- 总设计（三层开关/ADR 全表/Phase 1b-5 路线/风险/实测基线附录）：[2026-08-24-history-storage-master-plan.md](../designs/2026-08-24-history-storage-master-plan.md)
- 已实施地基：[2026-08-24-tag-registry-plan.md](../designs/2026-08-24-tag-registry-plan.md)
- 基准工程（仓库外磁盘工件）：`E:\Rs_Codes\tsink`（源码 tarball）、`E:\Rs_Codes\tsink-bench`（内嵌压测+run_*.log 原始数据）、`E:\Rs_Codes\tsink-http-bench`（server 压测+h1-h3.log）、fork：https://github.com/viccom/tsink
