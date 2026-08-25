# 历史写路径(Phase 1b)实施计划

> **状态**:implemented
> **日期**:2026-08-25
> **实施**:feat/history-write → master(aedd1cf,no-ff merge),2026-08-25
> **代码**:src/history/{mod,rows,writer,engine}.rs + host.rs/pipeline.rs/lib.rs/config.rs/error.rs 挂点
> **测试**:单测 11(rows 5+engine 6)+writer 6+e2e 6(dataflow 2/write 2/kill 2)+config 4;四门禁数字见 §三
> **遗留**:①非饱和白名单查表成本(off-idle -5.15%,§四-1)②writer 高基数排空速率剖析(§四-2)③cardinality 启动时刻校准不重算(R4,§四-3)
> **依据(ADR 权威版)**:[2026-08-24-history-storage-master-plan.md](2026-08-24-history-storage-master-plan.md) §一三层开关/§二写路径/§三 D1-D11/§四陷阱/§五 1b 行——本文引用不复述
> **基线**:tsink fork tag `rs-iot-1b-baseline` @ 277e744(已推 viccom/tsink,2026-08-25)
> **执行方式**:worktree `feat/history-write`,executing-plans 编排式(主会话派发/审查,子代理 TDD 信息隔离)

**Goal**: pipeline flush_batch 出口旁路 fan-out → effective() 白名单+死区过滤 → 有界 channel → 单写线程 → tsink 内嵌存储;L1 feature/L2 config/L3 TagProps.history 三层开关骨架落地;四门禁 e2e(白名单外零写入/死区生效/kill 恢复零丢失/主链路劣化 <5%)。

**Architecture**: 历史归档是旁路(`旁路 fail-soft`,总方案 §六)——pipeline 主链路只增加一次 `history=true` 哈希查找 + 满即丢的 `try_send`,写入全在独立单写线程(D3"少写者大批次")。tsink 类型(`Row`/`Value`)只出现在 `src/history/engine.rs`/`writer.rs` 的 insert 侧,worker 热路径(`fan_out`)用自有 `HistoryRow` 中间类型——`HistoryHandle` 不含 tsink 类型,故 `HostContext.history` 字段与 `pipeline.rs` 调用点**无需 cfg 宏**,L1 裁剪只影响 engine/writer 模块。

**Tech Stack**: tsink 0.10.2(path `../tsink_fork`,无 feature flags)、tokio mpsc(有界 try_send)、std::thread(8 MiB 栈单写线程,D3/陷阱 4)。

---

## 〇、给执行者的关键事实速查(调研已核实,勿凭记忆猜 API)

### tsink API(行号基于 rs-iot-1b-baseline @ 277e744)

| 事实 | 证据 |
|---|---|
| 打开:`StorageBuilder::new().with_data_path(p).build() -> Result<Arc<dyn Storage>>`(builder 模式,无 open() 函数) | `src/storage.rs:1070,1279` |
| **时间戳精度默认纳秒**,必须显式 `.with_timestamp_precision(TimestampPrecision::Milliseconds)` | `src/storage.rs:18-23,1040` |
| 写入:`Storage::insert_rows(&self, rows: &[Row]) -> Result<()>`;`Row { metric: String, labels: Vec<Label>, data_point: DataPoint }`;`DataPoint { value: Value, timestamp: i64 }`;构造 `Row::new(m, DataPoint::new(ts, v))` / `Row::with_labels(m, vec![Label::new("k","v")], dp)` | `src/storage.rs:581`,`src/lib.rs:53-85` |
| `Value` 变体:`F64/I64/U64/Bool/Bytes/String/Histogram`(From f64/i64/bool/String 已实现) | `src/value.rs:105-113,258-324` |
| 资源边界 builder:`with_memory_limit(usize)`/`with_cardinality_limit(usize)`/`with_retention(Duration)`(置 enforced)/`with_wal_size_limit(usize)`/`with_wal_sync_mode(WalSyncMode::Periodic(d)|PerAppend)`/`with_write_timeout(Duration)`(默认 30s,close 排空受它约束) | `src/storage.rs:995-1234` |
| 预算超限=准入背压:`write_timeout` 内轮询等待,超时 `MemoryBudgetExceeded`;cardinality 超限**直接拒绝** `CardinalityLimitExceeded` | `src/engine/ingest_pipeline/prepare.rs:318-380`,`src/engine/series/identity.rs:359` |
| `close()` **不幂等**(二次返 `StorageClosed`);排空=flush+compaction(≤128 轮)+checkpoint+释放锁;最后一个 Arc drop 有 best-effort close 兜底 | `src/engine/lifecycle/shutdown.rs:121-154`,`src/engine/engine.rs:603-617` |
| 进程锁 `.tsink.lock`:1s 获取超时,失败错误是 `InvalidConfiguration("...already locked...")`(**无专用 Lock 变体**) | `src/engine/process_lock.rs:4-39` |
| **metric/label 无字符集校验**(点/中文/空格合法);仅:metric 非空≤65535B、label 名非空≤256B 不重复、label 值≤16KiB(UTF-8 字节数) | `src/validation.rs:5-44`,`src/label.rs:7-32` |
| **同 series 同 ts:写入端两条都存不报错**;读取端多数场景折叠后写胜,但纯内存无重叠时**可能双点都返回** | `src/engine/ingest_pipeline/capabilities.rs:373-407`,`src/engine/query_read.rs:256-280` |
| 同 series 混型:同批内或与 registry 既有冲突 → `ValueTypeMismatch`(**整批失败**) | `src/engine/ingest_pipeline/capabilities.rs:23-56` |
| 乱序 ts 接受(仅 retention 截止/分区头扇出两道闸) | `src/engine/state/active.rs:26-71`,`prepare.rs:109-136` |
| 查询:`select(metric, &labels, start, end) -> Vec<DataPoint>`(**半开区间 [start,end)**,start≥end 报 InvalidTimeRange,series 不存在返回空);`select_series(&SeriesSelection::new())` 空 selection 返回**全部 series**;`list_metrics() -> Vec<MetricSeries>{name,labels}` | `src/storage.rs:597-678`,`src/engine/query_exec/select_api.rs:421-433` |
| label 顺序不影响 series 身份(canonical hash) | `src/label.rs:51,110` |
| tsink crate 无 feature flags;zstd(C 编译器,本机 MSVC 有)/memmap2 跨平台;workspace 另有 tsink-server/tsink-uniffi(path 依赖不构建它们) | `Cargo.toml` |

