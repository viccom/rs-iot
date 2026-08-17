//! 数据管道：dispatcher 按 devid hash 把 Sample 路由到 N 个 worker，
//! 每 worker 持独立 `EmbeddedClient::clone`，攒批窗口 50ms → `rtdata::write_batch` → lux。
//!
//! worker 是消费者侧 fan-out，与 Source 解耦（单 Source 启停不影响）。
//! lux 不动、Source trait / Sample 结构不动。
//!
//! 背压链：Source `tx: mpsc::Sender<Sample>`（send 背压）→ hub（512）→ dispatcher
//! → worker channel（256）→ worker 攒批 → lux。dispatcher `send().await` 阻塞即
//! 背压传导至 hub，hub 满则 Source send 阻塞，形成端到端背压。
//!
//! HOL blocking：dispatcher 串行路由，某 worker channel 满(256)时 send 阻塞，其他空闲
//! worker 暂时饿死(head-of-line)。量级不瓶颈(路由 ~300ns/sample，远快于 lux 写)；lux
//! SAVE 期间全 worker 变慢会触发全局背压(端到端背压特性，非 bug)。

use crate::db;
use crate::event_bus::{now_iso8601, EventPayload};
use crate::host::HostContext;
use crate::plugin::ticker::fnv1a_64;
use crate::plugin::traits::Processor;
use crate::types::Sample;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// devid → worker idx（FNV-1a 64bit，复用 `plugin::ticker::fnv1a_64`）。
///
/// 同 devid 恒映射同 worker（同设备数据落同一 lux field，无并发写竞争）；
/// 不同 devid 散布到 [0, n) 多 worker（负载分摊）。`n.max(1)` 防 0 除。
fn route(devid: &str, n: usize) -> usize {
    (fnv1a_64(devid) as usize) % n.max(1)
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
/// 与攒批 buffer。dispatcher 退出后先 `token.cancel()` 级联 cancel 所有 worker
/// （同时处理卡 `flush.tick` 与卡 `rx.recv` 的 worker），再 `drop(worker_txs)`
/// 让 worker 的 `rx.recv()` 返 None，最后 3s timeout join 每个 worker task
/// （防 worker 卡 lux 写）。
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

    // dead[idx]=true: 该 worker channel 已关闭，dispatcher 跳过后续 send（防忙循环 + 日志洪水）。
    let mut dead: Vec<bool> = vec![false; n];

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
                if dead[idx] {
                    // 该 worker 已退出，分片停入库；静默丢弃（首次关闭已告警）。
                    continue;
                }
                // select! 包 send(I3):原裸 send().await 在 worker channel 满(256)时阻塞,
                // 外层 select! 的 cancelled 分支无法观测直到 channel 腾空。内层 select! 让
                // cancel 时立即中断 send(break 跳外层 loop),提升取消响应。
                tokio::select! {
                    _ = token.cancelled() => break,
                    send_res = worker_txs[idx].send(s) => {
                        if send_res.is_err() {
                            // worker channel 关闭（worker 已退出）→ 标 dead，后续该 idx 跳过 send。
                            dead[idx] = true;
                            tracing::error!("[pipeline] worker {idx} closed (devid 分片停入库)");
                        }
                    }
                }
            }
        }
    }
    // 退出：先 cancel 父 token（级联所有 worker child token，同时处理卡 flush.tick 与
    // 卡 rx.recv 的 worker——两条 break 分支统一走此路径），再 drop worker_txs
    // （让仍在 rx.recv() 的 worker 立即返 None），最后 3s timeout join。
    token.cancel();
    drop(worker_txs);
    for (i, t) in worker_tasks.into_iter().enumerate() {
        match tokio::time::timeout(Duration::from_secs(3), t).await {
            Ok(Ok(())) => tracing::info!("[pipeline] worker {i} joined"),
            Ok(Err(e)) if e.is_panic() => {
                // worker panic:task 内未调 worker_exited,补 gauge 减 + 计死亡数(I2 修复:
                // 原 abort 抢占 + 无 panic 检测,gauge 永久偏 + 无死亡计数)。
                ctx.metrics.worker_exited();
                ctx.metrics.worker_died();
                tracing::error!("[pipeline] worker {i} panic: {e}");
            }
            Ok(Err(e)) => tracing::warn!("[pipeline] worker {i} join error: {e}"),
            Err(_) => tracing::warn!("[pipeline] worker {i} join timeout(仍 detach 运行)"),
        }
    }
    tracing::info!("[pipeline] stopped");
}

