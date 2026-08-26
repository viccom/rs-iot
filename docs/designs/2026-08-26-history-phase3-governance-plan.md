# 历史存储 Phase 3 余项(治理运维收尾)实施计划

> **状态**:planned
> **日期**:2026-08-26
> **依据(ADR 权威版)**:[2026-08-24-history-storage-master-plan.md](2026-08-24-history-storage-master-plan.md) §五 Phase 3 行——薄切片已前置(master ed54a6d:/sysstatus history 块 + /metrics 7 family + 前端四件;D10 关停序列已随 1b 落地),本计划收口余项
> **余项范围**(用户指令,2026-08-26):①完整 metrics 治理(archived_tags/deadband 削减率/内存水位/写错误分型)②前端标签管理页深化(历史数据查询 UI,消费 Phase 2 读端点)③小项:init_failed 态独立 e2e + 四件新 UI 浏览器嵌入实测
> **执行方式**:worktree `feat/history-phase3`,executing-plans 编排式(主会话派发/审查,子代理 TDD 信息隔离;浏览器实测为 main-agent-only 任务)

**Goal**: 历史旁路可观测性从"7 个累计计数器"升级为完整治理面——白名单规模/死区削减率/引擎内存水位(预算口径)/写错误分型全部进 /metrics 与 /sysstatus;标签管理页新增历史数据查询面板(raw+downsample,手写 SVG 曲线);init_failed 态有真实路径 e2e;前端四件旧 UI+新查询 UI 过真实浏览器嵌入实测。

**Architecture**: 治理数据两路——①rs-iot 自有计数(`HistoryStats` 扩展:死区评估计数+写错误分型)②tsink 引擎内部口径(新 `HistoryTelemetry` trait 无条件编译,engine.rs 实现,读 `Storage::memory_used()/memory_budget()/list_metrics()`,**不改 tsink fork**——trait 已公开,D6 恒设 memory_limit 故记账常开、读数纯原子)。渲染层延续"仅引擎 ok 输出"契约。前端零新依赖:查询面板 + 手写 SVG(min/max 抽稀折线)。

**Tech Stack**: tsink 0.10.2 observability trait 方法、axum spawn_blocking(读 list_metrics)、Vue3 手写 SVG 组件、vitest。

---

## 〇、给执行者的关键事实速查(三路调研已核实,勿凭记忆猜 API)

### tsink 观测面(rs-iot-1b-baseline @ 277e744,均为 Storage trait 公开方法)

| 事实 | 证据 |
|---|---|
| `fn memory_used(&self) -> usize` / `fn memory_budget(&self) -> usize`(trait 默认 0/MAX,引擎覆盖) | tsink `src/storage.rs:952-961` |
| **记账开启条件=builder 设了 with_memory_limit**;开启后读数纯原子;未开启时 `memory_used()` 每次全量重算(贵)。rs-iot D6 恒设 256MiB → 恒开启 | tsink `src/engine/construction/assembly.rs:143-157`,`engine/engine.rs:459-469` |
| series 数官方口径=`list_metrics().len()`(`Vec<MetricSeries>{name,labels}`,O(series) 分配) | tsink-server `crates/tsink-server/src/handlers.rs:2099-2105` 现成先例 |
| 写错误变体(分类依据):`MemoryBudgetExceeded{budget,required}` / `CardinalityLimitExceeded{..}` / `WalSizeLimitExceeded{limit,required}` / `InsufficientDiskSpace{..}` / `StorageShuttingDown` / `StorageClosed`;其余见 `src/error.rs`(pub enum,变体可构造) | tsink `src/error.rs:40-61,142` |
| 告警惯例:memory_used/budget > 0.90 | tsink `docs/monitoring.md:482` |

### rs-iot 现状(master 46180c7)

