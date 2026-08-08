# M3 日志兜底 + 自愈可信 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 默认落盘日志（保留期 + 退出 flush）+ 实例 task 异常退出真实标 Failed（supervisor），补 scenario-gap §8 P0 两块现场可用性底线。

**Architecture:** 方案 A（最小内嵌，零新依赖）。日志：`init_tracing` 返回 `WorkerGuard`（不再 forget）由 main/service 入口持有到退出 flush + 自建 `purge_old_logs` 纯函数 + cleanup task。自愈：`instance.rs` 每 source/sink spawn supervisor task 唯一 await source join，以「token 是否 cancelled」判定 Failed，stop 语义不变。

**Tech Stack:** Rust + tokio + tracing/tracing-appender + inventory + lux（既有）。

**Spec:** [`docs/superpowers/specs/2026-08-08-m3-log-and-selfheal-design.md`](../specs/2026-08-08-m3-log-and-selfheal-design.md)

---

## 文件结构

| 文件 | 责任 | 改动 |
|---|---|---|
| `src/config.rs` | `LogConfig` | 加 `max_days`/`max_size_mb` + Default（dir 默认 `./logs`） |
| `src/lib.rs` | tracing init + 日志清理 + 编排 | `init_tracing` 返回 guard；`purge_old_logs` 纯函数；cleanup task 接入 `run_app_with_shutdown` |
| `src/main.rs` | CLI 入口 | Run 分支持有 guard |
| `src/service/windows.rs` | Windows Service 入口 | service_main 持有 guard + exit(0) 前显式 drop flush |
| `src/plugin/instance.rs` | 实例生命周期 | start_source/start_sink 加 supervisor；InstanceHandle.join 语义改 supervisor handle |
| `tests/instance_failed_e2e.rs` | supervisor 集成测（新建） | mock source 早退→Failed / 正常 stop→Stopped |
| `CLAUDE.md` / `config.toml` | 文档 | 日志/实例状态描述同步 + `[log]` 注释示例 |

---

## Task 1: `config.rs` — `LogConfig` 加保留期字段

**Files:**
- Modify: `src/config.rs:123-140`（`LogConfig` struct + `Default`）
- Test: `src/config.rs` 内 `#[cfg(test)] mod tests`（新建）

- [ ] **Step 1: 写失败测试（锁默认值）**

在 `src/config.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_config_default_enables_file_logging() {
        let c = LogConfig::default();
        // 默认落盘 ./logs（修复 scenario-gap §3 🔴 P0「默认未启用文件日志」）
        assert_eq!(c.dir, "./logs");
        assert_eq!(c.level, "info");
        assert_eq!(c.max_days, 7);
        assert_eq!(c.max_size_mb, 0);
    }

    #[test]
    fn log_config_toml_default_section() {
        // 无 [log] section 的 toml → Default（dir=./logs）
        let toml = "";
        let cfg: AppConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.log.dir, "./logs");
        assert_eq!(cfg.log.max_days, 7);
    }

    #[test]
    fn log_config_empty_dir_disables_file_layer() {
        // 显式 dir="" 关闭文件层（保留逃生口）
        let toml = "[log]\ndir = \"\"\n";
        let cfg: AppConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.log.dir, "");
    }
}
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib config::tests`
Expected: FAIL（`max_days`/`max_size_mb` 字段不存在 → 编译错误）

- [ ] **Step 3: 实现 — 加字段 + Default**

修改 `src/config.rs:123-140`，替换整个 `LogConfig` struct 与 `Default`：

```rust
/// 日志配置。终端输出始终开启；`dir` 非空时额外写文件（daily 轮转 + 保留期清理）。
///
/// - `dir` 默认 `./logs`（开箱即落盘，修复 scenario-gap §3 🔴 P0）；显式 `dir=""` 关闭文件层。
/// - `max_days`/`max_size_mb` 控制保留期清理（M3，见 `lib::purge_old_logs`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// 日志文件目录。空 = 仅终端；非空（如 `./logs`）= 额外写文件。
    pub dir: String,
    /// 日志级别（仅当未设 `RUST_LOG` 时作为缺省；`RUST_LOG` 优先）。
    pub level: String,
    /// 按文件名日期保留最近 N 天（0 = 不按天删）。
    pub max_days: u64,
    /// 目录总大小上限（MB）；0 = 不限，>0 时按最旧优先删直到低于阈值。
    pub max_size_mb: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            dir: "./logs".into(),
            level: "info".into(),
            max_days: 7,
            max_size_mb: 0,
        }
    }
}
```