/// 单 worker：攒批窗口 50ms → `write_batch`。所有 lux 错误 catch 降级，不 propagate。
///
/// **Processor 契约**：`process(s)` 在 select! recv 分支内 await，cancel 延迟 = process 时长。
/// 故 `process` 须非阻塞/快速；慢逻辑(脚本 eval/重 CPU)应内部 `spawn_blocking`，否则阻塞
/// worker 线程 + 取消不安全(Rhai 同步 `eval_with_scope` 即此隐患，见 `transform/rhai.rs`；
/// 本框架不强制，由 Processor 实现负责)。
///
/// 攒批：`tokio::time::interval` 50ms flush ticker（首个立即 tick 跳过）；recv 与
/// flush 在 select! 中竞争——有数据则攒入 buf，到窗口则批量写。失败仅 warn（≥1s
/// 间隔去重，防 lux 挂时 N×20 warn/s 洪水）+ `pipeline_write_fail`（每次都计，计数器
/// 准确），buf 照常 clear（丢这批，不阻塞后续）。
///
/// 退出路径：select! 的 `token.cancelled` 与 `rx.recv() == None` 两分支都 break，
/// 出循环后先 best-effort flush 残留 buf,再调一次 `worker_exited()`。
/// 正常 shutdown 丢失窗口 = flush(≤50ms) + worker channel(≤256/worker) + hub(≤512)积压;
/// 最新值语义下旧数据过期可接受。对称:`set_workers_active(n)` + n×`worker_exited()`
/// 不下溢;panic 路径 task 内未调 worker_exited,由 dispatcher join 检测补调(见 `run` 退出段)。
async fn worker_run(
    ctx: HostContext,
    mut rx: mpsc::Receiver<Sample>,
    processor: Arc<dyn Processor>,
    lux: lux::EmbeddedClient,
    token: CancellationToken,
) {
    let mut buf: Vec<Sample> = Vec::new();
    // 上次 write_batch 失败告警时刻；None=未失败过。≥1s 才再 warn（防洪水），计数器仍每次计。
    let mut last_fail_warn: Option<Instant> = None;
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
                flush_batch(&ctx, &lux, &mut buf, &mut last_fail_warn).await;
            }
        }
    }
    // 退出前 best-effort flush 残留 buf。丢失窗口见上方 worker_run doc(flush ≤50ms
    // + worker channel 256/worker + hub 512 积压)。
    if !buf.is_empty() {
        flush_batch(&ctx, &lux, &mut buf, &mut last_fail_warn).await;
    }
    ctx.metrics.worker_exited();
    tracing::info!("[pipeline] worker exited");
}

