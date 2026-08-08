//! rs-iot：轻量跨平台工业 IoT 边缘框架（Rust）。
//!
//! 内嵌 lux 作为进程内 Redis 兼容数据库；南向 Source 采集 → hub → pipeline →
//! lux rtdata；北向 Sink（mqtt，STEP 5 起为 pull 模式）发布到外部 broker；
//! axum REST 管理 API（默认 :8780）。
//!
#![warn(
    missing_docs,
    rust_2018_idioms,
    missing_debug_implementations,
    unreachable_pub
)]

pub mod api;
/// 应用配置（toml 加载，`AppConfig`）。
pub mod config;
/// lux 嵌入数据库辅助（建配置、身份引导、rtdata 读写、优雅关闭）。
pub mod db;
/// 库错误类型（`RsIotError`、`TResult`）。
pub mod error;
pub mod host;
/// 硬件 ID 生成。
pub mod hwid;
pub mod pipeline;
/// 插件系统（trait 契约、注册表、实例管理、内置实现）。
pub mod plugin;
pub mod transform;
pub mod types;

use error::{RsIotError, TResult};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 构建版本标识（供 `/api/v1/sysinfo` 与日志使用）。
pub const VERSION: &str = "20250123";

/// 清理日志目录：保留最近 `max_days` 天（today .. today-(max_days-1)），删 `today-max_days`
/// 及更早的 `rs-iot.log.YYYY-MM-DD` 文件；`max_size_mb>0` 时再按总大小删最旧直到低于阈值。
/// 返回删除文件数。单文件 IO/解析失败仅 warn 跳过。
///
/// `today` 由调用方传入（cleanup task 传系统当前日期），使本函数纯函数可测。
/// 文件名格式 `rs-iot.log.YYYY-MM-DD`（tracing-appender daily）；MB = 1_000_000 字节（SI）。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "M3 Task 4 cleanup task 将调用；接入后移除此 attribute"
    )
)]
fn purge_old_logs(
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

    // 1) 按天删：保留最近 max_days 天（today .. today-(max_days-1)），删 today-max_days 及更早。
    //    边界：date == today-max_days 即满 max_days 天，应删（"保留 N 天" 语义）。
    if max_days > 0 {
        let cutoff = today.checked_sub_days(Days::new(max_days)).unwrap_or(today);
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

/// 初始化全局 tracing subscriber。
///
/// 终端输出始终开启；当 `cfg.dir` 非空时额外写日志文件（`tracing-appender` daily 轮转 +
/// non_blocking 专用日志线程）。级别优先读 `RUST_LOG`，缺省用 `cfg.level`。
///
/// `cfg = None` 表示仅终端、级别 info（供 install/uninstall 等管理命令使用）。
/// 用 `try_init` 容忍重复调用（进程级 subscriber 单例，重复设置静默忽略）。
///
/// 非 blocking writer 的 `WorkerGuard` 被刻意 `forget`：服务常驻进程需日志线程全程存活，
/// 进程退出时线程随之结束；v1 不做退出前 flush（可能丢最后几条），如需精确 flush 改为
/// 在 shutdown 流程显式持有并 drop guard。
pub fn init_tracing(cfg: Option<&config::LogConfig>) {
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

    if let Some(c) = cfg {
        let dir = c.dir.trim();
        if !dir.is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("init_tracing: create log dir '{dir}' failed: {e}");
            } else {
                let appender = tracing_appender::rolling::daily(dir, "rs-iot.log");
                let (writer, guard) = tracing_appender::non_blocking(appender);
                std::mem::forget(guard);
                layers.push(fmt::layer().with_writer(writer).with_filter(filter).boxed());
            }
        }
    }

    let _ = tracing_subscriber::registry().with(layers).try_init();
}

/// CLI `run` 入口：内部建 shutdown + ctrl_c 监听，委托给 `run_app_with_shutdown`。
#[tracing::instrument(skip(cfg))]
pub async fn run_app(cfg: config::AppConfig) -> Result<(), RsIotError> {
    let shutdown = CancellationToken::new();
    let s = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ctrl_c received, shutting down");
        s.cancel();
    });
    run_app_with_shutdown(cfg, shutdown).await
}