- [ ] **Step 4: 跑测试验证通过**

Run: `cargo test --lib config::tests`
Expected: PASS（3 个测试全绿）

- [ ] **Step 5: 提交**

```bash
git add src/config.rs
git commit -m "feat(log): LogConfig 加 max_days/max_size_mb + dir 默认 ./logs（M3）"
```

---

## Task 2: `lib.rs` — `purge_old_logs` 纯函数

**Files:**
- Modify: `src/lib.rs`（新增 `purge_old_logs` 函数 + `#[cfg(test)] mod tests`）
- Test: `src/lib.rs` 内单测（用 `std::env::temp_dir`，沿用 `tests/common` 模式，不引 tempfile）

> 设计要点：加 `today: NaiveDate` 参数使纯函数可测（不依赖系统时钟）。文件名格式 `rs-iot.log.YYYY-MM-DD`（tracing-appender daily），解析失败跳过。MB = 1_000_000 字节（SI）。

- [ ] **Step 1: 写失败测试**

在 `src/lib.rs` 找到顶部 `mod`/`use` 区，确认 `chrono::NaiveDate` 可用（`chrono` 已是依赖，`Cargo.toml:49`）。在文件末尾追加测试模块：

```rust
#[cfg(test)]
mod log_purge_tests {
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
        touch(&dir, "random.txt");            // 非日志 → 不动
        touch(&dir, "rs-iot.log.notadate");   // 文件名日期解析失败 → 不动
        let n = purge_old_logs(dir.to_str().unwrap(), today, 7, 0).expect("purge");
        assert_eq!(n, 1);
        assert!(dir.join("random.txt").exists());
        assert!(dir.join("rs-iot.log.notadate").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib log_purge_tests`
Expected: FAIL（`purge_old_logs` 未定义 → 编译错误）

- [ ] **Step 3: 实现 `purge_old_logs`**

在 `src/lib.rs` 中 `init_tracing` 函数**之前**插入：

```rust
/// 清理日志目录：删早于 `max_days` 的 `rs-iot.log.YYYY-MM-DD` 文件；`max_size_mb>0` 时
/// 再按总大小删最旧直到低于阈值。返回删除文件数。单文件 IO/解析失败仅 warn 跳过。
///
/// `today` 由调用方传入（cleanup task 传系统当前日期），使本函数纯函数可测。
/// 文件名格式 `rs-iot.log.YYYY-MM-DD`（tracing-appender daily）；MB = 1_000_000 字节（SI）。
fn purge_old_logs(dir: &str, today: chrono::NaiveDate, max_days: u64, max_size_mb: u64) -> TResult<usize> {
    use chrono::Days;
    const PREFIX: &str = "rs-iot.log.";

    let entries = std::fs::read_dir(dir)
        .map_err(|e| RsIotError::Config(format!("read log dir '{dir}': {e}")))?;

    // 收集 (路径, 日期, 字节大小)
    let mut files: Vec<(std::path::PathBuf, chrono::NaiveDate, u64)> = Vec::new();
    for ent in entries.flatten() {
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

    // 1) 按天删：早于 today - max_days
    if max_days > 0 {
        let cutoff = today.checked_sub_days(Days::new(max_days)).unwrap_or(today);
        files.retain(|(path, date, _size)| {
            if *date < cutoff {
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

    // 2) 按总大小删最旧
    if max_size_mb > 0 {
        let limit = max_size_mb.saturating_mul(1_000_000);
        files.sort_by_key(|(_p, date, _s)| *date); // 最旧在前
        let mut total: u64 = files.iter().map(|(_, _, s)| s).sum();
        for (path, _date, size) in files {
            if total <= limit {
                break;
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
```

- [ ] **Step 4: 跑测试验证通过**

Run: `cargo test --lib log_purge_tests`
Expected: PASS（4 个测试全绿）

- [ ] **Step 5: 提交**

```bash
git add src/lib.rs
git commit -m "feat(log): purge_old_logs 纯函数（按天/按大小删旧，M3）"
```

---

## Task 3: `lib.rs`/`main.rs`/`service` — `init_tracing` 返回 guard + 调用点持有

**Files:**
- Modify: `src/lib.rs:48-77`（`init_tracing` 签名 + 去 forget）
- Modify: `src/main.rs:51,55,59`（Run 持有 guard；install/uninstall 接 None）
- Modify: `src/service/windows.rs:75`（持有 guard + exit 前 drop flush）
- Test: `src/lib.rs` 内单测