/// 攒批写出 + 降级：`write_batch` 成功计 `processed_batch(n)+batch()`，失败计
/// `write_fail()` + warn（≥1s 去重）。无论成败 `buf.clear()`。
async fn flush_batch(
    ctx: &HostContext,
    lux: &lux::EmbeddedClient,
    buf: &mut Vec<Sample>,
    last_fail_warn: &mut Option<Instant>,
) {
    if buf.is_empty() {
        return;
    }
    let n = buf.len();
    match db::rtdata::write_batch(lux, buf).await {
        Ok(n_failed) => {
            ctx.metrics.pipeline_processed_batch(n);
            ctx.metrics.pipeline_batch();
            if n_failed > 0 {
                // I1:per-devid 容错,部分 devid 写失败(其余成功,不丢整批)。记失败数 + warn。
                ctx.metrics.pipeline_write_fail_n(n_failed);
                maybe_warn(
                    last_fail_warn,
                    &format!("write_batch {n_failed} devid 写失败(其余成功)"),
                );
            }
            // rtdata 变更通知（批次级聚合，模块 M2b）：一批一次 publish，不含 value。
            // 50万点/秒下每点 publish 会爆 SSE——此处仅发"哪些 dev 变了"，前端轮询拉值。
            // dev_ids 去重 + 截断 RTDATA_NOTIFY_MAX_DEVS（现场极端多设备时防 payload 过大，
            // count 仍标真实点数总量）。无 SSE 消费者时 publish 静默零开销（broadcast 特性）。
            publish_rtdata_notify(ctx, buf);
        }
        Err(e) => {
            // prep 错(serde_json cell 编码等),整批未写。
            ctx.metrics.pipeline_write_fail();
            maybe_warn(last_fail_warn, &format!("write_batch prep 错: {e}"));
        }
    }
    buf.clear();
}

/// rtdata 变更通知 payload 的 devid 列表截断上限（现场极端多设备时防 payload 过大）。
/// 超出的 dev 不进 dev_ids，但 count 仍记录真实点数总量（前端据此判断是否回退全量拉取）。
const RTDATA_NOTIFY_MAX_DEVS: usize = 50;

/// 发一批 rtdata 变更通知（去重 + 截断 dev_ids，不带 value）。
///
/// 从 `buf` 聚合唯一 devid（同批多点同设备只列一次），截断到 RTDATA_NOTIFY_MAX_DEVS，
/// 经 `ctx.event_tx.publish` 投递。`count` 始终是 buf 真实点数（未截断），前端据此
/// 估算变更规模。无 SSE 消费者时 publish 静默（broadcast::send 无 receiver 返 Err 被忽略）。
fn publish_rtdata_notify(ctx: &HostContext, buf: &[Sample]) {
    let mut seen = std::collections::HashSet::new();
    let dev_ids: Vec<String> = buf
        .iter()
        .map(|s| s.devid.clone())
        .filter(|d| seen.insert(d.clone()))
        .take(RTDATA_NOTIFY_MAX_DEVS)
        .collect();
    ctx.event_tx.publish(EventPayload::Rtdata {
        dev_ids,
        count: buf.len(),
        ts: now_iso8601(),
    });
}

/// 限流 warn:首次或距上次 ≥1s 才记(防 lux 挂时 N×20 warn/s 洪水)。
fn maybe_warn(last: &mut Option<Instant>, msg: &str) {
    let now = Instant::now();
    let should = last
        .map(|t| now.duration_since(t) >= Duration::from_secs(1))
        .unwrap_or(true);
    if should {
        tracing::warn!("[pipeline] {msg}");
        *last = Some(now);
    }
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

    /// route 与 ticker::fnv1a_64 共用同一算法（DRY 锁定：任一方改算法此测试拦漂移）。
    #[test]
    fn route_uses_ticker_fnv1a() {
        // route(d, n) = fnv1a_64(d) % n  →  fnv1a_64(d) = route(d, n) + k*n。
        // 取 n=1：route(d, 1) 恒 0，故 fnv1a_64(d) 必 = route(d, large) % large 的同源。
        // 直接跨模块比对：route(d, n) == (fnv1a_64(d) as usize) % n。
        for d in ["devA", "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7"] {
            for n in [1usize, 2, 4, 8] {
                assert_eq!(
                    route(d, n),
                    (fnv1a_64(d) as usize) % n.max(1),
                    "route({d}, {n}) 必须等于 fnv1a_64({d}) % {n}"
                );
            }
        }
    }
}
