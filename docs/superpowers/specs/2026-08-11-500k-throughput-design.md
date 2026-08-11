# 50万点/秒采集入库性能改造设计

- **日期**: 2026-08-11
- **状态**: 设计已确认(brainstorming 4 节分节获批),待写实现计划
- **场景**: 电网区域中心,单 rs-iot 进程,南向 100 采集实例 × 5000 点/秒 = 50万点/秒入库 lux,北向 mqtt 发布最新值
- **相关代码**: `src/pipeline.rs` / `src/db/rtdata.rs` / `src/lib.rs` / `src/plugin/builtin/*_client/mod.rs` / `src/process_metrics.rs` / `src/config.rs`
- **相关文档**: `CLAUDE.md`(架构原则/验证规则)、`docs/lux-upstream-sync-strategy.md`

---

## 1. 背景与目标

电网区域中心场景:一个 rs-iot 进程部署在区域中心,南向 100 个采集实例分别连几十个下级配电站/变电所的 OPC UA / S7 / Modbus server,每实例每秒采集 5000 点,合计 **50 万点/秒** 入库 lux,北向 mqtt 发布最新值快照。

**目标**:在当前架构(依赖 lux 进程内库)上,以**低风险、优雅、分阶段**方式,稳定支撑单进程 50 万点/秒覆盖写入。核心约束——不动 lux、不动 Source trait、不动 Sample 结构、不破坏"单 Source 启停不影响管道"的解耦原则。

**现状瓶颈**:入库链 `pipeline` 单消费者 + `rtdata::write` 逐点串行 HSET,全挤 1 个 task + 1 个 `EmbeddedClient` session Mutex,实测上限约 **10-20 万点/秒**。lux 本身不是瓶颈(多 shard + 批量原语齐备)。

## 2. 需求约束(澄清结论)

| 维度 | 决策 | 影响 |
|---|---|---|
| 数据语义 | **仅最新值快照**(覆盖写) | 不引入时序存储/时序北向;HMSET 化即可 |
| 部署拓扑 | **区域中心单进程集中** | 单进程扛全部 50万/秒,最考验设计 |
| 采集周期 | **秒级(1-2s)为主** | period 配置层不动;每秒一个 50万点脉冲,重点在削峰 |
| 北向范围 | **mqtt 优化(OPC UA Server 后置)** | mqtt 最小改动,不引入增量 publish |

## 3. 现状瓶颈分析(代码证据)

数据流:
```
100 Source ─send().await─► hub mpsc(256) ─recv─► pipeline 单消费者 ─► rtdata::write ─► lux
```

瓶颈点:
- hub 容量 256(`lib.rs:188`)
- pipeline 单消费者单 task(`pipeline.rs:32` 一次 recv 一个 Sample)
- `rtdata::write` 逐点串行 HSET(`db/rtdata.rs:22-36` `for tag in values { c.execute("HSET").await }`)
- 全部走同一 `EmbeddedClient` session Mutex(`lux/src/lib.rs:2021`)→ 有效并发 = 1

**lux 侧(经 lux 源码调查确认,非瓶颈)**:
- `EmbeddedClient::clone`(`lux/src/lib.rs:2073`)= `Self::new(runtime.clone())` → 共享 store、**独立 session Mutex**,可并发
- `HSET`/`HMSET` 同分支接受多对(`lux/src/command.rs:1667`,`step_by(2)` 遍历)→ 现有 `execute("HSET", &[key, f, v])` 可直接扩多对
- `EmbeddedPipeline`/`PreparedPipeline` + `execute_embedded_pipeline_discard`(`lux/src/embedded.rs:1503`)纯写、不解析回复、最快;同 shard 连续写自动合并 shard-lock + WAL `append_commands` 批写
- lux 多 shard(`cpus×16` clamp[16,1024])并发,写只锁目标 shard
- WAL everysecond(fsync 后台 1s/次),瓶颈是同步 pagecache write 带宽,批量化摊销

## 4. 方案选择

三方案对比,选 **B(批量化 + 多 worker 分片)**:

| | A 批量化单链 | **B 批量化+多worker(选)** | C 去hub直写 |
|---|---|---|---|
| 并行来源 | 单核批量摊销 | 批量 + 多核并行 | 每 Source 独立写 |
| 风险 | 最低 | 低(数据面内部) | 高(破坏解耦) |
| 50万确定性 | 不确定(赌单核) | 高(线性扩展) | 理论最高 |
| 改动面 | rtdata+pipeline | +dispatcher+worker | Source trait + 去 hub |

