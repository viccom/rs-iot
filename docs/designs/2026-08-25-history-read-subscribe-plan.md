# 历史读路径 + KSUB 订阅升级(Phase 2)实施计划

> **状态**:implemented(T1-T6 全六 Task 完成)
> **实施**:feat/history-read-sub → master(66976d4,no-ff merge),2026-08-25:T1 eed2fab/T2 d8c50f1/T3 9d0347a/T4 86b5e67/T5 13332b6(+lint 整理 c2cf5a8、bench 档工具 d12d70d)/T6 1a0268c
> **代码**:读路径——src/history/mod.rs(HistoryReader trait 族)+ engine.rs(EngineReader raw/downsample)+ src/api/routes_history.rs + AppState.history_reader;订阅——src/api/rtdata_watch.rs(RtdataWatchService:ksubscribe("rtdata:*") 消费 + per-devid 版本表 + epoch 重同步 + KEYS 铺底)+ src/api/routes_rtdata_subscribe.rs(read_delta 精准读 + poll 分支原样保留)+ src/config.rs([rtdata] sub_impl)+ AppState.rtdata_watch + examples/bench_pipeline_e2e.rs(RSIOT_BENCH_SUB 档)
> **遗留**:无(已并 master 66976d4)
> **日期**:2026-08-25
> **依据(ADR 权威版)**:[2026-08-24-history-storage-master-plan.md](2026-08-24-history-storage-master-plan.md) §二读/订阅两路径 + D7(类型化 /api/v1/history)+ D8(KSUB dirty-set)+ §一 L2 off 返 503 承诺 + §六风险表"KSUB 改造回归→双 impl 过渡开关"——本文引用不复述
> **前置已就绪**:Phase 1b 写路径(aedd1cf:engine 持 StorageHandle 留作本 Phase 读)+ 可观测性薄切片(ed54a6d:AppState 已有 history_engine_state 三态)
> **执行方式**:worktree `feat/history-read-sub`,executing-plans 编排式(主会话派发/审查,子代理 TDD 信息隔离)

**Goal**: ①`GET /api/v1/history` 类型化读端点(raw + downsample 六聚合,L2/L1 非 ok 态返 503,兑现总方案承诺);②`/rtdatas/subscribe` 订阅读侧从"每连接每 tick 全量 HGETALL"升级为"lux KSUB → 进程内共享 dirty-set → 精准读",SSE 语义/参数完全冻结,`sub_impl="poll"` 过渡开关可一键回退。

**Architecture**: 读端点经无条件编译的 `HistoryReader` trait(P6 先例,tsink 类型不出 engine.rs)挂在 AppState,handler 按 AppState 三态判 503;订阅升级采用"只换读源、不动节奏"方案——保留每连接 tick interval 与 diff_scope 纯函数(语义冻结),新增 `RtdataWatchService` 单例消费 `ksubscribe("rtdata:*")` 维护 per-devid 单调版本号,连接按"版本号变了才 HGETALL 该设备"精准读。

**Tech Stack**: tsink `select_with_options`(raw 分页下推 + downsample)、lux `EmbeddedClient::ksubscribe`(key 级事件,push,RAII)、axum SSE 既有结构不动。

---

## 〇、给执行者的关键事实速查(三路调研已核实,勿凭记忆猜 API)

### tsink 查询 API(rs-iot-1b-baseline)

