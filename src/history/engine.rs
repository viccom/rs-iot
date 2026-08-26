//! tsink 引擎组装与生命周期：StorageBuilder 资源边界组装（D6，毫秒精度）+
//! 进程锁互斥（D2）+ close 排空包装（D10）。
//!
//! L2 判定入口是 [`maybe_init`]：`enabled=false` 时零副作用（不建目录、
//! 不碰 `.tsink.lock`）直接返回 `None`。组装契约与决策依据见
//! `docs/designs/2026-08-25-history-write-path-plan.md`「Task 3」+ 速查表。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tsink::{
    Aggregation, DataPoint, Label, QueryOptions, Storage, StorageBuilder, TimestampPrecision,
    TsinkError, Value, WalSyncMode,
};

use crate::config::HistoryConfig;
use crate::error::{RsIotError, TResult};
use crate::history::rows::HistoryHandle;
use crate::history::writer::spawn_writer;
use crate::types::{SampleValue, TagProps};

/// tsink 存储句柄的透明包装：`dyn Storage` 未实现 `Debug`（trait 无
/// `Debug` supertrait，孤儿规则下也无法在外部补），而引擎句柄与错误断言
/// 路径（`Result::expect_err` 要求 Ok 侧 `Debug`）都需要它。
///
/// 通过 `Deref` 透传 `dyn Storage` 的全部方法（`insert_rows`/`select`/
/// `close` 等），语义与 `Arc<dyn Storage>` 等价。`Clone` 即 Arc 克隆——
/// engine 与 writer 线程各持一份共享同一底层存储（Task 5：close 排空由
/// writer 侧执行，engine 侧句柄在 close 后进入 StorageClosed 可观测态）。
///
/// 附带 writer 退出信号（Task 5）：tokio mpsc 的断开语义要求**全部** sender
/// drop，而 `HistoryHandle` clone（HostContext/测试侧）在 `engine.close` 后
/// 仍可存活——writer 若只认 Disconnected 会永不退出、进程锁不释放。故
/// `close` 经共享 `AtomicBool` 通知 writer 排空退出（≤1 个攒批窗内生效），
/// 存活 clone 的后续 send 只入无人消费的 channel，无害。无 writer 挂靠的
/// 句柄（同目录重开/Phase 2 读路径）上该标志恒 false，惰性无害。
#[derive(Clone)]
pub struct StorageHandle {
    /// 被包装的 tsink 存储句柄。
    storage: Arc<dyn Storage>,
    /// 挂靠 writer 的退出信号（所有 clone 共享同一 Arc）。
    writer_shutdown: Arc<AtomicBool>,
}

impl std::fmt::Debug for StorageHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageHandle").finish_non_exhaustive()
    }
}

impl std::ops::Deref for StorageHandle {
    type Target = dyn Storage;

    fn deref(&self) -> &Self::Target {
        &*self.storage
    }
}

/// series 基数自动校准：effective 快照中 `history=true` 计数 ×1.2 向上取整，
/// 空表/全 false 兜底 `≥1`（D6）。整数运算 saturating（H4 双保险：校验后
/// 输入域理论不可达溢出）。
///
/// 仅在启动时刻校准一次，运行期开白名单不重算——超限行为是 tsink 侧
/// `CardinalityLimitExceeded` 拒新 series（计划已知边界 R4，重算留 Phase 3）。
pub fn auto_cardinality(effective: &HashMap<String, TagProps>) -> usize {
    let whitelisted = effective.values().filter(|props| props.history).count();
    whitelisted.saturating_mul(12).div_ceil(10).max(1)
}

/// 按配置组装 tsink 内嵌存储（D6 资源边界，组装项与顺序照计划速查表）。
///
/// - `create_dir_all` 先行：父目录不存在时 tsink 不会代建（IO 错误经既有
///   `Io(#[from])` 变体上抛）；
/// - 时间戳精度**显式毫秒**：builder 默认纳秒，是本写路径的头号陷阱；
/// - 资源边界换算 saturating（H4 双保险：`HistoryConfig::validate` 拒绝后
///   理论不可达，防御绕过校验的直接调用点）；
/// - `wal_sync_ms==0` 映射 `PerAppend` 严格档，`>0` 映射 `Periodic(ms)`（P9）；
///   `wal_enabled` 保持 builder 默认 `true` 不动；
/// - build 失败（含同目录进程锁互斥——tsink 以
///   `InvalidConfiguration("…already locked…")` 拒绝，无专用 Lock 变体）统一
///   串化为 [`RsIotError::HistoryEngine`]：tsink 错误类型不进公共错误面。
pub fn build_storage(cfg: &HistoryConfig, cardinality: usize) -> TResult<StorageHandle> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let wal_sync_mode = if cfg.wal_sync_ms == 0 {
        WalSyncMode::PerAppend
    } else {
        WalSyncMode::Periodic(Duration::from_millis(cfg.wal_sync_ms))
    };
    StorageBuilder::new()
        .with_data_path(&cfg.data_dir)
        // builder 默认纳秒精度，必须显式毫秒（Sample.timestamp 秒 ×1000 的锚点）
        .with_timestamp_precision(TimestampPrecision::Milliseconds)
        .with_memory_limit(cfg.memory_limit_mb.saturating_mul(1024 * 1024))
        .with_cardinality_limit(cardinality)
        .with_retention(Duration::from_secs(
            cfg.retention_days.saturating_mul(86400),
        ))
        .with_wal_size_limit(cfg.wal_size_limit_mb.saturating_mul(1024 * 1024))
        .with_wal_sync_mode(wal_sync_mode)
        .build()
        .map(|storage| StorageHandle {
            storage,
            writer_shutdown: Arc::new(AtomicBool::new(false)),
        })
        .map_err(|e| RsIotError::HistoryEngine(e.to_string()))
}

