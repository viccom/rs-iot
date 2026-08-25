//! tsink 引擎组装与生命周期：StorageBuilder 资源边界组装（D6，毫秒精度）+
//! 进程锁互斥（D2）+ close 排空包装（D10）。
//!
//! L2 判定入口是 [`maybe_init`]：`enabled=false` 时零副作用（不建目录、
//! 不碰 `.tsink.lock`）直接返回 `None`。组装契约与决策依据见
//! `docs/designs/2026-08-25-history-write-path-plan.md`「Task 3」+ 速查表。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tsink::{Storage, StorageBuilder, TimestampPrecision, WalSyncMode};

use crate::config::HistoryConfig;
use crate::error::{RsIotError, TResult};
use crate::history::rows::HistoryHandle;
use crate::history::writer::spawn_writer;
use crate::types::TagProps;

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
/// 空表/全 false 兜底 `≥1`（D6）。
///
/// 仅在启动时刻校准一次，运行期开白名单不重算——超限行为是 tsink 侧
/// `CardinalityLimitExceeded` 拒新 series（计划已知边界 R4，重算留 Phase 3）。
pub fn auto_cardinality(effective: &HashMap<String, TagProps>) -> usize {
    let whitelisted = effective.values().filter(|props| props.history).count();
    (((whitelisted as f64) * 1.2).ceil() as usize).max(1)
}

/// 按配置组装 tsink 内嵌存储（D6 资源边界，组装项与顺序照计划速查表）。
///
/// - `create_dir_all` 先行：父目录不存在时 tsink 不会代建（IO 错误经既有
///   `Io(#[from])` 变体上抛）；
/// - 时间戳精度**显式毫秒**：builder 默认纳秒，是本写路径的头号陷阱；
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
        .with_memory_limit(cfg.memory_limit_mb * 1024 * 1024)
        .with_cardinality_limit(cardinality)
        .with_retention(Duration::from_secs(cfg.retention_days * 86400))
        .with_wal_size_limit(cfg.wal_size_limit_mb * 1024 * 1024)
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

/// [`crate::history::HistoryReader`] 的引擎侧实现（Phase 2 Task 1 骨架）：
/// 持 StorageHandle clone 与 writer/引擎共享同一底层存储。
///
/// Task 1 只落接口面（503/400 矩阵 + 响应形的 e2e 锚定），select 的 raw
/// 分支在 Task 2 实现——本占位诚实返回 `Err` 而非 `todo!()`（编译过且
/// 运行期可观测），Task 2 替换。
struct EngineReader {
    /// tsink 存储句柄（clone，共享同一 Arc）。占位 select 暂不读取，Task 2
    /// raw 分支接线后移除 expect。
    #[expect(dead_code)]
    storage: StorageHandle,
}

impl crate::history::HistoryReader for EngineReader {
    fn select(
        &self,
        _metric: &str,
        _devid: &str,
        _q: &crate::history::HistoryQuery,
    ) -> Result<Vec<crate::history::HistoryPoint>, String> {
        Err("raw select 待 Task 2 实现".to_string())
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
pub fn maybe_init(
    cfg: &HistoryConfig,
    effective: &HashMap<String, TagProps>,
) -> Option<TResult<HistoryEngine>> {
    if !cfg.enabled {
        return None;
    }
    let cardinality = if cfg.cardinality_limit > 0 {
        cfg.cardinality_limit
    } else {
        auto_cardinality(effective)
    };
    Some(build_storage(cfg, cardinality).map(|storage| {
        // engine 内部接线（Task 5）：storage clone 给 writer（共享同一 Arc），
        // handle/join 存入 engine——编排层经 `handle()` 取 clone，不经返回值。
        let (handle, writer_join) = spawn_writer(storage.clone(), cfg);
        HistoryEngine {
            storage,
            handle: Some(handle),
            writer_join: Some(writer_join),
            closed: false,
        }
    }))
}

impl HistoryEngine {
    /// writer 发送句柄（clone 语义与 tokio mpsc `Sender` 一致）：writer 存活时
    /// `Some`；编排层注入 `HostContext.history`，pipeline flush 出口 fan_out 用。
    pub fn handle(&self) -> Option<HistoryHandle> {
        self.handle.clone()
    }

    /// 历史读句柄（Phase 2 Task 1，P1）：clone 存储句柄包成
    /// `Arc<dyn HistoryReader>` 交 REST 层（AppState.history_reader），与
    /// writer/引擎共享同一底层存储 Arc。
    pub fn reader(&self) -> Arc<dyn crate::history::HistoryReader> {
        Arc::new(EngineReader {
            storage: self.storage.clone(),
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
}