选 B 理由:A 单核天花板不确定(赌批量化够不够),B 多 worker 给确定性且仍属数据面内部低风险改造,C 违背项目架构原则("单 Source 启停不影响管道"是生存约束)。

**分阶段实施策略**:bench 先行 → 阶段1 HMSET 化单 worker → bench → 够则止(YAGNI)/ 不够阶段2 多 worker。

## 5. 详细设计

### 5.1 总体架构与数据流

改后数据流:
```
100 Source ─send().await─► hub mpsc(512) ─► dispatcher ─hash(devid)%N─► N × worker
           (背压不变)                     (唯一 hub_rx 持有者)          │ 各持 EmbeddedClient::clone
                                                                    │ 攒批窗口 50ms
                                                                    │ write_batch → lux 多 shard
```

**不变性保证(低风险核心)**:
- `Source` trait 签名不动(仍 `tx: mpsc::Sender<Sample>`)
- `Sample` 结构不动
- hub 上游契约(`mpsc<Sample>` + send 背压)不变
- lux 零改动
- `CtlRegistry` / 北向 mqtt / REST / bridge 不受影响
- 单 Source 启停不影响管道(worker 是消费者侧 fan-out,与 Source 解耦)

**改动文件**:
- `src/pipeline.rs`(单消费者 → dispatcher + N worker;若改造后超 500 行,拆为 `pipeline/{mod,dispatcher,worker}.rs` 子模块,守 CLAUDE.md 文件卫生红线)
- `src/db/rtdata.rs`(逐点 `write` → `write_batch`)
- `src/lib.rs`(hub 256→512;`pipeline::run` 换新结构)
- `src/plugin/ticker.rs`(**新模块**:`jittered_ticker(inst_id, period)` helper,单一职责)
- 各采集插件 ticker 创建处(6 处:opcua/s7/modbus/datasim/lua/sidecar)各换 1 行调 `jittered_ticker`
- `src/process_metrics.rs`(worker 维度指标)
- `src/config.rs`(`[pipeline] workers` 配置项)

### 5.2 入库核心三件

**收益来源(按大小)**:
| 层 | 改法 | execute 次数(单 Sample 5000 点) |
|---|---|---|
| ① Sample 内多点合一 | `HSET key f1 v1 ... f5000 v5000`(多对) | 5000 → **1**(主收益) |
| ② 跨 devid 批提交 | `EmbeddedPipeline` discard,lux 同 shard 合并 WAL | 每 devid 一次 → 整批一次 |
| ③ 多 worker 并行 | N × `clone` 并发打不同 shard | 串行 → N 路并行 |

**组件 1:`db::rtdata::write_batch`(批量逻辑内聚,纯函数易测)**
```rust
pub async fn write_batch(c: &EmbeddedClient, samples: &[Sample]) -> Result<(), RsIotError>
// 1. 按 devid 聚合 HashMap<devid, Vec<(field, cell_json)>>;同 devid 跨 Sample 后入覆盖(BTreeMap insert 语义,HSET 覆盖一致)
// 2. local_time 批内取一次(对标现有 rtdata.rs:21 循环外取一次)
// 3. 构造 EmbeddedPipeline:每 devid 一条 raw("HSET", [key, f1,v1, ..., fN,vN])
// 4. c.execute_embedded_pipeline_discard(&pipe).await  ← 一次提交,lux 内部按 shard 合并 WAL
```
现有 `write()`(单 Sample 逐点)保留为兼容入口,内部调 `write_batch(&[sample])`,REST/测试存量调用不破坏。

**组件 2:pipeline dispatcher + N worker(替代单消费者)**
```
hub_rx ─[dispatcher task]─hash(devid)%N─► N × worker_tx(mpsc<Sample>)
每 worker task:
  select! {
    recv sample → buf.push(sample)
    interval(50ms) → 若 buf 非空 { rtdata::write_batch(&clone, &buf).await; buf.clear() }
  }   // 失败 warn + pipeline_write_fail 计数,不阻塞,不 propagate
```
- dispatcher 唯一 hub_rx 持有者,只 `hash(devid)%N` + `send().await`(背压),不碰 lux,不瓶颈(路由比 lux 写快 ~100×)
- 路由键 `devid`:保证同 device 多点进同 worker → `write_batch` 同 devid 聚合无跨 worker 竞争
- 攒批窗口 50ms:秒级采集下延迟不可感(SCADA 刷新 1-2s);worker 每 50ms flush,跨 Sample/devid 合一次 Pipeline 提交(收益②)

