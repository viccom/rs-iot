//! 阶段0 bench：量化 lux rtdata 单 worker 写入吞吐，决定 50 万点/秒改造是否需要多 worker。
//!
//! 四场景（同一批预计算 payload，仅写策略不同——从而隔离批量化变量）：
//! - A 逐点 HSET：每点一次 `execute("HSET",[key,field,cell])`（复刻 `rtdata::write` 现状，baseline）
//! - B HSET 多对：每 Sample（devid）一次 `execute("HSET",[key,f1,c1,...,fN,cN])`
//! - C PreparedPipeline：全批 N 个 devid 各一条多对 HSET，push_argv 后一次 discard
//! - D N×clone 并发（N=4）：client.clone() 各处理 chunk，每 chunk 一个 PreparedPipeline + discard
//!
//! 验证门：A < B ≤ C < D 应成立（批量化 + 并发应单调提升吞吐）。
//!
//! 设计取舍：
//! - cell JSON（`[ts,localTime,val]`，对标 `rtdata::write`）在 prep 阶段一次性算好，
//!   不计入计时——四个 bench 测的是纯 lux 写路径，公平对比写策略。
//! - temp data_dir 用 `std::env::temp_dir()` + pid + nanos 唯一化（不引入 tempfile 依赖）。
//! - RESP 端口 0（OS 分配），仅本地，bench 走进程内 `handle.client()`，不连 RESP。
//! - 每场景跑 `ROUNDS + 1` 轮，**首轮作 warmup 丢弃**（不计入 rates）：四场景顺序跑同一批
//!   key（rtdata:dev0..dev99），warmup 吸收哈希槽/类型首次分配成本，使每个场景都测稳态、可比。
//! - 报告 mean + min + max（ROUNDS 轮），操作员可据 min/max 判别异常首轮或抖动。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lux::{
    AuthConfig, EmbeddedClient, LuxError, PreparedPipeline, ServerConfig, StorageConfig,
    StorageMode,
};
use rs_iot::types::{Hwid, Sample, SampleValue};

/// 每 Sample 的变量数（= 每 devid 的点数）。
const N_POINTS: usize = 5000;
/// Sample 数（= devid 数，互异）。
const N_SAMPLES: usize = 100;
/// 每 bench 计时轮数（取平均点/秒）。
const ROUNDS: usize = 5;
/// 场景 D 的并发 clone 数。
const CLONES: usize = 4;

/// 一个 devid 的预计算写入负载：`rtdata:<devid>` key + (field, cell) 对。
/// prep 阶段产出，所有 bench 共享——计时区只做 lux 写。
#[derive(Clone)]
struct PreppedDev {
    /// Hash key，`rtdata:<devid>`。
    key: String,
    /// (field, cell) 对，field = `<devid>.<tag>`，cell = JSON `[ts, localTime, val]`。
    pairs: Vec<(String, String)>,
}

/// 单场景稳态吞吐统计（warmup 丢弃后的 ROUNDS 轮聚合）。
struct BenchStats {
    /// 均值点/秒。
    mean: f64,
    /// 最小点/秒。
    min: f64,
    /// 最大点/秒。
    max: f64,
}

/// 构造 `n_samples` 个 Sample，各带 `n_points` 个点（devid 互异，值用 Float）。
fn mk_samples(n_points: usize, n_samples: usize) -> Vec<Sample> {
    let hwid = Hwid::from("bench-hwid".to_string());
    let mut out = Vec::with_capacity(n_samples);
    for s in 0..n_samples {
        let mut values = BTreeMap::new();
        for p in 0..n_points {
            values.insert(
                format!("tag{p}"),
                SampleValue::Float(p as f64 * 0.1 + s as f64),
            );
        }
        out.push(Sample {
            hwid: hwid.clone(),
            devid: format!("dev{s}"),
            values,
            timestamp: 1_700_000_000 + s as i64,
        });
    }
    out
}

