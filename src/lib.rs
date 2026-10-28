//! rs-iot：轻量跨平台工业 IoT 边缘框架（Rust）。
//!
//! 内嵌 lux 作为进程内 Redis 兼容数据库；南向 Source 采集 → hub → pipeline →
//! lux rtdata；北向走默认 luaNorth 实例 mqtt-pub-default（T8 起，mqttPub 兼容
//! 脚本，pull 模式周期发布）发布到外部 broker；axum REST 管理 API（默认 :8790）。
//!
#![warn(
    missing_docs,
    rust_2018_idioms,
    missing_debug_implementations,
    unreachable_pub
)]
// clippy 1.99 新增 lint：async_trait 宏为 async trait 方法生成 `#[must_use]`，而返回类型
// （Pin<Box<dyn Future>>）本身已是 must_use → 宏展开处必然双标记。触发点在宏生成的
// 不可编辑代码里，源处无法消除；非本仓代码质量信号。待 async_trait 更新或迁移到原生
// async fn in trait 后移除本豁免。
#![allow(clippy::double_must_use)]

pub mod api;
/// 应用配置（toml 加载，`AppConfig`）。
pub mod config;
/// lux 嵌入数据库辅助（建配置、身份引导、rtdata 读写、优雅关闭）。
pub mod db;
/// 库错误类型（`RsIotError`、`TResult`）。
pub mod error;
/// 事件总线（SSE 实时推送 · 阶段一）：进程内 broadcast 扇出。
pub mod event_bus;
/// 历史时序存储写路径（Phase 1b）：旁路 fan-out → 有界 channel → 单写线程 → tsink。
pub mod history;
pub mod host;
/// 硬件 ID 生成。
pub mod hwid;
/// 报文 trace：per-instance 运行时开关 + 字节 dump（裸报文调试）。
pub mod io_trace;
/// 日志保留期清理（按天/按总大小 + 周期 cleanup task）。
mod log_cleanup;
/// 日志 ring buffer（P1 子系统 3）：tracing layer 截获最近 N 条日志到内存。
pub mod log_ringbuffer;
/// 设备模型注册表（物模型）域：地址参数化/属性合并/类型校验/存储/迁移。
pub mod model;
pub mod pipeline;
/// 插件系统（trait 契约、注册表、实例管理、内置实现）。
pub mod plugin;
/// 进程级指标（M4）：Atomic 计数器，REST /sysstatus 直读。
pub mod process_metrics;
pub mod ticker;
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
/// `event_bus_capacity` 为 EventBus broadcast 容量（C 阶段 T2 配置化，调用方传
/// `cfg.event_bus_capacity`；无完整配置的管理命令传
/// `config::DEFAULT_EVENT_BUS_CAPACITY`）。
/// 用 `try_init` 容忍重复调用（进程级 subscriber 单例，重复设置静默忽略）。
///
/// 返回 tuple：
/// - `WorkerGuard`：调用方必须持有到 shutdown 末尾 drop，触发 non_blocking worker
///   drain 完剩余日志，保证故障末尾落盘（M3，修复 `std::mem::forget` 丢末尾问题）。
///   `None` = 文件层未启用（dir 空 或 建目录失败），无需持有。
/// - `Arc<LogRingBuffer>`：日志 ring buffer（P1 子系统 3），注入 AppState 供
///   `GET /api/v1/logs/recent` 读。同一 Arc 被 `RingBufferLayer` 持有截获日志。
/// - `EventBus`：SSE 事件总线（阶段一）。同一 bus 注入 `RingBufferLayer`（日志扇出）、
///   `HostContext`（报文/状态扇出）、`AppState`（SSE endpoint 订阅），三方共享。
pub fn init_tracing(
    cfg: Option<&config::LogConfig>,
    event_bus_capacity: usize,
) -> (
    Option<tracing_appender::non_blocking::WorkerGuard>,
    std::sync::Arc<crate::log_ringbuffer::LogRingBuffer>,
    crate::event_bus::EventBus,
) {
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

    // P1 子系统 3：日志 ring buffer（tracing layer 截获最近 N 条日志到内存）。
    // 独立 EnvFilter("info,io_trace=trace")：全局 filter 是 info 会滤掉 trace level，
    // ring buffer layer 用独立 filter 才能抓报文 trace（target io_trace，trace level）。
    let ring_capacity = cfg.map(|c| c.ring_buffer_capacity).unwrap_or(2000);
    let ring_buffer = std::sync::Arc::new(crate::log_ringbuffer::LogRingBuffer::new(ring_capacity));
    let ring_filter =
        EnvFilter::try_new("info,io_trace=trace").unwrap_or_else(|_| EnvFilter::new("info"));
    // 阶段一 SSE：建事件总线（容量配置化，C 阶段 T2/R6——Rtdata 载荷带值后
    // 事件变大，慢消费者更易 lagged，可经 config 调大），同一 bus 注入
    // RingBufferLayer（日志扇出）、HostContext（报文/状态扇出）、
    // AppState（SSE endpoint 订阅）。
    let event_bus = crate::event_bus::EventBus::new(event_bus_capacity);
    layers.push(
        crate::log_ringbuffer::RingBufferLayer::new(ring_buffer.clone(), event_bus.clone())
            .with_filter(ring_filter)
            .boxed(),
    );

    let _ = tracing_subscriber::registry().with(layers).try_init();
    (guard, ring_buffer, event_bus)
}

