//! 日志保留期清理（M3）：按天/按总大小删旧日志 + 周期 cleanup task。

use crate::error::{RsIotError, TResult};
use tokio_util::sync::CancellationToken;

/// 清理日志目录：保留最近 `max_days` 天（today .. today-(max_days-1)），删 `today-max_days`
/// 及更早的 `rs-iot.log.YYYY-MM-DD` 文件；`max_size_mb>0` 时再按总大小删最旧直到低于阈值
/// （size-purge 跳过 today —— tracing-appender 正在写的活动文件，删了丢日志）。返回删除文件数。
/// 单文件 IO/解析失败与目录 entry 迭代错误仅 warn 跳过。
///
/// 无隐式系统时钟依赖（`today` 由调用方注入，便于测试）。
/// 文件名格式 `rs-iot.log.YYYY-MM-DD`（tracing-appender daily）；MB = 1_000_000 字节（SI）；
/// 按 age 判定时 `date <= today-max_days` 即删（"保留最近 max_days 天" 边界语义）。
pub(crate) fn purge_old_logs(
    dir: &str,
    today: chrono::NaiveDate,
    max_days: u64,
    max_size_mb: u64,
) -> TResult<usize> {
    use chrono::Days;
    const PREFIX: &str = "rs-iot.log.";

    let entries = std::fs::read_dir(dir)
        .map_err(|e| RsIotError::Config(format!("read log dir '{dir}': {e}")))?;

    // 收集 (路径, 日期, 字节大小)
    let mut files: Vec<(std::path::PathBuf, chrono::NaiveDate, u64)> = Vec::new();
    for res in entries {
        let Ok(ent) = res else {
            tracing::warn!("purge_old_logs: iterate entry failed, skipped");
            continue;
        };
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(date_str) = fname.strip_prefix(PREFIX) else {
            continue; // 非日志文件
        };
        let Ok(date) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") else {
            continue; // 文件名日期解析失败，跳过
        };
        let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
        files.push((path, date, size));
    }

    let mut removed = 0usize;

    // 1) 按天删：早于/等于 today - max_days（underflow 则跳过 —— 防御性默认"删无"）
    if max_days > 0 {
        if let Some(cutoff) = today.checked_sub_days(Days::new(max_days)) {
            files.retain(|(path, date, _size)| {
                if *date <= cutoff {
                    if let Err(e) = std::fs::remove_file(path) {
                        tracing::warn!("purge_old_logs: remove {} failed: {e}", path.display());
                    } else {
                        removed += 1;
                    }
                    false // 已处理（删或失败），从列表移除
                } else {
                    true
                }
            });
        }
    }

    // 2) 按总大小删最旧（跳过 today —— tracing-appender 正在写的活动文件，删了丢日志）
    if max_size_mb > 0 {
        let limit = max_size_mb.saturating_mul(1_000_000);
        files.sort_by_key(|(_p, date, _s)| *date); // 最旧在前
        let mut total: u64 = files.iter().map(|(_, _, s)| s).sum();
        for (path, date, size) in files {
            if total <= limit {
                break;
            }
            if date == today {
                continue; // 不删当天活动日志
            }
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("purge_old_logs: remove {} failed: {e}", path.display());
            } else {
                removed += 1;
                total = total.saturating_sub(size);
            }
        }
    }

    Ok(removed)
}