| 事实 | 证据 |
|---|---|
| 7 family 全 counter,自研 Prometheus text 渲染(无 prometheus crate),`render_history(h:&HistoryStats)` | `src/api/routes_metrics/render.rs:326-375` |
| handler 挂点:`if let Some(history)=&st.history_stats` 才 append(仅引擎 ok);/metrics 免 JWT | `src/api/routes_metrics/mod.rs:50-52`,`src/api/mod.rs:78` |
| `HistoryStats` 恰 7 个 AtomicU64(sent/written/dropped/deadbanded/skipped/lane_mismatch/errors) | `src/history/rows.rs:42-58` |
| writer insert 失败:`errors.fetch_add(n)`(n=批行数,整批丢弃),错误对象仅进 1s 节流 warn,类型被丢;**无分型** | `src/history/writer.rs:275-293` |
| deadband 判定点(writer flush 状态机):deadband=Some 且数值且 `|new−last|<db` → deadbanded++ | `src/history/writer.rs:241-248` |
| /sysstatus history 块:`{feature,enabled,engine,stats}`,stats 为 7 计数 JSON(Some 时) | `src/api/routes_sys.rs:54-67` |
| AppState 三字段:`history_stats:Option<Arc<HistoryStats>>`/`history_engine_state:Arc<str>`/`history_reader:Option<Arc<dyn HistoryReader>>`;**测试构造点约 16 文件**(加字段=机械补 None) | `src/api/mod.rs:57-69`,Phase 2 R5 |
| `HistoryReader` trait 无条件编译在 mod.rs,impl 藏 engine.rs(P6 先例——本计划 telemetry 照此) | `src/history/mod.rs:84-93` |
| lib.rs init 折叠:maybe_init Err → error! → None → "init_failed";AppState 在 init 后构造直取 | `src/lib.rs:317-354` |
| 全量 app 进程内起法:`run_app_with_shutdown` 已有测试先例 | `tests/dataflow.rs`,`src/lib.rs:197` |
| visibility e2e 现状:注入态(`test_app(stats,state)` 手工 AppState,ok 用 preset_stats 构造值);无真实引擎、无 init_failed | `tests/history_visibility_e2e.rs:35-73,119-186` |
| archived_tags 无任何暴露:history=true 计数仅 auto_cardinality 启动消费 | `src/history/engine.rs:61-69,273-277` |
| effective() 全局快照随时可读(api 层直呼) | `src/model/cache.rs:17-26` |

### 前端现状

| 事实 | 证据 |
|---|---|
| 标签管理页=DeviceModels.vue(/models,四面板);面板③标签总览有 historyOnly 过滤/批量覆盖/对账 | `frontend/src/views/DeviceModels.vue:497-600` |
| **无图表库**(deps 无 echarts/chart/uplot)——曲线手写 SVG | `frontend/package.json` |
| store 自带裸 fetch 封装(Bearer + `/api/rsiot` 前缀),**history 读端点前端零调用** | `frontend/src/stores/devreg.ts:84-118` |
| GET /api/v1/history 契约:dev_id/tag/start/end 必填秒半开;agg∈min|max|avg|sum|count|last 且与 interval 秒成对;limit 缺省 10000 上限 100000 取最旧;桶数 ≤100000;响应 `{dev_id,tag,points:[{ts,value}],count}`,value 类型保真;503 `{error,reason}` reason∈feature_not_compiled|history_disabled|init_failed;400 双形态(解析层纯文本/引擎层 JSON) | `docs/reference/rest-api.md:424-489`,`src/api/routes_history.rs` |
| dist 经 rust-embed 嵌 **rsiot-gateway**(非主 crate);改动须 `npm run build` + 重编 gateway | `crates/rsiot-gateway/src/static_assets.rs:20-22` |
| 前端单测 14 文件 168 测基线;vitest 默认 node 环境 | CLAUDE.md |
| 完整运行:gateway :8080(嵌 dist+反代 /api/rsiot→rs-iot :8790),登录 admin/admin → `POST /api/gw/login`;主服务 `./target/debug/rs-iot --config config.toml` | `gateway.toml`,`config.toml`,CLAUDE.md |
| 本机 cargo 一律 `-j 2`;Git Bash 别名 ls/ps 坏,用 fd/rg;残留端口 netstat+taskkill 清 | CLAUDE.md,FIELD_E2E_LOCAL.md |

### 本计划实现级裁定(ADR 未细化处,按"最可逆保守")

