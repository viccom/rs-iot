//! `selfMonitor`：rs-iot 自监控设备插件（devid=`rsiot`）。
//!
//! 自身识别/资源/统计标签周期上报 + 三可写标签（watchdog/note/report_period）；
//! 编排层无条件自启（替换旧 sim1 demo）。
//! 完整标签表见 docs/designs/2026-08-21-self-monitor-device-plan.md。

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 实例 ID（编排层直启的固定 ID；stat 排除口径也锚定它）。
const INST_ID: &str = "self";
/// 设备 ID（rs-iot 自身即设备）。
const DEVID: &str = "rsiot";
/// 默认上报周期（秒），`app_config.period` 可覆盖。
const DEFAULT_REPORT_PERIOD_SECS: u64 = 5;
/// 上报周期钳制下界（秒）——防 0 周期 busy loop。
const REPORT_PERIOD_MIN_SECS: u64 = 1;
/// 上报周期钳制上界（秒）。
const REPORT_PERIOD_MAX_SECS: u64 = 3600;
/// note 标签最大字符数（超长截断）。
const NOTE_MAX_CHARS: usize = 256;
/// 写成功日志中 note 值的显示截断（字符）。
const NOTE_LOG_DISPLAY_CHARS: usize = 32;

/// 一次 tick 的原始观测（Source run 循环从 lux/metrics/静态信息组装；纯函数不耦合异步）。
#[derive(Debug, Default)]
pub(crate) struct SelfObservation {
    pub hwid: String,
    pub version: String,
    pub platform: String,
    pub uptime_s: u64,
    pub cpu: f32,
    pub mem_bytes: u64,
    pub collector_count: usize,
    pub device_count: usize,
    pub tag_count: usize,
    pub inst_running: usize,
    pub inst_degraded: usize,
    pub inst_failed: usize,
    pub alarm_active: u64,
    pub dbsize: i64,
    pub pipeline_processed: u64,
    pub hub_depth: u64,
    /// 历史旁路丢弃行数（累计，T16）：HistoryStats.dropped，引擎缺席恒 0。
    pub history_dropped: u64,
    /// 历史写错误行数（累计，T16）：HistoryStats.errors，引擎缺席恒 0。
    pub history_errors: u64,
    /// 历史写入成功行数（累计，T16）：HistoryStats.written，引擎缺席恒 0。
    pub history_written: u64,
    /// 可写标签当前值（None = 从未写过，不插键）。
    pub watchdog: Option<i64>,
    pub note: Option<String>,
}

/// selfMonitor 隐式模型 id（内置注册进 devmodels——"运行时设备也走模型"的
/// 完整性验证，决策 §一.5）。
pub(crate) const SYS_MODEL_ID: &str = "_sys_rsiot";

/// 隐式模型定义（tags 与 [`collect`] 表同源——代码是运行时权威，模型是元数据
/// 登记面；加标签时两处同步，`sys_model_matches_collect` 单测锁定漂移）。
pub(crate) fn sys_model() -> crate::model::types::DeviceModel {
    use crate::model::types::{DeviceModel, TagTemplate};
    let tag = |name: &str, data_type: &str, desc: &str| TagTemplate {
        name: name.into(),
        data_type: data_type.into(),
        desc: Some(desc.into()),
        unit: None,
        addr: None, // 纯软设备（采集由代码驱动，无协议地址）
        rw: crate::model::types::TagRw::R,
        props: crate::types::TagProps::default(),
        ..Default::default()
    };
    DeviceModel {
        model_id: SYS_MODEL_ID.into(),
        vendor: Some("rs-iot".into()),
        model_no: Some("builtin".into()),
        category: Some("自监控".into()),
        desc: Some("系统自监控设备内置隐式模型（启动幂等注册）".into()),
        params: vec![],
        tags: vec![
            tag("id", "string", "硬件 ID"),
            tag("version", "string", "构建版本"),
            tag("platform", "string", "平台（os/arch）"),
            tag("uptime_s", "int", "运行时长秒"),
            tag("cpu_percent", "float", "进程 CPU %"),
            tag("mem_bytes", "int", "进程内存字节"),
            tag("healthy", "bool", "自身健康"),
            tag("collector_count", "int", "实例总数"),
            tag("device_count", "int", "设备总数（devreg）"),
            tag("tag_count", "int", "标签总数（effective）"),
            tag("inst_running", "int", "运行实例数"),
            tag("inst_degraded", "int", "降级实例数"),
            tag("inst_failed", "int", "失败实例数"),
            tag("alarm_active", "int", "活跃告警数"),
            tag("dbsize", "int", "lux 键总数"),
            tag("pipeline_processed_count", "int", "pipeline 累计处理样本"),
            tag("hub_depth_last", "int", "hub 深度最近值"),
            tag(
                "history_dropped_total",
                "int",
                "历史旁路丢弃行数（累计，引擎缺席恒 0）",
            ),
            tag(
                "history_errors_total",
                "int",
                "历史写错误行数（累计，引擎缺席恒 0）",
            ),
            tag(
                "history_written_total",
                "int",
                "历史写入成功行数（累计，引擎缺席恒 0）",
            ),
            tag("watchdog", "int", "看门狗计数（可选）"),
            tag("note", "string", "人工备注（可选）"),
        ],
        ..Default::default()
    }
}

