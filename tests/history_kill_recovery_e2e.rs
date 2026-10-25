//! 门禁三（kill 恢复零丢失，Task 6 / P10 主验证——Periodic 500ms 默认档）：
//!
//! 子进程模式（`crash_child_mode` + env `RSIOT_HISTORY_CRASH_CHILD`）在
//! build_storage 后单次 insert_rows 5000 行（50 series × 100 点，互异 ms ts），
//! **确认 insert 返回 Ok 后立即 `std::process::abort()`**——不跑任何 Rust
//! 析构/排空/close，WAL 是唯一恢复边界。父测试 spawn `current_exe()` 以
//! `--exact crash_child_mode -q` 运行子模式，try_wait 轮询（60s 超时，R2：
//! Windows abort 可能触发 WER 卡死 → 超时 `kill()` 硬终止，同样是 crash
//! 语义），随后同目录重开（WAL 回放）→ 逐 series select → 唯一 ts 总数
//! 必须 == 5000（零丢失）。
//!
//! P10/WAL 档位结论（实测后回填验收记录）：tsink `WalSyncMode::Periodic`
//! 语义为"append 即 write 到 OS（WAL BufWriter 每次 append 必 flush）+
//! 发布水位标记文件随 commit 落 OS，仅 fsync 延迟"——进程级 kill
//! （abort/TerminateProcess）下 OS 页缓存存活，理论零丢失；丢失窗口只
//! 覆盖整机断电/蓝屏。本测试即该结论的实证。
#![cfg(feature = "history")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rs_iot::config::HistoryConfig;
use rs_iot::history::engine::build_storage;
use tsink::{DataPoint, Label, Row};

/// 子模式标记 env：值为崩溃子进程的 tsink 数据目录。
const CHILD_ENV: &str = "RSIOT_HISTORY_CRASH_CHILD";
/// 崩溃写入规模：50 series × 100 点 = 5000 行（Task 6 指定）。
const SERIES_COUNT: usize = 50;
const POINTS_PER_SERIES: usize = 100;
const TOTAL_ROWS: usize = SERIES_COUNT * POINTS_PER_SERIES;

/// 唯一临时目录：tag + pid + 纳秒时钟，测试并行与 CI 并发 run 不冲突；
/// best-effort 清理责任在调用方（Windows 句柄释放有延迟，失败忽略）。
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-hist-kill-e2e-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 当前 Unix 毫秒（崩溃写入点时间基）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_millis() as i64
}

/// 崩溃写入配置：默认值 + 指定 data_dir——**wal_sync_ms 默认 500 即
/// Periodic(500ms) 默认档**（P10 优先验证档）；retention 默认 30 天覆盖
/// now 附近 ts。
fn crash_cfg(dir: &Path) -> HistoryConfig {
    HistoryConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    }
}

/// 与生产 writer 约定一致的 series labels：devid/hwid（Task 4 契约）。
fn series_labels() -> Vec<Label> {
    vec![
        Label::new("devid", "crashdev"),
        Label::new("hwid", "crash-hw"),
    ]
}

/// series 名（cs00..cs49）。
fn metric_name(s: usize) -> String {
    format!("cs{s:02}")
}

/// 崩溃写入行集：50 series × 100 点，全局互异毫秒 ts（now 起连续递增）。
fn crash_rows() -> Vec<Row> {
    let base = now_ms();
    let mut rows = Vec::with_capacity(TOTAL_ROWS);
    for s in 0..SERIES_COUNT {
        for k in 0..POINTS_PER_SERIES {
            let seq = (s * POINTS_PER_SERIES + k) as i64;
            rows.push(Row::with_labels(
                metric_name(s),
                series_labels(),
                DataPoint::new(base + seq, seq as f64),
            ));
        }
    }
    rows
}

/// 子模式测试（父测试经 `--exact crash_child_mode` 单独驱动）：env
/// `RSIOT_HISTORY_CRASH_CHILD` 有值 → 崩溃写入逻辑；无值（正常跑测）→
/// 直接返回。
///
/// 崩溃语义锚点：`insert_rows` 返回 Ok 后**立即** `std::process::abort()`
/// ——不 close、不排空、不跑任何析构，落盘持久性完全依赖 WAL。
#[test]
fn crash_child_mode() {
    let dir = std::env::var(CHILD_ENV).unwrap_or_default();
    if dir.is_empty() {
        return;
    }
    let cfg = crash_cfg(Path::new(&dir));
    let storage = build_storage(&cfg, 100).expect("子模式 build_storage 必须成功");
    storage
        .insert_rows(&crash_rows())
        .expect("子模式单次 insert 5000 行必须返回 Ok（确认后才崩）");
    std::process::abort();
}

/// 门禁三：崩溃后零丢失——子进程 abort（或超时 kill 硬终止，同为 crash
/// 语义）→ 同目录重开（WAL 回放，进程锁随进程死亡释放）→ 逐 series
/// select，唯一 ts 总数必须 == 5000。
#[test]
fn gate3_kill_recovery_zero_loss() {
    let dir = unique_dir("recovery");

    // 1. spawn 崩溃子进程：同 binary 只跑 crash_child_mode（--exact），env 指向临时目录
    let mut child =
        std::process::Command::new(std::env::current_exe().expect("current_exe 必须可用"))
            .arg("--exact")
            .arg("crash_child_mode")
            .arg("-q")
            .env(CHILD_ENV, &dir)
            .spawn()
            .expect("spawn 崩溃子进程必须成功");

    // 2. try_wait 轮询实现 wait_timeout（60s，500ms 间隔）：Windows abort 可能
    //    触发 WER 卡死（R2）→ 超时 kill() 硬终止——不跑析构，同样是 crash 语义
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut killed = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    child.kill().expect("超时后 kill 子进程必须成功");
                    let _ = child.wait();
                    killed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => panic!("try_wait 轮询失败: {e}"),
        }
    }

    // 3. 同目录重开：进程锁（File::try_lock）随进程死亡被 OS 释放；WAL 回放
    //    恢复崩溃前的已 ack 写入
    let reopened = build_storage(&crash_cfg(&dir), 100)
        .expect("崩溃后同目录重开（WAL 回放）必须成功——进程锁应随进程死亡释放");

    // 4. 门禁三断言：逐 series select，唯一 ts 总数 == 5000（零丢失）
    let mut total_unique_ts = 0usize;
    for s in 0..SERIES_COUNT {
        let pts = reopened
            .select(&metric_name(s), &series_labels(), 0, i64::MAX)
            .expect("重开后 select 必须成功");
        let unique_ts: HashSet<i64> = pts.iter().map(|p| p.timestamp).collect();
        assert_eq!(
            unique_ts.len(),
            POINTS_PER_SERIES,
            "series {} 零丢失：唯一 ts 必须满 100（实际 {} 点/唯一 ts {} 个）",
            metric_name(s),
            pts.len(),
            unique_ts.len()
        );
        total_unique_ts += unique_ts.len();
    }
    assert_eq!(
        total_unique_ts, TOTAL_ROWS,
        "门禁三：kill 恢复后唯一 ts 总数必须 == 5000（Periodic 500ms 默认档；实际 {total_unique_ts}；终止方式: {}）",
        if killed { "60s 超时后 kill 硬终止" } else { "abort 自行终止" },
    );

    reopened.close().expect("重开实例 close");
    let _ = std::fs::remove_dir_all(&dir);
}
