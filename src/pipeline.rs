//! 数据管道：消费 hub 汇聚的所有 Source 样本 → Processor 转换 →
//! fan-out 到 lux 写入与各 Sink。
//!
//! hub 模式：所有 Source 输出汇入共享 mpsc，pipeline 单消费者。
//! 单 Source 启停不影响 pipeline 与其他 Source/Sink。

use crate::db;
use crate::host::HostContext;
use crate::plugin::traits::Processor;
use crate::types::Sample;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn run(
    ctx: HostContext,
    mut hub_rx: mpsc::Receiver<Sample>,
    sink_txs: Vec<mpsc::Sender<Sample>>,
    processor: Arc<dyn Processor>,
    token: CancellationToken,
) {
    tracing::info!("[pipeline] started ({} sinks)", sink_txs.len());
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
                let processed = match processor.process(sample).await {
                    Some(p) => p,
                    None => continue,
                };
                if let Err(e) = db::rtdata::write(
                    &ctx.lux, &processed.hwid, &processed.devid,
                    &processed.values, processed.timestamp,
                ).await {
                    tracing::warn!("[pipeline] rtdata write failed: {e}");
                }
                for tx in &sink_txs {
                    let _ = tx.send(processed.clone()).await;
                }
            }
        }
    }
    tracing::info!("[pipeline] stopped");
}