/// 把 Sample 列表预计算为 lux 写入负载（field/cell JSON 格式化在此完成，不计入 bench 计时）。
/// cell 格式严格对标 `db::rtdata::write`：`[timestamp, localTime, valStr]`。
fn prep(samples: &[Sample]) -> Vec<PreppedDev> {
    let local_time = "2026-08-11 00:00:00".to_string();
    let mut out = Vec::with_capacity(samples.len());
    for s in samples {
        let key = format!("rtdata:{}", s.devid);
        let mut pairs = Vec::with_capacity(s.values.len());
        for (tag, val) in &s.values {
            let field = if tag.contains(&s.devid) {
                tag.clone()
            } else {
                format!("{}.{tag}", s.devid)
            };
            let val_str = match val {
                SampleValue::Float(f) => f.to_string(),
                SampleValue::Int(i) => i.to_string(),
                SampleValue::Bool(b) => b.to_string(),
                SampleValue::Str(x) => x.clone(),
                // SampleValue 标记 #[non_exhaustive]；bench 仅生成 Float，兜底不命中
                _ => "0".to_string(),
            };
            let cell =
                serde_json::to_string(&serde_json::json!([s.timestamp, local_time, val_str]))
                    .expect("cell JSON 编码不会失败");
            pairs.push((field, cell));
        }
        out.push(PreppedDev { key, pairs });
    }
    out
}

/// 一轮写入的总点数（所有 devid 的 pairs 之和）。
fn points_total(devs: &[PreppedDev]) -> u64 {
    devs.iter().map(|d| d.pairs.len() as u64).sum()
}

/// f64 均值。
fn avg(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// 从 ROUNDS 轮 rates 算 mean/min/max。
fn stats(rates: &[f64]) -> BenchStats {
    let min = rates.iter().copied().fold(f64::INFINITY, f64::min);
    let max = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    BenchStats {
        mean: avg(rates),
        min,
        max,
    }
}

/// A：逐点 HSET（baseline，复刻 `rtdata::write` 逐点 execute）。
async fn bench_a(client: &EmbeddedClient, devs: &[PreppedDev]) -> Result<BenchStats, LuxError> {
    let total = points_total(devs);
    let mut rates = Vec::with_capacity(ROUNDS);
    // 首轮 warmup 丢弃（round == 0 不计入 rates），吸收哈希槽首次分配成本。
    for round in 0..=ROUNDS {
        let t0 = Instant::now();
        for d in devs {
            for (field, cell) in &d.pairs {
                client
                    .execute("HSET", &[d.key.as_str(), field.as_str(), cell.as_str()])
                    .await?;
            }
        }
        if round > 0 {
            rates.push(total as f64 / t0.elapsed().as_secs_f64());
        }
    }
    Ok(stats(&rates))
}

/// B：HSET 多对（每 Sample/devid 一次 execute，多对 field/value 一次提交）。
async fn bench_b(client: &EmbeddedClient, devs: &[PreppedDev]) -> Result<BenchStats, LuxError> {
    let total = points_total(devs);
    let mut rates = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let t0 = Instant::now();
        for d in devs {
            let mut argv: Vec<&str> = Vec::with_capacity(1 + d.pairs.len() * 2);
            argv.push(d.key.as_str());
            for (f, c) in &d.pairs {
                argv.push(f.as_str());
                argv.push(c.as_str());
            }
            client.execute("HSET", &argv).await?;
        }
        if round > 0 {
            rates.push(total as f64 / t0.elapsed().as_secs_f64());
        }
    }
    Ok(stats(&rates))
}

/// C：PreparedPipeline 全批一次提交（每 devid 一条多对 HSET，push_argv 后一次 discard）。
async fn bench_c(client: &EmbeddedClient, devs: &[PreppedDev]) -> Result<BenchStats, LuxError> {
    let total = points_total(devs);
    let mut rates = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let t0 = Instant::now();
        let mut pipe = PreparedPipeline::new();
        for d in devs {
            let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + d.pairs.len() * 2);
            argv.push(b"HSET");
            argv.push(d.key.as_bytes());
            for (f, c) in &d.pairs {
                argv.push(f.as_bytes());
                argv.push(c.as_bytes());
            }
            pipe.push_argv(argv)?;
        }
        client.execute_prepared_pipeline_discard(&pipe).await?;
        if round > 0 {
            rates.push(total as f64 / t0.elapsed().as_secs_f64());
        }
    }
    Ok(stats(&rates))
}