| 事实 | 证据 |
|---|---|
| `QueryOptions { labels, start, end, aggregation, downsample: Option<DownsampleOptions{interval:i64}>, limit: Option<usize>, offset }`;builder:`new(start,end)`/`with_labels`/`with_aggregation`/`with_downsample(interval, agg)`/`with_pagination(offset, limit)` | tsink `src/storage.rs:1513-1568` |
| `Aggregation` 变体:`None/Sum/Min/Max/Avg/First/Last/Count/Median/Range/Variance/StdDev`(serde 线上名 PascalCase,REST 映射自建) | `storage.rs:1496-1511` |
| `select_with_options(metric, opts) -> Result<Vec<DataPoint>>`;**start≥end → InvalidTimeRange;空窗/不存在 series → Ok(vec![])**(不是 NoDataPoints) | `storage.rs:642`,`engine/query_read.rs:160-166` |
| 桶语义:**origin=查询 start**(非 epoch);空桶不输出(无 gap filling);桶点 ts=桶起点;downsample 未指定 agg 默认 Last | `query_aggregation.rs:578-633`,`query_exec/select_api.rs:112-125` |
| **类型退化矩阵**:Avg/Median/Variance/StdDev 恒 F64;Count 恒 U64;Sum 保数值域(混型→F64);Min/Max/First/Last 保原类型(含 Bool/String);**数值聚合作用于 Bool/String/Bytes → UnsupportedAggregation 错误** | `query_aggregation.rs:47-52,276-333,500-557` |
| **limit=Some(n) 取时间升序最旧 n 个**(非最新);raw 路径分页下推(免全窗物化),agg 路径事后截断;limit=Some(0) → 空 | `query_read/pagination.rs:50-66`,`query_read.rs:233-248` |
| 整窗聚合(无 interval)坍缩 1 点且 ts=末点——REST 层拒绝此形态 | `query_aggregation.rs:487-516` |

### lux ksubscribe(当前 ../lux)

| 事实 | 证据 |
|---|---|
| `client.ksubscribe("rtdata:*") -> EmbeddedSubscription`;同步建、RAII 退订(drop→kunsub);**订阅表挂 Broker 全局,任一 clone 订阅全局生效** | lux `src/lib.rs:1918`,`pubsub.rs:409` |
| 事件:`EmbeddedMessage{channel="rtdata:<devid>", payload=命令名小写(b"hset"/b"del"), kind=KeyEvent}`——**key 级无 field**;消费 `sub.recv().await`(async)或 `try_recv()` | `lib.rs:549-554,2613-2630` |
| **无订阅者零开销**:写路径前置 `has_key_subs()` 原子读;有订阅者则 enqueue(broadcast 容量 1024,溢出 per-key 合并回灌)——**watcher 常驻即每次 HSET 付一次 enqueue(Phase 2 新热路径成本,门禁四守)** | `pubsub.rs:479-495`,`shard_exec.rs:169` |
| `recv()` Lagged → `Err(LuxError::SubscriptionLagged(skipped))`——当"全量重同步"信号用 | `lib.rs:2623` |
| 不能走 RESP `client.cmd("KSUB")`(显式拦截),必须方法调用 | `lib.rs:2057-2059` |

### rs-iot 现状(master d83c659)

| 事实 | 证据 |
|---|---|
| 订阅 handler:`routes_rtdata_subscribe.rs`——每连接 spawn `run()`;Phase1 snapshot(逐设备 `event: snapshot` 帧)→ Phase2 `interval` tick(缺省 1000ms clamp[100,60000])`read_scope`(HGETALL 范围内每设备;无 dev 过滤时每 tick `KEYS rtdata:*`)→ `diff_scope`(值串=cell 第 3 元素,**值同新 ts 不算变更**,OPC UA 语义)→ `event: datachange`{changes,removed,ts} | `routes_rtdata_subscribe.rs:27-241` |
| 参数:`dev_id`(逗号分隔精确列表)/`tag`(field 末段 contains)/`interval`(ms)——**无 snapshot/datachange 开关(均为无条件语义)** | `:34-42` |
| 语义冻结基线 = 6 e2e(401/200+content-type/snapshot 全量逐设备帧/dev+tag 过滤/值变才推+同值新ts不推/removed)+ 10 单测(diff/clamp/parse 纯函数) | `tests/rtdata_subscribe_e2e.rs:138-224`,`routes_rtdata_subscribe.rs:243-367` |
| AppState 字段(lux/mgr/hwid/jwt_secret/version/metrics/trace_registry/log_ringbuffer/event_bus/config/history_stats/history_engine_state);**测试构造点 16 个文件**(加字段=16 处机械补 None) | `api/mod.rs:40-63` |
| `/api/v1/events` 是**另一套**(EventBus broadcast,前端 monitor 用它;pipeline 出口 publish_rtdata_notify 带全值但有 50 dev 截断/可 Lagged)——本 Phase **不动 /events**,只动 /rtdatas/subscribe(外部数据面) | `routes_events.rs:52-77`,`pipeline.rs:297-312` |
| 引擎:`HistoryEngine.storage: StorageHandle`(Phase 1b 留作读);AppState 构造在引擎 init **之后**(lib.rs ~385 vs ~317) | `src/history/engine.rs`,`lib.rs` |
| config 无 [rtdata] 节;SSE interval 三常量硬编码 | `config.rs:16-50`,`routes_rtdata_subscribe.rs:27-31` |