> 关键：返回 `Option<WorkerGuard>` 后，调用点**必须持有**，否则 guard 立即 drop 使文件层失效。`main` Run 分支正常返回会 drop（flush）；但 `service_main` 用 `process::exit(0)`（`src/service/windows.rs:132`）跳过 drop，**必须显式 `drop(guard)` 在 exit 前**。

- [ ] **Step 1: 写失败测试（guard 返回值）**

在 `src/lib.rs` 的 `log_purge_tests` 模块**之前**插入新测试模块（或并入；此处独立便于定位）：

```rust
#[cfg(test)]
mod init_tracing_tests {
    use crate::config::LogConfig;
    use crate::init_tracing;

    #[test]
    fn returns_guard_when_dir_nonempty() {
        let dir = std::env::temp_dir().join(format!("rsiot-init-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = LogConfig {
            dir: dir.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let guard = init_tracing(Some(&cfg));
        assert!(guard.is_some(), "dir 非空应返回 Some(guard)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_none_when_dir_empty() {
        let cfg = LogConfig {
            dir: String::new(),
            ..Default::default()
        };
        let guard = init_tracing(Some(&cfg));
        assert!(guard.is_none(), "dir 空应返回 None");
    }
}
```

> 注：`init_tracing` 内部 `try_init` 是进程级 subscriber 单例，重复调用静默忽略（既有行为）；guard 的 Some/None 仅取决于 dir 是否非空且建目录成功，与 subscriber 是否已设置无关，故可独立断言。

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --lib init_tracing_tests`
Expected: FAIL（`init_tracing` 当前返回 `()`，无法 `.is_some()` → 编译错误）

- [ ] **Step 3: 改 `init_tracing` 返回 guard（去 forget）**

修改 `src/lib.rs:48-77`，替换整个 `init_tracing`（含 doc 注释）：

```rust
/// 初始化全局 tracing subscriber。
///
/// 终端输出始终开启；当 `cfg.dir` 非空时额外写日志文件（`tracing-appender` daily 轮转 +
/// non_blocking 专用日志线程）。级别优先读 `RUST_LOG`，缺省用 `cfg.level`。
///
/// `cfg = None` 表示仅终端、级别 info（供 install/uninstall 等管理命令使用）。
/// 用 `try_init` 容忍重复调用（进程级 subscriber 单例，重复设置静默忽略）。
///
/// 返回 `WorkerGuard`：调用方必须持有到 shutdown 末尾 drop，触发 non_blocking worker
/// drain 完剩余日志，保证故障末尾落盘（M3，修复 `std::mem::forget` 丢末尾问题）。
/// `None` = 文件层未启用（dir 空 或 建目录失败），无需持有。
pub fn init_tracing(cfg: Option<&config::LogConfig>) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::Layer, prelude::*, EnvFilter};

    let default_level = cfg.map(|c| c.level.as_str()).unwrap_or("info");
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let mut layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> =
        vec![fmt::layer()
            .with_target(true)
            .with_filter(filter.clone())
            .boxed()];

    let mut guard = None;
    if let Some(c) = cfg {
        let dir = c.dir.trim();
        if !dir.is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("init_tracing: create log dir '{dir}' failed: {e}");
            } else {
                let appender = tracing_appender::rolling::daily(dir, "rs-iot.log");
                let (writer, g) = tracing_appender::non_blocking(appender);
                layers.push(fmt::layer().with_writer(writer).with_filter(filter).boxed());
                guard = Some(g); // 不再 forget：交调用方持有到 shutdown drop → flush
            }
        }
    }

    let _ = tracing_subscriber::registry().with(layers).try_init();
    guard
}
```

- [ ] **Step 4: 改 `main.rs` 三个调用点持有/接 None**

修改 `src/main.rs:48-67` 的 `match` 分支：

```rust
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            let cfg = rs_iot::config::AppConfig::load_or_default(cli.config.as_deref())?;
            // 持有 guard 到 run 返回（main 退出时 drop → flush 故障末尾日志）
            let _log_guard = rs_iot::init_tracing(Some(&cfg.log));
            run::run(cfg).await?;
        }
        Command::Install => {
            let _ = rs_iot::init_tracing(None);
            service::install()?
        }
        Command::Uninstall => {
            let _ = rs_iot::init_tracing(None);
            service::uninstall()?
        }
        #[cfg(windows)]
        Command::Service => {
            // SCM 调用此入口；dispatch 内部接管 SCM 协议与生命周期。
            // 不在此 init_tracing：service_main 加载 config 后再初始化（含文件日志）。
            service::windows::dispatch()?;
        }
    }
