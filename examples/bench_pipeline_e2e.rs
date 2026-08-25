//! 端到端 pipeline 吞吐 bench：K 源 → hub(512) → dispatcher → N worker → lux。
//!
//! 补证据链：bench_rtdata 只测纯 lux 写路径（prep 在计时区外，仅测 client.execute HSET）。
//! 生产路径叠加未覆盖：dispatcher 路由、Processor process、write_batch 预处理、
//! Sample clone、hub mpsc 交接。本 bench 直接构造 Sample 推 hub_tx（不走 datasim，
//! 避免 period≥1s 限制与采集逻辑干扰），测 pipeline 端到端稳态点/秒，验证 ≥ 50 万/秒。
//!
//! 设计：
//! - 起 lux（Tiered + port 0 + temp dir，照 bench_rtdata）+ identity bootstrap
//! - 组装 HostContext（照 throughput_e2e.rs）+ hub mpsc<Sample>(512) + pipeline::run（4 worker）
//! - K producer task 各持预构造 Sample 池（N_DEVID × N_POINTS），尽力推 hub_tx（背压自然限速）
//! - 跑 DURATION 秒，每秒采样 pipeline_processed_count 差值；去 WARMUP 前 2s，算 mean/min/max
//! - points/sec = sample_delta/sec × N_POINTS
//!   （pipeline_processed_count 计 Sample 数，每 Sample 含 N_POINTS 个点）
//!
//! 验证门：稳态点/秒 ≥ 50 万（B4 改造目标）；hub_depth 稳态饱和则 pipeline 是瓶颈（测量有效）。
//!
//! 推送方式：尽力推（send().await 背压自然限速）。producer 聚合速率 > pipeline 消费速率时，
//! hub 填满（depth_max→512），producer 阻塞在 send，此时 pipeline_processed_count 增速
//! 反映的是 pipeline 端到端容量（dispatcher+worker+write_batch+lux），即待测对象。
//!
//! 历史归档三档分解（门禁四归因，Phase 1b；env `RSIOT_BENCH_HISTORY`，缺省 `off`）：
//! - `off`（A 档基线）：不起引擎，`ctx.history=None`，行为与本 bench 改造前完全一致；
//! - `idle`（需 `--features history` 编译）：起引擎+注入 handle（同 on），但全部
//!   tag 建模 `history=false`（effective 无白名单）→ fan_out 每 flush 都被调、
//!   每 tag 查表判 skipped、零行发送（writer/tsink 空转）→ 结束照打 stats
//!   （skipped 巨量/written=0）+ close 排空。off vs idle = 引擎固定成本 +
//!   fan_out 判空成本；ts 互异重写仅 on 档（idle 零行发送，ts 无归档语义，
//!   避免原子序号成本混入 off-idle 归因）；
//! - `on`（B 档饱和，需 `--features history` 编译）：全部 tag 建模为 `history=true,
//!   deadband=None` → `maybe_init` 起引擎（独立临时目录）→ `ctx.history=handle`
//!   → flush 出口 fan_out 全量白名单旁路归档 → 结束打印 history stats
//!   （written 等计数证 B 档真在归档）→ close(15s) 排空。
//!   idle vs on = 饱和归档净增量；off vs on = 饱和端到端总劣化。
//!   三档除 env 外参数完全一致；B 相对 A 稳态均值劣化 <5% 为门禁四通过（R8 见总方案）。

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use lux::{AuthConfig, ServerConfig, StorageConfig, StorageMode};
use rs_iot::host::{CtlRegistry, HostContext};
use rs_iot::io_trace::TraceRegistry;
use rs_iot::plugin::traits::Processor;
use rs_iot::process_metrics::ProcessMetrics;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{Hwid, Sample, SampleValue};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 每 Sample 的变量数（= 每设备点数）。
const N_POINTS: usize = 250;
/// 互异 devid 数（dispatcher 路由分布键）。
const N_DEVID: usize = 100;
/// pipeline worker 数。
const WORKERS: usize = 4;
/// 并发 producer task 数（尽力推 hub_tx，需聚合快于 pipeline 消费才能压满 hub）。
const PRODUCERS: usize = 4;
/// 测量窗口（秒）；前 WARMUP_SECS 秒丢弃。
const DURATION_SECS: usize = 10;
/// 热身丢弃秒数（吸收 hub 填充 ramp-up + 首批 50ms 攒批 lag）。
const WARMUP_SECS: usize = 2;
/// hub channel 容量（对标生产 lib.rs 的 512）。
const HUB_CAP: usize = 512;
/// B4 改造目标（点/秒）。
const TARGET_PPS: f64 = 500_000.0;