### 本计划实现级裁定(ADR 未细化处,按"最可逆保守")

| # | 裁定 | 理由 |
|---|---|---|
| P1 | `HistoryReader` trait **无条件编译**(返回 rs-iot 类型 `HistoryPoint{ts_ms, value: SampleValue}`/`HistoryQuery{start_ms,end_ms,downsample,limit}`/`HistoryAgg{Min,Max,Avg,Sum,Count,Last}`),impl 藏 engine.rs(feature 门控)——AppState 加 `history_reader: Option<Arc<dyn HistoryReader>>` 零 cfg 散布 | 1b P6 先例;16 测试构造点只补一行 None |
| P2 | 时间单位:**REST 出入秒**(与 rtdata ts 同构),内部 ×1000/÷1000 ms | 全库秒基线;history 点本就秒对齐(1b D5) |
| P3 | 参数:`dev_id`+`tag` 必填(单 series);`start`/`end` 必填秒、半开 [start,end);`agg` ∈ min/max/avg/sum/count/last(六值);`interval` 秒,**有 agg 必有 interval,无 agg 有 interval → 400**;`limit` 缺省 10000、上限 100000(取最旧 n,tsink 语义) | 整窗聚合坍缩 1 点的怪形态直接拒;六值=tsink 调研推荐面 |
| P4 | `U64→SampleValue::Int`(Count 路径;i64 溢出饱和);Bytes/Histogram 不可达(lane 只写四型),防御性映射 Str(串化) | SampleValue 无 U64;1b P3 lane 保证 |
| P5 | **503 矩阵**:feature 未编译(cfg!)→`feature_not_compiled`;engine_state="disabled"→`history_disabled`;"init_failed"→`init_failed`——响应 `{error:"history unavailable", reason}` + 503 | 兑现总方案 §一;调用方可区分"未实现/未开启/初始化失败" |
| P6 | 读路径 handler 内 `tokio::task::spawn_blocking` 包 select(长窗 raw 扫描可能 >1ms,不占 runtime worker) | raw 无分页下推时全窗扫描 |
| P7 | watcher:`HashMap<devid, u64 单调版本>` + 全局 `epoch`(Lagged/退订异常 → epoch+1);连接各自持 `last_versions` + `last_epoch`,tick 时版本落后或 epoch 变 → 精准 HGETALL 该设备(diff_scope 不动);**tick 节奏/interval clamp/snapshot 首帧全不动** | 语义冻结的最小换源方案(调研方案 A) |
| P8 | 配置新节 `[rtdata] sub_impl = "ksub"\|"poll"`,**缺省 "ksub"**;仅 ksub 时 spawn watcher(poll 时 lux 零开销承诺保持);poll 分支保留原 read_scope 路径一行不删 | 默认开否则功能上暗线;回退=改一个配置键 |
| P9 | 新设备发现:首个 HSET 事件即入版本表(等价原 KEYS 发现);DEL 事件 → 版本 bump → HGETALL 空 → diff_scope removed(等价原语义);watcher 启动时一次 `KEYS rtdata:*` 铺底版本表 | dev_filter=None 场景语义等价 |
| P10 | 桶三怪点全部文档化(origin=查询 start/空桶省略/ts=桶起点),不自作聪明补点 | tsink 基线语义冻结,修正是 fork backlog |
| P11 | 等价 e2e **双 impl 参数化跑**(ksub+poll 各跑全套 6 e2e + 新增专项),防单边漂移 | 总方案"行为等价 e2e"落法 |
| P12 | reader 的 400 映射:InvalidTimeRange→400 "start must < end";interval≤0→400;UnsupportedAggregation→400(附类型矩阵提示);未知 agg→400 | REST 错误面统一 |