```

- [ ] **Step 5: 改 `service/windows.rs` 持有 guard + exit 前 drop**

修改 `src/service/windows.rs:75`（持有 guard）：

```rust
    // config 加载成功后初始化日志（含文件输出）。service_main 早期错误（register/config load）
    // 在此之前无 subscriber，由 SCM 事件日志兜底。持有 guard 直到关闭流程末尾显式 drop flush。
    let _log_guard = rs_iot::init_tracing(Some(&cfg.log));
```

再修改 `src/service/windows.rs:119-132`（exit 前显式 drop flush）：

```rust
    shutdown.cancel();

    // 等 run_app_with_shutdown 返回（确保 lux 已 SAVE），不等 runtime drop
    let _ = done_rx.recv();

    // run_app_with_shutdown 已返回，所有日志（含 "rs-iot stopped"）已入 non_blocking
    // channel。process::exit(0) 跳过 drop，故在此显式 drop guard → worker drain → flush。
    drop(_log_guard);

    report(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(0),
        0,
        std::time::Duration::default(),
    );
    std::process::exit(0);
```

- [ ] **Step 6: 跑测试验证通过 + 编译**

Run: `cargo test --lib init_tracing_tests`
Expected: PASS（2 测试绿）

Run: `cargo build --all-targets`
Expected: 编译成功（确认所有 init_tracing 调用点已适配新返回类型）

- [ ] **Step 7: 提交**

```bash
git add src/lib.rs src/main.rs src/service/windows.rs
git commit -m "feat(log): init_tracing 返回 WorkerGuard，入口持有到退出 flush（M3）

- 不再 std::mem::forget，guard 交 main/service 持有
- service_main 用 process::exit(0) 跳过 drop，显式 drop(guard) flush
- 修复 scenario-gap §3 🟠 退出丢末尾日志（故障现场）"
```

---

## Task 4: `lib.rs` — cleanup task 接入 `run_app_with_shutdown`

**Files:**
- Modify: `src/lib.rs`（`run_app_with_shutdown` 内 spawn cleanup task）

> cleanup task：启动立即跑一次 `purge_old_logs`（清上次遗留）+ 每 1h 跑一次；`shutdown.child_token()` 绑定退出。`today` 由 task 内 `chrono::Local::now().date_naive()` 取。

- [ ] **Step 1: 在 `run_app_with_shutdown` 内 spawn cleanup task**

在 `src/lib.rs` 的 `run_app_with_shutdown` 中，找到 `tracing::info!("rs-iot ready. shutdown signal will stop all components.");`（约 line 207）**之前**插入（紧接 bridge_handle 构造之后）：

```rust
    // 日志保留期清理 task（M3）：启动跑一次（清上次遗留）+ 每 1h 跑一次。
    // dir 非空才起；失败仅 warn，不阻塞主流程。shutdown.child_token() 绑定退出。
    {
        let log_dir = cfg.log.dir.trim().to_string();
        if !log_dir.is_empty() {
            let lt = shutdown.child_token();
            let max_days = cfg.log.max_days;
            let max_size_mb = cfg.log.max_size_mb;
            tokio::spawn(async move {
                let run_once = || {
                    let today = chrono::Local::now().date_naive();
                    if let Err(e) = purge_old_logs(&log_dir, today, max_days, max_size_mb) {
                        tracing::warn!("log cleanup: {e}");
                    }
                };
                run_once(); // 启动立即清一次
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = lt.cancelled() => break,
                        _ = interval.tick() => run_once(),
                    }
                }
            });
            tracing::info!("log retention cleanup task started (dir={log_dir}, max_days={max_days}, max_size_mb={max_size_mb})");
        }
    }
```

- [ ] **Step 2: 验证编译 + 现有测试不破**

Run: `cargo build --all-targets`
Expected: 编译成功

Run: `cargo test --test dataflow`
Expected: PASS（数据流集成测不受影响）

- [ ] **Step 3: 提交**

```bash
git add src/lib.rs
git commit -m "feat(log): 保留期 cleanup task 接入 run_app_with_shutdown（M3）