/// Windows console 信号监听(CTRL_C + CTRL_BREAK),供 warden 等监护器触发 graceful。
#[cfg(windows)]
mod console_shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// 收到 CTRL_C(0)/CTRL_BREAK(1)后置位。CTRL_BREAK 供 warden 等监护器经
    /// GenerateConsoleCtrlEvent 发给独立 process group 触发 graceful。
    pub(crate) static GOT: AtomicBool = AtomicBool::new(false);
    pub(crate) extern "system" fn on_console_ctrl(ctrl: u32) -> i32 {
        if ctrl == 0 || ctrl == 1 {
            GOT.store(true, Ordering::SeqCst);
            1 // TRUE:已处理,阻止默认 handler
        } else {
            0
        }
    }
}

/// 等待关闭信号:Windows=CTRL_C/CTRL_BREAK(底层 SetConsoleCtrlHandler),
/// Unix=SIGINT(tokio ctrl_c)。收到后返回,调用方据此 cancel shutdown token。
async fn shutdown_signal() {
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering;
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(console_shutdown::on_console_ctrl),
                1,
            );
        }
        while !console_shutdown::GOT.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// CLI `run` 入口：内部建 shutdown + 信号监听，委托给 `run_app_with_shutdown`。
///
/// `log_ringbuffer` 与 `event_bus` 由调用方（binary 入口 `init_tracing` 解构得到）注入，
/// 透传至 `run_app_with_shutdown` → AppState（同一 Arc/EventBus 供 `RingBufferLayer`
/// 与 GET 端点 / SSE 共享）。
#[tracing::instrument(skip(cfg, log_ringbuffer, event_bus))]
pub async fn run_app(
    cfg: config::AppConfig,
    log_ringbuffer: std::sync::Arc<crate::log_ringbuffer::LogRingBuffer>,
    event_bus: crate::event_bus::EventBus,
) -> Result<(), RsIotError> {
    let shutdown = CancellationToken::new();
    let s = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received, shutting down");
        s.cancel();
    });
    run_app_with_shutdown(cfg, shutdown, log_ringbuffer, event_bus).await
}