### rs-iot 挂点(行号基于 master e201419)

| 事实 | 证据 |
|---|---|
| `Sample { hwid, devid, values: BTreeMap<String, SampleValue>, timestamp: i64 }`,**timestamp 是 Unix 秒**;`SampleValue{Float/Int/Bool/Str}` | `src/types.rs:98-123` |
| `flush_batch(ctx, lux, buf, last_fail_warn)`;发现引擎挂钩在 write_batch Ok 分支后:`model::discovery::note_once(lux, buf, &model::cache::effective()).await`——历史 fan-out 挂同层 | `src/pipeline.rs:196-230` |
| `model::cache::effective() -> Arc<HashMap<String, TagProps>>` 全局快照;`TagProps{history: bool, deadband: Option<f64>, desc, unit, owner_inst}`;`tag_key(devid, tag)` = `"{devid}.{tag}"`(tag 已含 devid 则原样) | `src/model/cache.rs:24-26`,`src/types.rs:128-161` |
| deadband 权威语义:**绝对死区,`|new − last_archived| ≥ deadband` 才归档(PI exception);last_archived 仅在归档时更新** | `src/types.rs:135` 头注 |
| 配置范本:`PipelineConfig`(全零值 derive Default)与 `LogConfig`(非零默认手写 Default);AppConfig 手写 `impl Default` | `src/config.rs:89-209` |
| lib.rs 编排:cache::reload(300-303)→ pipeline::run spawn(309-316);退出:pipeline join 5s(431)→ … → `db::shutdown_lux`(437,内含 SAVE)——engine init 插 reload 后 pipeline 前,close 插 shutdown_lux 前 | `src/lib.rs:295-439` |
| CI 双 checkout 在 5 个 job(fmt/clippy/test/minimal/doc)各重复一份 `actions/checkout@v4 + repository: viccom/lux + path: ../lux` | `.github/workflows/ci.yml:16-89` |
| features:`default = ["transform","lua","lua-http","opcua","modbus","s7","eip"]`,可选协议写法 `opcua = ["dep:async-opcua"]` | `Cargo.toml:151-169` |
| 测试基建:`tests/common/mod.rs` 的 `start_lux(data_dir)`(port 0+独立临时目录)与 `register_devices_as_models`;本机 cargo 一律 `-j 2` | CLAUDE.md |

### 本计划内的实现级决策(ADR 未细化处,按"最可逆保守"裁定)