---

## 一、Task 拆分(串行依赖序;每 Task 红→绿→commit)

### Task 1:HistoryReader trait + /api/v1/history 路由骨架与 503/400 矩阵

**Files**:
- Modify: `src/history/mod.rs`(trait 定义+re-export)、`src/history/engine.rs`(reader() 与 impl)、`src/api/mod.rs`(AppState.history_reader + 路由注册)、`src/lib.rs`(注入)、16 个测试文件(AppState 构造补 `history_reader: None`)
- Create: `src/api/routes_history.rs`
- Test: `tests/history_read_e2e.rs`(`#![cfg(feature = "history")]`)

**红**:
1. `history_503_matrix`:三态——AppState{reader:None, engine_state:"disabled"} → 503 body reason=="history_disabled";{None,"init_failed"} → reason=="init_failed";feature 判定用 cfg!(编译期,feature on 时 feature_not_compiled 分支不可达,测试断言前两态即可)。
2. `history_400_param_matrix`:缺 dev_id/tag/start/end 各 → 400;start>=end → 400;agg=avg 无 interval → 400;agg 未知值 → 400;interval=0 → 400;limit>100000 → 400。
3. `history_ok_shape_empty`:reader 就绪态 + 不存在 series 查询 → 200 `{dev_id,tag,points:[],count:0}`。

**绿**:HistoryQuery/HistoryPoint/HistoryAgg 类型 + `HistoryReader` trait(`fn select(&self, q:&HistoryQuery, metric:&str, devid:&str) -> Result<Vec<HistoryPoint>, String>`——Result Err 串化供 400/500 映射)放 history/mod.rs 无条件;engine.rs `pub fn reader(&self) -> Arc<dyn HistoryReader>`(wraps storage.clone());routes_history.rs:参数解析(serde query)+ P5 503 + P12 400 + 空结果 200;lib.rs 注入 `history_reader: engine.reader()`(fail-soft None 时三态已可判);AppState 16 测试点补 None。
Run: `cargo test -j 2 --features history --test history_read_e2e` 绿;`cargo test -j 2 --lib`(默认)零回归;clippy 双档。

**Commit**: `feat(history): /api/v1/history 路由骨架——HistoryReader trait + 503/400 矩阵(D7/P5)`

### Task 2:raw select 读路径(类型保真回读)

**Files**: `src/history/engine.rs`(impl select 的 raw 分支)、`src/api/routes_history.rs`(秒↔ms/limit 映射)
**红**(tests/history_read_e2e.rs 追加):
1. `history_raw_roundtrip_typed`:真引擎(临时目录)写入四型 series(Float/Int/Bool/Str,各 5 点,ts 互异秒)→ GET 读回:points ts==写入秒、值类型精确(Float→JSON number、Int→number、Bool→bool、Str→string)、count==20。
2. `history_raw_window_half_open`:窗 [t1,t3) → 含 t1 不含 t3。
3. `history_raw_limit_oldest_first`:10 点 limit=3 → 返回最旧 3 点升序。
4. `history_raw_missing_series_200_empty`:未写过的 (dev,tag) → 200 空。
**绿**:engine impl raw 分支(select_with_options:labels=[devid,hwid] 字典序、limit 下推)+ Value→SampleValue 映射(P4)+ ms↔秒转换。
Run: 同 T1 + `cargo test -j 2 --features history --test history_dataflow_e2e` 零回归。

**Commit**: `feat(history): raw select 读路径——类型保真/半开窗/limit 最旧优先(D7/P2-P4)`

### Task 3:downsample 六聚合 + 类型矩阵 + 400 映射