启动跑一次 + 每 1h，shutdown.child_token 绑定退出；
修复 scenario-gap §3 🟠 daily 轮转不删旧文件（无限堆积）。"
```

---

## Task 5: `instance.rs` — supervisor 标 Failed

**Files:**
- Modify: `src/plugin/instance.rs:77-134`（start_source/start_sink 加 supervisor）
- Modify: `src/plugin/instance.rs:37-41`（InstanceHandle.join 注释）
- Test: `tests/instance_failed_e2e.rs`（新建，mock source + common::start_lux）

> 核心：「token 是否 cancelled」是「正常停 vs 异常死」唯一判据。supervisor 唯一 await source join（无竞态）；`InstanceHandle.join` 语义改为 supervisor handle，**stop 代码字面零改动**（await 对象从 source join 换成 supervisor join，行为一致）。
> 测试用 `#[cfg(test)]` 不可行（InstanceManager 构造需 HostContext 含 lux client，lib 单测起 lux 太重），故放集成测复用 `tests/common`。mock source 经 `inventory::submit` 注册（registry 编译期静态注册，集成测 crate 的 submit 经链接期全局 collect 可被 `find_source` 找到）。

- [ ] **Step 1: 加 dev-dependency + 写失败集成测试**

集成测试用 `inventory::submit!` 注册 mock source（registry 编译期静态注册；集成测试 crate 的 submit 经链接期全局 collect 可被 `find_source` 找到）。`inventory` 需对集成测试 crate 可见（rs_iot 的依赖不自动传递给集成测试 crate）。

在 `Cargo.toml` 末尾追加（`[dev-dependencies]` 仅测试用，不影响生产依赖）：

```toml
[dev-dependencies]
# 集成测试（instance_failed_e2e）注册 mock source + impl Source trait
inventory = "0.3"
async-trait = "0.1"
```

再新建 `tests/instance_failed_e2e.rs`：

```rust
//! M3 自愈可信：实例 task 异常退出 → 真实标 Failed（非 Running）。
//! 验证 supervisor 判定：早退/panic → Failed；正常 stop → Stopped。

mod common;

use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::{InstanceManager, InstState};
use rs_iot::plugin::traits::Source;
use rs_iot::plugin::registry::SourceEntry;
use rs_iot::types::{
    AppType, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// mock source：run() 立即返回（模拟配置错误早退）。
struct ImmediateExitSource;
#[async_trait]
impl Source for ImmediateExitSource {
    fn name(&self) -> &'static str { "__test_immediate_exit" }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: mpsc::Sender<Sample>,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        _token: CancellationToken,
    ) {
        // 立即返回（非 token cancel）→ supervisor 应标 Failed
    }
}
inventory::submit!(SourceEntry {
    name: "__test_immediate_exit",
    create: || -> Box<dyn Source> { Box::new(ImmediateExitSource) },
});

/// mock source：run() 内 panic。
struct PanicSource;
#[async_trait]
impl Source for PanicSource {
    fn name(&self) -> &'static str { "__test_panic" }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: mpsc::Sender<Sample>,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        _token: CancellationToken,
    ) {
        panic!("__test_panic boom");
    }
}
inventory::submit!(SourceEntry {
    name: "__test_panic",
    create: || -> Box<dyn Source> { Box::new(PanicSource) },
});

/// mock source：常驻直到 token cancel（模拟正常运行的实例）。
struct ObedientSource;
#[async_trait]
impl Source for ObedientSource {
    fn name(&self) -> &'static str { "__test_obedient" }
    async fn run(
        &self,
        _ctx: HostContext,
        _inst: InstConfig,
        _tx: mpsc::Sender<Sample>,
        _cmd_rx: mpsc::Receiver<rs_iot::types::CtlMessage>,
        token: CancellationToken,
    ) {
        token.cancelled().await;
    }
}
inventory::submit!(SourceEntry {
    name: "__test_obedient",
    create: || -> Box<dyn Source> { Box::new(ObedientSource) },
});

fn inst(id: &str, app_code: &str) -> InstConfig {
    InstConfig {
        inst_id: id.into(),
        inst_name: id.into(),
        app_code: PluginName::new(app_code),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({}),
        devices: vec![DeviceSpec {
            label: "d".into(),
            value: id.into(),
            children: vec![],
            slave_id: None,
        }],
        host_kind: HostKind::Builtin,
    }
}

async fn wait_for_state(mgr: &InstanceManager, id: &str, want: InstState) {
    for _ in 0..50 {
        if mgr.status().get(id).copied() == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{id} 未在 5s 内进入 {want:?}（当前 {:?})", mgr.status().get(id));
}

#[tokio::test]
async fn immediate_exit_marks_failed() {
    let data_dir = common::test_data_dir("instance_failed_imm");
    let (_handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    mgr.start_source(inst("imm", "__test_immediate_exit")).await.expect("start");
    wait_for_state(&mgr, "imm", InstState::Failed).await;
    assert_eq!(mgr.status().get("imm").copied(), Some(InstState::Failed));
}

#[tokio::test]
async fn panic_marks_failed() {
    let data_dir = common::test_data_dir("instance_failed_panic");
    let (_handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    mgr.start_source(inst("pan", "__test_panic")).await.expect("start");
    wait_for_state(&mgr, "pan", InstState::Failed).await;
    assert_eq!(mgr.status().get("pan").copied(), Some(InstState::Failed));
}

#[tokio::test]
async fn normal_stop_marks_stopped_not_failed() {
    let data_dir = common::test_data_dir("instance_failed_stop");
    let (_handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(16);
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("test"),
        shutdown: CancellationToken::new(),
        ctl_registry: CtlRegistry::new(),
        bridge_registry: BridgeRegistry::new(),
    };
    let mgr = InstanceManager::new(ctx, hub_tx);

    mgr.start_source(inst("obe", "__test_obedient")).await.expect("start");
    tokio::time::sleep(Duration::from_millis(200)).await; // 确认进入 Running
    assert_eq!(mgr.status().get("obe").copied(), Some(InstState::Running));

    mgr.stop("obe").await.expect("stop");
    assert_eq!(mgr.status().get("obe").copied(), Some(InstState::Stopped));
    assert_ne!(mgr.status().get("obe").copied(), Some(InstState::Failed));
}
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --test instance_failed_e2e`
Expected: FAIL（`immediate_exit_marks_failed` / `panic_marks_failed` 失败——当前 task 早退仍显 `Running`，不会变 `Failed`；`normal_stop` 可能通过）