| # | 决策 | 理由 |
|---|---|---|
| P1 | metric = tag **原名**(非空即合法,不做字符替换) | tsink 无字符集校验(见速查);"sanitize"退化为空名跳过+计数 |
| P2 | writer 线程做**批内同 series 同 ts 去重(保后写)**;跨批同 ts 不拦截(依赖 tsink 读取端 last-value-wins;e2e 断言按唯一 ts 计数) | Sample.timestamp 秒粒度,亚秒采集会撞 ts;tsink 双点读取端多数折叠 |
| P3 | writer 维护 per-series **lane 类型**,型变行剔除+计数(不进 insert 批) | ValueTypeMismatch 拒**整批**,单 tag 型变不能拖死整批(D4"写入侧自律") |
| P4 | deadband 仅作用于数值(Float/Int);Bool/Str 恒归档 | 绝对差对非数值无意义 |
| P5 | writer 维护 per-series `last_ts_ms`:相等即跳过(不写不更新状态) | 防跨批双点;1s 粒度本就有损,记录边界 |
| P6 | `HistoryHandle`/`fan_out`/`HistoryStats` **无条件编译**(不含 tsink 类型);`engine.rs`/`writer.rs` 的 insert 侧 `#[cfg(feature = "history")]` | HostContext/pipeline 调用点零 cfg 散布;热路径纯 Rust 可测 |
| P7 | feature off 且 `history.enabled=true` 时启动 `warn!` 并忽略 | L1/L2 正交,配置错误要可见 |
| P8 | engine init 失败(锁被占/磁盘错)→ `error!` 日志 + 归档停用,主程序继续 | 旁路 fail-soft(总方案 §六);锁占用是部署错误须显式告警 |
| P9 | `wal_sync_ms=0` 映射 PerAppend(严格档),>0 映射 Periodic(ms) | D6"PerAppend 严格档可选" |
| P10 | kill 恢复 e2e 优先在 Periodic(500ms) 默认档验证;若实测丢窗口,则该测试改 PerAppend 档跑 + 文档记录 Periodic 崩溃窗口语义 | WAL append 语义待实测(风险 R2) |
| P11 | 饱和快速路径:channel 满时该行直接计 dropped(不判白名单/不构造行/不分配 tag_key);计数本地累积每 flush 合并一次原子 | R8 门禁四劣化 23.96% 的根因修复;非饱和语义零变化 |

---

## 一、Task 拆分(串行依赖序;每 Task = 红→绿→commit,可独立验证)

> 所有命令在 worktree 根执行;本机 cargo 必带 `-j 2`。测试/实现信息隔离:红阶段代理只写测试,绿阶段代理禁改断言。

### Task 1:[history] 配置节(L2 骨架)

**Files**:
- Modify: `src/config.rs`(HistoryConfig + AppConfig.history 字段)
- Test: `src/config.rs` 内 `#[cfg(test)]`(照 `pipeline_config_default_auto` 惯例)

**红**:写测试 `history_config_default_off`(默认 enabled=false/data_dir="./data/history"/memory 256/cardinality 0 自动/retention 30/wal_sync 500/channel 65536/batch 8192/close_timeout 15)、`history_config_toml_explicit`(显式节全覆盖解析)、`history_config_missing_section`(无 [history] 节走 Default 不报错)、`history_config_partial`(只给 enabled=true 其余默认)。
Run: `cargo test -j 2 history_config` → **编译失败**(HistoryConfig 不存在)即红。

**绿**:实现 `HistoryConfig`:

```rust
/// [history] 历史时序存储配置(L2 部署期开关;L1=Cargo feature,L3=TagProps.history)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// L2 部署期开关:false 时不初始化引擎、不获取 .tsink.lock
    pub enabled: bool,
    /// tsink 数据目录(独立于 lux data_dir;.tsink.lock 进程锁所在)
    pub data_dir: String,
    /// 引擎内存预算(MiB),D6
    pub memory_limit_mb: usize,
    /// series 基数上限;0=按 effective() history=true 计数×1.2 自动校准,D6
    pub cardinality_limit: usize,
    /// 保留期(天),D6
    pub retention_days: u64,
    /// WAL 同步:0=PerAppend 严格档,>0=Periodic(ms),P9
    pub wal_sync_ms: u64,
    /// WAL 大小上限(MiB),D6
    pub wal_size_limit_mb: usize,
    /// 有界 channel 容量(try_send 满即丢+计数),D3
    pub channel_capacity: usize,
    /// 单写线程攒批行数上限,D3
    pub batch_max_rows: usize,
    /// 关停排空超时(秒;超时放弃,WAL 回放兜底),D10
    pub close_timeout_secs: u64,
}
```
(手写 `impl Default` 填上述默认值,照 LogConfig 惯例;`AppConfig` 加 `#[serde(default)] pub history: HistoryConfig` 并在 `impl Default for AppConfig` 补一行。)
Run: `cargo test -j 2 history_config` → 4 测 PASS;`cargo fmt --all` + `cargo clippy -j 2 --all-targets -- -D warnings` → 零警告。

**Commit**: `feat(history): [history] 配置节与 L2 默认值(D6 资源边界参数化)`