/// writer 退出信号访问面（仅 history 模块内 engine/writer 互用）。
impl StorageHandle {
    /// 置位退出信号：挂靠 writer 在 ≤1 个攒批窗内排空退出（engine.close 用）。
    pub(crate) fn request_writer_shutdown(&self) {
        self.writer_shutdown.store(true, Ordering::SeqCst);
    }

    /// 退出信号是否已置位（writer_loop 轮询用）。
    pub(crate) fn writer_shutdown_requested(&self) -> bool {
        self.writer_shutdown.load(Ordering::SeqCst)
    }
}

/// [`crate::history::HistoryReader`] 的引擎侧实现（Phase 2 Task 2/3）：
/// 持 StorageHandle clone 与 writer/引擎共享同一底层存储；hwid 参与系列
/// 身份（labels=[devid,hwid]，调用点传入）。
///
/// raw 与 downsample 分支均经 `select_with_options` 接线（见 [`EngineReader::
/// select`] 的桶语义文档）。
struct EngineReader {
    /// tsink 存储句柄（clone，共享同一 Arc）——select 经 Deref 直调
    /// `select_with_options`。
    storage: StorageHandle,
    /// 进程 hwid：series 身份 = metric + labels{devid, hwid}，engine 不存
    /// hwid、调用点传入（lib.rs 传进程 hwid，测试传铺数据同值 hwid）。
    hwid: String,
}

impl crate::history::HistoryReader for EngineReader {
    /// 单 series 查询（raw 或降采样）：
    ///
    /// - labels=`[devid,hwid]` 锚定单 series；半开窗 `[start_ms,end_ms)` 与
    ///   limit 全下推 `select_with_options` 承载；
    /// - downsample 桶语义（P10，tsink 基线冻结，不自作聪明补点）：桶
    ///   origin=**查询 start**（非 epoch 对齐）；空桶省略（无 gap filling）；
    ///   桶点 ts=桶起点毫秒；limit 在聚合后截断——取**前 n 桶**（经
    ///   `with_pagination` 继续下推，tsink 语义：聚合物上 offset+limit）；
    /// - Err 串分层（P12，供 REST 400/500 映射）：参数类 tsink 错误带
    ///   `bad request: ` 前缀，其余引擎类带 `history engine: ` 前缀（见
    ///   [`classify_err`]）。
    fn select(
        &self,
        metric: &str,
        devid: &str,
        q: &crate::history::HistoryQuery,
    ) -> Result<Vec<crate::history::HistoryPoint>, String> {
        // 公共底座（P2/P3）：labels + 半开窗 + limit 下推；downsample 在其上
        // 叠加聚合（with_downsample 同时设 interval 与 aggregation）。
        let opts = QueryOptions::new(q.start_ms, q.end_ms)
            .with_labels(series_labels(devid, &self.hwid))
            .with_pagination(0, Some(q.limit));
        let opts = match q.downsample {
            Some((interval_ms, agg)) => opts.with_downsample(interval_ms, map_agg(agg)),
            None => opts,
        };
        let points = self
            .storage
            .select_with_options(metric, opts)
            .map_err(classify_err)?;
        Ok(map_points(points))
    }
}

/// series labels 契约 `[devid, hwid]`（字典序，写路径 Task 4 锚定）：
/// raw 与 downsample 共用——series 身份 = metric + labels{devid, hwid}。
fn series_labels(devid: &str, hwid: &str) -> Vec<Label> {
    vec![Label::new("devid", devid), Label::new("hwid", hwid)]
}

/// series_count 缓存 TTL（M6）：生产 5s——sysstatus 5s 轮询与 15s scrape
/// 均命中，list_metrics 的 O(series) 全量分配（name String + labels Vec）
/// 频率收敛到 ≤1/TTL；测试档缩短到 2s 使过期路径可测（engine 单测
/// `telemetry_series_count_ttl_cached` 的字面 sleep 与此值耦合）。
#[cfg(not(test))]
const SERIES_CACHE_TTL: Duration = Duration::from_secs(5);
#[cfg(test)]
const SERIES_CACHE_TTL: Duration = Duration::from_secs(2);

