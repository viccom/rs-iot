//! 单写线程（Task 4 实现）：8 MiB 栈（D3/陷阱 4）+ 50ms 超时 recv 攒批 +
//! 批内同 series 同 ts 去重（P2 保后写）+ deadband/last_ts/lane 防混型状态机
//! （P3/P4/P5）→ tsink insert_rows（D4/D5）。
//!
//! 本文件生产项全部 `#[cfg(feature = "history")]`（mod.rs 的 `pub mod writer;`
//! 无条件声明，L1 门控落在本文件项级）；`rows` 模块的 `HistoryHandle` 无条件
//! 编译，故 spawn 之外的调用点零 cfg 散布（P6）。

#[cfg(feature = "history")]
use std::collections::HashMap;
#[cfg(feature = "history")]
use std::sync::atomic::Ordering;
#[cfg(feature = "history")]
use std::sync::Arc;
#[cfg(feature = "history")]
use std::time::{Duration, Instant};

#[cfg(feature = "history")]
use tokio::sync::mpsc;
#[cfg(feature = "history")]
use tsink::{DataPoint, Label, Row as TsinkRow, Value as TsinkValue};

#[cfg(feature = "history")]
use crate::config::HistoryConfig;
#[cfg(feature = "history")]
use crate::history::engine::StorageHandle;
#[cfg(feature = "history")]
use crate::history::rows::{HistoryHandle, HistoryRow, HistoryStats};
#[cfg(feature = "history")]
use crate::types::SampleValue;

/// 攒批窗口：recv 超时即 flush（与 pipeline 50ms 攒批节奏对齐，D5）。
#[cfg(feature = "history")]
const BATCH_WINDOW: Duration = Duration::from_millis(50);

/// insert 失败告警节流窗：1 秒内多次整批失败只 warn 一条（防日志风暴）。
#[cfg(feature = "history")]
const ERR_WARN_THROTTLE: Duration = Duration::from_secs(1);

/// series 类型车道（lane）：以 tsink `Value` 变体为准——Float→F64、Int→I64、
/// Bool→Bool、Str→String。Float 与 Int 虽同为数值但 Value 变体不同，互变也拒
/// （P3：tsink ValueTypeMismatch 拒整批，写入侧自律单 series 单变体）。
#[cfg(feature = "history")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// 浮点车道（SampleValue::Float → tsink Value::F64）。
    F64,
    /// 整数车道（SampleValue::Int → tsink Value::I64）。
    I64,
    /// 布尔车道。
    Bool,
    /// 字符串车道。
    String,
}

/// flush 状态机 per-series 状态（key = `(devid, tag)`）。
#[cfg(feature = "history")]
#[derive(Debug)]
struct SeriesState {
    /// 既有车道；None = 首见。型变行剔除并计数（P3）。
    lane: Option<Lane>,
    /// 最近一次**已归档**的数值（PI exception 语义：剔行者不更新；非数值恒 None）。
    last_val: Option<f64>,
    /// 最近一次已归档的毫秒 ts；相等即跳过不写不更新（P5 防跨批双点）。
    /// 初始化 i64::MIN 避开合法的 epoch 0 值。
    last_ts: i64,
}

#[cfg(feature = "history")]
impl Default for SeriesState {
    fn default() -> Self {
        Self {
            lane: None,
            last_val: None,
            last_ts: i64::MIN,
        }
    }
}

/// spawn 单写线程并返回发送句柄与线程 join（D3：8 MiB 栈，陷阱 4——默认栈
/// 不够 tsink 排空/compaction 递归）。
///
/// 线程内建一次 current_thread runtime 驱动 tokio mpsc 的超时 recv（std
/// thread 无法直接 await）：`timeout(50ms, rx.recv())` 攒批，窗口满
/// `batch_max_rows` 或超时即 flush；sender 全断（recv None）→ 排空残余 →
/// 最终 flush → `storage.close()`（tsink close 不幂等，仅此一次）→ 退出。
#[cfg(feature = "history")]
pub fn spawn_writer(
    storage: StorageHandle,
    cfg: &HistoryConfig,
) -> (HistoryHandle, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(cfg.channel_capacity);
    let stats = Arc::new(HistoryStats::default());
    let handle = HistoryHandle::new(tx, stats.clone());
    let batch_max = cfg.batch_max_rows;
    let join = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .name("history-writer".to_owned())
        .spawn(move || writer_loop(storage, stats, rx, batch_max))
        .expect("spawn history writer thread");
    (handle, join)
}