/// 稳态吞吐统计（去 warmup 后的秒级样本聚合）。
struct BenchStats {
    /// 均值点/秒。
    mean: f64,
    /// 最小点/秒。
    min: f64,
    /// 最大点/秒。
    max: f64,
}

/// f64 均值（空切片返 0.0）。
fn avg(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// 从秒级 rates 算 mean/min/max。
fn stats(rates: &[f64]) -> BenchStats {
    let min = rates.iter().copied().fold(f64::INFINITY, f64::min);
    let max = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    BenchStats {
        mean: avg(rates),
        min,
        max,
    }
}

/// 构造 N_DEVID 个预置 Sample 池（各 N_POINTS 点，devid 互异）。
///
/// producer 循环 clone 池中 Sample 推 hub_tx——隔离 pipeline 端到端成本
/// （不掺 BTreeMap 重复构造，对标 bench_rtdata 的 prep 阶段外置理念）。
fn build_pool() -> Vec<Sample> {
    let hwid = Hwid::from("bench-hwid".to_string());
    let mut out = Vec::with_capacity(N_DEVID);
    for d in 0..N_DEVID {
        let mut values = BTreeMap::new();
        for p in 0..N_POINTS {
            values.insert(
                format!("tag{p}"),
                SampleValue::Float(p as f64 * 0.1 + d as f64),
            );
        }
        out.push(Sample {
            hwid: hwid.clone(),
            devid: format!("dev{d}"),
            values,
            timestamp: 1_700_000_000,
        });
    }
    out
}

/// 构造 bench 用的 lux `ServerConfig`（照 bench_rtdata：Tiered + port 0 + save 60s）。
#[expect(
    clippy::field_reassign_with_default,
    reason = "对标 db::build_lux_config 风格"
)]
fn build_cfg(data_dir: String) -> ServerConfig {
    let mut sc = ServerConfig::default();
    sc.bind_host = "127.0.0.1".to_string();
    sc.port = 0; // OS 分配空闲端口，避免与运行中的 rs-iot 冲突
    sc.http_port = 0; // 禁用 lux 自带 HTTP
    sc.enable_resp = true;
    sc.require_auth = false; // 进程内 client 已预认证
    sc.password = String::new();
    sc.auth = AuthConfig::default();
    sc.data_dir = data_dir.clone();
    sc.storage = StorageConfig {
        mode: StorageMode::Tiered,
        dir: data_dir,
    };
    sc.save_interval = Duration::from_secs(60); // Tiered 下必须 >0
    sc
}