/// 服务态入口（M4 Windows Service / systemd）：shutdown 由外部（SCM/sigterm）触发。
///
/// 数据流（hub 模式，STEP 5 pull 化）：N 个 Source ──hub(mpsc)──► pipeline
/// ──Processor──► lux HSET rtdata:{devid}；北向默认 luaNorth 实例
/// （mqtt-pub-default，T8 起）周期从 lux 抽取 rtdata → PUBLISH `{hwid}/{devid}/datas`。
#[tracing::instrument(skip(cfg, shutdown, log_ringbuffer, event_bus))]
pub async fn run_app_with_shutdown(
    cfg: config::AppConfig,
    shutdown: CancellationToken,
    log_ringbuffer: std::sync::Arc<crate::log_ringbuffer::LogRingBuffer>,
    event_bus: crate::event_bus::EventBus,
) -> Result<(), RsIotError> {
    // hwid 覆盖（self-monitor 计划 T1）：cfg.hwid 非空=部署期指定唯一值
    //（多网关接同一平台规避同型号 CPU 撞号）；空=按硬件自动生成。
    let hwid = types::Hwid::from(if cfg.hwid.is_empty() {
        hwid::hardware_id()
    } else {
        cfg.hwid.clone()
    });
    tracing::info!(%hwid, "rs-iot starting");

    // 审计 B3 非破坏护栏：默认弱 JWT 密钥（公开常量，可伪造 token）只提示
    // 不拒启——生产部署（尤其 api_bind_host 非 loopback 时）必须显式配置。
    if cfg.jwt_secret == config::DEFAULT_JWT_SECRET {
        tracing::error!(
            "JWT 使用默认弱密钥（源码公开常量，任何读过源码者可伪造 token）——             生产部署请在 config.toml 显式配置强 jwt_secret"
        );
    }

    // D-SEC-1：secrets AEAD 密钥进程级初始化（jwt_secret 恒定）——先于任何
    // secrets 读写（REST/脚本/北向路径共享同一派生键）。
    db::secrets::init_key(&cfg.jwt_secret);

    std::fs::create_dir_all(&cfg.lux.data_dir)?;
    // D-LUX-1：非 loopback 绑定强制认证的前置校验。走 Err 而非 panic——
    // run_app 返回 Result 且 create_dir_all? 是既有启动失败惯例（main 经
    // anyhow 打印后非 0 退出）；build_lux_config 内的 panic 仅为绕过本入口的兜底。
    db::validate_lux_bind(&cfg.lux.bind_host, &cfg.lux.password)
        .map_err(crate::error::RsIotError::Config)?;
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

    let metrics = std::sync::Arc::new(process_metrics::ProcessMetrics::new());
    let ctl_registry = host::CtlRegistry::new(metrics.clone());
    let bridge_registry = host::BridgeRegistry::new();
    // 报文 trace 注册表：HostContext（task dump）与 AppState（REST 激活/查询）共享同一实例
    //（TraceRegistry 内部 Arc<DashMap>，clone 共享状态——REST activate 立即影响 task dump）。
    // with_sender 注入事件总线：dump 激活时扇出 Packet 到 SSE。
    let trace_registry = crate::io_trace::TraceRegistry::with_sender(event_bus.clone());
    // history 先置 None：引擎初始化在 cache::reload 之后（effective 物化才可
    // 校准 cardinality），init 区再统一注入 ctx.history（见下方 pipeline spawn 前）。
    let mut ctx = host::HostContext {
        lux: client.clone(),
        hwid: hwid.clone(),
        shutdown: shutdown.clone(),
        ctl_registry,
        bridge_registry: bridge_registry.clone(),
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        event_tx: event_bus.clone(),
        history: None,
    };

    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<types::Sample>(512);
    let mgr = Arc::new(plugin::instance::InstanceManager::new(ctx.clone(), hub_tx));

    // 北向 MQTT（T8 无损迁移）：cfg.mqtt.host 非空 → 默认 luaNorth 实例
    // （mqtt-pub-default + 内置 mqttPub 兼容脚本），经 InstanceManager 全套生命周期
    // 管理；R5 单通道锁在 compat::start_default_north（本进程已运行不二次启动、
    // lux 已有同名配置沿用不覆盖、auto_start=true 交 lux autoStart 路径）。
    #[cfg(feature = "lua")]
    {
        use plugin::builtin::lua_north::compat::{start_default_north, DefaultNorth};
        match start_default_north(
            &cfg,
            &hwid,
            std::path::Path::new(plugin::lua_core::loader::SCRIPTS_ROOT),
            &client,
            &mgr,
        )
        .await
        {
            DefaultNorth::Started => {
                tracing::info!("[mqtt] 默认北向实例 mqtt-pub-default 已启动（mqttPub 无损迁移）");
            }
            DefaultNorth::Disabled => {
                tracing::info!("[mqtt] [mqtt].host 为空——不生成默认北向实例（显式关闭）");
            }
            DefaultNorth::Failed(e) => {
                tracing::warn!("[mqtt] 默认北向实例启动失败：{e}");
            }
            // AlreadyRunning / DeferredToAutoStart：compat 内已记日志。
            _ => {}
        }
    }
    #[cfg(not(feature = "lua"))]
    if !cfg.mqtt.host.trim().is_empty() {
        tracing::warn!(
            "[mqtt] 本构建未启用 lua feature：[mqtt] 配置无效（mqttPub 已删，北向发布走 luaNorth 实例）"
        );
    }

    // pipeline：hub → processor → lux rtdata（不再 fan-out 到 sink）。
    // Processor 恒 Passthrough：rhai 已冻结不接线（Q7，见 transform/rhai.rs 头注）；
    // 数据加工走 luaTransform 订阅型实例（阶段 C），Processor 插槽留给未来 Rust 原生处理器。
    let processor: Arc<dyn plugin::traits::Processor> =
        Arc::new(transform::passthrough::Passthrough);
    // selfMonitor 隐式模型注册（M-2 T5）：`_sys_rsiot` 模型 + devreg 设备 rsiot
    // 幂等入册——运行时设备也走模型的完整性验证。须在 effective 装载**之前**
    // （缓存含 rsiot 标签）。
    crate::plugin::builtin::self_monitor::ensure_sys_model_registered(&client).await;
    // 设备模型注册表 effective 物化缓存装载（M-1 T8；模型∪覆盖 → tag_key →
    // TagProps，Phase 1b 历史/死区与发现引擎的 O(1) 读侧；fail-soft）。
    match model::cache::reload(&client).await {
        Ok(m) => tracing::debug!("[model] effective 物化缓存装载 {} 条", m.len()),
        Err(e) => tracing::warn!("[model] effective 物化缓存装载失败(空快照): {e}"),
    }
    // 发现队列启动装载（M-4 T1）：lux discoveries → 内存计数缓冲（count 接续）。
    match model::discovery::load(&client).await {
        Ok(n) => tracing::debug!("[discovery] 发现队列装载 {n} 条"),
        Err(e) => tracing::warn!("[discovery] 发现队列装载失败(计数重新累计): {e}"),
    }
    // 历史引擎初始化 + 编排件提取（Phase 1b/审计 B7）：cache::reload 之后
    //（auto cardinality 读 effective）、pipeline spawn 之前（handle 注入
    // HostContext）。P8 fail-soft：init 失败仅 error! 停用归档；三态推导与
    // 四件套提取收敛在 history::EngineParts（引擎所有权留此——D10 关停序
    // 需要 &mut close）。
    #[cfg(feature = "history")]
    let mut history_engine = history::engine::maybe_init(&cfg.history, &model::cache::effective())
        .and_then(|r| match r {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::error!("[history] 引擎初始化失败,归档停用: {e}");
                None
            }
        });
    #[cfg(not(feature = "history"))]
    if cfg.history.enabled {
        // P7：L1/L2 正交——feature off 而 enabled=true 是配置错误，告警并忽略。
        tracing::warn!("[history] 配置 enabled=true 但编译未含 history feature,忽略");
    }
    #[cfg(feature = "history")]
    let parts = history::EngineParts::of(history_engine.as_ref(), &cfg.history, hwid.as_str());
    #[cfg(not(feature = "history"))]
    let parts = history::EngineParts::disabled();
    ctx.history = parts.handle;
    let history_stats = parts.stats;
    let history_engine_state = parts.state;
    let history_reader = parts.reader;
    let history_telemetry = parts.telemetry;
    // 内建丢弃水位告警（Task 18，P6 收官）：引擎在场才起——engine=None
    //（feature off / L2 disabled / init_failed）时 dropped 恒 0，判定无意义，
    // 不 spawn（最小面）。独立任务而非 pipeline worker 上下文：worker 是 N 路
    // 并发数据面热循环（周期判定会 N 份各自触发），也不借 selfMonitor 周期
    //（用户可调上报节拍，告警节拍是基础设施关注点）；照 log_cleanup 先例
    // child token 随进程退。
    if let Some(stats) = history_stats.clone() {
        history::alarm::spawn_drop_alarm_task(
            stats,
            client.clone(),
            event_bus.clone(),
            shutdown.child_token(),
            // Task B/#4：告警阈值配置化（[history].alarm_drop_*，默认
            // 10/s、1% = T18 原常量零漂移）
            cfg.history.alarm_drop_rate_per_sec,
            cfg.history.alarm_drop_ratio,
        );
    }
    let pipeline_token = shutdown.child_token();
    let pipeline_handle = tokio::spawn(pipeline::run(
        ctx.clone(),
        hub_rx,
        processor,
        cfg.pipeline.workers,
        pipeline_token,
    ));

    // RtdataWatchService（订阅升级 Phase 2，P7/P9）：常驻构造注入 AppState；
    // 仅 ksub 模式 spawn 消费循环（poll 时 lux 无订阅者零开销承诺保持，P8）。
    // token 用 shutdown.child_token()——进程退出即退。T5 将接 subscribe
    // producer 消费本服务做精准 HGETALL。
    let rtdata_watch = Arc::new(api::rtdata_watch::RtdataWatchService::new());
    if cfg.rtdata.sub_impl == config::SubImpl::Ksub {
        // JoinHandle 分离运行（token 级联取消即退，无需 join）；命名绑定
        // `_watcher` 满足 must_use 且避开 clippy::let_underscore_future。
        let _watcher = rtdata_watch.spawn(client.clone(), shutdown.child_token());
        tracing::info!("[rtdataWatch] watcher started (sub_impl=ksub)");
    }

    // lux autoStart 实例（南向 Source / 北向 Sink）
    let started = try_start_from_lux(&mgr, &client).await;
    tracing::info!("started {started} source instance(s) from lux");

    // D-CLN-1：lux `devices` Hash 是历史遗留死表（权威源=模型注册表，见
    // routes_device）——存量非空仅提示，不自动删数据（保守）。
    if let Ok(lux::EmbeddedValue::Int(n)) = client.execute_value("HLEN", &["devices"]).await {
        if n > 0 {
            tracing::warn!(
                "[startup] lux `devices` Hash 存在 {n} 条历史遗留数据（已废弃，设备权威源=模型注册表；可 DEL devices 清理）"
            );
        }
    }

    // selfMonitor：rs-iot 自身即设备（devid=rsiot，17 只读+3 可写标签），随进程
    // 无条件自启（不落 lux instances——重启恒在；stopinst 可临时停）。
    let _ = mgr.start_source(self_monitor_config()).await;
    tracing::info!("started self-monitor instance (devid=rsiot)");

    // `_lib/` 站点共享库全局 watcher（T10/Q15）：`_lib/` 变更影响**所有** luaClient
    // 实例 → watcher 全局唯一，编排层单点启动一次（单例由唯一调用点保证，无需
    // OnceLock），随进程 shutdown token 存活；变更去抖后全实例 reload
    // （hotload::spawn_global_lib_watcher → lua_core::reload）。scripts_root 取
    // 默认 SCRIPTS_ROOT（与 REST save 同基）。
    #[cfg(feature = "lua")]
    crate::plugin::lua_core::hotload::spawn_global_lib_watcher(
        std::path::PathBuf::from(crate::plugin::lua_core::loader::SCRIPTS_ROOT),
        client.clone(),
        mgr.clone(),
        shutdown.clone(),
    );

    // REST :api_port（bind 失败仅告警，不中断数据流）
    let state = api::AppState {
        lux: client.clone(),
        mgr: mgr.clone(),
        hwid: hwid.clone(),
        jwt_secret: cfg.jwt_secret.clone(),
        version: VERSION,
        metrics: metrics.clone(),
        trace_registry: trace_registry.clone(),
        log_ringbuffer,
        event_bus,
        config: std::sync::Arc::new(cfg.clone()),
        history_stats,
        history_engine_state,
        history_reader,
        history_telemetry,
        rtdata_watch,
    };
    let app = api::build_router(state);
    let api_port = cfg.api_port;
    let api_bind_host = cfg.api_bind_host.clone();
    let axum_handle = tokio::spawn(async move {
        match tokio::net::TcpListener::bind((api_bind_host.as_str(), api_port)).await {
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

    // 日志保留期清理 task（M3）：启动跑一次（清上次遗留）+ 每 1h 跑一次。
    // dir 非空才起；失败仅 warn，不阻塞主流程。shutdown.child_token() 绑定退出。
    {
        let log_dir = cfg.log.dir.trim();
        if !log_dir.is_empty() {
            log_cleanup::spawn_cleanup_task(
                log_dir.to_string(),
                cfg.log.max_days,
                cfg.log.max_size_mb,
                shutdown.clone(),
            );
            tracing::info!(
                "log retention cleanup task started (dir={log_dir}, max_days={}, max_size_mb={})",
                cfg.log.max_days,
                cfg.log.max_size_mb
            );
        }
    }

    tracing::info!("rs-iot ready. shutdown signal will stop all components.");

    shutdown.cancelled().await;

    // 等 dispatcher 走完退出路径(token.cancel 级联 + drop worker_txs + join worker),
    // 让 worker 的 best-effort flush 在 shutdown_lux 前完成(防丢最后 ≤50ms 数据)。
    // 5s timeout 兜底防 worker 卡 lux 写致 shutdown 挂死(B3 修复:原 abort() 抢占退出)。
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), pipeline_handle).await;
    // 历史引擎关停（D10 序）：pipeline join 之后（flush 出口不再 fan_out）、
    // shutdown_lux 之前——close 断 writer sender → 排空 + 最终 flush +
    // storage.close（释放 .tsink.lock）；超时放弃由 WAL 回放兜底，不阻塞主序。
    #[cfg(feature = "history")]
    if let Some(e) = history_engine.as_mut() {
        e.close(std::time::Duration::from_secs(
            cfg.history.close_timeout_secs,
        ));
    }
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
            if inst.auto_start {
                // 按方向分发（R4）：South→Source、North→Sink（此前 North 被静默跳过）。
                let ok = match inst.app_type {
                    types::AppType::South => mgr.start_source(inst).await.is_ok(),
                    types::AppType::North => mgr.start_sink(inst).await.is_ok(),
                };
                if ok {
                    count += 1;
                }
            }
        } else {
            // M-2：旧内联形态实例在新类型下无法反序列化——强提示迁移（升级必经）。
            tracing::warn!(
                "[startup] 实例配置反序列化失败（旧内联点表形态？）：{}——执行 POST /api/v1/migrate/inline-devices 迁移后重启",
                crate::db::str_from_value(&arr[i])
            );
        }
        i += 2;
    }
    count
}