- [ ] **Step 3: 实现 supervisor（改 start_source）**

修改 `src/plugin/instance.rs:77-102` 的 `start_source`，在 `source_join` spawn 之后、`handles.insert` 之前插入 supervisor，并把 `handles.insert` 的 `join` 改为 `sup_join`：

```rust
    /// 启动南向 Source 实例。产出汇入共享 hub（非返回 rx）。
    #[tracing::instrument(skip(self, inst), fields(hwid = %self.ctx.hwid, inst_id = %inst.inst_id))]
    pub async fn start_source(&self, inst: InstConfig) -> Result<(), RsIotError> {
        let create = registry::find_source(inst.app_code.as_ref()).ok_or_else(|| {
            RsIotError::Config(format!("source plugin not found: {}", inst.app_code))
        })?;
        let source = create();
        let id_key = inst.inst_id.to_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(64);
        let token = self.ctx.shutdown.child_token();

        self.states.insert(id_key.clone(), InstState::Starting);
        self.ctx.ctl_registry.register(&id_key, cmd_tx);

        let ctx = self.ctx.clone();
        let tx = self.hub_tx.clone();
        let token_run = token.clone();
        let id_for_log = id_key.clone();
        let source_join = tokio::spawn(async move {
            source.run(ctx, inst, tx, cmd_rx, token_run).await;
            tracing::info!("[instance] source {id_for_log} exited");
        });

        // supervisor（M3 自愈可信）：唯一 await source_join，按 token 判定最终状态。
        // task 退出（panic/早退/配置错误 return）且 token 未 cancel → 标 Failed + 告警。
        let states_sup = self.states.clone();
        let token_sup = token.clone();
        let id_sup = id_key.clone();
        let sup_join = tokio::spawn(async move {
            let _ = source_join.await; // panic 以 JoinError 返回，被吞；转 Failed 判定
            if !token_sup.is_cancelled() {
                states_sup.insert(id_sup.clone(), InstState::Failed);
                tracing::error!("[instance] {id_sup} task exited unexpectedly → Failed");
            }
            // token 已 cancel = 主动 stop（stop 已标 Stopping→Stopped），不干预
        });

        self.states.insert(id_key.clone(), InstState::Running);
        self.handles.insert(id_key, InstanceHandle { token, join: sup_join });
        Ok(())
    }
```

- [ ] **Step 4: 同样改 `start_sink`**

修改 `src/plugin/instance.rs:105-134` 的 `start_sink`，同样在 sink task spawn 后加 supervisor，`handles.insert` 用 `sup_join`：

