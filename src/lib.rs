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

use error::RsIotError;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 构建版本标识（供 `/api/v1/sysinfo` 与日志使用）。
pub const VERSION: &str = "20250123";

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
        }],
        host_kind: types::HostKind::Builtin,
    }
}