| # | 裁定 | 理由 |
|---|---|---|
| P1 | metrics 新增 family(仅引擎 ok 输出,延续既有契约):gauge `rs_iot_history_archived_tags`(effective() history=true 计数,渲染时直呼全局缓存)、gauge `rs_iot_history_series`、gauge `rs_iot_history_memory_used_bytes`、gauge `rs_iot_history_memory_budget_bytes`、counter `rs_iot_history_deadband_evaluated_total`、gauge `rs_iot_history_deadband_reduction_ratio`、counter `rs_iot_history_write_errors_total{kind="memory_budget\|cardinality\|wal_disk\|closed\|other"}` | 覆盖总方案 Phase 3 行四诉求;命名对齐既有 `rs_iot_history_*` |
| P2 | 死区削减率口径:**ratio = deadbanded / deadband_evaluated**,evaluated=「deadband=Some 标签的数值点」计数(writer 判定点新增);分母为 0 时 ratio 输出 0 | 精确口径(分子分母同源),Phase 4 门禁"削减率 ≥50%"可直接对表 |
| P3 | 错误分型:**按行计**——`record_error_rows(kind,n)` 同时增 errors 与 kind 槽,不变量 Σkind == errors_total(单测锁);桶 5 类(memory_budget/cardinality/wal_disk/closed/other),wal_disk=WalSizeLimitExceeded∪InsufficientDiskSpace,closed=StorageClosed∪StorageShuttingDown | 行数口径与既有 errors 一致,分型和可对账;分型函数在 writer.rs(tsink 类型不出此文件) |
| P4 | `HistoryTelemetry` trait **无条件编译**(`memory_used_bytes/memory_budget_bytes/series_count` →u64),impl 藏 engine.rs;AppState 加 `history_telemetry:Option<Arc<dyn HistoryTelemetry>>`(~16 测试点补 None) | 1b P6/Phase 2 P1 先例;tsink 类型不散布 |
| P5 | series_count=`list_metrics().len()`(Err→0+debug 日志);handler 经 `tokio::task::spawn_blocking` 读 telemetry 三元组(list_metrics 有分配) | tsink-server 先例;不占 runtime worker |
| P6 | /sysstatus history 块扩展:engine ok 时增 `memory_used_bytes/memory_budget_bytes/series/archived_tags/deadbands:{evaluated,ratio}` + stats 增 `deadband_evaluated` 与 `errors_by_kind:{...}`;非 ok 这些字段省略(非 null) | 与 /metrics 同口径;additive 不破前端现有读取 |
| P7 | 前端零新依赖:HistoryQuery.vue 手写 SVG(min/max 抽稀折线,≤800 桶);**非数值 series(Bool/Str)表格-only 不画曲线**;抽稀/参数构建/错误归一为纯函数进 lib/historyQuery.ts + vitest | 供应链自控;数值曲线对非数值无意义 |
| P8 | 前端 API 走 gateway 全路径 `/api/rsiot/api/v1/history`(devreg fetch 口径,Bearer) | 与四件 UI 同口径 |
| P9 | init_failed 独立 e2e:**真实路径**——先 build_storage 占住临时目录 .tsink.lock,再 `run_app_with_shutdown`(cfg.history.enabled=true 指向同目录)进程内起全量 app,断言 /api/v1/history 503 reason=init_failed、/sysstatus engine=init_failed 且 stats 缺省、主链路端点仍 200(fail-soft)。**预期首跑即绿(行为已正确)→ 定性为固化测试,非红→绿,于此登记** | 走 lib.rs 真 init 折叠;红不可达时不造假红 |
| P10 | 浏览器嵌入实测为 **main-agent-only**(browser-use 禁委派);覆盖四件旧 UI+新查询 UI,留截图/网络证据 | skill 边界 + 用户指令"本轮只跑了 build/test"的补课 |

---

## 一、Task 拆分(串行依赖序;每 Task = 红→绿→commit)

> 所有命令在 worktree 根执行;cargo 必带 `-j 2`。TDD 信息隔离:红阶段代理只写测试,绿阶段代理禁改已红测试断言。

### Task 1:HistoryStats 扩展——死区评估计数 + 写错误分型计数(数据源层)

**Files**:
- Modify: `src/history/rows.rs`(WriteErrorKind + HistoryStats 新字段/方法)
- Modify: `src/history/writer.rs`(deadband 判定点 evaluated++ / insert 失败路径分型)
- Test: 两文件内 `#[cfg(test)]` 扩展(rows 无条件 / writer feature 档)