/// 启动幂等注册：`_sys_rsiot` 模型 + devreg 设备 `rsiot`（存在则跳过——
/// 人工改过模板不覆盖）。失败仅 warn（自监控是增强，不阻断主链路）。
pub(crate) async fn ensure_sys_model_registered(lux: &lux::EmbeddedClient) {
    use crate::model::store;
    if store::get_model(lux, SYS_MODEL_ID)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        if let Err(e) = store::set_model(lux, &sys_model()).await {
            tracing::warn!("[selfMonitor] _sys_rsiot 模型注册失败（不阻断）: {e}");
        }
    }
    if store::get_device(lux, DEVID).await.ok().flatten().is_none() {
        let dev = crate::model::types::DeviceEntry {
            devid: DEVID.into(),
            model: SYS_MODEL_ID.into(),
            ..Default::default()
        };
        if let Err(e) = store::set_device(lux, &dev).await {
            tracing::warn!("[selfMonitor] rsiot 设备注册失败（不阻断）: {e}");
        }
    }
}

/// 汇总为 rsiot 设备的标签表（BTreeMap——键有序，rtdata hash 与文档呈现稳定）。
/// 加标签=此处加一行 + [`sys_model`] tags 加一行（扩展点）。
pub(crate) fn collect(obs: &SelfObservation) -> BTreeMap<String, SampleValue> {
    use SampleValue::{Bool, Float, Int, Str};
    let mut m = BTreeMap::new();
    m.insert("id".to_string(), Str(obs.hwid.clone()));
    m.insert("version".to_string(), Str(obs.version.clone()));
    m.insert("platform".to_string(), Str(obs.platform.clone()));
    m.insert("uptime_s".to_string(), Int(obs.uptime_s as i64));
    m.insert("cpu_percent".to_string(), Float(obs.cpu as f64));
    m.insert("mem_bytes".to_string(), Int(obs.mem_bytes as i64));
    // 自身健康 Bool：无降级、无失败业务实例即健康（平台侧最直观的一眼状态）。
    m.insert(
        "healthy".to_string(),
        Bool(obs.inst_degraded == 0 && obs.inst_failed == 0),
    );
    m.insert(
        "collector_count".to_string(),
        Int(obs.collector_count as i64),
    );
    m.insert("device_count".to_string(), Int(obs.device_count as i64));
    m.insert("tag_count".to_string(), Int(obs.tag_count as i64));
    m.insert("inst_running".to_string(), Int(obs.inst_running as i64));
    m.insert("inst_degraded".to_string(), Int(obs.inst_degraded as i64));
    m.insert("inst_failed".to_string(), Int(obs.inst_failed as i64));
    m.insert("alarm_active".to_string(), Int(obs.alarm_active as i64));
    m.insert("dbsize".to_string(), Int(obs.dbsize));
    m.insert(
        "pipeline_processed_count".to_string(),
        Int(obs.pipeline_processed as i64),
    );
    m.insert("hub_depth_last".to_string(), Int(obs.hub_depth as i64));
    // T16 历史三计数：命名对齐 /metrics 的 rs_iot_history_*_total family——
    // 上层平台同一指标名跨出口对得上。累计 counter 语义，引擎缺席恒 0
    //（同 empty_instance_stats_zero 的缺数据=0 口径，只读键集恒定）。
    m.insert(
        "history_dropped_total".to_string(),
        Int(obs.history_dropped as i64),
    );
    m.insert(
        "history_errors_total".to_string(),
        Int(obs.history_errors as i64),
    );
    m.insert(
        "history_written_total".to_string(),
        Int(obs.history_written as i64),
    );
    if let Some(w) = obs.watchdog {
        m.insert("watchdog".to_string(), Int(w));
    }
    if let Some(n) = &obs.note {
        m.insert("note".to_string(), Str(n.clone()));
    }
    m
}

/// `selfMonitor` Source：rs-iot 自身即设备的周期上报。
#[derive(Debug, Default)]
pub struct SelfMonitor;