/// 写线程主循环（见 [`spawn_writer`] 文档）。
#[cfg(feature = "history")]
fn writer_loop(
    storage: StorageHandle,
    stats: Arc<HistoryStats>,
    mut rx: mpsc::Receiver<HistoryRow>,
    batch_max: usize,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // 运行时构建失败极罕见（资源耗尽）：尽力 close 释放进程锁后放弃。
            tracing::error!("[history] writer 运行时构建失败,归档写线程放弃: {e}");
            if let Err(close_err) = storage.close() {
                tracing::warn!("[history] writer 放弃路径 close 失败: {close_err}");
            }
            return;
        }
    };
    let mut writer = Writer {
        storage,
        stats,
        series: HashMap::new(),
        last_err_warn: None,
    };
    let mut buf: Vec<HistoryRow> = Vec::new();
    // 批内去重索引（P2）：(devid, tag) → (ts_ms, buf 下标)，同 series 同 ts
    // 原地替换值保后写；跨批不拦截（依赖 tsink 读取端 last-value-wins）。
    let mut dedup: HashMap<(String, String), (i64, usize)> = HashMap::new();
    loop {
        // timeout 的 Sleep 在构造期即取 runtime 计时器上下文——必须包进
        // block_on 轮询的 async 块内构造，不能在外部先建再传入。
        let received = rt.block_on(async { tokio::time::timeout(BATCH_WINDOW, rx.recv()).await });
        match received {
            Ok(Some(row)) => {
                push_dedup(&mut buf, &mut dedup, row);
                if buf.len() >= batch_max {
                    writer.flush(&mut buf, &mut dedup);
                }
            }
            Ok(None) => {
                // sender 全断：排空残余（期间已无并发写者）→ 最终 flush → 退出。
                while let Ok(row) = rx.try_recv() {
                    push_dedup(&mut buf, &mut dedup, row);
                }
                writer.flush(&mut buf, &mut dedup);
                break;
            }
            Err(_elapsed) => {
                // 窗口超时：非空即 flush（空批静默跳过）。
                if !buf.is_empty() {
                    writer.flush(&mut buf, &mut dedup);
                }
            }
        }
    }
    writer.finish();
}

/// 批内追加 + 同 series 同 ts 去重（P2 保后写）：命中索引则原地替换
/// value/deadband（后写整行胜出，buf 不增长），否则记索引并追加。
#[cfg(feature = "history")]
fn push_dedup(
    buf: &mut Vec<HistoryRow>,
    dedup: &mut HashMap<(String, String), (i64, usize)>,
    row: HistoryRow,
) {
    let key = (row.devid.clone(), row.tag.clone());
    match dedup.get(&key) {
        Some((ts_ms, idx)) if *ts_ms == row.ts_ms => {
            buf[*idx].value = row.value;
            buf[*idx].deadband = row.deadband;
        }
        _ => {
            dedup.insert(key, (row.ts_ms, buf.len()));
            buf.push(row);
        }
    }
}

/// flush 上下文：持有 storage/计数/per-series 状态与告警节流器。
#[cfg(feature = "history")]
struct Writer {
    /// tsink 存储句柄（与外部只读访问共享同一 Arc）。
    storage: StorageHandle,
    /// 旁路计数（written/deadbanded/lane_mismatch/errors 在本侧递增）。
    stats: Arc<HistoryStats>,
    /// per-series 状态机（P3/P4/P5）。
    series: HashMap<(String, String), SeriesState>,
    /// insert 失败告警节流锚点。
    last_err_warn: Option<Instant>,
}