**红**:
1. rows.rs(无条件):`error_kind_invariant`——`HistoryStats::new()` → `record_error_rows(MemoryBudget,3)` + `record_error_rows(Other,2)` → `errors==5` 且各 kind 槽正确、`WriteErrorKind::ALL` 遍历 `as_str()` 得五串。
2. writer.rs(feature):`classify_err_covers_variants`——构造 `TsinkError::MemoryBudgetExceeded{budget:1,required:2}`/`CardinalityLimitExceeded{..}`(字段按 error.rs 实际)/`WalSizeLimitExceeded{..}`/`InsufficientDiskSpace{..}`/`StorageClosed`/`StorageShuttingDown`/`Io(..)` → classify 各得预期桶。
3. writer.rs(feature):`writer_deadband_evaluated_counted`——既有 `writer_deadband_suppresses` 序列扩断言:序列 0.4/0.8/1.5(deadband=1.0)→ `deadband_evaluated==3 && deadbanded==1`;无 deadband series 不计 evaluated。
4. writer.rs(feature):`writer_error_kind_cardinality`——`build_storage`(cardinality=1 显式传)→ spawn writer → 发 2 个不同 series 各 1 行 → 轮询 `errors>=2` 且 `errors_by_kind(Cardinality)==2`(单 series 批先成功 1 行 written==1)。
Run: `cargo test -j 2 --features history writer_ rows::` → 编译失败(新 API 不存在)即红。

**绿**:
```rust
// rows.rs(无条件编译)
/// 写失败分型桶(writer.rs classify 产出;计数按行,Σkind == errors)
pub enum WriteErrorKind { MemoryBudget, Cardinality, WalDisk, Closed, Other }
impl WriteErrorKind {
    pub const ALL: [WriteErrorKind; 5] = [Self::MemoryBudget, Self::Cardinality, Self::WalDisk, Self::Closed, Self::Other];
    pub fn as_str(&self) -> &'static str { match self { Self::MemoryBudget=>"memory_budget", Self::Cardinality=>"cardinality", Self::WalDisk=>"wal_disk", Self::Closed=>"closed", Self::Other=>"other" } }
}
// HistoryStats 增:pub deadband_evaluated: AtomicU64、error_rows: [AtomicU64; 5]
// 增方法:record_error_rows(&self, kind, n)(errors 与 kind 槽同增)、error_rows_of(&self, kind)->u64、deadband_evaluated(&self)->u64(渲染用)
```
writer.rs:deadband 分支(deadband=Some 且数值)先 `deadband_evaluated++` 再比较;insert Err 处改:
```rust
let kind = classify_err(&e);
stats.record_error_rows(kind, n as u64);   // 原 errors.fetch_add(n) 由该方法承担
warn!(...)                                  // 原节流日志保留
```
Run: 上述测试全绿;`cargo test -j 2`(默认)确认 rows 测试无 feature 也编译运行;clippy 双档零警告。

**Commit**: `feat(history): 写错误五型分桶 + 死区评估计数——HistoryStats 治理数据源(P1-P3)`

### Task 2:HistoryTelemetry trait + engine impl + AppState/lib 接线

**Files**:
- Modify: `src/history/mod.rs`(trait 定义+re-export)
- Modify: `src/history/engine.rs`(`EngineTelemetry` impl + `HistoryEngine::telemetry()`)
- Modify: `src/api/mod.rs`(AppState.history_telemetry 字段)、`src/lib.rs`(engine ok 时注入)
- Modify: ~16 个测试文件 AppState 构造点机械补 `history_telemetry: None`

**红**(engine.rs 单测,feature 档):
1. `telemetry_memory_and_series`:init(临时目录,memory_limit_mb=64)→ 写 2 series 各 1 行 → 轮询 telemetry:`memory_budget_bytes()==64*1024*1024`、`memory_used_bytes()>0`、`series_count()==2`;close 后 series_count 仍可调不 panic(实现自由,断言≥0)。
2. `telemetry_trait_object_safe`:`Arc<dyn HistoryTelemetry>` 装箱调用(锁 trait object 面)。
Run: `cargo test -j 2 --features history telemetry_` → 编译失败即红。