**组件 3:EmbeddedClient 池**
- 启动时 `handle.client()` `clone()` 出 N 份,每 worker 一份
- clone 独立 session Mutex(`lux/src/lib.rs:2073`)→ N worker 的 `execute_embedded_pipeline_discard` 真正并发,打到 lux 不同 shard
- 零额外开销:clone 共享 store,只建新 session 锁

**worker 数 N(配置项)**:
```toml
[pipeline]
workers = 0   # 0 = auto(min(cpu,8));阶段1 用 1
```

### 5.3 削峰与 mqtt

**削峰三层**:
- **主力**:worker 攒批窗口(5.2 已内置)——瞬时脉冲缓冲后批量处理,不逐点冲击 lux,无需额外代码
- **辅助**:采集 jitter——`jittered_ticker(inst_id, period)`,`hash(inst_id) % period_ms` 确定性相位错开 100 实例的 ticker;6 个采集插件各换 1 行调用。低成本、确定性、纯平滑
- **兜底**:hub 容量 256 → 512(Sample 含 5000 点 `BTreeMap` ≈ ~500KB,512 × ~500KB ≈ 250MB 峰值上限;**不盲目 4096**,那要 ~2GB 不现实)

**mqtt 北向(最小改动,精准修改)**:
现状 `publish_rtdata`(`mqtt_pub/mod.rs:94-139`):每 `publish_interval_secs`(默认 5s)`KEYS rtdata:*`(全扫)→ 100 × `HGETALL` → 100 条 PUBLISH。50万点下 key 数不涨(只涨 field),压力主要在 HGETALL 读 50万 field/周期 + JSON 序列化。

- `publish_interval_secs` 文档化(50万点建议 ≥2s,默认 5s 合理)
- `KEYS rtdata:*` → devid 集合(`SADD rtdata:devids` / `SMEMBERS`)列为 **bench 后可选优化**(pull 周期下 KEYS 开销小,默认不改)
- **不引入增量 publish**(只发变化 field):"最新值" + pull 模式下要对比上次值,复杂度高、收益不确定,YAGNI 后置

### 5.4 错误处理与降级

**铁律:worker / dispatcher 内部一切 lux 错误都 catch 降级,不 propagate 出 task。** 对标现有 `pipeline.rs:51-54`(写失败 `warn!` + `pipeline_write_fail()` 计数 + 不阻塞)。

| 故障点 | 降级行为 | 检测/观测 |
|---|---|---|
| `write_batch` 失败(lux execute 报错) | `warn!`(限流去重防爆)+ per-worker `pipeline_write_fail` 计数 + **清空 buf 继续下一窗口**(避免毒丸卡死;最新值语义下次采集覆盖,不重试) | `/metrics` `rs_iot_pipeline_write_fail_total` |
| 单 worker task 异常退出 | 理论不发生(内部全 catch);若 panic 退出 → 该 devid 分片停入库,其余 worker 不受影响 | dispatcher `send` 收 `SendError`(worker_tx closed)→ `error!` + `pipeline_workers_active` 降一;**不自动重启**(YAGNI,worker 无状态纯消费,运维侧进程监督兜底) |
| dispatcher 退出 | 致命(数据面断),逻辑极简(recv+hash+send)panic 概率极低 | 等价"hub 关闭"(`pipeline.rs:36`),`error!` 告警 |
| lux 整体不可用 | 所有 worker write 批批失败 → warn 限流;采集侧背压;lux 恢复后自愈(下次 write 成功) | `pipeline_write_fail` 激增 |

**背压链不变(不丢点)**:Source `send().await` → hub(512) → dispatcher `send().await` → worker_tx → worker。全链 `send().await` 背压,**无 `try_send` 丢点**(仅 `CtlRegistry` 控制命令才 `try_send` 丢弃,对标 Go default,数据面不涉及)。worker 慢 → 逐级背压回 Source → 采集拉慢,不丢点,语义与现状一致。

### 5.5 监控指标(新增,接入现有 M4 + /metrics)

- `rs_iot_pipeline_workers_active`(gauge:活跃 worker 数,退出即降)
- `rs_iot_pipeline_write_batch_size`(gauge/summary:平均批 Sample 数,验证攒批效果)
- 复用现有 `hub_depth[_max]`、`pipeline_processed_total`、`pipeline_write_fail_total`

### 5.6 测试与验证策略

**阶段 0 bench(数字驱动,零生产改动)** —— `benches/rtdata_write.rs`(criterion):

| 场景 | 测什么 | 决定 |
|---|---|---|
| A 逐点 HSET ×5000点 | baseline(现状) | 量化现状上限 |
| B HSET 多对 ×5000点 | 收益①(5000→1 次 execute) | 单 Sample 改造收益 |
| C `EmbeddedPipeline` discard ×100 devid | 收益②(WAL 批写) | 跨 devid 批收益 |
| D N × clone 并发 | 收益③(多 shard 并行) | 单 worker 吞吐 × N,定 N |