#[async_trait]
impl Source for SelfMonitor {
    fn name(&self) -> &'static str {
        "selfMonitor"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: SampleSender,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // 上报周期（秒）：app_config.period 覆盖（照 dataSim 读法），缺省 5s；
        // 钳 [1,3600] 防 0 周期 busy loop。运行时经 report_period 写命令改（见下）。
        let (p, clamped) = clamp_period(
            inst.app_config
                .get("period")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_REPORT_PERIOD_SECS),
        );
        if clamped {
            tracing::warn!("[selfMonitor] app_config.period 越界，已钳到 {p}s");
        }
        let mut report_period = p;

        // jitter 相位（同 dataSim）：首 tick 散布，削整点脉冲。
        let phase = Duration::from_millis(crate::ticker::phase_ms(
            inst.inst_id.as_str(),
            Duration::from_secs(report_period),
        ));
        let mut next_tick = tokio::time::Instant::now() + phase;

        // StatTracker（健康状态落 lux stat:self）+ flush ticker（错峰 + 条件落盘）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst.inst_id.as_str());
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        // 可写标签状态：cmd 分支与 tick 分支同 task，select 循环内局部 mut 即可（无需锁）。
        let mut watchdog: Option<i64> = None;
        let mut note: Option<String> = None;
        // 重查询聚合缓存（AGGREGATE_TTL_SECS 内复用，见 fetch_aggregates）。
        let mut agg_cache: Option<(std::time::Instant, Aggregates)> = None;

        tracing::info!(
            "[selfMonitor] inst {} starting (devid={DEVID}, every {report_period}s)",
            inst.inst_id
        );

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[selfMonitor] inst {} stopping", inst.inst_id);
                    break;
                }
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        if m.cmd != "write" {
                            tracing::warn!("[selfMonitor] inst {} unknown cmd: {}", inst.inst_id, m.cmd);
                            continue;
                        }
                        match apply_write(&m.data) {
                            Ok(writes) => {
                                for w in writes {
                                    match w {
                                        SelfWrite::Watchdog(v) => {
                                            watchdog = Some(v);
                                            tracing::info!("[selfMonitor] write watchdog={v}");
                                        }
                                        SelfWrite::Note { value, truncated } => {
                                            if truncated {
                                                tracing::warn!(
                                                    "[selfMonitor] note 超 {NOTE_MAX_CHARS} 字符，已截断"
                                                );
                                            }
                                            tracing::info!(
                                                "[selfMonitor] write note={}",
                                                display_trunc(&value, NOTE_LOG_DISPLAY_CHARS)
                                            );
                                            note = Some(value);
                                        }
                                        SelfWrite::ReportPeriod { value, clamped } => {
                                            if clamped {
                                                tracing::warn!(
                                                    "[selfMonitor] report_period 越界，已钳到 {value}s"
                                                );
                                            }
                                            tracing::info!(
                                                "[selfMonitor] write report_period={value}s (下一 tick 生效)"
                                            );
                                            report_period = value;
                                        }
                                    }
                                }
                                // 设计如此：写后不立即上报，随下一 tick 全量标签出——
                                // rtdata 回读验证的正是完整写回环（含周期节拍）。
                            }
                            Err(reason) => {
                                tracing::warn!("[selfMonitor] write rejected: {reason}");
                            }
                        }
                    }
                }
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                }
                // 动态周期：sleep_until 每次 tick 后按**当前** report_period 重算下一拍
                // ——写命令改 report_period 后，已排定的下一拍照旧，再下一拍起新周期
                //（即"写后下一 tick 生效"）。Sleep 取消安全，select 他臂先完成即重建。
                _ = tokio::time::sleep_until(next_tick) => {
                    next_tick = tokio::time::Instant::now() + Duration::from_secs(report_period);
                    let started = std::time::Instant::now();
                    // 聚合命中缓存则零 lux 往返；过期/首拍现采并续期。
                    let agg = match &agg_cache {
                        Some((t, a)) if t.elapsed() < Duration::from_secs(AGGREGATE_TTL_SECS) => {
                            a.clone()
                        }
                        _ => {
                            let a = fetch_aggregates(&ctx).await;
                            agg_cache = Some((std::time::Instant::now(), a.clone()));
                            a
                        }
                    };
                    let obs = observe_light(&ctx, &agg, watchdog, note.clone());
                    let sample = Sample {
                        hwid: ctx.hwid.clone(),
                        devid: DEVID.to_string(),
                        values: collect(&obs),
                        timestamp: chrono::Local::now().timestamp(),
                    };
                    if tx.send(sample).await.is_err() {
                        break; // pipeline 已关闭
                    }
                    tracker.tick_ok(started.elapsed().as_millis() as u64);
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }

        // 统一退出处理（cancelled 触发；stat 落 stopped）。
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "selfMonitor",
        create: || Box::new(SelfMonitor),
    }
}

// ---- 写命令解析（纯函数，便于单测） ----

/// 一条 write 命令的应用结果（[`apply_write`] 产出；状态更新与日志由调用侧执行）。
#[derive(Debug, PartialEq)]
pub(crate) enum SelfWrite {
    /// watchdog 回读标签（写任意 i64，下周期回读）。
    Watchdog(i64),
    /// 现场备注（已 trim；超长已按字符截断到 [`NOTE_MAX_CHARS`]）。
    Note {
        /// 截断后的值。
        value: String,
        /// 是否发生了截断（调用侧据此 warn）。
        truncated: bool,
    },
    /// 上报周期秒（已钳 [`REPORT_PERIOD_MIN_SECS`]..=[`REPORT_PERIOD_MAX_SECS`]）。
    ReportPeriod {
        /// 钳制后的值。
        value: u64,
        /// 是否发生了钳制（调用侧据此 warn）。
        clamped: bool,
    },
}