```rust
    /// 启动北向 Sink 实例（sample_rx 由调用方/pipeline 提供）。
    #[tracing::instrument(skip(self, inst), fields(hwid = %self.ctx.hwid, inst_id = %inst.inst_id))]
    pub async fn start_sink(
        &self,
        inst: InstConfig,
        sample_rx: mpsc::Receiver<Sample>,
    ) -> Result<(), RsIotError> {
        let create = registry::find_sink(inst.app_code.as_ref()).ok_or_else(|| {
            RsIotError::Config(format!("sink plugin not found: {}", inst.app_code))
        })?;
        let sink = create();
        let id_key = inst.inst_id.to_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<CtlMessage>(64);
        let token = self.ctx.shutdown.child_token();

        self.states.insert(id_key.clone(), InstState::Starting);
        self.ctx.ctl_registry.register(&id_key, cmd_tx);

        let ctx = self.ctx.clone();
        let token_run = token.clone();
        let id_for_log = id_key.clone();
        let source_join = tokio::spawn(async move {
            sink.run(ctx, inst, sample_rx, cmd_rx, token_run).await;
            tracing::info!("[instance] sink {id_for_log} exited");
        });

        let states_sup = self.states.clone();
        let token_sup = token.clone();
        let id_sup = id_key.clone();
        let sup_join = tokio::spawn(async move {
            let _ = source_join.await;
            if !token_sup.is_cancelled() {
                states_sup.insert(id_sup.clone(), InstState::Failed);
                tracing::error!("[instance] {id_sup} task exited unexpectedly → Failed");
            }
        });

        self.states.insert(id_key.clone(), InstState::Running);
        self.handles.insert(id_key, InstanceHandle { token, join: sup_join });
        Ok(())
    }
```

- [ ] **Step 5: 更新 `InstanceHandle.join` 注释（语义=supervisor handle）**

修改 `src/plugin/instance.rs:37-41`：

```rust
#[derive(Debug)]
struct InstanceHandle {
    token: CancellationToken,
    /// supervisor task 的 handle（M3 自愈可信）：supervisor 内部唯一 await source/sink
    /// task 的 join，按 token 判定 Failed。`stop` await 此 handle，3s 超时语义不变。
    join: JoinHandle<()>,
}
```

- [ ] **Step 6: 跑测试验证通过**

Run: `cargo test --test instance_failed_e2e`
Expected: PASS（3 测试全绿：immediate_exit→Failed / panic→Failed / normal_stop→Stopped）

- [ ] **Step 7: 回归验证（现有测试不破）**

Run: `cargo test --test dataflow && cargo test --test sidecar_e2e && cargo test --test api_flow`
Expected: PASS（supervisor 不改数据流；stop 语义不变）

- [ ] **Step 8: 提交**

```bash
git add src/plugin/instance.rs tests/instance_failed_e2e.rs
git commit -m "feat(instance): supervisor 标 Failed（task 退出即 Failed，M3 自愈可信）

- 每实例 supervisor task 唯一 await source/sink join，token 未 cancel → Failed + error!
- InstanceHandle.join 语义改 supervisor handle，stop 代码字面零改动
- 修复 scenario-gap §2 🔴 Failed 定义了从未写入（task 死仍显 Running）"
```

---

## Task 6: 文档同步（`CLAUDE.md` + `config.toml`）

**Files:**
- Modify: `CLAUDE.md`（日志段 + 实例状态段）
- Modify: `config.toml`（加注释版 `[log]` 示例）

- [ ] **Step 1: 更新 `CLAUDE.md` 日志段**

找到 `CLAUDE.md` 中「环境依赖」段的日志描述（含「`config.toml` 的 `[log].dir` 非空时额外写文件」），更新为：

```markdown
- 日志：`tracing` + `tracing-subscriber` + `tracing-appender`（tokio-rs 官方）。终端输出始终开启；**`[log].dir` 默认 `./logs`（开箱即落盘，M3）**，daily 轮转 `rs-iot.log.YYYY-MM-DD` + **保留期清理**（`max_days` 默认 7 按天删旧 / `max_size_mb` 默认 0 不限，>0 按总大小删最旧；cleanup task 启动跑一次 + 每 1h）。**退出 flush**（`init_tracing` 返回 `WorkerGuard`，main/service 持有到退出 drop；service `process::exit` 前显式 drop）。级别优先 `RUST_LOG`，缺省 `[log].level`（默认 info）；显式 `dir=""` 关闭文件层。opcua/modbus 采集/连接/读写均有 `[opcuaClient]`/`[modbusClient]` 前缀日志。
```

- [ ] **Step 2: 更新 `CLAUDE.md` 实例状态描述**

找到 `CLAUDE.md` 中描述实例状态/生命周期的位置（`plugin/instance.rs` 职责行附近），补充 Failed 真实写入：