#[cfg(feature = "history")]
impl Writer {
    /// flush 状态机（逐行，顺序即后写胜出序）：
    /// 1. lane 型变（Float↔Int 互变也拒）→ `lane_mismatch++` 剔除（P3）；
    /// 2. `last_ts == ts_ms` → 静默跳过，不更新任何状态（P5）；
    /// 3. deadband=Some 且数值且 last_val 有值且 `|new − last| < db` →
    ///    `deadbanded++` 跳过，last_val 不更新（PI exception，P4）；
    /// 4. 通过 → 进 insert 批，更新 lane/last_val（数值）/last_ts。
    ///    Bool/Str 无死区恒档（P4）。
    fn flush(
        &mut self,
        buf: &mut Vec<HistoryRow>,
        dedup: &mut HashMap<(String, String), (i64, usize)>,
    ) {
        dedup.clear();
        let mut rows: Vec<TsinkRow> = Vec::with_capacity(buf.len());
        for row in buf.drain(..) {
            let lane = lane_of(&row.value);
            let st = self
                .series
                .entry((row.devid.clone(), row.tag.clone()))
                .or_default();
            // 1. lane 型变剔除（首见 lane=None 恒过）
            if st.lane.is_some_and(|prev| prev != lane) {
                self.stats.lane_mismatch.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            // 2. 同 ts 跨批双点静默跳过
            if st.last_ts == row.ts_ms {
                continue;
            }
            // 3. 绝对死区（仅数值；剔行者 last_val 不更新）
            if let (Some(db), Some(new), Some(last)) =
                (row.deadband, numeric(&row.value), st.last_val)
            {
                if (new - last).abs() < db {
                    self.stats.deadbanded.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
            }
            // 4. 通过：更新状态并构造 insert 行
            st.lane = Some(lane);
            if let Some(v) = numeric(&row.value) {
                st.last_val = Some(v);
            }
            st.last_ts = row.ts_ms;
            rows.push(TsinkRow::with_labels(
                row.tag.clone(),
                vec![
                    Label::new("devid", row.devid.clone()),
                    Label::new("hwid", row.hwid.clone()),
                ],
                DataPoint {
                    value: to_tsink_value(row.value),
                    timestamp: row.ts_ms,
                },
            ));
        }
        if !rows.is_empty() {
            self.insert(rows);
        }
    }

    /// insert_rows + 计数 + fail-soft：Ok→written+=n；Err→errors+=n 并 warn
    /// （整批丢弃不上抛不重试，1 秒节流防日志风暴）。
    fn insert(&mut self, rows: Vec<TsinkRow>) {
        let n = rows.len() as u64;
        match self.storage.insert_rows(&rows) {
            Ok(()) => {
                self.stats.written.fetch_add(n, Ordering::SeqCst);
            }
            Err(e) => {
                self.stats.errors.fetch_add(n, Ordering::SeqCst);
                let now = Instant::now();
                let throttled = self
                    .last_err_warn
                    .is_some_and(|t| now.duration_since(t) < ERR_WARN_THROTTLE);
                if !throttled {
                    tracing::warn!("[history] insert_rows 失败(整批丢弃,fail-soft): {e}");
                    self.last_err_warn = Some(now);
                }
            }
        }
    }

    /// 退出前收尾：最终 flush 已由调用方完成，这里只 close（tsink close 不
    /// 幂等，进程生命周期内仅此一次）；失败仅 warn——排空尽力，未落盘数据
    /// 由 WAL 回放兜底。
    fn finish(&mut self) {
        if let Err(e) = self.storage.close() {
            tracing::warn!("[history] writer close 失败(排空已尽力,依赖 WAL 回放兜底): {e}");
        }
    }
}

/// 取值的车道（P3：以 tsink Value 变体为准）。
#[cfg(feature = "history")]
fn lane_of(v: &SampleValue) -> Lane {
    match v {
        SampleValue::Float(_) => Lane::F64,
        SampleValue::Int(_) => Lane::I64,
        SampleValue::Bool(_) => Lane::Bool,
        SampleValue::Str(_) => Lane::String,
    }
}

/// 数值视图（P4：deadband 仅作用于 Float/Int；Bool/Str 返回 None 恒档）。
#[cfg(feature = "history")]
fn numeric(v: &SampleValue) -> Option<f64> {
    match v {
        SampleValue::Float(x) => Some(*x),
        SampleValue::Int(x) => Some(*x as f64),
        SampleValue::Bool(_) | SampleValue::Str(_) => None,
    }
}

/// D4 类型保真映射：Float→F64、Int→I64、Bool→Bool、Str→String。
#[cfg(feature = "history")]
fn to_tsink_value(v: SampleValue) -> TsinkValue {
    match v {
        SampleValue::Float(x) => TsinkValue::F64(x),
        SampleValue::Int(x) => TsinkValue::I64(x),
        SampleValue::Bool(b) => TsinkValue::Bool(b),
        SampleValue::Str(s) => TsinkValue::String(s),
    }
}

#[cfg(all(test, feature = "history"))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    use tsink::{Label, Value};

    use super::spawn_writer;
    use crate::config::HistoryConfig;
    use crate::history::engine::build_storage;
    use crate::history::rows::HistoryRow;
    use crate::types::SampleValue;

    /// 与生产 writer 约定一致的 series labels：devid/hwid（P1/Task 4 契约）。
    fn series_labels() -> Vec<Label> {
        vec![Label::new("devid", "dev1"), Label::new("hwid", "hw1")]
    }

    /// 唯一临时目录：tag + pid + 纳秒时钟，测试并行与 CI 并发 run 不冲突；
    /// 清理责任在调用方 best-effort（Windows 句柄释放有延迟）。
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rsiot-history-writer-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// 测试配置：默认值 + 指定 data_dir（写入点均取当前时刻附近 ts，默认 30 天
    /// retention 足够覆盖，无需放宽）。
    fn cfg_with_dir(dir: &Path) -> HistoryConfig {
        HistoryConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            ..HistoryConfig::default()
        }
    }