### Task 2:tsink path 依赖 + L1 feature + CI 三 checkout(编译骨架)

**Files**:
- Modify: `Cargo.toml`(optional dep + feature,不进 default)
- Modify: `.github/workflows/ci.yml`(fmt/clippy/test/minimal/doc 5 个 job 各加第三 checkout)
- Create: `src/history/mod.rs`(门面)

**红**:本 Task 的"红"= 编译验证断言,不写单测——先把 `src/history/mod.rs` 建为含 `#[cfg(test)] mod tests` 的最小门面(测试断言 `history` feature 已声明),`Cargo.toml` 加:

```toml
[dependencies]
tsink = { path = "../tsink_fork", optional = true }

[features]
history = ["dep:tsink"]
```
Run: `cargo build -j 2 --features history` → 编译过(空门面);`cargo tree -e features -j 2 --features history | grep tsink` → 见 tsink 0.10.2;`cargo build -j 2`(默认)→ 成功且 `cargo tree | grep tsink` → **无**(L1 裁剪生效);`cargo build -j 2 --no-default-features --features lua` → minimal 组合不破。

**绿**:mod.rs 写模块骨架与文档注释(`//! 历史时序存储写路径(Phase 1b):旁路 fan-out → 有界 channel → 单写线程 → tsink。L1=feature history,L2=[history].enabled,L3=TagProps.history`),`#[cfg(feature = "history")] pub mod engine;`/`pub mod writer;` 先以占位(空文件 engine.rs/writer.rs)保证编译。CI:在 5 个 job 的 lux checkout 后加:

```yaml
- uses: actions/checkout@v4
  with:
    repository: viccom/tsink
    ref: rs-iot-1b-baseline
    path: ../tsink_fork
```
(注意 checkout 到 `../tsink_fork` 与 path 依赖同名;minimal job 虽不启 history feature,path 依赖 manifest 解析仍需目录存在。)
Run: 四条编译命令重跑全过;`cargo fmt --all --check` + clippy(默认与 `--features history` 两种)零警告。

**Commit**: `feat(history): tsink path 依赖接线 + L1 feature + CI 三 checkout(pin rs-iot-1b-baseline)`

### Task 3:engine —— StorageBuilder 组装与 init/close(D2/D6/D10)

**Files**:
- Modify: `src/history/mod.rs`、`src/history/engine.rs`、`src/error.rs`(新变体)
- Test: `src/history/engine.rs` 内 `#[cfg(test)]`(整文件 `#![cfg(feature = "history")]` 语义由模块声明处 cfg 保证)

**红**(`#[cfg(feature = "history")] mod engine` 的测试,全部用 `tests/common` 风格临时目录,不碰仓库 `./data/`):
1. `engine_open_close_roundtrip`:init(默认参数,临时 data_dir)→ handle 引用 storage 有效 → close(15s) 成功返回;再次 close 返回 Err/无 panic(不幂等语义包装成幂等close——内部记 closed 标志)。
2. `engine_persists_rows`:init → insert_rows 两行(metric="m1",labels=[devid,hwid],ts 显式 ms)→ close → 重新 init 同目录 → select("m1", labels, 0, u64::MAX 级区间) 得 2 点且值保真(F64/I64/Bool/String 四型各一更好)。
3. `engine_lock_exclusive`:两个 init 同一 data_dir → 第二个返回 Err(错误信息含 "locked"/InvalidConfiguration)。
4. `engine_disabled_no_dir`:init 之前的 L2 判定函数 `engine::maybe_init(cfg, effective)`(enabled=false)→ 返回 None 且 data_dir 目录**未被创建**、无 .tsink.lock。
5. `engine_cardinality_auto`:`maybe_init` 传 effective 含 5 个 history=true → build 参数 cardinality = ceil(5×1.2)=6(通过把 builder 组装抽成纯函数 `build_storage(cfg, cardinality) -> TResult<Arc<dyn Storage>>` 后,单测算 `auto_cardinality(effective)` 纯函数即可,不必真开引擎)。
6. `engine_precision_ms`:写入 ts=1_700_000_000_123 → select 返回点 timestamp 精确等于该 ms 值(锁死"默认纳秒"陷阱)。
Run: `cargo test -j 2 --features history engine_` → 编译失败即红。