**Files**: `src/history/engine.rs`(downsample 分支)、`src/api/routes_history.rs`(agg 参数映射)
**红**:
1. `history_downsample_buckets`:已知数据(ts t0..t0+9 每 1s,v=1..10)interval=5s agg=sum → 2 桶(ts=t0 与 t0+5,值 15/40;P10 origin=start;原稿"15/35"系笔误,桶 t0+5 含 v6..v10=40,与 avg 8.0×count 5 交叉锁定);agg=last → 桶值 5/10;agg=min/max/count 同理断言。
2. `history_agg_type_matrix`:F64 series avg → number;I64 series count → number(整型);Bool series last → true/false;**Bool series avg → 400**(UnsupportedAggregation 映射);Str series min → 原串。
3. `history_downsample_empty_bucket_skipped`:数据留空洞(interval 内无点)→ 桶缺失不补点(断言输出点数)。
**绿**:HistoryAgg→tsink Aggregation 映射(with_downsample(interval_ms, agg));Err 串分层(参数类→400,引擎类→500,P12)。
Run: 定向 + clippy。

**Commit**: `feat(history): downsample 六聚合——桶语义/类型矩阵/400 映射(P3/P10-P12)`

### Task 4:RtdataWatchService + [rtdata] 配置节

**Files**:
- Create: `src/api/rtdata_watch.rs`(服务:ksub 消费循环 + 版本表 + epoch + KEYS 铺底 + CancellationToken 退出 + `devices_dirty_since(&versions,&epoch)->Vec<String>` 查询接口)
- Modify: `src/config.rs`(RtdataConfig{sub_impl},缺省 "ksub",照 HistoryConfig 惯例+测试)、`src/api/mod.rs`(AppState.rtdata_watch: Arc<RtdataWatchService>——服务常驻构造,poll 模式下不启动内部循环)、`src/lib.rs`(sub_impl=="ksub" 才 spawn 循环)、16 测试点补字段
**红**:
1. config:`rtdata_config_default_ksub`/`rtdata_config_toml_override`(src/config.rs 单测惯例)。
2. rtdata_watch 单元:版本 bump(coalesce:同设备多次 HSET 事件 tick 间只算一次脏)、epoch 递增、`devices_dirty_since` 增量语义(版本落后才报)——用注入假事件测纯逻辑(事件消费循环与版本表分离,可测)。
3. e2e(真 lux):`watch_service_marks_dirty_on_hset`——起服务→HGETALL 前提设备→HSET rtdata:devA→轮询 versions[devA] bump;`watch_service_marks_del`;`watch_service_initial_keys_scan`(启动前已有 rtdata:devB → 铺底后 devB 在版本表)。
**绿**:实现服务(tokio task 消费 `sub.recv()`,Lagged→epoch+1 并 KEYS 重铺;Closed→warn 退出);config 节;AppState/lib 接线。
Run: `cargo test -j 2 --features history --test history_read_e2e --lib` 相关 + 新测试文件 + clippy 双档 + 默认档(服务无条件编译,不许 cfg)。

**Commit**: `feat(subscribe): RtdataWatchService——ksubscribe("rtdata:*") 消费 + per-devid 版本表 + epoch 重同步(D8/P7/P9)`

### Task 5:subscribe producer 换源(ksub impl)+ 双 impl 等价 e2e + bench 守门

**Files**:
- Modify: `src/api/routes_rtdata_subscribe.rs`(run() 读源分支:ksub→dirty 驱动精准 HGETALL+diff_scope 冻结;poll→原 read_scope 路径原样)、AppState 消费
- Test: `tests/rtdata_subscribe_e2e.rs`(既有 6 e2e 参数化双 impl 跑:构造 helper 加 sub_impl 参数循环)+ 新增专项:
  1. `ksub_new_device_discovered_without_keys`(dev 过滤 None,连接建立后新设备写数据 → 合理时限内进 datachange)
  2. `ksub_removed_via_del_event`(DEL rtdata:devA → removed 帧)
  3. `ksub_multi_connection_isolation`(两连接不同 interval/过滤,各自独立基线)
  4. `poll_impl_still_works`(显式 sub_impl="poll" 跑 snapshot+datachange 主干)