    /// 当前 Unix 毫秒（写入点时间基）。
    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_millis() as i64
    }

    /// 固定 devid/hwid 的出口行。
    fn row(tag: &str, ts_ms: i64, value: SampleValue, deadband: Option<f64>) -> HistoryRow {
        HistoryRow {
            devid: "dev1".into(),
            hwid: "hw1".into(),
            tag: tag.into(),
            ts_ms,
            value,
            deadband,
        }
    }

    /// 轮询等待条件成立（20ms 间隔），超时返回 false。
    fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if f() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 带超时的 join：线程在时限内未退出即 panic（防挂死）；退出后 join 传播 panic。
    fn join_with_timeout(join: thread::JoinHandle<()>, timeout: Duration, ctx: &str) {
        let deadline = Instant::now() + timeout;
        while !join.is_finished() {
            assert!(
                Instant::now() < deadline,
                "{ctx}: writer 线程 join 超时（{timeout:?}）——疑似未随 sender 全_drop 而 Disconnected 退出"
            );
            thread::sleep(Duration::from_millis(10));
        }
        join.join().expect("{ctx}: writer 线程 panic 退出");
    }

    /// 从 tsink Value 提取 F64（类型漂移即 panic，带实际值便于定位）。
    fn f64_of(v: &Value) -> f64 {
        match v {
            Value::F64(x) => *x,
            other => panic!("期望 Value::F64，实际: {other:?}"),
        }
    }

    /// 5. 全量落盘：100 行（10 series × 10 个互异 ts，值各异，无死区）经 handle.send
    ///    → written==100（5s 轮询）→ drop 唯一 sender → join → 同目录重开 →
    ///    每 series 恰好回读 10 点。重开成功本身即验证 writer 退出前已 close 释放锁。
    #[test]
    fn writer_flushes_all() {
        let dir = unique_dir("flush");
        let cfg = cfg_with_dir(&dir);
        let storage = build_storage(&cfg, 100).expect("build_storage 必须成功");
        let (handle, join) = spawn_writer(storage, &cfg);
        let stats = handle.stats.clone();

        let base = now_ms();
        for s in 0..10u32 {
            for i in 0..10i64 {
                handle
                    .send(row(
                        &format!("s{s}"),
                        base + i,
                        SampleValue::Float(f64::from(s) * 100.0 + i as f64),
                        None,
                    ))
                    .expect("channel 容量远大于 100，send 必须成功");
            }
        }
        assert!(
            poll_until(Duration::from_secs(5), || {
                stats.written.load(Ordering::SeqCst) == 100
            }),
            "written 应达 100，实际 written={} dropped={}",
            stats.written.load(Ordering::SeqCst),
            stats.dropped.load(Ordering::SeqCst)
        );

        drop(handle);
        join_with_timeout(join, Duration::from_secs(10), "writer_flushes_all");
        assert_eq!(
            stats.written.load(Ordering::SeqCst),
            100,
            "join 后计数冻结在 100"
        );

        let reopened =
            build_storage(&cfg, 100).expect("writer 退出应已 close 释放进程锁，同目录重开必须成功");
        for s in 0..10u32 {
            let pts = reopened
                .select(&format!("s{s}"), &series_labels(), 0, i64::MAX)
                .expect("重开后 select 必须成功");
            assert_eq!(pts.len(), 10, "series s{s} 应恰好回读 10 个互异 ts 点");
        }
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 6. deadband 抑制（绝对死区，|new − last_archived| ≥ db 才档，首点必档，
    ///    剔行者不更新 last_archived）：值序列 0.4/0.8/1.5/2.0/2.6（互异 ts，db=1.0）
    ///    → 档 0.4（首点）、剔 0.8（|Δ|=0.4<1.0）、档 1.5（|Δ|=1.1≥1.0，last=1.5）、
    ///    剔 2.0（|Δ|=0.5<1.0）、档 2.6（|Δ|=1.1≥1.0）→ written==3、deadbanded==2；
    ///    重开 select 值集精确等于 {0.4, 1.5, 2.6}。
    #[test]
    fn writer_deadband_suppresses() {
        let dir = unique_dir("deadband");
        let cfg = cfg_with_dir(&dir);
        let storage = build_storage(&cfg, 8).expect("build_storage 必须成功");
        let (handle, join) = spawn_writer(storage, &cfg);
        let stats = handle.stats.clone();

        let base = now_ms();
        for (i, v) in [0.4f64, 0.8, 1.5, 2.0, 2.6].into_iter().enumerate() {
            handle
                .send(row(
                    "temp",
                    base + i as i64,
                    SampleValue::Float(v),
                    Some(1.0),
                ))
                .expect("send 必须成功");
        }
        assert!(
            poll_until(Duration::from_secs(5), || {
                stats.written.load(Ordering::SeqCst) == 3
                    && stats.deadbanded.load(Ordering::SeqCst) == 2
            }),
            "deadband 判定应得 written=3/deadbanded=2，实际 written={} deadbanded={}",
            stats.written.load(Ordering::SeqCst),
            stats.deadbanded.load(Ordering::SeqCst)
        );
        // 稳定期：跨至少一个攒批周期确认计数不再漂移
        thread::sleep(Duration::from_millis(200));

        drop(handle);
        join_with_timeout(join, Duration::from_secs(10), "writer_deadband_suppresses");
        assert_eq!(stats.written.load(Ordering::SeqCst), 3);
        assert_eq!(stats.deadbanded.load(Ordering::SeqCst), 2);
        assert_eq!(stats.skipped.load(Ordering::SeqCst), 0);

        let reopened = build_storage(&cfg, 8).expect("writer close 后同目录重开必须成功");
        let pts = reopened
            .select("temp", &series_labels(), 0, i64::MAX)
            .expect("重开后 select 必须成功");
        let mut got: Vec<f64> = pts.iter().map(|p| f64_of(&p.value)).collect();
        got.sort_by(|a, b| a.partial_cmp(b).expect("有限 f64 可比较"));
        assert_eq!(
            got,
            vec![0.4, 1.5, 2.6],
            "落盘值集必须精确为 {{0.4, 1.5, 2.6}}"
        );
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 7. 非数值恒档（P4：deadband 仅作用于数值，Bool/Str 无条件归档）：
    ///    Bool 与 Str 各一 series 带 deadband=Some(1.0)，各发 3 行（互异 ts）
    ///    → 全档：written==6、deadbanded==0；重开各回读 3 点。
    #[test]
    fn writer_deadband_non_numeric_archives() {
        let dir = unique_dir("deadband-nonnum");
        let cfg = cfg_with_dir(&dir);
        let storage = build_storage(&cfg, 8).expect("build_storage 必须成功");
        let (handle, join) = spawn_writer(storage, &cfg);
        let stats = handle.stats.clone();

        let base = now_ms();
        for (i, b) in [true, false, true].into_iter().enumerate() {
            handle
                .send(row("b", base + i as i64, SampleValue::Bool(b), Some(1.0)))
                .expect("send 必须成功");
        }
        for (i, s) in ["a", "b", "c"].into_iter().enumerate() {
            handle
                .send(row(
                    "t",
                    base + 100 + i as i64,
                    SampleValue::Str(s.to_string()),
                    Some(1.0),
                ))
                .expect("send 必须成功");
        }
        assert!(
            poll_until(Duration::from_secs(5), || {
                stats.written.load(Ordering::SeqCst) == 6
            }),
            "Bool/Str 应全档 written=6，实际 written={} deadbanded={}",
            stats.written.load(Ordering::SeqCst),
            stats.deadbanded.load(Ordering::SeqCst)
        );
        thread::sleep(Duration::from_millis(200));

        drop(handle);
        join_with_timeout(
            join,
            Duration::from_secs(10),
            "writer_deadband_non_numeric_archives",
        );
        assert_eq!(stats.written.load(Ordering::SeqCst), 6);
        assert_eq!(
            stats.deadbanded.load(Ordering::SeqCst),
            0,
            "P4：非数值不做死区抑制"
        );

        let reopened = build_storage(&cfg, 8).expect("writer close 后同目录重开必须成功");
        let bool_pts = reopened
            .select("b", &series_labels(), 0, i64::MAX)
            .expect("select b 必须成功");
        assert_eq!(bool_pts.len(), 3, "Bool series 全档应回读 3 点");
        let str_pts = reopened
            .select("t", &series_labels(), 0, i64::MAX)
            .expect("select t 必须成功");
        assert_eq!(str_pts.len(), 3, "Str series 全档应回读 3 点");
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 8. 批内同 series 同 ts 去重（P2 保后写）：同 ts 两行不同值（v=1.0 先发、
    ///    v=2.0 紧随，同批）→ written==1；drop+join 后重开 select 该 ts 单点且
    ///    value==2.0（后写胜出）。
    #[test]
    fn writer_batch_dedup_same_ts() {
        let dir = unique_dir("dedup");
        let cfg = cfg_with_dir(&dir);
        let storage = build_storage(&cfg, 8).expect("build_storage 必须成功");
        let (handle, join) = spawn_writer(storage, &cfg);
        let stats = handle.stats.clone();

        let ts = now_ms();
        handle
            .send(row("m", ts, SampleValue::Float(1.0), None))
            .expect("send 必须成功");
        handle
            .send(row("m", ts, SampleValue::Float(2.0), None))
            .expect("send 必须成功");

        // 进度信号：至少 1 行已写；随后跨攒批周期观察，若实现错误分两批会在此暴露
        assert!(
            poll_until(Duration::from_secs(5), || {
                stats.written.load(Ordering::SeqCst) >= 1
            }),
            "written 应至少达 1，实际 {}",
            stats.written.load(Ordering::SeqCst)
        );
        thread::sleep(Duration::from_millis(200));

        drop(handle);
        join_with_timeout(join, Duration::from_secs(10), "writer_batch_dedup_same_ts");
        assert_eq!(
            stats.written.load(Ordering::SeqCst),
            1,
            "批内同 series 同 ts 必须去重（P2），written 只计 1"
        );
        assert_eq!(stats.skipped.load(Ordering::SeqCst), 0);
        assert_eq!(stats.lane_mismatch.load(Ordering::SeqCst), 0);

        let reopened = build_storage(&cfg, 8).expect("writer close 后同目录重开必须成功");
        let pts = reopened
            .select("m", &series_labels(), 0, i64::MAX)
            .expect("重开后 select 必须成功");
        assert_eq!(pts.len(), 1, "该 ts 必须单点");
        assert_eq!(pts[0].timestamp, ts, "ts 精确回读");
        assert_eq!(
            pts[0].value,
            Value::F64(2.0),
            "保后写：value 必须是第二发的 2.0"
        );
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 9. lane 防混型（P3：ValueTypeMismatch 拒整批，写入侧自律剔除型变行）：
    ///    同 series 先 Float(1.0) 后 Str("x")（互异 ts）→ Str 行被剔不进 insert 批：
    ///    written==1、lane_mismatch==1；重开 select 值精确为 1.0（Float）。
    #[test]
    fn writer_lane_change_drops() {
        let dir = unique_dir("lane");
        let cfg = cfg_with_dir(&dir);
        let storage = build_storage(&cfg, 8).expect("build_storage 必须成功");
        let (handle, join) = spawn_writer(storage, &cfg);
        let stats = handle.stats.clone();

        let ts_float = now_ms();
        let ts_str = ts_float + 1;
        handle
            .send(row("m", ts_float, SampleValue::Float(1.0), None))
            .expect("send 必须成功");
        handle
            .send(row("m", ts_str, SampleValue::Str("x".to_string()), None))
            .expect("send 必须成功");

        assert!(
            poll_until(Duration::from_secs(5), || {
                stats.written.load(Ordering::SeqCst) >= 1
            }),
            "Float 行应正常写入，实际 written={}",
            stats.written.load(Ordering::SeqCst)
        );
        thread::sleep(Duration::from_millis(200));

        drop(handle);
        join_with_timeout(join, Duration::from_secs(10), "writer_lane_change_drops");
        assert_eq!(
            stats.written.load(Ordering::SeqCst),
            1,
            "只有首行 Float 落盘"
        );
        assert_eq!(
            stats.lane_mismatch.load(Ordering::SeqCst),
            1,
            "型变行必须剔除并计数（P3）"
        );
        assert_eq!(stats.deadbanded.load(Ordering::SeqCst), 0);

        let reopened = build_storage(&cfg, 8).expect("writer close 后同目录重开必须成功");
        let pts = reopened
            .select("m", &series_labels(), 0, i64::MAX)
            .expect("重开后 select 必须成功");
        assert_eq!(pts.len(), 1);
        assert_eq!(
            pts[0].value,
            Value::F64(1.0),
            "落盘值必须保持首行 Float(1.0)"
        );
        assert_eq!(pts[0].timestamp, ts_float);
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 10. 类型保真（D4）：Float(42.5)/Int(-7)/Bool(true)/Str("txt") 四 series 各
    ///     一行 → written==4 → 重开逐 series 断言 SampleValue 精确映射回读
    ///     （Float→F64、Int→I64、Bool→Bool、Str→String）。
    #[test]
    fn writer_type_fidelity() {
        let dir = unique_dir("fidelity");
        let cfg = cfg_with_dir(&dir);
        let storage = build_storage(&cfg, 8).expect("build_storage 必须成功");
        let (handle, join) = spawn_writer(storage, &cfg);
        let stats = handle.stats.clone();

        let ts = now_ms();
        let cases = [
            row("f", ts, SampleValue::Float(42.5), None),
            row("i", ts, SampleValue::Int(-7), None),
            row("b", ts, SampleValue::Bool(true), None),
            row("t", ts, SampleValue::Str("txt".to_string()), None),
        ];
        for r in cases {
            handle.send(r).expect("send 必须成功");
        }
        assert!(
            poll_until(Duration::from_secs(5), || {
                stats.written.load(Ordering::SeqCst) == 4
            }),
            "四型各一行应全档 written=4，实际 written={}",
            stats.written.load(Ordering::SeqCst)
        );

        drop(handle);
        join_with_timeout(join, Duration::from_secs(10), "writer_type_fidelity");
        assert_eq!(stats.written.load(Ordering::SeqCst), 4);

        let reopened = build_storage(&cfg, 8).expect("writer close 后同目录重开必须成功");
        let expected = [
            ("f", Value::F64(42.5)),
            ("i", Value::I64(-7)),
            ("b", Value::Bool(true)),
            ("t", Value::String("txt".to_string())),
        ];
        for (metric, want) in &expected {
            let pts = reopened
                .select(metric, &series_labels(), 0, i64::MAX)
                .expect("重开后 select 必须成功");
            assert_eq!(pts.len(), 1, "metric {metric} 应恰好单点");
            assert_eq!(
                &pts[0].value, want,
                "metric {metric} 值类型与值必须保真（D4）"
            );
        }
        reopened.close().expect("重开实例 close");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
