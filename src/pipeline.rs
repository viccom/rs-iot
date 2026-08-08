//! 数据管道：消费 hub 汇聚的所有 Source 样本 → Processor 转换 → 写 lux rtdata。
//!
//! hub 模式：所有 Source 输出汇入共享 mpsc，pipeline 单消费者。
//! 单 Source 启停不影响 pipeline。北向发布由常驻 `MqttPub` 独立周期从 lux
//! 抽取（STEP 5 起），不再走 pipeline fan-out。

use crate::db;
use crate::host::HostContext;
use crate::plugin::traits::Processor;
use crate::types::Sample;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 消费 hub 汇聚的样本：经 `Processor` 转换后写 lux rtdata。
///
/// 单消费者模型：所有 Source 输出汇入 `hub_rx`，本函数独占消费。
/// 北向 MQTT 发布由独立的 `MqttPub` 周期从 lux 抽取，不再走 pipeline fan-out。
pub async fn run(
    ctx: HostContext,
    mut hub_rx: mpsc::Receiver<Sample>,
    processor: Arc<dyn Processor>,
    token: CancellationToken,
) {
    tracing::info!("[pipeline] started");
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                tracing::info!("[pipeline] cancelled, exiting");
                break;
            }
            sample = hub_rx.recv() => {
                let sample = match sample {
                    Some(s) => s,
                    None => {
                        tracing::info!("[pipeline] hub closed, exiting");
                        break;
                    }
                };
                // hub 背压 gauge：消费一个 sample 后采样队列剩余深度（M4）。
                ctx.metrics.hub_depth(hub_rx.len());
                let processed = match processor.process(sample).await {
                    Some(p) => p,
                    None => continue,
                };
                match db::rtdata::write(
                    &ctx.lux, processed.hwid.as_ref(), &processed.devid,
                    &processed.values, processed.timestamp,
                ).await {
                    Ok(()) => ctx.metrics.pipeline_processed(),
                    Err(e) => {
                        ctx.metrics.pipeline_write_fail();
                        tracing::warn!("[pipeline] rtdata write failed: {e}");
                    }
                }
            }
        }
    }
    tracing::info!("[pipeline] stopped");
}