/// D：N×clone 并发（N=CLONES）：client.clone() 各处理 chunk，每 chunk 一个 PreparedPipeline + discard。
async fn bench_d(client: &EmbeddedClient, devs: &[PreppedDev]) -> Result<BenchStats, LuxError> {
    let total = points_total(devs);
    let chunk_size = devs.len().div_ceil(CLONES);
    // 预切分一次（计时区外）：Arc 包裹，spawn 时 cheap clone（不重复拷贝 payload）。
    let chunks: Vec<Arc<Vec<PreppedDev>>> = devs
        .chunks(chunk_size)
        .map(|c| Arc::new(c.to_vec()))
        .collect();
    let mut rates = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let t0 = Instant::now();
        let mut handles = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let c = client.clone();
            let chunk = Arc::clone(chunk);
            handles.push(tokio::spawn(async move {
                let mut pipe = PreparedPipeline::new();
                for d in chunk.iter() {
                    let mut argv: Vec<&[u8]> = Vec::with_capacity(2 + d.pairs.len() * 2);
                    argv.push(b"HSET");
                    argv.push(d.key.as_bytes());
                    for (f, cell) in &d.pairs {
                        argv.push(f.as_bytes());
                        argv.push(cell.as_bytes());
                    }
                    pipe.push_argv(argv)?;
                }
                c.execute_prepared_pipeline_discard(&pipe).await
            }));
        }
        for h in handles {
            // h.await = Result<Result<(), LuxError>, JoinError>：外层 JoinError 手转，
            // 内层 LuxError 直接 `?`（JoinError 不 impl Into<LuxError>）。
            let inner = h
                .await
                .map_err(|e| LuxError::Command(format!("bench D task join 失败: {e}")))?;
            inner?;
        }
        if round > 0 {
            rates.push(total as f64 / t0.elapsed().as_secs_f64());
        }
    }
    Ok(stats(&rates))
}

/// 构造 bench 用的 lux `ServerConfig`：Tiered 持久化 + save_interval 60s + 端口 0（OS 分配）。
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

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 唯一 temp data_dir（不引入 tempfile 依赖）。
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("系统时钟异常: {e}"))?
        .as_nanos();
    let data_dir = std::env::temp_dir().join(format!("rs-iot-bench-{pid}-{ts}"));
    std::fs::create_dir_all(&data_dir)?;

    let cfg = build_cfg(data_dir.to_string_lossy().into_owned());
    let handle = lux::run_with_config(cfg).await?;
    let client = handle.client();

    // prep 阶段（不计入 bench 计时）：构造 Sample + 预算 field/cell。
    let samples = mk_samples(N_POINTS, N_SAMPLES);
    let devs = prep(&samples);
    let total_per_round = points_total(&devs);

    println!(
        "lux rtdata bench — {N_SAMPLES} devid × {N_POINTS} 点 = {total_per_round} 点/轮, warmup 1 + 计时 {ROUNDS} 轮, 场景 D 并发 {CLONES}"
    );
    println!("{:-<72}", "");

    let a = bench_a(&client, &devs).await?;
    println!(
        "{:>10}  mean {:>10.0}  min {:>10.0}  max {:>10.0}  点/秒",
        "A 逐点", a.mean, a.min, a.max
    );

    let b = bench_b(&client, &devs).await?;
    println!(
        "{:>10}  mean {:>10.0}  min {:>10.0}  max {:>10.0}  点/秒",
        "B 多对", b.mean, b.min, b.max
    );

    let c = bench_c(&client, &devs).await?;
    println!(
        "{:>10}  mean {:>10.0}  min {:>10.0}  max {:>10.0}  点/秒",
        "C 管道", c.mean, c.min, c.max
    );

    let d = bench_d(&client, &devs).await?;
    println!(
        "{:>10}  mean {:>10.0}  min {:>10.0}  max {:>10.0}  点/秒",
        "D 并发", d.mean, d.min, d.max
    );

    println!("{:-<72}", "");
    println!(
        "加速比(mean): B/A={:.2}x  C/A={:.2}x  D/A={:.2}x  D/C={:.2}x",
        b.mean / a.mean,
        c.mean / a.mean,
        d.mean / a.mean,
        d.mean / c.mean,
    );

    // 优雅关闭：先 SAVE 刷盘再 shutdown（对标 db::shutdown_lux）。
    let _ = client.execute("SAVE", &[]).await;
    drop(client);
    handle.shutdown_and_wait().await?;

    // 清理 temp dir（best-effort，失败不掩盖 bench 结果）。
    if let Err(e) = std::fs::remove_dir_all(&data_dir) {
        eprintln!("warn: 清理 temp dir 失败 {data_dir:?}: {e}");
    }
    Ok(())
}