/// 启动日志保留期清理 task：启动立即跑一次（清上次遗留）+ 每 1h 跑一次；
/// `shutdown.child_token()` 绑定退出（select! 响应 cancel 即退，无需 abort）。失败仅 warn，不阻塞主流程。
///
/// fire-and-forget（不返回 JoinHandle）：task 靠 child_token cancel 自行退出，非如
/// axum/mqtt 那些需 abort 强杀的不可中断 task，故不加入关闭段的 abort 块。
pub(crate) fn spawn_cleanup_task(
    dir: String,
    max_days: u64,
    max_size_mb: u64,
    shutdown: CancellationToken,
) {
    let lt = shutdown.child_token();
    tokio::spawn(async move {
        let run_once = || {
            let today = chrono::Local::now().date_naive();
            if let Err(e) = purge_old_logs(&dir, today, max_days, max_size_mb) {
                tracing::warn!("log cleanup: {e}");
            }
        };
        run_once(); // 启动立即清一次（清上次遗留）
                    // 之后每 1h 跑一次；interval_at 延迟首次触发，避免与上面 run_once 在 t=0 重复
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = lt.cancelled() => break,
                _ = interval.tick() => run_once(),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::purge_old_logs;
    use chrono::NaiveDate;
    use std::fs;

    /// 造一个临时日志目录，返回路径（调用方负责清理）。
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rsiot-purge-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn touch(dir: &std::path::Path, fname: &str) {
        fs::write(dir.join(fname), b"x").expect("write");
    }

    #[test]
    fn purges_files_older_than_max_days() {
        let dir = scratch("old");
        let today = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        touch(&dir, "rs-iot.log.2026-08-01"); // 7 天前 → 删
        touch(&dir, "rs-iot.log.2026-08-07"); // 昨天 → 留
        touch(&dir, "rs-iot.log.2026-08-08"); // 今天 → 留

        let n = purge_old_logs(dir.to_str().unwrap(), today, 7, 0).expect("purge");
        assert_eq!(n, 1);
        assert!(!dir.join("rs-iot.log.2026-08-01").exists());
        assert!(dir.join("rs-iot.log.2026-08-07").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn max_days_zero_skips_age_purge() {
        let dir = scratch("zerodays");
        let today = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        touch(&dir, "rs-iot.log.2020-01-01"); // 很旧但 max_days=0 不按天删
        let n = purge_old_logs(dir.to_str().unwrap(), today, 0, 0).expect("purge");
        assert_eq!(n, 0);
        assert!(dir.join("rs-iot.log.2020-01-01").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn purges_by_total_size_oldest_first() {
        let dir = scratch("size");
        let today = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        // 三个文件各 1_000_000 字节，max_size_mb=2 → 留最新 2 个，删最旧 1 个
        fs::write(dir.join("rs-iot.log.2026-08-06"), vec![b'a'; 1_000_000]).expect("w");
        fs::write(dir.join("rs-iot.log.2026-08-07"), vec![b'b'; 1_000_000]).expect("w");
        fs::write(dir.join("rs-iot.log.2026-08-08"), vec![b'c'; 1_000_000]).expect("w");

        let n = purge_old_logs(dir.to_str().unwrap(), today, 0, 2).expect("purge");
        assert_eq!(n, 1);
        assert!(!dir.join("rs-iot.log.2026-08-06").exists()); // 最旧被删
        assert!(dir.join("rs-iot.log.2026-08-08").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_non_log_files() {
        let dir = scratch("nonlog");
        let today = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        touch(&dir, "rs-iot.log.2020-01-01"); // 旧日志 → 删
        touch(&dir, "random.txt"); // 非日志 → 不动
        touch(&dir, "rs-iot.log.notadate"); // 文件名日期解析失败 → 不动
        let n = purge_old_logs(dir.to_str().unwrap(), today, 7, 0).expect("purge");
        assert_eq!(n, 1);
        assert!(dir.join("random.txt").exists());
        assert!(dir.join("rs-iot.log.notadate").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_purge_never_deletes_today() {
        let dir = scratch("today");
        let today = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        // 只有 today 的文件，2MB 超 max_size_mb=1 —— 但 today 是活动日志，不可删
        fs::write(dir.join("rs-iot.log.2026-08-08"), vec![b'x'; 2_000_000]).expect("w");
        let n = purge_old_logs(dir.to_str().unwrap(), today, 0, 1).expect("purge");
        assert_eq!(n, 0);
        assert!(dir.join("rs-iot.log.2026-08-08").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
