//! rs-iot：轻量跨平台工业 IoT 边缘框架（Rust）。
//!
//! 内嵌 lux 作为进程内 Redis 兼容数据库；南向 Source 采集 → hub → pipeline →
//! lux rtdata；北向 Sink（mqtt，STEP 5 起为 pull 模式）发布到外部 broker；
//! axum REST 管理 API（默认 :8780）。
//!
//! 开发期：MVP 各模块逐步落地，暂允许 dead_code。
#![allow(dead_code)]
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

/// CLI `run` 入口：内部建 shutdown + ctrl_c 监听，委托给 `run_app_with_shutdown`。
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
/// 数据流（hub 模式）：N 个 Source ──hub(mpsc)──► pipeline ──Processor──►
///     ├─ lux HSET rtdata:`<devid>`
///     └─ Sink(mqttPub) PUBLISH `<hwid>`/`<devid>`/datas
pub async fn run_app_with_shutdown(
    cfg: config::AppConfig,
    shutdown: CancellationToken,
) -> Result<(), RsIotError> {
    let hwid = hwid::hardware_id();
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
    let ctx = host::HostContext {
        lux: client.clone(),
        hwid: hwid.clone(),
        shutdown: shutdown.clone(),
        ctl_registry,
    };

    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<types::Sample>(256);
    let mgr = Arc::new(plugin::instance::InstanceManager::new(ctx.clone(), hub_tx));

    // 北向 Sink（固定）：mqttPub
    let mqtt_inst = types::InstConfig {
        inst_id: "mqtt1".into(),
        inst_name: "mqtt publisher".into(),
        app_code: "mqttPub".into(),
        app_type: types::AppType::North,
        auto_start: true,
        app_config: serde_json::json!({
            "host": cfg.mqtt.host,
            "port": cfg.mqtt.port,
            "username": cfg.mqtt.username,
            "password": cfg.mqtt.password,
        }),
        devices: vec![],
    };
    let (mqtt_tx, mqtt_rx) = tokio::sync::mpsc::channel::<types::Sample>(256);
    mgr.start_sink(mqtt_inst, mqtt_rx).await?;

    // pipeline
    let processor: Arc<dyn plugin::traits::Processor> =
        Arc::new(transform::passthrough::Passthrough);
    let pipeline_token = shutdown.child_token();
    let pipeline_handle = tokio::spawn(pipeline::run(
        ctx.clone(),
        hub_rx,
        vec![mqtt_tx],
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

    tracing::info!("rs-iot ready. shutdown signal will stop all components.");

    shutdown.cancelled().await;

    pipeline_handle.abort();
    axum_handle.abort();
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
    }
}