/// 服务态入口（M4 Windows Service / systemd）：shutdown 由外部（SCM/sigterm）触发。
///
/// 数据流（hub 模式，STEP 5 pull 化）：N 个 Source ──hub(mpsc)──► pipeline
/// ──Processor──► lux HSET rtdata:{devid}；北向 `MqttPub` 独立常驻，
/// 周期从 lux 抽取 rtdata → PUBLISH `{hwid}/{devid}/datas`。
#[tracing::instrument(skip(cfg, shutdown))]
pub async fn run_app_with_shutdown(
    cfg: config::AppConfig,
    shutdown: CancellationToken,
) -> Result<(), RsIotError> {
    let hwid = types::Hwid::from(hwid::hardware_id());
    tracing::info!(%hwid, "rs-iot starting");

    std::fs::create_dir_all(&cfg.lux.data_dir)?;
    let lux_cfg = db::build_lux_config(&cfg);
    let handle = lux::run_with_config(lux_cfg).await?;
    if let Some(addr) = handle.local_addr() {
        tracing::info!("lux RESP listening on {addr}");
    } else {
        tracing::warn!("lux RESP listener not bound");
    }

    let client = handle.client();
    db::identity::bootstrap(&client).await?;
    tracing::info!("identity bootstrap done");

    let ctl_registry = host::CtlRegistry::new();
    let bridge_registry = host::BridgeRegistry::new();
    let ctx = host::HostContext {
        lux: client.clone(),
        hwid: hwid.clone(),
        shutdown: shutdown.clone(),
        ctl_registry,
        bridge_registry: bridge_registry.clone(),
    };

    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<types::Sample>(256);
    let mgr = Arc::new(plugin::instance::InstanceManager::new(ctx.clone(), hub_tx));

    // 北向 MQTT 发布器（pull 模式，STEP 5）：常驻独立 task，周期从 lux 抽取 rtdata 发布。
    let mqtt_token = shutdown.child_token();
    let mqtt_handle = tokio::spawn(
        plugin::builtin::mqtt_pub::MqttPub::new(cfg.mqtt.clone()).run(ctx.clone(), mqtt_token),
    );

    // pipeline：hub → processor → lux rtdata（不再 fan-out 到 sink）
    let processor: Arc<dyn plugin::traits::Processor> =
        Arc::new(transform::passthrough::Passthrough);
    let pipeline_token = shutdown.child_token();
    let pipeline_handle = tokio::spawn(pipeline::run(
        ctx.clone(),
        hub_rx,
        processor,
        pipeline_token,
    ));

    // 南向 Source：lux autoStart 实例；无则 demo
    let started = try_start_from_lux(&mgr, &client).await;
    if started == 0 {
        let _ = mgr.start_source(demo_datasim_config()).await;
        tracing::info!("started demo source sim1 (no autoStart instances in lux)");
    } else {
        tracing::info!("started {started} source instance(s) from lux");
    }

    // REST :api_port（bind 失败仅告警，不中断数据流）
    let state = api::AppState {
        lux: client.clone(),
        mgr: mgr.clone(),
        hwid: hwid.clone(),
        jwt_secret: cfg.jwt_secret.clone(),
        version: VERSION,
    };
    let app = api::build_router(state);
    let api_port = cfg.api_port;
    let axum_handle = tokio::spawn(async move {
        match tokio::net::TcpListener::bind(("0.0.0.0", api_port)).await {
            Ok(listener) => {
                tracing::info!("REST API listening on :{api_port} (apiTest: http://localhost:{api_port}/apitest)");
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!("axum serve error: {e}");
                }
            }
            Err(e) => tracing::error!("bind :{api_port} failed: {e}"),
        }
    });

    // 外部插件 bridge（sidecarClient acceptor）：仅在 cfg.bridge.enable 时启动。
    // 先 bind listener（拿实际端口用于日志/测试），再 spawn acceptor（acceptor 只做 accept 循环）。
    let bridge_handle = if cfg.bridge.enable {
        let bt = shutdown.child_token();
        match tokio::net::TcpListener::bind((cfg.bridge.bind_host.as_str(), cfg.bridge.port)).await
        {
            Ok(listener) => Some(tokio::spawn(plugin::builtin::sidecar_client::acceptor(
                listener,
                std::time::Duration::from_secs(cfg.bridge.accept_hello_timeout_secs),
                cfg.bridge.max_connections,
                client.clone(),
                std::sync::Arc::<str>::from(cfg.jwt_secret.as_str()),
                bridge_registry.clone(),
                bt,
            ))),
            Err(e) => {
                tracing::error!(
                    "bridge bind {}:{} failed: {e}",
                    cfg.bridge.bind_host,
                    cfg.bridge.port
                );
                None
            }
        }
    } else {
        None
    };

    tracing::info!("rs-iot ready. shutdown signal will stop all components.");

    shutdown.cancelled().await;

    pipeline_handle.abort();
    mqtt_handle.abort();
    axum_handle.abort();
    if let Some(h) = bridge_handle {
        h.abort();
    }
    mgr.stop_all().await;
    db::shutdown_lux(handle).await?;
    tracing::info!("rs-iot stopped");
    Ok(())
}

async fn try_start_from_lux(
    mgr: &Arc<plugin::instance::InstanceManager>,
    client: &lux::EmbeddedClient,
) -> usize {
    let arr = match client.execute_value("HGETALL", &["instances"]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => return 0,
    };
    let mut count = 0;
    let mut i = 0;
    while i + 1 < arr.len() {
        let s = match &arr[i + 1] {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => {
                i += 2;
                continue;
            }
        };
        if let Ok(inst) = serde_json::from_str::<types::InstConfig>(&s) {
            if inst.auto_start
                && inst.app_type == types::AppType::South
                && mgr.start_source(inst).await.is_ok()
            {
                count += 1;
            }
        }
        i += 2;
    }
    count
}

fn demo_datasim_config() -> types::InstConfig {
    types::InstConfig {
        inst_id: "sim1".into(),
        inst_name: "data simulator".into(),
        app_code: "dataSim".into(),
        app_type: types::AppType::South,
        auto_start: true,
        app_config: serde_json::json!({}),
        devices: vec![types::DeviceSpec {
            label: "demo".into(),
            value: "devA".into(),
            children: vec![
                types::ChildSpec {
                    label: "温度".into(),
                    value: "temp".into(),
                    r#type: "float".into(),
                },
                types::ChildSpec {
                    label: "湿度".into(),
                    value: "humidity".into(),
                    r#type: "float".into(),
                },
            ],
            slave_id: None,
        }],
        host_kind: types::HostKind::Builtin,
    }
}

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
        touch(&dir, "random.txt"); // 非日志 → 不动
        touch(&dir, "rs-iot.log.notadate"); // 文件名日期解析失败 → 不动
        let n = purge_old_logs(dir.to_str().unwrap(), today, 7, 0).expect("purge");
        assert_eq!(n, 1);
        assert!(dir.join("random.txt").exists());
        assert!(dir.join("rs-iot.log.notadate").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