/// [`crate::history::HistoryTelemetry`] 的引擎侧实现（Phase 3 Task 2，
/// P4/P5）：持 StorageHandle clone（Arc 语义）与 writer/引擎共享同一底层
/// 存储——engine close 后句柄仍存活，读数进入 StorageClosed 可观测态
/// （`list_metrics` Err → 0 + debug 日志，不 panic）。
struct EngineTelemetry {
    /// tsink 存储句柄（clone，共享同一 Arc）——三读数经 Deref 直调
    /// `dyn Storage` 方法。
    storage: StorageHandle,
    /// series_count TTL 缓存（M6）：`(采集时刻, 值)`；None=未采集。采集
    /// 失败（Err→0）不写缓存——瞬时故障不被冻结 0 一个 TTL。
    series_cache: Mutex<Option<(Instant, u64)>>,
}

impl crate::history::HistoryTelemetry for EngineTelemetry {
    fn memory_used_bytes(&self) -> u64 {
        self.storage.memory_used() as u64
    }

    fn memory_budget_bytes(&self) -> u64 {
        self.storage.memory_budget() as u64
    }

    fn series_count(&self) -> u64 {
        // M6 TTL 缓存：命中直接返回，过期（或未采集）才走 list_metrics。
        // 锁中毒取残值——缓存 worst case 是过期数据，读比崩好（临界区无
        // await/纯内存赋值，中毒仅意味着曾有人在临界区 panic）。
        let mut cached = self.series_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, v)) = &*cached {
            if at.elapsed() < SERIES_CACHE_TTL {
                return *v;
            }
        }
        let v = match self.storage.list_metrics() {
            Ok(v) => v.len() as u64,
            Err(e) => {
                tracing::debug!("[history] telemetry list_metrics 失败(按 0 计): {e}");
                return 0;
            }
        };
        *cached = Some((Instant::now(), v));
        v
    }
}

/// HistoryAgg → tsink Aggregation（P3 六值同名直映）。
///
/// HistoryAgg 六变体恰好覆盖 tsink 的六个同名变体，穷尽 match 即可——
/// 同 crate 内 `#[non_exhaustive]` 不生效，补 `_ =>` 反成不可达模式
/// （`-D warnings` 下编译错）。
fn map_agg(agg: crate::history::HistoryAgg) -> Aggregation {
    use crate::history::HistoryAgg as HA;
    match agg {
        HA::Min => Aggregation::Min,
        HA::Max => Aggregation::Max,
        HA::Avg => Aggregation::Avg,
        HA::Sum => Aggregation::Sum,
        HA::Count => Aggregation::Count,
        HA::Last => Aggregation::Last,
    }
}

/// tsink 错误串化分层（P12）：参数类（InvalidTimeRange/InvalidConfiguration/
/// UnsupportedAggregation/OutOfRetention——调用方可修正的输入问题）带
/// `bad request: ` 前缀 → REST 层映射 400；其余引擎类带 `history engine: `
/// 前缀 → 500。raw 分支同步套用（该路径现只有参数类错误可能，行为不变）。
fn classify_err(e: TsinkError) -> String {
    if matches!(
        e,
        TsinkError::InvalidTimeRange { .. }
            | TsinkError::InvalidConfiguration(_)
            | TsinkError::UnsupportedAggregation { .. }
            | TsinkError::OutOfRetention { .. }
    ) {
        format!("bad request: {e}")
    } else {
        format!("history engine: {e}")
    }
}

/// tsink 点集 → HistoryPoint（P4）：`value_to_sample` 类型映射 + ts_ms 直传
/// （downsample 桶点 ts=桶起点毫秒）。raw 与 downsample 共用，勿复制粘贴。
fn map_points(points: Vec<DataPoint>) -> Vec<crate::history::HistoryPoint> {
    points
        .into_iter()
        .map(|p| crate::history::HistoryPoint {
            ts_ms: p.timestamp,
            value: value_to_sample(&p.value),
        })
        .collect()
}

/// tsink `Value` → `SampleValue`（P4）：写路径 lane 只落 F64/I64/Bool/String
/// 四型，前四型直映（类型保真）；U64 经 i64 饱和转 `Int`——`u64 > i64::MAX`
/// 时取 `i64::MAX`（R8：现实不可达，计划风险表登记）。Bytes/Histogram 对
/// 1b 写路径不可达，防御性串化为 `Str`（不静默丢点）。
fn value_to_sample(v: &Value) -> SampleValue {
    match v {
        Value::F64(f) => SampleValue::Float(*f),
        Value::I64(i) => SampleValue::Int(*i),
        // R8：SampleValue 无 U64 变体，i64 域外饱和取 i64::MAX
        Value::U64(u) => SampleValue::Int(i64::try_from(*u).unwrap_or(i64::MAX)),
        Value::Bool(b) => SampleValue::Bool(*b),
        Value::String(s) => SampleValue::Str(s.clone()),
        // 不可达：1b lane 只写四型；防御性串化
        Value::Bytes(b) => SampleValue::Str(format!("{b:?}")),
        Value::Histogram(h) => SampleValue::Str(format!("{h:?}")),
    }
}

