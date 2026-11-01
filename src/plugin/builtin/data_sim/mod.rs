//! `dataSim`：模拟数据生成 Source。
//!
//! 对标 Lite_IOT `plugins/shared/datasim/datasim_core.go`：ticker 周期产出
//! `{hwid, devid, values, timestamp}`，推入 mpsc 给 pipeline。
//! 同时监听命令下行 channel（命令由北向 MQTT `<hwid>/command` 注入）。

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 模拟数据 Source（`dataSim`）：按设备规格周期产出正弦变化的样本。
#[derive(Debug, Default)]
pub struct DataSim;

#[async_trait]
impl Source for DataSim {
    fn name(&self) -> &'static str {
        "dataSim"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: SampleSender,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        // M-2 模型驱动装配：绑定 → devreg/devmodels 展开 → 合成点表
        // （纯软模型 addr=None → child.value = tag 名，本插件只消费 tag 名/类型）。
        let devices = match crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(
                    "[dataSim] inst {} 绑定展开失败（设备未注册？）: {e}",
                    inst.inst_id
                );
                return;
            }
        };
        if devices.is_empty() {
            tracing::warn!("[dataSim] inst {} has no devices, exiting", inst.inst_id);
            return;
        }

        // 采集周期（秒），可在 app_config.period 覆盖，默认 3s。
        let period_secs = parse_period(&inst.app_config);

        let period = std::time::Duration::from_secs(period_secs);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase = std::time::Duration::from_millis(crate::ticker::phase_ms(
            inst.inst_id.as_str(),
            period,
        ));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // StatTracker（M4 实例指标）+ flush ticker（周期落 lux stat:<inst>）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst.inst_id.as_str());
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        tracing::info!(
            "[dataSim] inst {} starting ({} devices, every {}s)",
            inst.inst_id,
            devices.len(),
            period_secs
        );

        'run: loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[dataSim] inst {} stopping", inst.inst_id);
                    break 'run;
                }
                msg = cmd_rx.recv() => {
                    // G-2（2026-11-01 审查）：通道关闭即退出（对齐 selfMonitor/sidecar；
                    // None 空过会让 select 立即重 Poll 热转）。
                    let Some(m) = msg else { break 'run };
                    tracing::info!("[dataSim] inst {} cmd: {} (devid={})", inst.inst_id, m.cmd, m.devid);
                }
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                }
                _ = ticker.tick() => {
                    let started = std::time::Instant::now();
                    let now = chrono::Local::now().timestamp();
                    for dev in &devices {
                        let mut values = BTreeMap::new();
                        for child in &dev.children {
                            if child.value.is_empty() {
                                continue;
                            }
                            values.insert(child.value.clone(), simulate(&child.value, &child.r#type, now));
                        }
                        if values.is_empty() {
                            values.insert("value".to_string(), simulate("value", "float", now));
                        }
                        let sample = Sample {
                            hwid: ctx.hwid.clone(),
                            devid: dev.value.clone(),
                            values,
                            timestamp: now,
                        };
                        if tx.send(sample).await.is_err() {
                            // pipeline 已关闭。G-1（2026-11-01 审查）：退出整个循环
                            // ——此前只 break 内层 for，task 每 tick 空转零输出、无
                            // Failed 信号（selfMonitor/sidecar 同场景都是退出）。
                            tracing::warn!("[dataSim] inst {} hub 已关闭，退出", inst.inst_id);
                            return;
                        }
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

/// 解析采集周期（秒）：`app_config.period`，缺省 3s，**最小钳制 1 秒**。
///
/// 纯函数（便于单测）：`period=0` 直通的话 `tokio::time::interval_at` 会 panic
/// （tokio 对零周期直接炸），用户一次误配就能把实例 task 打崩（supervisor 标 Failed）。
/// 与 modbus/opcua/s7/eip 的 `parse_cfg` 同口径（`.max(1)`）。
fn parse_period(app: &serde_json::Value) -> u64 {
    app.get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .max(1)
}

/// 基于 tag + 时间产生一个平滑变化的模拟值（确定性，便于观察）。
fn simulate(tag: &str, kind: &str, now: i64) -> SampleValue {
    let phase = (tag.bytes().fold(0u32, |a, b| a.wrapping_add(b as u32)) % 360) as f64;
    let t = now as f64;
    let v = 50.0 + 40.0 * ((t / 10.0 + phase).to_radians().sin());
    let v = (v * 100.0).round() / 100.0;
    match kind {
        "int" | "Int" => SampleValue::Int(v as i64),
        "bool" | "Bool" => SampleValue::Bool(v > 50.0),
        _ => SampleValue::Float(v),
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "dataSim",
        create: || Box::new(DataSim),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simulate_is_deterministic_per_tag_and_time() {
        let a = simulate("temp", "float", 1000);
        let b = simulate("temp", "float", 1000);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        let c = simulate("temp", "float", 1010);
        assert_ne!(
            format!("{a:?}"),
            format!("{c:?}"),
            "value should change over time"
        );
    }

    #[test]
    fn parse_period_defaults_to_3() {
        assert_eq!(parse_period(&serde_json::json!({})), 3);
    }

    #[test]
    fn parse_period_keeps_explicit_value() {
        assert_eq!(parse_period(&serde_json::json!({"period": 10})), 10);
    }

    #[test]
    fn parse_period_clamps_zero_to_one() {
        // 回归：period=0 曾直通 interval_at → tokio panic（Zero period），实例 task 崩。
        assert_eq!(parse_period(&serde_json::json!({"period": 0})), 1);
    }
}