- Modify: `examples/bench_pipeline_e2e.rs`(档位扩展:`RSIOT_BENCH_HISTORY=on` 下再叠 `RSIOT_BENCH_SUB=ksub|poll`——ksub 档 spawn watcher 常驻,度量 enqueue 热路径成本;off 档不受影响)
**红**:新 e2e 先写跑红(ksub 路径不存在)。
**绿**:run() 分支实现;等价套件双 impl 全绿。
**门禁四扩展(bench,本机手跑登记)**:A(history on + sub=poll)vs B(history on + sub=ksub)交替 3+3,劣化 <5%(R1:watcher enqueue 是新热路径成本);若超 → 裁决(先查 enqueue 实现/批量合并,不得静默放宽)。
Run: 全套订阅 e2e + `cargo test -j 2 --all-targets --all-features` 全量零回归 + clippy 双档 + fmt。

**Commit**: `feat(subscribe): KSUB dirty-set 精准读 + 双 impl 等价 e2e + bench 守门(D8/P7/P8/P11)`

### Task 6:文档收口 + 全量门禁 + 合并

**Files**: `docs/reference/rest-api.md`(§4.18 扩读端点全契约:参数/503/400/桶语义/类型矩阵/limit 最旧优先;§rtdatas/subscribe 补 sub_impl 开关与回退说明)、`CLAUDE.md`(模块表 history 读面 + rtdata_watch;验证规则加 history_read_e2e/双 impl 订阅套件/bench SUB 档)、`docs/README.md` 索引、总方案 §五 Phase 2 行状态、本文状态块 implemented+验收回填。
**门禁**:
```
cargo fmt --all --check
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo clippy -j 2 --workspace --all-targets --features history -- -D warnings
cargo test -j 2 --all-targets --all-features
cargo test -j 2 --no-default-features --features lua
python scripts/check_docs.py
cd frontend && npm run build && npm run test -- --run   # 零前端改动也应零回归
```

**Commit**: `docs(history): Phase 2 收口——读端点/订阅升级文档 + implemented 块 + 验收记录`

---

## 二、风险与已知边界

| # | 风险 | 裁断 |
|---|---|---|
| R1 | watcher 常驻使 lux 每次 HSET 付 enqueue 成本(broadcast 容量 1024) | T5 bench A/B(poll vs ksub)<5% 守门;超→停下修,不静默放宽 |
| R2 | Lagged(突发写)重同步正确性 | epoch 全量重读兜底(P7);单测锁 epoch 逻辑,自然触发不可测(登记) |
| R3 | 等价漂移 | 双 impl 参数化全套 e2e(P11);diff_scope 纯函数一行不动——**裁决兑现**:4 主干 e2e 双档逐字一致全绿 + read_delta 单元红门 2 测锁"只读脏设备/epoch 重同步";poll 分支原路径保留,漂移可随时暴露 |
| R4 | 桶 origin=查询 start 的怪语义 | 文档化(P10);fork 修正是 backlog |
| R5 | AppState 16 测试点 × 2 字段机械改动 | 一次性机械补 None,编译器兜底 |
| R6 | spawn_blocking 读在 SSE/高并发下的线程池占用;downsample 大窗全窗聚合读放大 | raw 有 limit 上限 100000 + `select_raw_series_page` 分页早停兜底;**审查修复补桶数护栏** `(end-start)/interval > 100000` → 400(2026-08-26);Phase 4 三线同场再验 |
| R7 | /events(EventBus)路径 50 dev 截断/丢帧问题 | 明确不在本 Phase(独立机制,前端在用);登记 Phase 4 评估反哺 |
| R8 | U64 Count 值 > i64::MAX | 饱和转换,文档登记(现实中不可达) |
| R9 | ts 出秒 `ts_ms / 1000` 对负时间戳(1970 前)向零截断,方向与正数不一致 | 理论边界(REST 秒基线场景不可达),登记不处理(审查 Low,2026-08-26) |
| R10 | 版本表不删项(设计声明"表即注册表")——脏键/历史键可致无界增长 | IoT 网关设备量级可控,登记;恶化时加 TTL/容量逐出(审查 Low,2026-08-26) |
| R11 | ksub Partial 分支逐设备串行 HGETALL,脏集大时单 tick 耗时长 | embedded lux 进程内延迟低 + MissedTickBehavior::Skip 兜底;backlog:脏集大时 join_all 并行(审查 Low,2026-08-26 登记) |
| R12 | tag 名含点使 `dev_segment` 按最后点切分误归设备域(ksub 增量投影路径) | 表现为删→重推抖动,不丢数据;rest-api.md §4.14 声明 tag 不含点约束(审查 Low,2026-08-26) |