/// 历史引擎：持有 tsink 存储句柄、writer 发送句柄与生命周期标志（D10）。
///
/// 生命周期：`maybe_init`（启动初始化，L2 enabled 判定 + 内部 spawn writer）
/// → `handle()`（clone 发送句柄给编排层注入 `HostContext.history`）→
/// `storage()`（Phase 2 读路径与外部只读访问共享同一句柄）→ `close`（关停
/// 排空，幂等）。drop 不做隐式关闭——排空与进程锁释放必须走显式 `close`；
/// 漏调时由 tsink 最后一个 `Arc` drop 的 best-effort close 兜底。
#[derive(Debug)]
pub struct HistoryEngine {
    /// tsink 存储句柄（与 writer 线程共享同一 Arc clone；writer 正常退出路径
    /// 自行 close，close 后本句柄进入 StorageClosed 可观测态）。
    storage: StorageHandle,
    /// writer 发送句柄原件：`handle()` 从此 clone；`close` 时 take 并 drop
    /// （断 sender → writer 排空 + 最终 flush + storage.close()）。
    handle: Option<HistoryHandle>,
    /// writer 线程 join 句柄（`close` 时取出 join，带超时看门狗防挂死）。
    writer_join: Option<std::thread::JoinHandle<()>>,
    /// 幂等 close 标志：置位后二次 `close` 为 no-op（tsink 的 close 本身不幂等）。
    closed: bool,
}

/// L2 判定 + 引擎初始化：`enabled=false` 返回 `None`（不 `create_dir_all`、
/// 不获取 `.tsink.lock`，零副作用）；`enabled=true` 返回 `Some(初始化结果)`。
///
/// `cardinality_limit>0` 用配置显式值，否则按 [`auto_cardinality`] 从 effective
/// 快照自动校准。初始化失败以 `Err` 交还调用方（P8：由编排层记 `error!` 日志
/// 并停用归档，主程序继续——旁路 fail-soft），本函数不吞错。
///
/// H4：配置边界校验在 spawn_scoped **之前**——非法配置（零值/超上限）走既有
/// init_failed Err 通道（`RsIotError::Config`），不进入乘法换算/组装路径。
pub fn maybe_init(
    cfg: &HistoryConfig,
    effective: &HashMap<String, TagProps>,
) -> Option<TResult<HistoryEngine>> {
    if !cfg.enabled {
        return None;
    }
    if let Err(msg) = cfg.validate() {
        return Some(Err(RsIotError::Config(format!(
            "[history] 配置校验失败: {msg}"
        ))));
    }
    // 陷阱 4（2026-08-26 本机复现）：tsink StorageBuilder build 深链在 1MiB
    // 线程（Windows debug 主线程）栈溢出（0xC00000FD）——与 D3 writer 线程
    // 同一缓解：init 全程放 8MiB 专线执行，调用方栈深无关。scope 借用
    // cfg/effective 免 clone；join 解包传播子线程 panic。
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(s, || maybe_init_impl(cfg, effective))
            .expect("spawn init 8MiB 专线线程必须成功")
            .join()
            .expect("init 8MiB 专线线程 panic 必须传播给调用方")
    })
}

/// [`maybe_init`] 的 init 深链本体（陷阱 4：只在 8MiB 专线线程上执行）：
/// enabled 判定在 [`maybe_init`] 入口完成（disabled 零副作用不 spawn），
/// 此处恒走组装路径。
fn maybe_init_impl(
    cfg: &HistoryConfig,
    effective: &HashMap<String, TagProps>,
) -> Option<TResult<HistoryEngine>> {
    let cardinality = if cfg.cardinality_limit > 0 {
        cfg.cardinality_limit
    } else {
        auto_cardinality(effective)
    };
    Some(build_storage(cfg, cardinality).and_then(|storage| {
        // engine 内部接线（Task 5）：storage clone 给 writer（共享同一 Arc），
        // handle/join 存入 engine——编排层经 `handle()` 取 clone，不经返回值。
        // spawn 失败（M3/P8：线程创建资源耗尽）串化进既有 init_failed Err
        // 通道（error! + 引擎 None + 主程序继续），不再 panic 击穿启动序。
        let (handle, writer_join) =
            spawn_writer(storage.clone(), cfg).map_err(RsIotError::HistoryEngine)?;
        Ok(HistoryEngine {
            storage,
            handle: Some(handle),
            writer_join: Some(writer_join),
            closed: false,
        })
    }))
}

impl HistoryEngine {
    /// writer 发送句柄（clone 语义与 tokio mpsc `Sender` 一致）：writer 存活时
    /// `Some`；编排层注入 `HostContext.history`，pipeline flush 出口 fan_out 用。
    pub fn handle(&self) -> Option<HistoryHandle> {
        self.handle.clone()
    }

    /// 历史读句柄（Phase 2 Task 2，P1）：clone 存储句柄包成
    /// `Arc<dyn HistoryReader>` 交 REST 层（AppState.history_reader），与
    /// writer/引擎共享同一底层存储 Arc。
    ///
    /// `hwid` 由调用点传入（engine 不存）：series 身份 = metric +
    /// labels{devid, hwid}，reader 需知道 hwid 才能构造精确 labels——lib.rs
    /// 传进程 hwid，测试传铺数据同值 hwid。
    pub fn reader(&self, hwid: &str) -> Arc<dyn crate::history::HistoryReader> {
        Arc::new(EngineReader {
            storage: self.storage.clone(),
            hwid: hwid.to_string(),
        })
    }