/// 解析一条 write 命令负载：JSON 对象按顶层键分派到三可写标签（纯函数）。
///
/// 整条拒绝（`Err` 携带原因，调用侧 warn 丢弃不 panic，照 opcua handle_write 风格）：
/// 坏 JSON / 非对象 / 空对象 / 未知标签（含已知标签混入未知键）/ 值类型不符。
/// 多个**已知**键的合法载荷逐键产出（一命令写多标签可用）。
fn apply_write(data: &str) -> Result<Vec<SelfWrite>, String> {
    let v: serde_json::Value = serde_json::from_str(data).map_err(|e| format!("bad json: {e}"))?;
    let obj = v.as_object().ok_or("payload is not a json object")?;
    if obj.is_empty() {
        return Err("empty payload (no writable tag)".into());
    }
    let mut out = Vec::new();
    for (k, val) in obj {
        match k.as_str() {
            "watchdog" => {
                let n = val
                    .as_i64()
                    .ok_or_else(|| format!("watchdog: expect int, got {val}"))?;
                out.push(SelfWrite::Watchdog(n));
            }
            "note" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| format!("note: expect string, got {val}"))?;
                let trimmed = s.trim();
                let (value, truncated) = if trimmed.chars().count() > NOTE_MAX_CHARS {
                    (trimmed.chars().take(NOTE_MAX_CHARS).collect(), true)
                } else {
                    (trimmed.to_string(), false)
                };
                out.push(SelfWrite::Note { value, truncated });
            }
            "report_period" => {
                let n = val
                    .as_u64()
                    .ok_or_else(|| format!("report_period: expect uint, got {val}"))?;
                let (value, clamped) = clamp_period(n);
                out.push(SelfWrite::ReportPeriod { value, clamped });
            }
            other => return Err(format!("unknown tag: {other}")),
        }
    }
    Ok(out)
}

/// 钳制上报周期到合法区间，返回 (钳后值, 是否发生钳制)。
fn clamp_period(n: u64) -> (u64, bool) {
    if n < REPORT_PERIOD_MIN_SECS {
        (REPORT_PERIOD_MIN_SECS, true)
    } else if n > REPORT_PERIOD_MAX_SECS {
        (REPORT_PERIOD_MAX_SECS, true)
    } else {
        (n, false)
    }
}

/// 显示用截断（字符级，附加省略号）。
fn display_trunc(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}...")
    } else {
        s.to_string()
    }
}

// ---- 一次 tick 的观测组装（collect 的输入侧） ----

/// 平台描述（启动期一次采样，OnceLock 防每 tick 调 sysinfo）。
///
/// sysinfo 0.32 的 `System::name()`/`System::os_version()` 是**静态关联函数**
/// （无需 `new_all()` 实例），拼接形如 "Windows 10.0.19044 x86_64"。
fn platform() -> &'static str {
    static PLATFORM: OnceLock<String> = OnceLock::new();
    PLATFORM.get_or_init(|| {
        let mut parts = Vec::new();
        if let Some(n) = sysinfo::System::name() {
            parts.push(n);
        }
        if let Some(v) = sysinfo::System::os_version() {
            parts.push(v);
        }
        parts.push(std::env::consts::ARCH.to_string());
        parts.join(" ")
    })
}

/// 重查询聚合缓存 TTL（秒）：`instance_stats`/`instance_health`/`alarms`/`DBSIZE`
/// 合计 ~10 次跨线程 lux 往返（含 KEYS 全键扫），每 5s tick 全查是周期 CPU 脉冲的
/// 组成部分；TTL 内复用上次结果。CPU/内存/uptime/pipeline 计数（进程内 atomics）
/// 仍每 tick 现采——资源类标签保持 5s 新鲜，计数类标签接受 ≤60s 陈旧。
const AGGREGATE_TTL_SECS: u64 = 60;

/// 一次重查询聚合（[`fetch_aggregates`] 产物；TTL 缓存载体）。
#[derive(Debug, Default, Clone)]
pub(crate) struct Aggregates {
    pub collector_count: usize,
    pub device_count: usize,
    pub tag_count: usize,
    pub inst_running: usize,
    pub inst_degraded: usize,
    pub inst_failed: usize,
    pub alarm_active: u64,
    pub dbsize: i64,
}

/// 现采重查询聚合（lux 慢路径）。来源与口径同旧 `observe` 直查实现，无语义变化。
async fn fetch_aggregates(ctx: &HostContext) -> Aggregates {
    // 统计与配置白名单同源（同一次 HGETALL）——健康口径只认配置内实例。
    let (collector_count, device_count, tag_count, configured) = instance_stats(&ctx.lux).await;
    let (inst_running, inst_degraded, inst_failed) = instance_health(&ctx.lux, &configured).await;
    // 复用 db 层告警封装：list 只计有效记录（坏 JSON 被跳过），比裸 HLEN 口径更准。
    let alarm_active = crate::db::alarms::list(&ctx.lux)
        .await
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let dbsize = match ctx.lux.execute_value("DBSIZE", &[]).await {
        Ok(lux::EmbeddedValue::Int(n)) => n,
        _ => 0,
    };
    Aggregates {
        collector_count,
        device_count,
        tag_count,
        inst_running,
        inst_degraded,
        inst_failed,
        alarm_active,
        dbsize,
    }
}