## 三、验收记录(2026-08-25 收口回填,均为真实运行输出)

- **503/400 矩阵**(3 测,全文件 10 passed / 0 failed,`cargo test -j 2 --features history --test history_read_e2e`):`history_503_matrix`——reader=None + engine_state 三态映射(disabled→`history_disabled`/init_failed→`init_failed`;`feature_not_compiled` 是编译期 `cfg!` 分支,feature on 构建下不可达,按矩阵逻辑测前两态)+ `history_400_param_matrix`(缺 dev_id/tag/start/end/start>=end/agg 无 interval/interval 无 agg/未知 agg/interval=0/limit>100000 全档)+ `history_ok_shape_empty`(不存在 series → 200 `{points:[],count:0}`)。
- **raw 类型保真/半开窗/limit**(4 测):`history_raw_roundtrip_typed`(Float/Int/Bool/Str 四型各 5 点回读,JSON 类型精确——number/number/boolean/string,count==20)、`history_raw_window_half_open`([t1,t3) 含 t1 不含 t3)、`history_raw_limit_oldest_first`(10 点 limit=3 → 最旧 3 点升序)、`history_raw_missing_series_200_empty`。
- **downsample 桶语义/类型矩阵/400**(3 测):`history_downsample_buckets`(六 agg × interval=5s 两桶,桶 ts==[t0,t0+5] 锁 origin=查询 start;**sum 第二桶 40.0——计划原稿"15/35"系算术笔误,与同测 avg 8.0×count 5=40 及数据铺设互斥,任何实现无法满足原稿值,按冻结语义纠正为 40 并于此记录**);`history_agg_type_matrix`(I64 count→JSON 整数 3(Count 恒 U64)/Bool last→boolean(保原类型)/Bool avg→**400**(UnsupportedAggregation 的 P12 映射)/Str min→"a"(保原串));`history_downsample_empty_bucket_skipped`(空洞桶 t0+5/t0+10/t0+15 不补点,count 桶值 3/2 锚定点归属)。
- **订阅双 impl 等价**(`rtdata_subscribe_e2e` 10 passed + `rtdata_watch_e2e` 3 passed):4 主干 e2e(snapshot 全量逐设备帧/dev+tag 过滤/值变才推·同值新 ts 不推/removed)单测内循环 ksub/poll 双档——两档独立 lux data_dir/AppState/连接,**断言逐字一致**;2 鉴权(无 JWT 401/200+content-type,与档无关不参数化);4 专项(ksub_new_device_discovered_without_keys/ksub_removed_via_del_event/ksub_multi_connection_isolation/poll_impl_still_works);watcher 3 e2e(HSET 标脏/启动 KEYS 铺底/DEL 标脏)。**e2e 层红不可达裁决记录**:绿阶段 SSE 输出是黑盒——帧内容无法区分"只读了脏设备"还是"读了全部"(两行为同输出),等价红门落单元层:`read_delta` 2 测(`read_delta_only_reads_dirty_devices`:3 设备铺数仅 mark devA → 只 HGETALL devA;`read_delta_epoch_bump_resync`:epoch 变更先重建游标再全量重读),真 lux 起服务验证;`diff_scope` 纯函数一行未动。
- **bench A/B(watcher 常驻成本,R1/D8 守门)**:`RSIOT_BENCH_HISTORY=on` 下 `RSIOT_BENCH_SUB=poll` vs `ksub`(release,交替):poll 2,049,604 vs ksub 2,080,990 pts/s(mean)——**-1.5%**(<5% 门禁过;ksub 反而略快系噪声内,单轮方差大于该差值,结论以"无劣化"计,不声称 ksub 更快)。
- **watcher 事件消费佐证**:bench ksub 档收尾输出 `watcher device_count=100 / 100`(= N_DEVID 100)——版本表即设备注册表,全量设备入表即证 watcher 真在收 HSET key 事件(非空转常驻)。
- **全量门禁**(T6,2026-08-25,worktree feat/history-read-sub 全部真实输出):`cargo fmt --all --check` 通过;clippy 双档(`-D warnings`,默认 + `--features history`)均 Finished 零警告;`cargo test -j 2 --all-targets --all-features` exit 0(27 个测试目标全绿,含上述三文件);`cargo test -j 2 --no-default-features --features lua` exit 0(裁剪档+Doc-tests);前端 `npm ci && npm run build` 产 dist(gateway rust-embed 依赖)+ `npm run test -- --run` 14 文件 168 测全过(基线无回归);`python scripts/check_docs.py` OK(51 md)。