```markdown
| `plugin/` | ... `instance.rs`（生命周期管理 + `send_ctl` 写命令投递 + **M3 supervisor：task 异常退出（panic/早退/配置错误）标 Failed + 告警，REST `/inststatus` 可见；不自重启**） ... |
```

并在「常见流程」或「验证规则」相关处补充一句（若已有实例状态说明，更新之）：

```markdown
- **实例自愈可信（M3）**：`start_source`/`start_sink` spawn source/sink task 后再 spawn supervisor task 唯一 await 其 join，task 退出且 `token` 未 cancel → 标 `InstState::Failed` + `error!`（覆盖 panic/配置错误早退/异常 return）；主动 `stop` 走 Stopping→Stopped（token cancel）不误判。状态仅内存（进程重启重置）。不自重启（后置）。`GET /api/v1/inststatus` 返回 Failed 实例；运维 `startinst` 重拉（Failed→Starting→Running）。
```

- [ ] **Step 3: `config.toml` 加注释版 `[log]` 示例**

在 `config.toml` 适当位置（如其他 `[xxx]` section 附近）加：

```toml
# 日志（默认即可：落盘 ./logs，保留 7 天，daily 轮转，退出自动 flush）。
# 不写 [log] section 也走此默认（M3）。
[log]
dir = "./logs"        # 默认 "./logs"；置 "" 关闭文件层（仅终端）
level = "info"        # RUST_LOG 优先
max_days = 7          # 按文件名日期保留最近 N 天（0 = 不按天删）
max_size_mb = 0       # 0=不限；>0 时按目录总大小删最旧
```

- [ ] **Step 4: 提交**

```bash
git add CLAUDE.md config.toml
git commit -m "docs(m3): CLAUDE.md 日志/实例状态同步 + config.toml [log] 示例"
```

---

## Task 7: 全量验证（CI 门槛）

**Files:** 无（仅验证）

> CI 门槛（CLAUDE.md 验证规则）：`fmt` + `clippy -D warnings` + `test --all-targets --all-features` 三项全绿。Windows 规避 link.exe 并行 OOM 用 `CARGO_BUILD_JOBS=1`。

- [ ] **Step 1: 格式检查**

Run: `cargo fmt --all --check`
Expected: 无输出（格式 OK）；若有偏差，`cargo fmt --all` 修后重检。

- [ ] **Step 2: clippy 零警告**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: 无 warning / error。重点关注：
- `init_tracing` 返回的 `Option<WorkerGuard>` 调用点是否被 `must_use` 警告（main/service 已 `let _` / `let _log_guard` 持有）
- supervisor 闭包 move 的 `states_sup`/`token_sup` 是否有冗余 clone

- [ ] **Step 3: 全量测试（含 all-features）**

Run: `CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features`
Expected: 全绿（含新增 `config::tests` / `log_purge_tests` / `init_tracing_tests` / `instance_failed_e2e` + 现有全部集成测回归）。

- [ ] **Step 4: 端到端验证（手动，可选但推荐）**

```bash
# 1. 默认落盘（无 [log] 的 config 或默认）：
./target/debug/rs-iot --config config.toml &
sleep 2
ls ./logs/                                    # → rs-iot.log.YYYY-MM-DD 存在
# 2. 退出 flush：
kill %1                                       # 或 ctrl_c
tail -5 ./logs/rs-iot.log.*                   # → 末尾含 "rs-iot stopped"（不丢末尾）
# 3. Failed 可见（构造一个 app_code 不存在或会早退的实例经 REST startinst）：
#    GET /api/v1/inststatus → 对应实例 "failed"
```

- [ ] **Step 5: 若 Step 1-3 有修正，提交**

```bash
git add -A
git commit -m "chore(m3): CI 全量验证修正（fmt/clippy/test）" || echo "无需修正"
```

---

## Self-Review 结论

- **Spec 覆盖**：§4.1 配置→Task 1；§4.1.2 flush→Task 3；§4.1.3 purge+cleanup→Task 2/4；§4.2 supervisor→Task 5；§5 错误处理→各 Task 实现内（warn 不 panic）；§6 测试→Task 1/2/3/5；§7 改动清单→各 Task Files；§9 配置示例→Task 6。全覆盖。
- **Placeholder**：无 TBD/TODO；每个代码 step 含完整代码。
- **类型一致**：`purge_old_logs(dir, today, max_days, max_size_mb) -> TResult<usize>` 全 plan 一致；`InstanceHandle { token, join }` 字段名不变（join 语义改 supervisor handle，注释更新）；`init_tracing -> Option<WorkerGuard>` 所有调用点一致。