/// 组装一次轻量观测（进程内现采）+ 缓存的聚合值。
///
/// 轻量项：hwid/version/platform（静态）/uptime/cpu/mem（进程内）/pipeline 计数
/// （atomics）/可写标签回显。聚合项由调用方传缓存（见 run 的 `agg_cache`）。
fn observe_light(
    ctx: &HostContext,
    agg: &Aggregates,
    watchdog: Option<i64>,
    note: Option<String>,
) -> SelfObservation {
    let (cpu, mem_bytes) = crate::process_metrics::sample_process_resource();
    SelfObservation {
        hwid: ctx.hwid.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: platform().to_string(),
        uptime_s: ctx.metrics.started.elapsed().as_secs(),
        cpu,
        mem_bytes,
        collector_count: agg.collector_count,
        device_count: agg.device_count,
        tag_count: agg.tag_count,
        inst_running: agg.inst_running,
        inst_degraded: agg.inst_degraded,
        inst_failed: agg.inst_failed,
        alarm_active: agg.alarm_active,
        dbsize: agg.dbsize,
        pipeline_processed: ctx
            .metrics
            .pipeline_processed_count
            .load(std::sync::atomic::Ordering::Relaxed),
        hub_depth: ctx
            .metrics
            .hub_depth_last
            .load(std::sync::atomic::Ordering::Relaxed),
        // T16 历史三计数：ctx.history（pipeline fan_out 同一共享句柄）的
        // HistoryStats 原子快照——与 sysstatus//metrics 同源，writer 增量即时
        // 可见。引擎缺席（feature off/L2 disabled/init_failed fail-soft）时
        // history=None → 恒 0（缺数据=0，同 dbsize 读失败口径）。
        history_dropped: ctx
            .history
            .as_ref()
            .map(|h| h.stats.dropped.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0),
        history_errors: ctx
            .history
            .as_ref()
            .map(|h| h.stats.errors.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0),
        history_written: ctx
            .history
            .as_ref()
            .map(|h| h.stats.written.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0),
        watchdog,
        note,
    }
}

/// lux `instances` 全表统计 → (collector_count, device_count, tag_count, 配置实例 ID 集)。
///
/// 直接 RESP（HGETALL + 逐条反序列化，照 lib.rs `try_start_from_lux` 形态）：
/// db 层无 instances 封装；api::hgetall 属 REST 回显层（serde_json Value 形态），
/// 插件反向依赖 api 层不合适。单条 InstConfig 反序列化失败跳过并 warn，不阻断整表
/// （坏条目也不进 ID 集——其 stat 键同属不可信）。
async fn instance_stats(
    lux: &lux::EmbeddedClient,
) -> (usize, usize, usize, std::collections::HashSet<String>) {
    let configured = std::collections::HashSet::new();
    let arr = match lux.execute_value("HGETALL", &["instances"]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => return (0, 0, 0, configured),
    };
    let mut collectors = 0;
    let mut configured = std::collections::HashSet::new();
    let mut i = 0;
    while i + 1 < arr.len() {
        let raw = match &arr[i + 1] {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => {
                i += 2;
                continue;
            }
        };
        match serde_json::from_str::<crate::types::InstConfig>(&raw) {
            Ok(inst) => {
                collectors += 1;
                configured.insert(inst.inst_id.to_string());
            }
            Err(e) => tracing::warn!(
                "[selfMonitor] instances 项反序列化失败，跳过统计：{e}（raw={}）",
                display_trunc(&raw, 64)
            ),
        }
        i += 2;
    }
    // M-2：设备/标签口径切模型注册表（devreg 全局设备数 + effective 缓存标签数，
    // 实例配置只剩绑定无点表）。读失败降级 0（概览指标，不阻断采集）。
    let devices = crate::model::store::list_devices(lux)
        .await
        .map(|v| v.len())
        .unwrap_or(0);
    let tags = crate::model::cache::effective().len();
    (collectors, devices, tags, configured)
}