**绿**:
```rust
// mod.rs(无条件)
/// 历史引擎资源水位(Phase 3 治理;tsink 口径——D6 恒设 memory_limit 故记账常开,读数纯原子)
pub trait HistoryTelemetry: Send + Sync {
    fn memory_used_bytes(&self) -> u64;
    fn memory_budget_bytes(&self) -> u64;
    fn series_count(&self) -> u64;
}
```
engine.rs:`struct EngineTelemetry(StorageHandle)`;`memory_used_bytes()->storage.memory_used() as u64`;`series_count()->storage.list_metrics().map(|v| v.len() as u64).unwrap_or(0)`(Err 时 debug! 日志);`HistoryEngine::telemetry(&self)->Arc<dyn HistoryTelemetry>`(包 storage clone)。AppState/lib 注入照 history_reader 模式(engine Some 才 Some)。
Run: 单测绿;`cargo test -j 2 --all-targets`(默认)零回归(16 点补 None 编译器兜底);clippy 双档。

**Commit**: `feat(history): HistoryTelemetry trait——内存水位/series 数引擎口径外露(P4/P5)`

### Task 3:/metrics 新 family + /sysstatus 扩展 + visibility e2e 扩展

**Files**:
- Modify: `src/api/routes_metrics/render.rs`(render_history 扩参:telemetry 快照 + archived_tags)
- Modify: `src/api/routes_metrics/mod.rs`(handler spawn_blocking 采 telemetry 三元组)
- Modify: `src/api/routes_sys.rs`(history 块新字段,P6)
- Test: `tests/history_visibility_e2e.rs`(test_app 加 telemetry 参数 + FakeTelemetry + 注册模型断言 archived_tags)

**红**(e2e,feature 档):
1. `metrics_history_governance_families`:test_app(stats=preset(含新计数值), telemetry=Fake{used=111,budget=268435456,series=7}, state="ok";先经 devmodels store 注册含 1 个 history=true tag 的模型并 cache::reload)→ GET /metrics 文本断言:`rs_iot_history_archived_tags 1`、`rs_iot_history_series 7`、`rs_iot_history_memory_used_bytes 111`、`rs_iot_history_memory_budget_bytes 268435456`、`rs_iot_history_deadband_evaluated_total <preset 值>`、`rs_iot_history_deadband_reduction_ratio <preset deadbanded/evaluated>`、五条 `rs_iot_history_write_errors_total{kind="..."} <值>` 行齐全且 HELP/TYPE 行存在。
2. `metrics_history_governance_omitted_when_not_ok`:telemetry=Some 但 state="disabled"(stats=None)→ 全部新 family 不出现(仅引擎 ok 契约)。
3. `sysstatus_history_governance_fields`:同 ok 态 → /sysstatus history 块断言新字段精确值(memory_used_bytes/budget/series/archived_tags/deadbands.ratio/stats.deadband_evaluated/stats.errors_by_kind 五键)。
Run: `cargo test -j 2 --features history --test history_visibility_e2e` → 红(字段/渲染不存在)。

**绿**:render_history 增参 `t: &TelemetrySnapshot, archived_tags: usize`(快照 struct 放 render.rs);ratio=evaluated>0 时 deadbanded/evaluated else 0.0(f64);labeled family 逐 kind 输出(值 0 也输出,齐五桶);sysstatus 按 P6 组 JSON。handler 侧:`let t = st.history_telemetry.clone(); let snap = tokio::task::spawn_blocking(move || ...).await.unwrap_or_default()`(缺 telemetry → 空快照,但该分支实际不可达——stats Some 则 telemetry 必 Some,防御即可)。
Run: e2e 绿;`cargo test -j 2 --all-targets --features history` 定向回归;clippy 双档。

**Commit**: `feat(history): 治理 metrics 全量——archived_tags/series/内存水位/死区削减率/错误分型上 metrics+sysstatus(P1/P2/P6)`

### Task 4:init_failed 真实路径独立 e2e

**Files**:
- Create: `tests/history_init_failed_e2e.rs`(`#![cfg(feature = "history")]`)