/// selfMonitor 实例配置（rs-iot 自监控设备，`selfMonitor` 计划 T3；M-2 T5 模型化）。
///
/// `devices` 绑定 devreg 设备 `rsiot`（模型 `_sys_rsiot`，启动幂等注册）——运行时
/// 设备也走模型的完整性验证；tag 值仍由插件代码生成（`self_monitor::collect`），
/// 实例 runtime-only（不落 lux `instances`），随进程无条件自启。
fn self_monitor_config() -> types::InstConfig {
    types::InstConfig {
        inst_id: "self".into(),
        inst_name: "rs-iot 自监控".into(),
        app_code: "selfMonitor".into(),
        app_type: types::AppType::South,
        auto_start: true,
        app_config: serde_json::json!({}),
        devices: vec![types::DeviceBinding {
            devid: "rsiot".into(),
            bind: Default::default(),
        }],
        host_kind: types::HostKind::Builtin,
    }
}

#[cfg(test)]
mod self_monitor_tests {
    use crate::self_monitor_config;

    /// self 实例编排配置形态：无条件自启的 rsiot 自监控设备（绑定 devreg）。
    #[test]
    fn self_monitor_config_shape() {
        let c = self_monitor_config();
        assert_eq!(c.inst_id.as_str(), "self");
        assert_eq!(c.inst_name, "rs-iot 自监控");
        assert_eq!(c.app_code.as_str(), "selfMonitor");
        assert_eq!(c.app_type, crate::types::AppType::South);
        assert!(c.auto_start, "随进程无条件自启");
        assert_eq!(c.devices.len(), 1, "绑定 devreg 设备 rsiot（模型化）");
        assert_eq!(c.devices[0].devid, "rsiot");
        assert!(matches!(c.host_kind, crate::types::HostKind::Builtin));
        assert!(c.app_config.as_object().is_some(), "app_config 为空对象");
    }