/// 历史归档档位（门禁四三档分解，env `RSIOT_BENCH_HISTORY`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HistMode {
    /// A 档基线：不起引擎，`ctx.history=None`。
    Off,
    /// 引擎+handle 注入同 on，但建模全 `history=false`——fan_out 每 flush
    /// 每 tag 判 skipped、零行发送（writer/tsink 空转）。
    Idle,
    /// B 档饱和：全量白名单旁路归档。
    On,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 历史归档档位（门禁四三档分解）：缺省 off（A 档基线，行为与改造前完全一致）。
    let hist_mode = match std::env::var("RSIOT_BENCH_HISTORY") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "" | "off" => HistMode::Off,
            "idle" => HistMode::Idle,
            "on" => HistMode::On,
            other => {
                return Err(format!("RSIOT_BENCH_HISTORY 仅接受 off/idle/on，实际: {other}").into())
            }
        },
        Err(_) => HistMode::Off,
    };
    // 仅 on 开全白名单：tag 建模 history=true + producer ts 互异重写共用。
    let history_on = hist_mode == HistMode::On;
    // idle+on 起引擎：handle 注入 + 收尾 close/stats/hist 目录清理共用。
    let engine_on = hist_mode != HistMode::Off;
    // L1 裁剪：无 history feature 的二进制跑不了 idle/on——fail-fast 提示而非静默跑 A。
    #[cfg(not(feature = "history"))]
    if engine_on {
        return Err(
            "RSIOT_BENCH_HISTORY=idle/on 需要 --features history 编译（当前二进制未含 L1）"
                .into(),
        );
    }

    // 唯一 temp data_dir（不引入 tempfile 依赖）。
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("系统时钟异常: {e}"))?
        .as_nanos();
    let data_dir = std::env::temp_dir().join(format!("rs-iot-bench-pipeline-{pid}-{ts}"));
    std::fs::create_dir_all(&data_dir)?;

    let cfg = build_cfg(data_dir.to_string_lossy().into_owned());
    let handle = lux::run_with_config(cfg).await?;
    let client = handle.client();
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // M-4 起发现引擎以 effective 缓存判定建模——bench 场景须反映生产稳态
    //（全部点已建模，出口走 seen 纯内存路径）：预注册 1 模型 × N_DEVID 台设备
    // 后装载 effective 缓存。未建模脏点场景由 model::discovery 单测覆盖
    //（LRU/节流），不经本吞吐 bench。
    {
        use rs_iot::model::store;
        use rs_iot::model::types::{DeviceEntry, DeviceModel, TagTemplate};
        // on 档全部 tag 开白名单（history=true，deadband=None）——off/idle 档保持
        // 缺省 props（history=false）：off 不起引擎；idle 靠零白名单使 fan_out
        // 全走 skipped（判空路径照跑，writer 空转）。模型注册与改造前逐字节一致。
        let tag_props = if history_on {
            rs_iot::types::TagProps {
                history: true,
                deadband: None,
                ..Default::default()
            }
        } else {
            Default::default()
        };
        let model = DeviceModel {
            model_id: "_bench".into(),
            tags: (0..N_POINTS)
                .map(|p| TagTemplate {
                    name: format!("tag{p}"),
                    data_type: "f32".into(),
                    props: tag_props.clone(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        store::set_model(&client, &model)
            .await
            .expect("bench model");
        for d in 0..N_DEVID {
            let dev = DeviceEntry {
                devid: format!("dev{d}"),
                model: "_bench".into(),
                ..Default::default()
            };
            store::set_device(&client, &dev)
                .await
                .expect("bench device");
        }
        rs_iot::model::cache::reload(&client)
            .await
            .expect("effective reload");
    }

    // idle/on 档：起历史引擎（L2 on + 独立临时目录，照本 bench 临时目录惯例）并取
    // 发送句柄。off 档（engine_on=false）本块整体短路为 None——ctx.history=None，
    // 与改造前完全一致。idle 档 effective 全 history=false：maybe_init 照常起引擎
    // （auto_cardinality 空白名单下限 1），fan_out 每查表必 skipped、零行发送。
    #[cfg(feature = "history")]
    let hist_dir = std::env::temp_dir().join(format!("rs-iot-bench-pipeline-hist-{pid}-{ts}"));
    #[cfg(feature = "history")]
    let (mut history_engine, history_handle) = if engine_on {
        let hist_cfg = rs_iot::config::HistoryConfig {
            enabled: true,
            data_dir: hist_dir.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let effective = rs_iot::model::cache::effective();
        let engine = rs_iot::history::engine::maybe_init(&hist_cfg, &effective)
            .expect("history=idle/on 时 maybe_init 必须返回 Some")
            .expect("临时目录上历史引擎初始化必须成功");
        let handle = engine
            .handle()
            .expect("writer 存活时 engine.handle() 必须返回 Some");
        (Some(engine), Some(handle))
    } else {
        (None, None)
    };

    // 组装 HostContext（照 throughput_e2e.rs：CtlRegistry/BridgeRegistry/metrics/trace/EventBus::dummy）。
    // history 句柄：idle/on 档注入引擎 handle（flush 出口 fan_out 用）；off 档/feature off 恒 None。
    #[cfg(feature = "history")]
    let ctx_history = history_handle.clone();
    #[cfg(not(feature = "history"))]
    let ctx_history: Option<rs_iot::history::HistoryHandle> = None;
    let shutdown = CancellationToken::new();
    let metrics = Arc::new(ProcessMetrics::new());
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::from("bench-hwid".to_string()),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(metrics.clone()),
        bridge_registry: rs_iot::host::BridgeRegistry::new(),
        metrics: metrics.clone(),
        trace_registry: TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
        history: ctx_history,
    };

    // hub + pipeline（N worker，auto_workers(N>0) 直用）。
    let (hub_tx, hub_rx) = mpsc::channel::<Sample>(HUB_CAP);
    let processor: Arc<dyn Processor> = Arc::new(Passthrough);
    let pipeline_token = shutdown.child_token();
    let pipeline_handle = tokio::spawn(rs_iot::pipeline::run(
        ctx.clone(),
        hub_rx,
        processor,
        WORKERS,
        pipeline_token,
    ));

    // 等 pipeline worker 就位（set_workers_active 在 pipeline::run 开头同步设）。
    // 200ms 足够 runtime 调度 run 的首次 poll（spawn worker 前），warmup 也会吸收残余 lag。
    tokio::time::sleep(Duration::from_millis(200)).await;

    // K producer task：各持 pool 的 Arc，循环 clone Sample 推 hub_tx（尽力推，背压限速）。
    let pool = Arc::new(build_pool());
    let producer_token = CancellationToken::new();
    let pushed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // on 档 ts 源：时序归档要求同 series ts 互异（writer 同 ts 去重 P5），且默认
    // 30 天保留期会拒池内固定 2023 ts（OutOfRetention 整批拒）——on 档以
    // now + 全局单调序号生成互异秒；off/idle 档不触该分支（idle 零行发送，ts
    // 无归档语义，且避免原子序号成本混入 off-idle 归因），池内固定 ts 原样。
    let hist_ts_base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("系统时钟异常: {e}"))?
        .as_secs() as i64;
    let hist_seq = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let mut producer_handles = Vec::with_capacity(PRODUCERS);
    for _ in 0..PRODUCERS {
        let hub_tx = hub_tx.clone();
        let pool = Arc::clone(&pool);
        let token = producer_token.clone();
        let pushed = Arc::clone(&pushed);
        let hist_seq = Arc::clone(&hist_seq);
        producer_handles.push(tokio::spawn(async move {
            let n = pool.len();
            let mut i = 0usize;
            loop {
                let mut sample = pool[i % n].clone();
                if history_on {
                    sample.timestamp = hist_ts_base + hist_seq.fetch_add(1, Ordering::Relaxed);
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    res = hub_tx.send(sample) => {
                        if res.is_err() {
                            // hub 关闭（pipeline 退出），停推。
                            break;
                        }
                        pushed.fetch_add(1, Ordering::Relaxed);
                        i = i.wrapping_add(1);
                    }
                }
            }
        }));
    }

    // 稳态测量：每秒采样 pipeline_processed_count 差值（Sample 数），× N_POINTS 得点/秒。
    match hist_mode {
        HistMode::Off => {}
        HistMode::Idle => println!(
            "history 档位: idle——引擎已起+handle 注入,{} tag 全 history=false(fan_out 每 flush 每 tag 判 skipped,零行发送)",
            N_DEVID * N_POINTS
        ),
        HistMode::On => println!(
            "history 档位: on(B 档)——引擎已起,{} series 全量白名单(history=true,deadband=None)",
            N_DEVID * N_POINTS
        ),
    }
    println!(
        "pipeline e2e bench — {PRODUCERS} producer × {N_DEVID} devid × {N_POINTS} 点, {WORKERS} worker, hub {HUB_CAP}, 测量 {DURATION_SECS}s (去 warmup {WARMUP_SECS}s)"
    );
    println!("{:-<72}", "");

    let mut prev = metrics.pipeline_processed_count.load(Ordering::Relaxed);
    let mut per_sec: Vec<f64> = Vec::with_capacity(DURATION_SECS);
    for sec in 0..DURATION_SECS {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let cur = metrics.pipeline_processed_count.load(Ordering::Relaxed);
        let delta_samples = cur - prev;
        prev = cur;
        let rate_pps = delta_samples as f64 * N_POINTS as f64;
        let hub_last = metrics.hub_depth_last.load(Ordering::Relaxed);
        per_sec.push(rate_pps);
        println!(
            "[{:>2}s] samples={:>8}  rate={:>12.0} 点/秒  hub_depth_last={}",
            sec + 1,
            delta_samples,
            rate_pps,
            hub_last
        );
    }

    // 停 producer → drain → 停 pipeline。
    producer_token.cancel();
    for h in producer_handles {
        let _ = h.await;
    }
    drop(hub_tx); // 关闭 hub → dispatcher recv None 退出
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), pipeline_handle).await;

    // 稳态统计（去 warmup 前 WARMUP_SECS 秒）。
    let warmup = WARMUP_SECS.min(per_sec.len());
    let steady = &per_sec[warmup..];
    let s = stats(steady);
    let total_pushed = pushed.load(Ordering::Relaxed);
    let total_processed = metrics.pipeline_processed_count.load(Ordering::Relaxed);
    let hub_max = metrics.hub_depth_max.load(Ordering::Relaxed);
    let write_fails = metrics.pipeline_write_fail_count.load(Ordering::Relaxed);
    let verdict = if s.mean >= TARGET_PPS { "PASS" } else { "FAIL" };

    println!("{:-<72}", "");
    println!(
        "稳态点/秒（去 warmup {WARMUP_SECS}s）: mean {:>10.0}  min {:>10.0}  max {:>10.0}",
        s.mean, s.min, s.max
    );
    println!(
        "总推送 samples={total_pushed} ({} 点) / 总入库 samples={total_processed} ({} 点) / write_fail={write_fails}",
        total_pushed * N_POINTS as u64,
        total_processed * N_POINTS as u64,
    );
    // hub_depth gauge 在 dispatcher recv 后读 hub_rx.len()，故理论上限 = cap-1（recv 取走 1 个）。
    // max 触及 cap-1 即真饱和：producer 聚合快于 pipeline 消费，pipeline 是瓶颈，测量有效。
    let hub_saturated = hub_max >= (HUB_CAP - 1) as u64;
    println!(
        "hub_depth: max={hub_max} (cap {HUB_CAP}, gauge 上限 {}) — {}",
        HUB_CAP - 1,
        if hub_saturated {
            "饱和(producer 快于 pipeline，pipeline 是瓶颈，测量有效)"
        } else {
            "未饱和(pipeline 跟得上 producer)"
        }
    );
    println!("目标 {TARGET_PPS:.0} 点/秒: {verdict}");

    // idle/on 档收尾：先 close 排空（D10 序：pipeline join 后、lux shutdown 前；
    // close 后计数定格）再打印 history stats——on 档 written 等非零即证真在归档，
    // idle 档应见 skipped 巨量/written=0（证 fan_out 判空路径真在跑且零行发送）。
    // off 档（engine=None）本块零输出零行为，与改造前一致。
    #[cfg(feature = "history")]
    if let Some(engine) = history_engine.as_mut() {
        engine.close(Duration::from_secs(15));
    }
    #[cfg(feature = "history")]
    if let Some(h) = &history_handle {
        let st = h.stats();
        println!(
            "history stats: sent={} written={} dropped={} deadbanded={} skipped={} lane_mismatch={} errors={}",
            st.sent.load(Ordering::SeqCst),
            st.written.load(Ordering::SeqCst),
            st.dropped.load(Ordering::SeqCst),
            st.deadbanded.load(Ordering::SeqCst),
            st.skipped.load(Ordering::SeqCst),
            st.lane_mismatch.load(Ordering::SeqCst),
            st.errors.load(Ordering::SeqCst),
        );
    }

    // 优雅关闭 lux（先 SAVE 刷盘再 shutdown，对标 db::shutdown_lux）。
    let _ = client.execute("SAVE", &[]).await;
    drop(client);
    handle.shutdown_and_wait().await?;

    // 清理 temp dir（best-effort，失败不掩盖 bench 结果）。idle/on 档 hist 目录
    // 在 engine.close 释放 .tsink.lock 后一并清理。
    #[cfg(feature = "history")]
    if engine_on {
        let _ = std::fs::remove_dir_all(&hist_dir);
    }
    if let Err(e) = std::fs::remove_dir_all(&data_dir) {
        eprintln!("warn: 清理 temp dir 失败 {data_dir:?}: {e}");
    }
    Ok(())
}