**红/固化**(P9:预期首跑即绿——红阶段先证测试基建可运行,断言写全):
1. `init_failed_real_lock_503_and_failsoft`:临时目录 A 先 `build_storage` 占锁(句柄存活);构造 AppConfig{history.enabled=true,data_dir=A,api_port=空闲固定端口, lux 独立目录};`run_app_with_shutdown`(照 tests/dataflow.rs 形态)起全量 app → 轮询端口就绪 → ①GET /api/v1/history?dev_id=d&tag=t&start=1&end=2(JWT)→ 503 body reason=="init_failed";②GET /sysstatus → history.engine=="init_failed" 且 stats 无/缺省;③GET /api/v1/tags(或 /rtdatas)→ 200(主链路 fail-soft 存活);④shutdown token 取消 → app 正常退出。drop 占锁句柄。
Run: `cargo test -j 2 --features history --test history_init_failed_e2e -- --nocapture` → 若首跑即绿:登记"固化测试"(P9 预期,非红→绿);若红(真缺陷)→ 回修 lib.rs/engine.rs 并记录。

**绿**:无需生产改动(除非上步暴露真缺陷)。
Run: 该 e2e + `cargo test -j 2 --features history --test history_visibility_e2e` 零回归。

**Commit**: `test(history): init_failed 真实锁占用 e2e——503/三态/fail-soft 固化(P9)`

### Task 5:前端历史查询 UI + SystemStatus 治理字段扩展

**Files**:
- Create: `frontend/src/lib/historyQuery.ts` + `frontend/src/lib/historyQuery.test.ts`
- Create: `frontend/src/components/devmeta/HistoryQuery.vue`
- Modify: `frontend/src/views/DeviceModels.vue`(面板③ head"历史查询"折叠按钮 + 归档行"查"快捷入口 + 组件挂载/预填)
- Modify: `frontend/src/types.ts`(TsdbHistoryPoint/TsdbHistoryResponse;SysStatusHistory 扩展 P6 字段)
- Modify: `frontend/src/components/monitor/SystemStatus.vue`(title 悬浮扩展:deadband_evaluated/errors_by_kind/水位/series)

**红**(vitest,纯函数):
1. `buildHistoryUrl`:agg+intervalSec 成对输出;无 agg 无 interval;limit 缺省含 10000;dev_id/tag URL 编码;秒参数命名 start/end。
2. `decimate`:均匀 10 点分 3 桶 → 3 组 min/max/last 精确;空数组→[];单点;非均匀点按桶归属。
3. `buildSparklinePath`:全等值序列(平坦线不 NaN,clamp 边界);min==max 时 y 居中;坐标在 [0,w]×[0,h] 内。
4. `historyErrorMessage`:503 JSON reason 三值→中文文案;400 纯文本体与 JSON 体双形态;其他状态透传。
5. `windowToRange`:'15m'/'1h'/'6h'/'24h' → {startSec,endSec} 差值精确。
Run: `cd frontend && npm run test -- --run historyQuery` → 失败即红。

**绿**:
```ts
// historyQuery.ts 关键面(完整签名见测试)
export interface TsdbHistoryPoint { ts: number; value: number | boolean | string }
export interface TsdbHistoryResponse { dev_id: string; tag: string; points: TsdbHistoryPoint[]; count: number }
export function buildHistoryUrl(p: HistoryQueryParams): string          // /api/rsiot/api/v1/history?...
export function decimate(pts: {ts:number;value:number}[], buckets: number): DecimatedPoint[]
export function buildSparklinePath(d: DecimatedPoint[], w: number, h: number): string
export function historyErrorMessage(status: number, body: string): string
export function windowToRange(preset: WindowPreset, nowSec: number): { startSec: number; endSec: number }
```
HistoryQuery.vue:props 预填 {devId,tag};表单=窗口预设(15m/1h/6h/24h/自定义 datetime-local)+模式(raw/agg+六聚合 select+interval 秒)+limit;fetch 走 Bearer(照 devreg.ts 口径);结果=count 摘要+SVG 曲线(全数值点时,decimate 800 桶,min/max 双折线+首末时间/值域标注)+表格(ts 原值/value 类型保真展示);503/400 错误横幅;非数值 series 只表格+提示。SystemStatus.vue title 增补新字段行。
Run: vitest 全绿(168 基线+新) + `npm run build`(vue-tsc 零错)。