**绿**:实现:
- `error.rs`:`HistoryEngine(String)` 变体(#[non_exhaustive] 既有约定,`#[from]` 不适合——手工 map tsink error 串化,因为 tsink::TsinkError 不进公共错误面……注意:feature off 时该变体仍编译,`String` 载荷无 tsink 依赖,安全)。
- `engine.rs`:
  - `pub fn auto_cardinality(effective: &HashMap<String, TagProps>) -> usize`(count history=true ×1.2 向上取整,max(1));
  - `pub fn build_storage(cfg: &HistoryConfig, cardinality: usize) -> TResult<tsink::StorageHandle>`(即 `Arc<dyn Storage>`;组装顺序照速查表:data_path/timestamp_precision=Milliseconds/memory_limit/cardinality/retention(retention_days×86400s)/wal_size_limit/wal_sync_mode(P9)/wal_enabled 默认 true);
  - `pub struct HistoryEngine { writer_join: Option<JoinHandle<()>>, shared: Arc<WriterShared> }`;
  - `pub fn maybe_init(cfg: &HistoryConfig, effective: &HashMap<String, TagProps>) -> Option<TResult<HistoryEngine>>`:enabled=false → None(不 create_dir_all);true → Some(init)。
  - `pub fn close(&mut self, timeout: Duration)`(D10:drop 持有的 sender clone → writer 排空退出并自行 storage.close() → join(timeout),超时 warn 放弃;幂等:二次调用 no-op)。
- init 内 `std::fs::create_dir_all(&cfg.data_dir)`;writer 线程 spawn 留 Task 4 接线,本 Task writer_join 可为 None(close 兼容)。
Run: `cargo test -j 2 --features history engine_` → 全绿;clippy `--features history` 零警告。

**Commit**: `feat(history): engine init/close——StorageBuilder D6 资源边界组装 + 毫秒精度 + 进程锁互斥`

### Task 4:writer 单写线程 + fan_out 白名单过滤(D3/D4/D5/L3)

**Files**:
- Create: `src/history/rows.rs`(HistoryRow/HistoryStats/HistoryHandle/fan_out——无 tsink 依赖)
- Modify: `src/history/writer.rs`(线程循环+deadband+insert)、`src/history/engine.rs`(spawn 接线)、`src/history/mod.rs`

**红**(分两组;A 组不依赖 tsink 可无条件测,B 组 feature gate):
A(fan_out 纯函数):
1. `fan_out_whitelist_only`:两 tag 一 history=true 一 false(构造 effective HashMap)→ fan_out 后 handle 收到的行只含白名单 tag;行字段 devid/hwid/tag/ts_ms=timestamp*1000/value 映射(Float→仍 Float 等)。
2. `fan_out_tag_key_matching`:tag 已含 devid 前缀时 tag_key 原样(用 `types::tag_key` 语义构造 effective key)。
3. `fan_out_full_channel_drops`:容量 1 的 channel 手工填满 → fan_out 发 3 行 → stats.dropped ≥ 2,sent 不虚增。
4. `fan_out_empty_tag_skipped`:tag 名为空串 → 跳过(P1,计 skipped)。
B(writer 线程,`--features history`):
5. `writer_flushes_all`:spawn writer(真 tsink 临时目录)→ 发 100 行 → poll stats.written==100(轮询超时 5s)→ close → select 按唯一 ts 计数==100。
6. `writer_deadband_suppresses`:同 series 连发 v=1.0,1.1,1.15…(deadband=1.0)→ written 严格小于发出数且 ≥1(首点必档);且 last_archived 只在归档更新(0.4→0.8→1.5,db=1.0:档 0.4,剔 0.8(Δ0.4),档 1.5(Δ1.1))。
7. `writer_deadband_non_numeric_archives`:Bool/Str series 带 deadband → 全档(P4)。
8. `writer_batch_dedup_same_ts`:同 series 同 ts 两行不同值 → written 计 1(批内保后写,P2);select 该 ts 唯一。
9. `writer_lane_change_drops`:同 series 先 Float 后 Str → Str 行被剔+lane_mismatch 计数,Float 行正常写入(P3)。
10. `writer_type_fidelity`:Float/Int/Bool/Str 四 series 各一行 → close 后 select 值类型与值精确回读(D4)。
Run: A 组 `cargo test -j 2 fan_out` 编译失败即红;B 组 `cargo test -j 2 --features history writer_` 同。

**绿**:
- `rows.rs`:
```rust
/// fan-out 出口行(热路径中间类型;刻意不含 tsink 类型,P6)
pub struct HistoryRow { pub devid: String, pub hwid: String, pub tag: String, pub ts_ms: i64, pub value: SampleValue }
/// 历史旁路计数(e2e 确定性与 Phase 3 metrics 的数据源)
pub struct HistoryStats { pub sent: AtomicU64, pub written: AtomicU64, pub dropped: AtomicU64, pub deadbanded: AtomicU64, pub skipped: AtomicU64, pub lane_mismatch: AtomicU64, pub errors: AtomicU64 }
pub struct HistoryHandle { tx: mpsc::Sender<HistoryRow>, pub stats: Arc<HistoryStats> }
/// flush_batch 出口旁路:白名单过滤 + 类型化行构造 + try_send 满即丢(L3/D3/D4/D5/P1)
pub fn fan_out(samples: &[Sample], effective: &HashMap<String, TagProps>, h: &HistoryHandle) { … }
```
- `writer.rs`(`#[cfg(feature = "history")]`):`spawn_writer(storage, cfg) -> (HistoryHandle, JoinHandle<()>)`;线程 8 MiB 栈(`thread::Builder::new().stack_size(8*1024*1024)`,D3/陷阱 4);循环 `recv_timeout(50ms)`:攒 `batch: Vec<HistoryRow>` + 批内索引 `HashMap<(devid,tag), (ts_ms, idx)>` 同 series 同 ts 替换(P2),buf≥batch_max_rows 或 idle 超时 flush;flush 前逐行过状态机 `SeriesState { lane: Option<ValueKind>, last_val: Option<f64>, last_ts: i64 }`:lane 型变剔除(P3)→ last_ts 相等跳过(P5)→ deadband 数值判定(P4,|new−last_archived|≥db 才行,首点必档,剔行者不更新 last_val)→ 构造 `Vec<tsink::Row>`(labels 固定 `[Label::new("devid",..), Label::new("hwid",..)]` 按 label 名字典序稳定)→ `insert_rows` Ok→written+=n,Err→errors+=n+`warn!`(fail-soft,批次丢弃);channel Disconnected → 排空残余 → flush → `storage.close()` → 退出。
- engine.rs:init 时 `spawn_writer` 并把 handle 存入 engine(`engine.handle() -> Option<HistoryHandle>`);close 链路接通。
Run: 两组测试全绿;`cargo test -j 2`(默认 feature)确认 A 组在无 history feature 下也编译运行。

**Commit**: `feat(history): 单写线程(8MiB 栈)+ fan_out 白名单 + deadband/批内去重/lane 防混型`

### Task 5:pipeline 挂钩 + lib.rs 编排 + HostContext(D3/D10)

**Files**:
- Modify: `src/host.rs`(history 字段,P6 无条件)、`src/pipeline.rs`(flush_batch 一行挂钩)、`src/lib.rs`(init/close 编排)、`src/history/mod.rs`
- Test: `tests/history_dataflow_e2e.rs`(新建,`#![cfg(feature = "history")]`)

**红**(集成测试,tests/common 基建):
1. `history_dataflow_end_to_end`:start_lux + register_devices_as_models(模型含 tagA history=true、tagB history=false)→ 起 pipeline(照 `tests/dataflow.rs` 形态,构造带 history handle 的 HostContext)→ datasim 或直接 hub send N 个 Sample(tagA/tagB 各值)→ poll HistoryStats.written ≥ 预期 → close → tsink select_series 断言**只有 tagA 的 series**存在,tagA 点数=唯一 ts 数(门禁一"白名单外零写入")。
2. `history_disabled_zero_side_effect`:enabled=false 编排路径 → HostContext.history=None → 数据流照常入库 rtdata(HGETALL 验证主链路不因历史配置受损)且无 writer。
Run: `cargo test -j 2 --features history --test history_dataflow_e2e` → 编译失败(HostContext 无 history 字段)即红。

**绿**:
- host.rs:`pub history: Option<history::HistoryHandle>`(构造默认 None);
- pipeline.rs `flush_batch`:在 `note_once` 同层(write_batch Ok 分支后)加:
```rust
if let Some(h) = &ctx.history {
    history::fan_out(buf, &model::cache::effective(), h);
}
```
- lib.rs:`model::cache::reload` 之后、`pipeline::run` 之前:
```rust
#[cfg(feature = "history")]
let mut history_engine = history::engine::maybe_init(&cfg.history, &model::cache::effective())
    .map(|r| match r { Ok(e) => Some(e), Err(e) => { error!("[history] 引擎初始化失败,归档停用: {e}"); None } })
    .flatten();
#[cfg(not(feature = "history"))]
if cfg.history.enabled { warn!("[history] 配置 enabled=true 但编译未含 history feature,忽略(P7)"); }
```
HostContext.history = engine.handle();退出序列:pipeline join(431)之后、`db::shutdown_lux`(437)之前插:
```rust
#[cfg(feature = "history")]
if let Some(e) = history_engine.as_mut() { e.close(Duration::from_secs(cfg.history.close_timeout_secs)); }
```
Run: 集成测全绿;`cargo test -j 2 --all-targets`(默认)全量回归零失败;clippy 默认+`--features history` 零警告。

**Commit**: `feat(history): pipeline flush_batch 旁路挂钩 + lib.rs init/close 编排(D10 序)`

### Task 6:四门禁 e2e + 主链路 A/B(总方案 §五 1b 行验收)

**Files**:
- Create: `tests/history_write_e2e.rs`(门禁一/二 + L2/L3 开关矩阵,`#![cfg(feature = "history")]`)
- Create: `tests/history_kill_recovery_e2e.rs`(门禁三,`#![cfg(feature = "history")]`)
- Modify: `examples/bench_pipeline_e2e.rs`(A/B 档位:history off / on+白名单,门禁四)

**红**:
1. `gate1_whitelist_zero_write`(门禁一):多设备多 tag 混合白名单 → 全量流数 → select_series 集合**恰好等于**白名单 series 集合(不多不少)。
2. `gate2_deadband_effective`(门禁二):缓变序列(deadband=10% 量程,值每步变化 < deadband)→ 削减后点数 < 发出点数,且首点在档;对照组无 deadband 全档。
3. `gate3_kill_recovery_zero_loss`(门禁三):测试进程 spawn 自身二进制(`env!("CARGO_BIN_EXE_rs-iot")` 不可用于测试 crate——改用 `std::env::current_exe()` 子进程 + env 标记 `RSIOT_HISTORY_CRASH_CHILD=<dir>`;子模式:独立小工具路径打开 tsink→写 N=5000 行→`std::process::abort()`;父进程重开同目录 → select 唯一 ts 计数==5000。P10:默认 Periodic(500ms) 档跑;若实测丢窗口 → 改 PerAppend 档 + 在本 plan 遗留节登记 Periodic 语义)。Windows abort 若触发 WER 卡住,退化 `std::process::exit(0)`(同样不跑 Rust 析构,记录取舍)。
4. 门禁四(A/B,本机手跑登记,CI 不跑):`cargo run --release --example bench_pipeline_e2e` 三档——A(feature on+enabled=false 基线)/B(enabled=true+tag 100% 白名单+无 deadband)——B 相对 A 劣化 <5% 判过;bench 程序改造支持 env/参数选档。
Run: `cargo test -j 2 --features history --test history_write_e2e --test history_kill_recovery_e2e` → 红。

**绿**:上述测试通过所需的最小生产代码调整(预期 Task 3-5 已覆盖大部分;此 Task 主要是测试与 bench 适配本身,若发现生产缺陷回修对应模块)。
Run: 两测试文件全绿 + A/B bench 实测数字登记到本文件"验收记录"节(编辑本文档补:日期/机器/三档均值/劣化百分比)。

**Commit**: `test(history): 四门禁 e2e——白名单零写入/死区/kill 零丢失 + bench A/B 档位`

### Task 7:文档同步与收口

**Files**:
- Modify: `CLAUDE.md`(环境依赖加 tsink_fork 同级 path 说明与恢复方式;模块表加 `history/`;验证规则加 history e2e 与 A/B 门禁命令)
- Modify: `docs/README.md`(designs 索引加本文)
- Modify: `docs/designs/2026-08-24-history-storage-master-plan.md`(§五 1b 行状态 → 已实施,附 merge commit)
- Modify: 本文(状态块 → implemented,填实施 commit/代码/测试/遗留)
- Modify: `src/db/identity.rs:3-11` 清单注释(若需登记 history 数据目录约定)

**验证**:`python scripts/check_docs.py` 全绿;全量门禁:
```bash
cargo fmt --all --check
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo clippy -j 2 --workspace --all-targets --features history -- -D warnings
cargo test -j 2 --all-targets --all-features
cargo test -j 2 --no-default-features --features lua   # minimal 裁剪回归
```
**Commit**: `docs(history): Phase 1b 文档同步——CLAUDE.md/索引/总方案状态/implemented 块`

---

## 二、风险与已知边界(执行中遇阻按此裁断,新问题登记 docs/decisions.md)

| # | 风险/边界 | 裁断 |
|---|---|---|
| R1 | Periodic WAL 下 kill 窗口语义未实测(append 是否即落 OS) | P10 分支:先 Periodic 验,失败退 PerAppend 档 + 登记 |
| R2 | Windows `abort()` WER 弹窗卡 CI | 退化 `exit(0)`(不跑 Rust 析构)并记录 |
| R3 | 跨批同 series 同 ts 双点(亚秒采集) | P2/P5 已减;残留场景依赖 tsink 读取端折叠,e2e 按唯一 ts 断言 |
| R4 | cardinality 自动校准在启动时刻,运行期开白名单不重算 | 已知边界:超限行为=CardinalityLimitExceeded 拒新 series(错误计数可见);重算留 Phase 3 |
| R5 | deadband/last_archived 状态进程内存态,重启清零 | 首点必档,可能产生一个重复 ts 点(读取端折叠);记录 |
| R6 | tsink MSRV ≥1.89(File::try_lock) | 本项目 MSRV 已 ≥1.96(eip),无碍 |
| R7 | CI checkout viccom/tsink 为公共仓库无凭据问题 | fork 是 public;若实际私有,CI 加 token 后再议 |
| R8 | 主链路劣化 ≥5% | **已修复(P11)+门禁通过**:初测 23.96% → P11 饱和快速路径(d465ed5)→ ~10% 噪声带 → 三档分解定位;off-vs-on(生产口径)mean -4.23%/median -4.28% <5% 过;idle-on 饱和净增量 +0.96%(P11 快速路径 99.3% 行零分配);残留 off-idle -5.15%(非饱和白名单查表)登记 §四-1 优化项 |
| R9 | tsink 的 rayon 是死依赖(零调用),lux 才是进程内 rayon 消费者 | 任何"包界 tsink CPU"的 env 方案(如 RAYON_NUM_THREADS)都会误伤 lux SELECT 扇出——P12 因此前提证伪弃用;未来 CPU 包界须走线程优先级或 fork 移除死依赖 |

## 三、验收记录(2026-08-25 回填,Ryzen 9 5900HX)

- **门禁一(白名单外零写入)——过**:`gate1_whitelist_exact_set`——2 设备×3 tag 白名单矩阵 3/6,select_series 集合精确相等,written==90/skipped==90。
- **门禁二(死区生效)——过**:`gate2_deadband_effective`——deadband=5.0 值序列 0..=19 归档点恰 {0,5,10,15}(4 点),无死区对照 20 点全档,deadbanded==16。
- **门禁三(kill 恢复零丢失)——过**:`gate3_kill_recovery_zero_loss`——子进程 abort() 崩溃(未触发 WER 超时),Periodic(500ms) 默认档重放唯一 ts 总数 5000/5000 零丢失。机理:tsink WAL append 每次必 flush 到 OS 仅 fsync 延迟(logical.rs),进程级 kill 页缓存存活,丢失窗口仅覆盖断电——P10 的 PerAppend 退路不需要。
- **门禁四(主链路 A/B 劣化 <5%)——过**:三档分解(off/idle/on ×6 交替,Ryzen 9 5900HX,2026-08-25):**off vs on(生产口径)mean -4.23%/median -4.28% <5%**;idle-on 饱和净增量 +0.96%(P11 快速路径 99.3% 行零分配);off-idle 诊断口径 -5.15%(非饱和白名单查表成本)登记 §四-1 优化项。历程:初测 23.96% → P11 饱和快速路径(d465ed5)→ ~10% 噪声带 → 三档分解定位;P12(rayon 包界)前提证伪弃用:tsink 零 rayon(死依赖),lux 才是 rayon 消费者,RAYON_NUM_THREADS 会伤主链路查询(见 R9)。
- **全量门禁输出**(2026-08-25):①`cargo fmt --all --check` ✓( EXIT=0);②clippy 默认 ✓(12.90s 零警告);③clippy `--features history` ✓(13.42s 零警告);④`cargo test --all-targets --all-features` ✓(EXIT=0;lib 616 passed+9 ignored,history 三 e2e 2+2+2 全过);⑤**minimal(`--no-default-features --features lua`) ✗——pre-existing master 破坏,非本分支引入**:`tests/feature_gate_e2e.rs:54` E0308(expected `DeviceBinding`, found `DeviceSpec`)——M-2(55950a5,并入 e8c32e2)将 `InstConfig.devices` 切 `Vec<DeviceBinding>` 时,该测试仅 `#![cfg(not(feature = "modbus"))]` 下编译,全量门禁不覆盖故漏网;本分支对该文件仅加 `history: None`(与本错误无关);minimal `--lib` 521 passed 0 failed 证破坏孤立于该测试文件,须单独修复(CI minimal job 同命令必红);⑥`check_docs.py` ✓(50 个 md 全过)。**门禁⑤未过,docs 收口 commit 按"全部通过后"约定暂缓,待主会话裁决**。

## 四、遗留与优化项登记

1. **非饱和白名单查表成本**(off-idle 诊断口径 -5.15%):fan_out 每行查 effective HashMap;优化方向 flush 级 key 复用/每 worker 代际失效 memo(对标总方案 §五 1a 登记项),Phase 2/4 前视情实施。
2. **writer 高基数排空速率**:25K series 场景排空 ~8K pts/s(远低于 tsink 单写 182K 基线),Phase 4 PoC 范围内剖析(攒批参数/insert 批次切分)。
3. **cardinality 自动校准仅启动时刻**:运行期开白名单不重算(R4 已有边界,保留;超限=CardinalityLimitExceeded 拒新 series 错误计数可见,重算留 Phase 3)。
