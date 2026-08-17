//! Windows Service dispatch：被 SCM 调用的服务入口。
//!
//! install 注册的 binPath 是 `rs-iot.exe service`，SCM start 时以 `service`
//! 子命令调用本入口。dispatch 接管 SCM 协议（start/stop 状态机），在独立
//! tokio runtime 上跑 `run_app_with_shutdown`；SCM Stop 触发 shutdown token，
//! rs-iot 优雅关闭（停实例 → lux SAVE → shutdown）。
//!
//! 关闭策略：等 `run_app_with_shutdown` 返回（保证 lux 已 SAVE）后立即
//! `process::exit(0)`，不等待 tokio runtime 的 worker 线程 drop——后者可能
//! 因 mqtt/连接 task 残留而阻塞，导致 SCM 超时强杀（WIN32_EXIT_CODE 1066）。

use std::sync::mpsc;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;

pub(crate) const SERVICE_NAME: &str = "rs-iot";

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<std::ffi::OsString>) {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let stop_tx_clone = stop_tx;
    let event_handler = move |control_event: ServiceControl| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                let _ = stop_tx_clone.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("register control handler failed: {e:?}");
            return;
        }
    };

    report(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(0),
        1,
        std::time::Duration::from_secs(10),
    );

    let cfg = match rs_iot::config::AppConfig::load_or_default(None) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("config load failed: {e}");
            report(
                &status_handle,
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                ServiceExitCode::ServiceSpecific(1),
                0,
                std::time::Duration::default(),
            );
            return;
        }
    };

    // config 加载成功后初始化日志（含文件输出）。service_main 早期错误（register/config load）
    // 在此之前无 subscriber，由 SCM 事件日志兜底。持有 guard 直到关闭流程末尾显式 drop flush。
    // ring_buffer + event_bus 随 cfg 一并 move 进 app 线程，注入 run_app_with_shutdown → AppState。
    let (_log_guard, log_ringbuffer, event_bus) = rs_iot::init_tracing(Some(&cfg.log));

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    let _app_thread = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("tokio runtime: {e}");
                let _ = done_tx.send(false);
                return;
            }
        };
        let ok = rt
            .block_on(rs_iot::run_app_with_shutdown(
                cfg,
                shutdown_clone,
                log_ringbuffer,
                event_bus,
            ))
            .is_ok();
        let _ = done_tx.send(ok);
        // rt 在此后 drop；若慢，service_main 已 exit(0) 回收，不影响
    });

    report(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP,
        ServiceExitCode::ServiceSpecific(0),
        0,
        std::time::Duration::default(),
    );

    // 阻塞直到 SCM Stop
    let _ = stop_rx.recv();

    report(
        &status_handle,
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::ServiceSpecific(0),
        1,
        std::time::Duration::from_secs(30),
    );
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
}

fn report(
    h: &ServiceStatusHandle,
    state: ServiceState,
    controls: ServiceControlAccept,
    exit_code: ServiceExitCode,
    checkpoint: u32,
    wait_hint: std::time::Duration,
) {
    let _ = h.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: controls,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    });
}

pub(crate) fn dispatch() -> anyhow::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("service dispatch failed: {e:?}"))
}