## 四、审查修复记录(2026-08-26,worktree fix/phase2-review,Phase 2 深度审查后修复包)

审查发现 2 High + 3 Medium + 6 Low,逐一处置(红→绿证据均为真实输出):

- **High#1 连接建立期丢帧窗口**:`run()` 游标基线后置到 snapshot 之后,snapshot 半程中已推设备的 mark 被游标吸收,变更永不推送。修复:游标初始化前移到 snapshot 之前(`cursor()` 文档语义"先取快照再全量读")。红门 e2e `ksub_change_during_snapshot_not_lost`(反压构造:20 设备 > SSE channel 容量 8,producer 阻塞在中途 send 时全量写新值,已推旧值的 ≥9 台丢帧窗口确定性打开)——修复前红(无 datachange),修复后绿;全套 subscribe e2e 11/11。
- **High#2 watcher 异常退出静默冻结(违背 P7"退订异常→epoch+1")**:`Err(e)` 直接 break 后版本表停更,所有 ksub 连接 dirty_since 永空,静默冻结在旧值。修复:`classify_failure` 纯函数三分类(单测 `watch_failure_classification` 红→绿)——Lagged 原地继续/Closed 退出/其余 Restart:退订→bump_epoch→重铺 KEYS→退避 1s→重建 ksubscribe。e2e `watch_service_respawn_recovers` 锁重启机制在真 lux 可用(backfill 补断订窗口设备+重建订阅消费新事件)。
- **Med#3 interval `as i64` 负值绕过溢出校验**:u64::MAX→-1→-1000ms 负桶宽下推(原靠 tsink InvalidConfiguration 兜底改文案)。修复 `i64::try_from` 拒域外值;400 矩阵补 case 先红(200 打到 FakeReader)后绿。
- **Med#4 downsample 大窗读放大无护栏**:R6 补桶数护栏 `(end-start)/interval > 100000` → 400(rest-api.md §4.18.1 参数表同步)。
- **Med#5 dirty_since 每连接每 tick O(N×M) 全表扫描**:marks 计数器与表变更同一锁临界区递增(mark/backfill 新增),`WatchCursor` 三元组(versions+epoch+marks)`cursor()` 锁内对齐快照;`dirty_since` O(1) 早退免锁免全扫,慢路径锁内定格 marks 精确对应扫描态不丢帧。单测 `watch_cursor_marks_alignment` 锚定快路径后新 mark 不被吞(红→绿;含一次测试自身时序修正——游标须先于事件建立,同真实序)。
- **Low 处置**:400 矩阵补"interval 无 agg"case(既有校验缺覆盖);R9-R12 登记(ts 负截断/版本表不删项/串行 HGETALL backlog/tag 含点约束——rest-api.md §4.14 已声明);400 双形态(纯文本/JSON)文档已有,补客户端区分提示。
- 门禁:lib 617/617、rtdata_subscribe_e2e 11/11、rtdata_watch_e2e 4/4、history_read_e2e 10/10;fmt/clippy 双档/minimal 见 merge commit。