/// lux `stat:*` 状态计数 → (running, degraded, failed)；**排除 `stat:self`**（口径=业务实例）。
///
/// 只统计 `configured`（lux instances 配置集）内的键——残留 stat 键（旧实例被杀
/// 进程未落 stopped、已删配置的历史痕迹）不计入，配置是唯一权威源。键枚举直接
/// RESP（db 层无 scan/list 封装；routes_inst 的 KEYS 遍历是私有 DEL 用途），单键
/// 解析复用 [`crate::db::stat::read`]（InstanceStat 反序列化 + 坏值容错）。
/// stopped 等其它 status 不计入三桶。
async fn instance_health(
    lux: &lux::EmbeddedClient,
    configured: &std::collections::HashSet<String>,
) -> (usize, usize, usize) {
    let keys = match lux.execute_value("KEYS", &["stat:*"]).await {
        Ok(lux::EmbeddedValue::Array(ks)) => ks,
        _ => return (0, 0, 0),
    };
    let self_key = format!("stat:{INST_ID}");
    let mut running = 0;
    let mut degraded = 0;
    let mut failed = 0;
    for k in keys {
        let key = match &k {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => continue,
        };
        if key == self_key {
            continue;
        }
        let Some(inst_id) = key.strip_prefix("stat:") else {
            continue;
        };
        if !configured.contains(inst_id) {
            continue;
        }
        match crate::db::stat::read(lux, inst_id).await {
            Ok(Some(st)) => match st.status.as_str() {
                "running" => running += 1,
                "degraded" => degraded += 1,
                "failed" => failed += 1,
                _ => {}
            },
            Ok(None) => {}
            Err(e) => tracing::warn!("[selfMonitor] {key} 读取失败，跳过：{e}"),
        }
    }
    (running, degraded, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 满观测样本（全部字段非默认值，避免实现靠 Default 蒙混；可写标签 None 见专项用例）。
    fn full_obs() -> SelfObservation {
        SelfObservation {
            hwid: "hw-001".to_string(),
            version: "0.1.2".to_string(),
            platform: "Windows 10 x86_64".to_string(),
            uptime_s: 1234,
            cpu: 12.5,
            mem_bytes: 45_678_901,
            collector_count: 3,
            device_count: 7,
            tag_count: 42,
            inst_running: 2,
            inst_degraded: 1,
            inst_failed: 0,
            alarm_active: 5,
            dbsize: 999,
            pipeline_processed: 8_675_309,
            hub_depth: 3,
            history_dropped: 6_550_001,
            history_errors: 42,
            history_written: 7_512_345,
            watchdog: None,
            note: None,
        }
    }

    #[test]
    fn readonly_tag_set_exact() {
        // 只读标签清单执行态：键集精确等于 20 名（多/漏都失败，加标签先改这里）。
        // BTreeMap keys 按字典序，期望 Vec 同序。
        let keys: Vec<_> = collect(&full_obs()).keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "alarm_active",
                "collector_count",
                "cpu_percent",
                "dbsize",
                "device_count",
                "healthy",
                "history_dropped_total",
                "history_errors_total",
                "history_written_total",
                "hub_depth_last",
                "id",
                "inst_degraded",
                "inst_failed",
                "inst_running",
                "mem_bytes",
                "pipeline_processed_count",
                "platform",
                "tag_count",
                "uptime_s",
                "version",
            ]
        );
    }

    #[test]
    fn writable_none_omitted() {
        // watchdog/note 从未写过（None）→ 不插键，总键数=只读 20
        let m = collect(&full_obs());
        assert!(!m.contains_key("watchdog"));
        assert!(!m.contains_key("note"));
        assert_eq!(m.len(), 20);
    }

    #[test]
    fn writable_some_inserted_with_value() {
        // 写过才插键且值直传：watchdog Int、note Str（中文原样）
        let mut obs = full_obs();
        obs.watchdog = Some(7);
        obs.note = Some("机房A-1号网关".to_string());
        let m = collect(&obs);
        assert_eq!(m.get("watchdog"), Some(&SampleValue::Int(7)));
        assert_eq!(
            m.get("note"),
            Some(&SampleValue::Str("机房A-1号网关".to_string()))
        );
        assert_eq!(m.len(), 22);
    }

    #[test]
    fn value_types_covered() {
        // 四型覆盖：id/version/platform 是 Str，cpu_percent 是 Float（含小数），
        // healthy 是 Bool（降级/失败计数全零 → true），计数是 Int
        let m = collect(&full_obs());
        assert!(matches!(m["id"], SampleValue::Str(_)));
        assert!(matches!(m["version"], SampleValue::Str(_)));
        assert!(matches!(m["platform"], SampleValue::Str(_)));
        assert!(matches!(m["cpu_percent"], SampleValue::Float(_)));
        assert_eq!(
            m["healthy"],
            SampleValue::Bool(false),
            "full_obs inst_degraded=1 → 不健康"
        );
        for k in [
            "uptime_s",
            "mem_bytes",
            "collector_count",
            "device_count",
            "tag_count",
            "inst_running",
            "inst_degraded",
            "inst_failed",
            "alarm_active",
            "dbsize",
            "pipeline_processed_count",
            "hub_depth_last",
            "history_dropped_total",
            "history_errors_total",
            "history_written_total",
        ] {
            assert!(matches!(m[k], SampleValue::Int(_)), "{k} 应为 Int");
        }
    }

    #[test]
    fn numeric_values_passed_through() {
        // 数值直传不丢精度：Int 精确等值、Float 保留小数
        let m = collect(&full_obs());
        assert_eq!(m["uptime_s"], SampleValue::Int(1234));
        assert_eq!(m["mem_bytes"], SampleValue::Int(45_678_901));
        assert_eq!(m["alarm_active"], SampleValue::Int(5));
        assert_eq!(m["dbsize"], SampleValue::Int(999));
        assert_eq!(m["pipeline_processed_count"], SampleValue::Int(8_675_309));
        assert_eq!(m["hub_depth_last"], SampleValue::Int(3));
        // T16 历史三计数直传（互异值防串位）
        assert_eq!(m["history_dropped_total"], SampleValue::Int(6_550_001));
        assert_eq!(m["history_errors_total"], SampleValue::Int(42));
        assert_eq!(m["history_written_total"], SampleValue::Int(7_512_345));
        assert_eq!(m["cpu_percent"], SampleValue::Float(12.5));
        assert_eq!(m["id"], SampleValue::Str("hw-001".to_string()));
    }

    #[test]
    fn empty_instance_stats_zero() {
        // 空实例口径：collector/device/tag/inst_* 全 0 → 对应键 Int(0)（而非缺键）
        let obs = SelfObservation {
            hwid: "hw-001".to_string(),
            version: "0.1.2".to_string(),
            platform: "Windows 10 x86_64".to_string(),
            ..SelfObservation::default()
        };
        let m = collect(&obs);
        for k in [
            "collector_count",
            "device_count",
            "tag_count",
            "inst_running",
            "inst_degraded",
            "inst_failed",
        ] {
            assert_eq!(m[k], SampleValue::Int(0), "{k} 空实例应为 Int(0)");
        }
        assert_eq!(m["healthy"], SampleValue::Bool(true), "全零 → 健康");
    }

    // ---- apply_write（三可写标签写命令解析，纯函数） ----

    /// ReportPeriod 臂的 (value, clamped) 简取。
    fn period_of(out: &[SelfWrite]) -> (u64, bool) {
        match &out[0] {
            SelfWrite::ReportPeriod { value, clamped } => (*value, *clamped),
            other => panic!("应是 ReportPeriod：{other:?}"),
        }
    }

    #[test]
    fn apply_write_watchdog_stores_i64() {
        let out = apply_write(r#"{"watchdog": 7}"#).unwrap();
        assert_eq!(out, vec![SelfWrite::Watchdog(7)]);
        // 负值也是合法 i64（写通路回检任意值）
        assert_eq!(
            apply_write(r#"{"watchdog": -1}"#).unwrap(),
            vec![SelfWrite::Watchdog(-1)]
        );
    }

    #[test]
    fn apply_write_note_trims() {
        let out = apply_write(r#"{"note": "  机房A-1号网关  "}"#).unwrap();
        assert_eq!(
            out,
            vec![SelfWrite::Note {
                value: "机房A-1号网关".to_string(),
                truncated: false
            }]
        );
    }

    #[test]
    fn apply_write_note_truncates_over_256_chars() {
        let long = "备".repeat(300);
        let out = apply_write(&format!(r#"{{"note": "{long}"}}"#)).unwrap();
        match &out[0] {
            SelfWrite::Note { value, truncated } => {
                assert!(truncated, "300 字符应标记截断");
                assert_eq!(value.chars().count(), 256, "按字符截到 256");
                assert!(value.chars().all(|c| c == '备'));
            }
            other => panic!("应是 Note：{other:?}"),
        }
    }

    #[test]
    fn apply_write_report_period_clamps_to_bounds() {
        // 0 → 钳 1；5000 → 钳 3600；界内值直通不标记
        assert_eq!(
            period_of(&apply_write(r#"{"report_period": 0}"#).unwrap()),
            (1, true)
        );
        assert_eq!(
            period_of(&apply_write(r#"{"report_period": 5000}"#).unwrap()),
            (3600, true)
        );
        assert_eq!(
            period_of(&apply_write(r#"{"report_period": 30}"#).unwrap()),
            (30, false)
        );
        // 边界值本身直通
        assert_eq!(
            period_of(&apply_write(r#"{"report_period": 1}"#).unwrap()),
            (1, false)
        );
        assert_eq!(
            period_of(&apply_write(r#"{"report_period": 3600}"#).unwrap()),
            (3600, false)
        );
    }

    #[test]
    fn apply_write_rejects_bad_json() {
        assert!(apply_write("not json").is_err(), "坏 JSON 整条拒");
        assert!(apply_write("").is_err());
    }

    #[test]
    fn apply_write_rejects_unknown_tag() {
        assert!(apply_write(r#"{"foo": 1}"#).is_err(), "未知标签拒");
        // 已知标签混入未知键：整条拒（未知即拒，不部分应用）
        assert!(apply_write(r#"{"watchdog": 1, "foo": 2}"#).is_err());
    }

    #[test]
    fn apply_write_multi_known_tags_all_applied() {
        // 一命令写多标签可用：全部已知键逐键产出。
        // 注意产出序是 serde_json 对象键序（默认 BTreeMap 字典序：note < watchdog），
        // 非输入序——断言按字典序写死。
        let out = apply_write(r#"{"watchdog": 3, "note": "x"}"#).unwrap();
        assert_eq!(
            out,
            vec![
                SelfWrite::Note {
                    value: "x".to_string(),
                    truncated: false
                },
                SelfWrite::Watchdog(3)
            ]
        );
    }

    #[test]
    fn apply_write_rejects_wrong_types_and_empty() {
        assert!(
            apply_write(r#"{"watchdog": "x"}"#).is_err(),
            "watchdog 非整数拒"
        );
        assert!(apply_write(r#"{"note": 5}"#).is_err(), "note 非字符串拒");
        assert!(apply_write(r#"{"report_period": -1}"#).is_err(), "负周期拒");
        assert!(apply_write("{}").is_err(), "空对象拒（无可写标签）");
        assert!(apply_write(r#""str""#).is_err(), "非对象拒");
    }

    #[test]
    fn platform_descriptor_nonempty() {
        // sysinfo 静态函数 + ARCH 恒非空；具体值随环境（此处只验封装非空）
        assert!(!platform().is_empty());
    }

    // ---- T16：历史三计数的数据源接线（ctx.history → HistoryStats → 观测） ----

    /// 构造注入指定 `ctx.history` 的最小 HostContext（其余槽照运行时形态占位）。
    fn host_ctx_with_history(
        lux: lux::EmbeddedClient,
        history: Option<crate::history::rows::HistoryHandle>,
    ) -> HostContext {
        HostContext {
            lux,
            hwid: crate::types::Hwid::new("hw-001"),
            shutdown: CancellationToken::new(),
            ctl_registry: crate::host::CtlRegistry::new(std::sync::Arc::new(
                crate::process_metrics::ProcessMetrics::new(),
            )),
            bridge_registry: crate::host::BridgeRegistry::new(),
            metrics: std::sync::Arc::new(crate::process_metrics::ProcessMetrics::new()),
            trace_registry: crate::io_trace::TraceRegistry::new(),
            event_tx: crate::event_bus::EventBus::dummy(),
            history,
        }
    }

    /// 假 HistoryStats 注入：observe_light 必须从 `ctx.history` 的共享计数器
    /// 读 dropped/errors/written（Relaxed 快照）；引擎缺席（history=None）时
    /// 三观测字段恒 0（计数器语义对齐 empty_instance_stats_zero：缺数据=0
    /// 而非缺键——只读键集保持恒定 20）。
    #[tokio::test]
    async fn observe_light_reads_history_stats() {
        let c = test_client("hist-stats").await;
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let stats = std::sync::Arc::new(crate::history::rows::HistoryStats::default());
        stats
            .dropped
            .store(6_550_001, std::sync::atomic::Ordering::Relaxed);
        stats.errors.store(42, std::sync::atomic::Ordering::Relaxed);
        stats
            .written
            .store(7_512_345, std::sync::atomic::Ordering::Relaxed);
        let handle = crate::history::rows::HistoryHandle::new(vec![tx], stats);

        let ctx = host_ctx_with_history(c.client(), Some(handle));
        let obs = observe_light(&ctx, &Aggregates::default(), None, None);
        assert_eq!(
            obs.history_dropped, 6_550_001,
            "dropped 必须来自 ctx.history.stats"
        );
        assert_eq!(obs.history_errors, 42, "errors 必须来自 ctx.history.stats");
        assert_eq!(
            obs.history_written, 7_512_345,
            "written 必须来自 ctx.history.stats"
        );

        let ctx = host_ctx_with_history(c.client(), None);
        let obs = observe_light(&ctx, &Aggregates::default(), None, None);
        assert_eq!(
            (obs.history_dropped, obs.history_errors, obs.history_written),
            (0, 0, 0)
        );
    }

    // ---- instance_health 口径（配置白名单）----

    /// 唯一临时 data_dir（照 db/alarms::tests 模式，tag 防并行互扰）。
    async fn test_client(tag: &str) -> crate::db::TestLux {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("selfmon-{tag}-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        crate::db::TestLux::new(handle)
    }

    /// 残留 stat 键（无配置实例，如旧版 sim1 被杀进程未落 stopped）不计入健康
    /// 统计——配置（lux instances）是唯一权威源；`stat:self` 恒排除。
    #[tokio::test]
    async fn instance_health_ignores_stale_stat_keys() {
        let c = test_client("stale-stat").await;
        // 配置一个业务实例 mb1；stat 写 mb1(running) + ghost(running 残留) + self。
        let inst = serde_json::json!({
            "inst_id": "mb1", "app_code": "luaClient", "app_type": "south",
            "app_config": {}, "devices": [], "host_kind": "builtin"
        });
        let _: lux::EmbeddedValue = c
            .execute_value("HSET", &["instances", "mb1", &inst.to_string()])
            .await
            .expect("hset instances");
        for (id, status) in [
            ("mb1", "running"),
            ("ghost", "running"),
            ("self", "running"),
        ] {
            let stat = serde_json::json!({ "status": status, "tick_count": 1,
                "fail_count": 0, "last_activity_ts": 0, "last_activity_duration_ms": 0,
                "last_err": "" });
            let _: lux::EmbeddedValue = c
                .execute_value("SET", &[&format!("stat:{id}"), &stat.to_string()])
                .await
                .expect("set stat");
        }
        let (_, _, _, configured) = instance_stats(&c).await;
        assert_eq!(configured.len(), 1, "配置集只含 mb1");
        let (r, d, f) = instance_health(&c, &configured).await;
        assert_eq!((r, d, f), (1, 0, 0), "ghost 残留与 self 不计入");
    }
}
