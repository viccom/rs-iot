//! 数据管道：dispatcher 按 devid hash 把 Sample 路由到 N 个 worker，
//! 每 worker 持独立 `EmbeddedClient::clone`，攒批窗口 50ms → `rtdata::write_batch` → lux。
//!
//! worker 是消费者侧 fan-out，与 Source 解耦（单 Source 启停不影响）。
//! lux 不动、Source trait / Sample 结构不动。
//!
//! 背压链：Source `tx: mpsc::Sender<Sample>`（send 背压）→ hub（512）→ dispatcher
//! → worker channel（256）→ worker 攒批 → lux。dispatcher `send().await` 阻塞即
//! 背压传导至 hub，hub 满则 Source send 阻塞，形成端到端背压。

use crate::db;
use crate::host::HostContext;
use crate::plugin::traits::Processor;
use crate::types::Sample;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// devid → worker idx（FNV-1a 64bit，与 `plugin::ticker::fnv1a_64` 同算法保一致）。
///
/// 同 devid 恒映射同 worker（同设备数据落同一 lux field，无并发写竞争）；
/// 不同 devid 散布到 [0, n) 多 worker（负载分摊）。`n.max(1)` 防 0 除。
fn route(devid: &str, n: usize) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in devid.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h as usize) % n.max(1)
}

/// 解析 worker 数：`requested > 0` 直接用；否则按 CPU 并行度自动，上限 8
/// （工业边缘机核数有限，过多 worker 抢 lux session 与调度无益）。
fn auto_workers(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cpu.min(8)
}

/// 启动 dispatcher + N worker。dispatcher 持有 `hub_rx`，按 devid 路由。
///
/// dispatcher 是 hub 的唯一消费者（背压传导点）；每 worker 独占一个 lux session
/// 与攒批 buffer。dispatcher 退出后经父 `token` 级联 cancel 所有 worker，
/// 再 3s timeout join 每个 worker task（防 worker 卡 lux 写）。
pub async fn run(
    ctx: HostContext,
    mut hub_rx: mpsc::Receiver<Sample>,
    processor: Arc<dyn Processor>,
    workers: usize,
    token: CancellationToken,
) {
    let n = auto_workers(workers);
    tracing::info!("[pipeline] starting {n} worker(s)");
    ctx.metrics.set_workers_active(n);

    let (worker_txs, worker_tasks): (Vec<mpsc::Sender<Sample>>, Vec<_>) = (0..n)
        .map(|_| {
            let (wtx, wrx) = mpsc::channel::<Sample>(256);
            let ctx_w = ctx.clone();
            let proc_w = processor.clone();
            let lux_w = ctx.lux.clone(); // clone: 独立 session Mutex，共享 store
            let token_w = token.child_token();
            let task = tokio::spawn(worker_run(ctx_w, wrx, proc_w, lux_w, token_w));
            (wtx, task)
        })
        .unzip();

    // dispatcher: 唯一 hub_rx 消费者，路由 + 背压 send。
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                tracing::info!("[pipeline] dispatcher cancelled");
                break;
            }
            sample = hub_rx.recv() => {
                let s = match sample {
                    Some(s) => s,
                    None => {
                        tracing::info!("[pipeline] hub closed");
                        break;
                    }
                };
                ctx.metrics.hub_depth(hub_rx.len());
                let idx = route(&s.devid, n);
                if worker_txs[idx].send(s).await.is_err() {
                    // worker channel 关闭（worker 已退出）→ 该 devid 分片停入库。
                    // dispatcher 不退出（其他分片仍可入库），仅告警。
                    tracing::error!("[pipeline] worker {idx} closed (devid 分片停入库)");
                }
            }
        }
    }
    // 退出:cancel 所有 worker token（经父 token 已级联）+ 等 task（3s timeout）。
    for (i, t) in worker_tasks.into_iter().enumerate() {
        let _ = tokio::time::timeout(Duration::from_secs(3), t).await;
        tracing::info!("[pipeline] worker {i} joined");
    }
    tracing::info!("[pipeline] stopped");
}

/// 单 worker：攒批窗口 50ms → `write_batch`。所有 lux 错误 catch 降级，不 propagate。
///
/// 攒批：`tokio::time::interval` 50ms flush ticker（首个立即 tick 跳过）；recv 与
/// flush 在 select! 中竞争——有数据则攒入 buf，到窗口则批量写。失败仅 warn +
/// `pipeline_write_fail`，buf 照常 clear（丢这批，不阻塞后续）。
///
/// 退出路径：select! 的 `token.cancelled` 与 `rx.recv() == None` 两分支都 break，
/// 出循环后调一次 `worker_exited()`。对称保证：`set_workers_active(n)` + n×
/// `worker_exited()` 不会下溢（详见 `ProcessMetrics::worker_exited` 契约）。
async fn worker_run(
    ctx: HostContext,
    mut rx: mpsc::Receiver<Sample>,
    processor: Arc<dyn Processor>,
    lux: lux::EmbeddedClient,
    token: CancellationToken,
) {
    let mut buf: Vec<Sample> = Vec::new();
    let mut flush = tokio::time::interval(Duration::from_millis(50));
    flush.tick().await; // 跳过首个立即 tick
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            s = rx.recv() => match s {
                Some(s) => {
                    if let Some(p) = processor.process(s).await {
                        buf.push(p);
                    }
                }
                None => break,
            },
            _ = flush.tick() => {
                if buf.is_empty() {
                    continue;
                }
                let n = buf.len();
                match db::rtdata::write_batch(&lux, &buf).await {
                    Ok(()) => {
                        ctx.metrics.pipeline_processed_batch(n);
                        ctx.metrics.pipeline_batch();
                    }
                    Err(e) => {
                        ctx.metrics.pipeline_write_fail();
                        tracing::warn!("[pipeline] write_batch failed: {e}");
                    }
                }
                buf.clear();
            }
        }
    }
    ctx.metrics.worker_exited();
    tracing::info!("[pipeline] worker exited");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同 devid 恒映射同 worker；不同 devid 散布到多 worker（确定性路由）。
    #[test]
    fn route_devid_to_worker_deterministic() {
        let n = 4;
        let w_a1 = route("devA", n);
        let w_a2 = route("devA", n);
        assert_eq!(w_a1, w_a2, "同 devid 必同 worker");
        assert!(w_a1 < n);
        let workers: std::collections::HashSet<usize> =
            ["d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7"]
                .iter()
                .map(|d| route(d, n))
                .collect();
        assert!(workers.len() >= 2, "应散布到多 worker");
    }

    /// n=0 不 panic（`.max(1)` 防御），落到 worker 0（避免 0 除 panic）。
    #[test]
    fn route_zero_workers_does_not_panic() {
        let idx = route("devA", 0);
        assert_eq!(idx, 0);
    }

    /// requested>0 直接用；0 走自动（CPU 并行度，上限 8）。
    #[test]
    fn auto_workers_respects_requested_and_caps() {
        assert_eq!(auto_workers(1), 1);
        assert_eq!(auto_workers(16), 16);
        let auto = auto_workers(0);
        let cpu = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        assert_eq!(auto, cpu.min(8));
    }
}
