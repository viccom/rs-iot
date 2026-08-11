use anyhow::Result;
use std::sync::Arc;

/// `run` 子命令：委托给 lib::run_app。
///
/// `log_ringbuffer` 与 `event_bus` 透传至 run_app → AppState（由 binary 入口
/// init_tracing 解构注入）。
pub(crate) async fn run(
    cfg: rs_iot::config::AppConfig,
    log_ringbuffer: Arc<rs_iot::log_ringbuffer::LogRingBuffer>,
    event_bus: rs_iot::event_bus::EventBus,
) -> Result<()> {
    rs_iot::run_app(cfg, log_ringbuffer, event_bus).await?;
    Ok(())
}