    /// 隐式模型 tags 与 collect 表同源锁定（加标签两处同步的漂移守卫）。
    #[test]
    fn sys_model_matches_collect() {
        let model = crate::plugin::builtin::self_monitor::sys_model();
        let obs = crate::plugin::builtin::self_monitor::SelfObservation::default();
        let collected = crate::plugin::builtin::self_monitor::collect(&obs);
        let model_tags: std::collections::BTreeSet<&str> =
            model.tags.iter().map(|t| t.name.as_str()).collect();
        for key in collected.keys() {
            assert!(
                model_tags.contains(key.as_str()),
                "collect 标签 {key} 未登记进 _sys_rsiot 模型（两处同步，见 sys_model 注释）"
            );
        }
    }
}

#[cfg(test)]
mod init_tracing_tests {
    use crate::config::LogConfig;
    use crate::config::DEFAULT_EVENT_BUS_CAPACITY;
    use crate::init_tracing;

    #[test]
    fn returns_guard_when_dir_nonempty() {
        let dir = std::env::temp_dir().join(format!("rsiot-init-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = LogConfig {
            dir: dir.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let (guard, _ring, _event_bus) = init_tracing(Some(&cfg), DEFAULT_EVENT_BUS_CAPACITY);
        assert!(guard.is_some(), "dir 非空应返回 Some(guard)");
        drop(guard); // 先释放日志文件句柄，再清理目录
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_none_when_dir_empty() {
        let cfg = LogConfig {
            dir: String::new(),
            ..Default::default()
        };
        let (guard, _ring, _event_bus) = init_tracing(Some(&cfg), DEFAULT_EVENT_BUS_CAPACITY);
        assert!(guard.is_none(), "dir 空应返回 None");
    }
}