→ A/B/C/D 数字钉死"单 worker 批量化能上多少",决定阶段 1 后是否需要阶段 2 多 worker。

**单测**(纯函数,易测):
- `write_batch`:同 devid 多点 → 1 条 HSET 多对(pipeline 命令数 = distinct devid);跨 devid → 同 pipeline;同 devid 跨 Sample 后入覆盖(最新值);空批不提交;field 前缀逻辑(`rtdata.rs:23` `tag.contains(devid)`)保持。用 `tests/common::start_lux`(端口 0 + 临时 data_dir)
- `jittered_ticker`:同 inst_id 确定性同相位、不同 inst_id 散布 0..period
- `dispatcher` 路由:`hash(devid)%N` 正确、同 devid 恒进同 worker

**集成测试** —— 新 `tests/throughput_e2e.rs`:N 个 datasim Source × M 点 → hub → pipeline(workers) → lux,`HGETALL` 验证最新值正确 + 点数完整 + worker 路由正确 + 背压不丢点。复用 `tests/common`。

**回归**:现有 `tests/`(dataflow / api_flow / config_load / sidecar / modbus_multi_slave)全绿 + `cargo fmt/clippy/test --all-targets --all-features`(CI 门槛)。

**端到端(手动,CI 不跑)**:100 datasim × 5000 点 × 1Hz → `/metrics` 看 `pipeline_processed_total ≈ 50万/s` + `hub_depth` 稳态近 0 + 无 `write_fail`。

## 6. 分阶段实施路线

| 阶段 | 改动 | 验证门(达标即停) | 风险 | 回退 |
|---|---|---|---|---|
| **0 bench** | `benches/` 打 lux(A/B/C/D) | 产出单 worker 吞吐数字 + 定 N | 零(独立文件) | 删文件 |
| **1 HMSET化+单worker** | `write_batch` + pipeline 单 worker 调它 + hub 256→512 + jitter helper(6 插件) | bench:单 worker 够 50万/秒? | 低(2 文件 + helper) | `write_batch` 回退逐点 `write` |
| **1b bench** | 阶段 1 后实测 | 够 → **收手(YAGNI)** / 不够 → 阶段 2 | — | — |
| **2 多 worker** | dispatcher + N worker + EmbeddedClient 池 + `[pipeline] workers` 配置 | `/metrics` 50万稳态 + hub_depth 近 0 | 中(pipeline 结构) | `workers=1` 退回阶段 1 |
| **3 mqtt 文档化** | config 注释 `publish_interval` 容量指引 | mqtt 验证不回归 | 极低 | 文档 |
| (可选) mqtt devid 集合 | bench 后若 `KEYS` 瓶颈 | — | 低 | 保留 `KEYS` |

**每阶段独立验证门,达标即停**——阶段 1 可能就够(不赌,bench 拍板)。`workers=1` 配置永远可退回阶段 1 形态,无不可逆改动。

## 7. 不变性保证(低风险核心,汇总)

- **不动**:`Source` trait / `Sample` 结构 / hub 上游契约 / lux / `CtlRegistry` / 北向 mqtt / REST / bridge
- **改**:`pipeline.rs` / `db/rtdata.rs` / `lib.rs`(hub 容量 + pipeline 结构) / 采集 ticker(jitter) / `process_metrics` / `config`

## 8. 风险与回退

- **单核批量化不够 50万** → 阶段 2 多 worker 兜底(`workers` 配置可调)
- **worker 路由不均**(devid hash 倾斜)→ N 足够大 + hash 函数均匀;监控 `write_batch_size`
- **jitter 引入采集相位偏移** → 秒级采集下 <1s 偏移不可感
- **lux `EmbeddedPipeline` API 行为**(同 shard 合并 WAL)→ 阶段 0 bench 实测验证
- **回退总闸**:`workers=1` 退阶段 1;`write_batch` 回退逐点 `write`;删 dispatcher 回退单消费者

## 9. 后置项(YAGNI,本期不做)

- OPC UA Server 北向(独立大模块,下期)
- 时序历史存储(最新值语义下不需要)
- mqtt 增量 publish(只发变化 field)
- worker 自动重启(运维进程监督兜底)
- mqtt QoS1 / 断网补传(最新值语义下,断网补传意义有限,恢复后下次采集即覆盖)
- OPC UA subscription(秒级采集下周期 read 足够)
- period 毫秒化(秒级采集不需要)