**Commit**: `feat(frontend): 历史数据查询面板——raw+六聚合/手写 SVG 曲线/503 归一 + SystemStatus 治理字段(P7/P8)`

### Task 6:浏览器嵌入实测(main-agent-only,不委派)

前置:`cd frontend && npm run build` → `cargo build -j 2 --features history` + `cargo build -j 2 -p rsiot-gateway`(嵌入新 dist);临时 config.toml(api_port 空闲、lux 独立目录、`[history] enabled=true` 独立 data_dir、一个 datasim 实例产数);gateway.toml;清残留端口进程。

实测清单(browser-use,admin/admin 登录,留截图/网络证据):
1. Overview 发现入口:datasim 未建模标签 → 队列计数卡显示与跳转。
2. DeviceModels 发现队列 adopt:逐行类型 select → 采纳 → 模型/注册表出现。
3. 批量覆盖:glob 选中新 tag → history=true(+deadband)→ 提交回显 matched/updated。
4. 对账按钮:漂移表空态/正常态展示。
5. 白名单过滤 checkbox:勾选后仅归档行。
6. SystemStatus 历史引擎行:ok 态着色+扩展悬浮字段(水位/series/削减率)。
7. **历史查询面板**(新):选归档 tag → raw 出表格+曲线;切 agg=avg interval=60 → 桶曲线;503 场景(临时关 [history] 重启或查 init_failed 目录)文案正确。
产物:问题清单(即改即回归)或全过证据;截图路径登记本 plan §三。

**Commit**: `test(e2e-manual): 四件 UI+历史查询面板浏览器嵌入实测记录`

### Task 7:文档收口 + 全量门禁 + 合并

**Files**:
- Modify: `docs/reference/rest-api.md`(/sysstatus history 新字段 + metrics family 清单,既有 rs_iot_history 段扩)
- Modify: `CLAUDE.md`(模块表 history 行补 Phase 3 一句;前端工程段补历史查询面板)
- Modify: `docs/README.md`(索引)、总方案 §五 Phase 3 行 → **已实施(余项收口)**
- Modify: 本文(状态块 implemented + §三验收记录回填)
**门禁**(worktree 根,全真实输出):
```bash
cargo fmt --all --check
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo clippy -j 2 --workspace --all-targets --features history -- -D warnings
cargo test -j 2 --all-targets --all-features
cargo test -j 2 --no-default-features --features lua
python scripts/check_docs.py
cd frontend && npm run build && npm run test -- --run
```
**Commit**: `docs(history): Phase 3 收口——metrics/sysstatus/前端面板文档 + implemented 块 + 验收记录`

---

## 二、风险与已知边界

| # | 风险/边界 | 裁断 |
|---|---|---|
| R1 | list_metrics().len() 在 10 万 series 的 scrape 成本(O(series) 分配) | spawn_blocking + scrape 低频(15s 级)可受;Phase 4 PoC 实测,超限再加 TTL 缓存(backlog) |
| R2 | memory_used() 在未配 memory_limit 时全量重算 | D6 恒设(默认 256MiB)→ 记账常开;若未来允许 0=不限,须同步改 telemetry(登记) |
| R3 | init_failed e2e 预期首跑即绿(红不可达) | P9 定性固化测试;真红则回修并记录 |
| R4 | AppState ~16 测试点补字段 | 编译器兜底机械补 None(Phase 2 R5 先例) |
| R5 | 浏览器实测环境抖动(端口残留/进程僵死/datasim 产数节奏) | 实测前 netstat 清残留;datasim 周期确认后轮询等数;失败重试一轮再判 |
| R6 | raw limit 10000 点全量渲染卡顿 | decimate ≤800 桶(P7);表格仅首 N 行+总数展示 |
| R7 | tsink `CardinalityLimitExceeded` 构造字段名与计划笔误 | 以 tsink `src/error.rs` 实际字段为准(速查已给行号) |
| R8 | ratio 语义误读(削减率 vs 保留率) | HELP 文本明写 "suppressed/evaluated";sysstatus 字段名 deadbands.ratio 同口径 |

## 三、验收记录(收口回填)

(待 Task 1-7 完成后回填:各测真实输出、门禁结果、浏览器实测证据路径)