    /// 治理观测句柄（Phase 3 Task 2，P4/P5）：clone 存储句柄包成
    /// `Arc<dyn HistoryTelemetry>` 交 REST 层（AppState.history_telemetry），
    /// 读 tsink 内存水位/预算/series 数（close 后仍可调——StorageHandle
    /// 是 Arc clone，存活即读，Err 按 0，见 [`EngineTelemetry`]）。
    pub fn telemetry(&self) -> Arc<dyn crate::history::HistoryTelemetry> {
        Arc::new(EngineTelemetry {
            storage: self.storage.clone(),
            series_cache: Mutex::new(None),
        })
    }

    /// tsink 存储句柄引用（Phase 2 读路径与外部只读访问共享同一句柄）。
    pub fn storage(&self) -> &StorageHandle {
        &self.storage
    }

    /// 关闭引擎（幂等，D10）：`closed` 标志保证二次调用 no-op——tsink 的
    /// `close()` 本身不幂等（二次返 `StorageClosed`），本包装屏蔽该差异。
    ///
    /// 排空序：置位共享退出信号 + take 并 drop `handle`（writer 在 ≤1 个攒批
    /// 窗内排空残余 + 最终 flush + `storage.close()`，进程锁随之释放——clone
    /// 存活也不阻塞退出，见 [`StorageHandle`] 文档）→ join **带超时看门狗**
    /// （std JoinHandle 无 join_timeout，以辅助线程包 join + `recv_timeout`
    /// 实现）；超时仅 `warn!` 放弃等待（线程脱落，进程将退可接受），未落盘
    /// 数据由 WAL 回放兜底——关停路径不因归档失败中断主序。writer 正常路径
    /// 毫秒级完成，返回后 `storage()` 的写入即以 `StorageClosed` 被拒（排空
    /// 已发生）。
    pub fn close(&mut self, timeout: Duration) {
        if self.closed {
            return;
        }
        self.closed = true;
        // 退出信号先行：`HistoryHandle` clone 在 close 后仍可存活（HostContext/
        // 测试侧），断 sender 不足以让 writer 退出——置位共享标志（≤1 攒批窗
        // 生效），再断 engine 侧 sender。
        self.storage.request_writer_shutdown();
        self.handle = None;
        let join = match self.writer_join.take() {
            Some(j) => j,
            None => return,
        };
        // join 看门狗：辅助线程包 join，主线程 recv_timeout 等待；超时则辅助
        // 线程脱落（不 join 它——它还阻塞在被放弃的 join 上）。writer panic
        // 只记日志不二次传播（关停路径 fail-soft，WAL 回放兜底）。
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watcher = std::thread::spawn(move || {
            if let Err(panic) = join.join() {
                tracing::error!("[history] writer 线程 panic 退出(排空未完成): {panic:?}");
            }
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(timeout) {
            Ok(()) => {
                // writer 已排空并 close 完毕；watcher 发信后即返回，join 它回收。
                let _ = watcher.join();
                tracing::info!("[history] writer 排空完成,存储已 close(进程锁已释放)");
            }
            Err(_) => {
                tracing::warn!(
                    "[history] close 超时({timeout:?})放弃,WAL 回放兜底(writer 线程脱落)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HistoryConfig;
    use crate::types::TagProps;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tsink::{DataPoint, Label, Row, Value};

    /// 固定毫秒时间戳样本（2023-11-14T22:13:20.123Z）：精度锚点 + 值保真回读基准。
    const TS_MS: i64 = 1_700_000_000_123;

    /// 唯一临时目录：tag + pid + 纳秒时钟，保证测试并行与 CI 并发 run 不冲突。
    /// 不在 helper 内清理（Windows 句柄释放有延迟），清理责任在调用方 best-effort。
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rsiot-history-engine-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// 测试配置：默认值 + 指定 data_dir，唯 retention_days 放宽为 36_500（≈100 年）。
    ///
    /// 原因：TS_MS 是 2023 年的历史时间戳，而 build_storage 按计划配置
    /// `with_retention`（=enforced），tsink 会在 ingest 侧以 `OutOfRetention`
    /// 拒掉早于 now−retention 的整批写入（prepare.rs validate_points_against_retention）；
    /// 默认 30 天保不住本锚点要求的固定历史 ts，故测试统一放宽保留期。
    fn cfg_with_dir(dir: &Path) -> HistoryConfig {
        HistoryConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            retention_days: 36_500,
            ..HistoryConfig::default()
        }
    }

    /// 与生产 writer 约定一致的 series labels：devid/hwid（P1/Task 4 契约）。
    fn series_labels() -> Vec<Label> {
        vec![Label::new("devid", "dev1"), Label::new("hwid", "hw1")]
    }

    /// 构造 effective 快照：n_true 个 history=true + n_false 个 false 的 tag。
    fn effective_with(n_true: usize, n_false: usize) -> HashMap<String, TagProps> {
        let mut effective = HashMap::new();
        for i in 0..n_true {
            effective.insert(
                format!("dev1.t{i}"),
                TagProps {
                    history: true,
                    ..TagProps::default()
                },
            );
        }
        for i in 0..n_false {
            effective.insert(
                format!("dev1.f{i}"),
                TagProps {
                    history: false,
                    ..TagProps::default()
                },
            );
        }
        effective
    }

    /// 1. 打开→可用→close→关闭后写入被拒：maybe_init(enabled=true) 产出引擎，
    ///    storage() 返回可用句柄；close 后 insert_rows 以 tsink StorageClosed 被拒；
    ///    close 幂等（内部 closed 标志），二次调用为 no-op 不 panic。
    #[test]
    fn engine_open_close_roundtrip() {
        let dir = unique_dir("roundtrip");
        // 红阶段构造缺陷修复(非断言,绿阶段代理显式报告):本测锚定 enabled=true
        // (函数头注释与下方 expect 消息),而 cfg_with_dir 继承默认 enabled=false,
        // 与测试 4 的 enabled=false→None 断言互斥——任何实现都无法同时满足,
        // 补显式 true 使构造与断言意图一致。断言本身零改动。
        let cfg = HistoryConfig {
            enabled: true,
            ..cfg_with_dir(&dir)
        };
        let effective = HashMap::new();

        let mut engine = maybe_init(&cfg, &effective)
            .expect("enabled=true 必须返回 Some")
            .expect("临时目录上引擎初始化必须成功");

        let probe = Row::with_labels("m_probe", series_labels(), DataPoint::new(TS_MS, 1.25f64));
        engine
            .storage()
            .insert_rows(&[probe])
            .expect("close 前 storage() 必须可用");

        engine.close(Duration::from_secs(cfg.close_timeout_secs));

        let probe_after = Row::with_labels(
            "m_probe",
            series_labels(),
            DataPoint::new(TS_MS + 1, 2.5f64),
        );
        let res = engine.storage().insert_rows(&[probe_after]);
        assert!(
            matches!(res, Err(tsink::TsinkError::StorageClosed)),
            "close 后 insert_rows 必须以 StorageClosed 失败，实际: {res:?}"
        );

        // 幂等 close：二次调用 no-op，不得 panic/报错
        engine.close(Duration::from_secs(15));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2. 四型值保真持久化：F64/I64/Bool/String 各一 series 写入 → close →
    ///    同目录重开 → 每 metric 恰好单点且值精确回读。
    #[test]
    fn engine_persists_rows() {
        let dir = unique_dir("persist");
        let cfg = cfg_with_dir(&dir);

        let storage = build_storage(&cfg, 100).expect("首次 build_storage 必须成功");
        let rows = [
            Row::with_labels("m_f64", series_labels(), DataPoint::new(TS_MS, 42.5f64)),
            Row::with_labels("m_i64", series_labels(), DataPoint::new(TS_MS, -7i64)),
            Row::with_labels("m_bool", series_labels(), DataPoint::new(TS_MS, true)),
            Row::with_labels("m_str", series_labels(), DataPoint::new(TS_MS, "txt")),
        ];
        storage.insert_rows(&rows).expect("四行写入必须成功");
        storage.close().expect("close 排空并释放进程锁");

        let reopened = build_storage(&cfg, 100).expect("close 后同目录重开必须成功");

        let cases = [
            ("m_f64".to_string(), Value::F64(42.5)),
            ("m_i64".to_string(), Value::I64(-7)),
            ("m_bool".to_string(), Value::Bool(true)),
            ("m_str".to_string(), Value::String("txt".to_string())),
        ];
        for (metric, expected) in cases {
            let pts = reopened
                .select(&metric, &series_labels(), 0, i64::MAX)
                .expect("重开后 select 必须成功");
            assert_eq!(pts.len(), 1, "metric {metric} 应恰好回读一个点");
            assert_eq!(pts[0].value, expected, "metric {metric} 值类型与值必须保真");
        }
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 3. 进程锁互斥：同目录两个存活 storage，第二个 build 必须失败且错误含
    ///    "lock"（tsink InvalidConfiguration("…already locked…") 语义）。
    #[test]
    fn engine_lock_exclusive() {
        let dir = unique_dir("lock");
        let cfg = cfg_with_dir(&dir);

        let first = build_storage(&cfg, 8).expect("首个 storage 获取 .tsink.lock");
        let err = build_storage(&cfg, 8).expect_err("同目录第二个 storage 必须被锁拒绝");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("lock"),
            "错误串应含 lock（进程锁互斥），实际: {msg}"
        );

        first.close().expect("首个 close 释放锁");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 4. L2 关闭零副作用：enabled=false → maybe_init 返回 None，且既不创建
    ///    data_dir 也不落 .tsink.lock（父路径本身不存在的全新子目录）。
    #[test]
    fn engine_disabled_no_dir() {
        let dir = unique_dir("disabled").join("history");
        let cfg = cfg_with_dir(&dir);
        let effective = effective_with(3, 0);

        assert!(
            maybe_init(&cfg, &effective).is_none(),
            "enabled=false 必须返回 None"
        );
        assert!(!dir.exists(), "disabled 不得创建 data_dir");
        assert!(
            !dir.join(".tsink.lock").exists(),
            "disabled 不得创建进程锁文件"
        );
    }

    /// 5. cardinality 自动校准：history=true 计数 ×1.2 向上取整，白名单外不计数，
    ///    空表/全 false 兜底 ≥1。
    #[test]
    fn engine_cardinality_auto() {
        // 5 白名单 + 3 非白名单 → ceil(5×1.2)=6（非白名单不计数）
        assert_eq!(auto_cardinality(&effective_with(5, 3)), 6);
        // 10 白名单 → ceil(10×1.2)=12
        assert_eq!(auto_cardinality(&effective_with(10, 0)), 12);
        // 空保护：0 白名单（空表 / 全 false）至少 1
        assert!(auto_cardinality(&HashMap::new()) >= 1, "空 effective 需 ≥1");
        assert!(
            auto_cardinality(&effective_with(0, 3)) >= 1,
            "全 false effective 需 ≥1"
        );
    }

    /// 6. 毫秒精度回读：写入 ts=1_700_000_000_123 → select 回读点 timestamp
    ///    精确等于原 ms 值（锁死"builder 默认纳秒"陷阱：必须显式
    ///    with_timestamp_precision(Milliseconds)）。
    #[test]
    fn engine_precision_ms() {
        let dir = unique_dir("precision");
        let cfg = cfg_with_dir(&dir);

        let storage = build_storage(&cfg, 8).expect("build_storage 必须成功");
        let row = Row::with_labels("m_ms", series_labels(), DataPoint::new(TS_MS, 1.0f64));
        storage.insert_rows(&[row]).expect("写入 ms 点必须成功");

        let pts = storage
            .select("m_ms", &series_labels(), 0, i64::MAX)
            .expect("select 必须成功");
        assert_eq!(pts.len(), 1, "应恰好回读一个点");
        assert_eq!(
            pts[0].timestamp, TS_MS,
            "timestamp 必须以毫秒精度精确回读（builder 需显式配置 Milliseconds）"
        );

        storage.close().expect("close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 7. 治理观测面（P4/P5）：init（memory_limit_mb=64，临时目录）→ 直接经
    ///    storage 写 2 个不同 metric series 各 1 行 → `telemetry()` 装箱为
    ///    `Arc<dyn HistoryTelemetry>` 轮询断言：预算=64MiB（D6 即配即读）、
    ///    内存水位 >0、series 计数=2；close 后 telemetry 读数仍可调用不 panic
    ///    （值断言 ≥0 即可——u64 值域天然 ≥0，取值成功即满足；显式 `>= 0`
    ///    恒真比较会被 clippy absurd_extreme_comparisons 拒，故以取值代替）。
    #[test]
    fn telemetry_memory_and_series() {
        let dir = unique_dir("telemetry");
        let cfg = HistoryConfig {
            enabled: true,
            memory_limit_mb: 64,
            // 写 2 个 series 需基数上限 ≥2：空 effective 下 auto 校准兜底 1，
            // 会以 CardinalityLimitExceeded 拒掉第二个 series，故显式放宽。
            cardinality_limit: 8,
            ..cfg_with_dir(&dir)
        };

        let mut engine = maybe_init(&cfg, &HashMap::new())
            .expect("enabled=true 必须返回 Some")
            .expect("临时目录上引擎初始化必须成功");

        let rows = [
            Row::with_labels("m_tel_a", series_labels(), DataPoint::new(TS_MS, 1.5f64)),
            Row::with_labels(
                "m_tel_b",
                series_labels(),
                DataPoint::new(TS_MS + 1, 2.5f64),
            ),
        ];
        engine
            .storage()
            .insert_rows(&rows)
            .expect("两 series 各 1 行写入必须成功");

        // 红阶段构造修正(非断言改动,绿阶段代理显式报告):计划调用面写作
        // `rs_iot::history::HistoryTelemetry`,但 crate 自名路径在 lib 单元
        // 测试内不可解析(E0433,自名引用仅集成测试/外部 crate 可用)——改用
        // crate 内规范等价路径 `crate::history::` 引用同一 trait,断言语义零改动。
        let t: std::sync::Arc<dyn crate::history::HistoryTelemetry> = engine.telemetry();

        // 轮询 5s（50ms 步进）：写入与记账/series 可见之间可能存在传播窗。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let ready = t.memory_budget_bytes() == 64 * 1024 * 1024
                && t.memory_used_bytes() > 0
                && t.series_count() == 2;
            if ready || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            t.memory_budget_bytes(),
            64 * 1024 * 1024,
            "预算必须等于 D6 配置 64MiB"
        );
        assert!(t.memory_used_bytes() > 0, "写入后内存水位必须 >0");
        assert_eq!(
            t.series_count(),
            2,
            "2 个不同 metric series 各 1 行，计数必须为 2"
        );

        engine.close(Duration::from_secs(cfg.close_timeout_secs));
        // close 后读数不 panic：实现自由（恒 0/冻结值/Err→0 皆可），u64 值域恒 ≥0。
        let _after_close: u64 = t.series_count();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 8. trait object 面（P4）：`telemetry()` 返回值直接绑定
    ///    `Arc<dyn HistoryTelemetry>` 并调用三方法——锁死 trait 的 dyn 兼容
    ///    （未来误加泛型方法/关联项时本测试编译即红）。无写入即读：预算恒
    ///    等于 D6 配置值（即配即读，不需轮询）；used/series 仅证明经 dyn
    ///    面可调，无写入时 used 可为 0，不作值断言。
    #[test]
    fn telemetry_trait_object_safe() {
        let dir = unique_dir("tel-object");
        let cfg = HistoryConfig {
            enabled: true,
            memory_limit_mb: 64,
            ..cfg_with_dir(&dir)
        };

        let mut engine = maybe_init(&cfg, &HashMap::new())
            .expect("enabled=true 必须返回 Some")
            .expect("临时目录上引擎初始化必须成功");

        // 路径修正同 telemetry_memory_and_series 测试:crate:: 等价路径。
        let t: std::sync::Arc<dyn crate::history::HistoryTelemetry> = engine.telemetry();
        let _used = t.memory_used_bytes();
        let budget = t.memory_budget_bytes();
        let _series = t.series_count();
        assert_eq!(budget, 64 * 1024 * 1024, "无写入时预算也必须等于配置 64MiB");

        engine.close(Duration::from_secs(cfg.close_timeout_secs));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 9. 配置校验前置（H4）：非法配置（memory_limit_mb=1<<40 超上限）→
    ///    maybe_init 返回 Err 而非组装引擎返回 Ok——init_failed fail-soft
    ///    通道（编排层 error! + 引擎 None + /history 503 + 主程序继续）。
    ///    当前实现无校验直接以巨值组装（红：Ok）。
    #[test]
    fn engine_invalid_config_err_not_panic() {
        let dir = unique_dir("invalid-cfg");
        let cfg = HistoryConfig {
            enabled: true,
            memory_limit_mb: 1 << 40,
            ..cfg_with_dir(&dir)
        };

        let res = maybe_init(&cfg, &HashMap::new());

        assert!(
            matches!(res, Some(Err(_))),
            "非法配置必须经 Err 通道返回（init_failed fail-soft），实际: {res:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 10. series_count TTL 缓存（M6）：sysstatus 5s 轮询与 15s scrape 不再
    ///     每次触发 list_metrics 的 O(series) 全量分配——TTL 内重复调用命中
    ///     缓存（直插第 2 series 不可见），TTL 过期后重采。红：当前无缓存，
    ///     第 2 series 对引擎可见后立即再调返回 2。
    #[test]
    fn telemetry_series_count_ttl_cached() {
        let dir = unique_dir("tel-ttl");
        let cfg = HistoryConfig {
            enabled: true,
            memory_limit_mb: 64,
            // 两 series 需基数上限 ≥2（auto 校准空 effective 兜底 1 会拒第二 series）
            cardinality_limit: 8,
            ..cfg_with_dir(&dir)
        };
        let mut engine = maybe_init(&cfg, &HashMap::new())
            .expect("enabled=true 必须返回 Some")
            .expect("临时目录上引擎初始化必须成功");

        let t = engine.telemetry();
        engine
            .storage()
            .insert_rows(&[Row::with_labels(
                "m_tel_c1",
                series_labels(),
                DataPoint::new(TS_MS, 1.0f64),
            )])
            .expect("首 series 写入必须成功");
        // 轮询首采 ==1（写入与 series 记账间有传播窗）
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while t.series_count() != 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(t.series_count(), 1, "首采必须为 1");

        // 绕过 telemetry 直插第 2 series，并等它对 list_metrics 可见
        engine
            .storage()
            .insert_rows(&[Row::with_labels(
                "m_tel_c2",
                series_labels(),
                DataPoint::new(TS_MS + 1, 2.0f64),
            )])
            .expect("第 2 series 写入必须成功");
        let visible = |n: usize| engine.storage().list_metrics().is_ok_and(|v| v.len() == n);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !visible(2) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(visible(2), "第 2 series 必须已对引擎可见（缓存断言的前置）");
        // TTL 内立即再调：必须命中缓存仍 1（红：当前实现此处返回 2）
        assert_eq!(t.series_count(), 1, "TTL 内必须命中缓存返回旧值 1");

        // TTL 过期后重采 ==2（此处字面量须与 cfg(test) 档 SERIES_CACHE_TTL 一致）
        std::thread::sleep(Duration::from_millis(2_150));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while t.series_count() != 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(t.series_count(), 2, "TTL 过期后必须重采为 2");

        engine.close(Duration::from_secs(cfg.close_timeout_secs));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
