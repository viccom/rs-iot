//! `eipClient`：EtherNet/IP 南向采集插件（rust-ethernet-ip 1.2，罗克韦尔 AB
//! CompactLogix / ControlLogix）。
//!
//! 周期连接 PLC，按 `devices` 配置批量读 tag，组装 `Sample` 推入 hub；
//! 同时消费下行命令（`cmd="write"`）写回 tag。
//!
//! 配置（`InstConfig::app_config` JSON，缺失字段走默认）：
//! ```json
//! {
//!   "endpoint": "192.168.1.100:44818",
//!   "period": 3,
//!   "slot": null,
//!   "reconnect_secs": 5,
//!   "health_check_secs": 0,
//!   "max_packet_size": null,
//!   "batch_max_operations": null, "batch_max_packet_size": null,
//!   "batch_packet_timeout_ms": null, "batch_continue_on_error": null,
//!   "batch_optimize_packing": null
//! }
//! ```
//! 变量映射（复用 `devices`，与其他南向插件对齐）：
//! - `DeviceSpec.value` = 设备 ID（即 `rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = tag 全名（`Program:Main.Tag` / `MyTag` / `MyArray[3]` /
//!   `MyDint.5` 位寻址——读写两侧均由库自动识别 bit 路径）
//! - `ChildSpec.label` = values map 的 key（显示名；空则用 tag 名）
//! - `ChildSpec.type = "string"` = 该 tag 走 `read_string_tag`（自定义 STRING 类型
//!   如 Str82 的批量路径解不了，须专用解码）；缺省走 `read_tags_batch`。
//!
//! 写命令 `data` JSON：`{"tag":"Program:Main.SetPoint","value":1500,"plc_type":"dint"}`。
//! `plc_type` 可选（CIP 类型化写入，建议带）：bool/sint/int/dint/lint/usint/uint/
//! udint/ulint/real/lreal/string；缺省推断 bool→Bool、整数→Dint、浮点→Real、串→String。

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use rust_ethernet_ip::{BatchConfig, EipClient, PlcValue, RoutePath};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// EtherNet/IP 采集插件（结构体名避开库的 `EipClient`）。
#[derive(Debug, Default)]
pub struct EipClientPlugin;

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
struct EipCfg {
    endpoint: String,
    period: u64,
    /// ControlLogix 背板槽号；None 走普通 connect（CompactLogix 常见）。
    slot: Option<u8>,
    /// 断线重连节流（秒）。
    reconnect_secs: u64,
    /// >0 时周期调 `check_health_detailed`（keep-alive + 自动重注册 session）。
    health_check_secs: u64,
    /// 透传 `EipClient::set_max_packet_size`。
    max_packet_size: Option<u32>,
    /// BatchConfig 透传字段（None = 库默认）。
    batch_max_operations: Option<usize>,
    batch_max_packet_size: Option<usize>,
    batch_packet_timeout_ms: Option<u64>,
    batch_continue_on_error: Option<bool>,
    batch_optimize_packing: Option<bool>,
}

fn parse_cfg(app: &serde_json::Value) -> Result<EipCfg, String> {
    let endpoint = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?
        .to_string();
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .max(1);
    let slot = app.get("slot").and_then(|v| v.as_u64()).map(|v| {
        u8::try_from(v).unwrap_or_else(|_| {
            tracing::warn!("[eipClient] slot {v} 超出 u8，按 255 处理");
            u8::MAX
        })
    });
    Ok(EipCfg {
        endpoint,
        period,
        slot,
        reconnect_secs: app
            .get("reconnect_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .max(1),
        health_check_secs: app
            .get("health_check_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        max_packet_size: app.get("max_packet_size").and_then(|v| v.as_u64()).map(v32),
        batch_max_operations: app
            .get("batch_max_operations")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        batch_max_packet_size: app
            .get("batch_max_packet_size")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        batch_packet_timeout_ms: app.get("batch_packet_timeout_ms").and_then(|v| v.as_u64()),
        batch_continue_on_error: app.get("batch_continue_on_error").and_then(|v| v.as_bool()),
        batch_optimize_packing: app.get("batch_optimize_packing").and_then(|v| v.as_bool()),
    })
}

/// u64 → u32 饱和（配置容错，超界取 MAX 并在解析期已无 per-field 日志面）。
fn v32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// 建立连接并应用透传配置（max_packet_size / batch 参数）。
async fn connect(cfg: &EipCfg) -> Result<EipClient, String> {
    let mut client = match cfg.slot {
        Some(slot) => EipClient::with_route_path(&cfg.endpoint, RoutePath::new().add_slot(slot))
            .await
            .map_err(|e| format!("connect {} slot {slot}: {e}", cfg.endpoint))?,
        None => EipClient::connect(&cfg.endpoint)
            .await
            .map_err(|e| format!("connect {}: {e}", cfg.endpoint))?,
    };
    if let Some(sz) = cfg.max_packet_size {
        client.set_max_packet_size(sz);
    }
    let mut bc = BatchConfig::default();
    if let Some(v) = cfg.batch_max_operations {
        bc.max_operations_per_packet = v;
    }
    if let Some(v) = cfg.batch_max_packet_size {
        bc.max_packet_size = v;
    }
    if let Some(v) = cfg.batch_packet_timeout_ms {
        bc.packet_timeout_ms = v;
    }
    if let Some(v) = cfg.batch_continue_on_error {
        bc.continue_on_error = v;
    }
    if let Some(v) = cfg.batch_optimize_packing {
        bc.optimize_packet_packing = v;
    }
    client.configure_batch_operations(bc);
    Ok(client)
}

/// PlcValue → SampleValue（数值类归一 Float/Int，Bool→Bool，串类→Str）。
///
/// UDT（v1 不做成员级展开）序列化为 JSON 字符串入库可见——`UdtData`
/// 自带 serde Serialize，`{"symbol_id":..,"data":[..]}` 结构化可读。
fn plc_to_sample(v: &PlcValue) -> SampleValue {
    match v {
        PlcValue::Bool(b) => SampleValue::Bool(*b),
        PlcValue::Sint(i) => SampleValue::Int(*i as i64),
        PlcValue::Int(i) => SampleValue::Int(*i as i64),
        PlcValue::Dint(i) => SampleValue::Int(*i as i64),
        PlcValue::Lint(i) => SampleValue::Int(*i),
        PlcValue::Usint(u) => SampleValue::Int(*u as i64),
        PlcValue::Uint(u) => SampleValue::Int(*u as i64),
        PlcValue::Udint(u) => SampleValue::Int(*u as i64),
        // u64 > i64::MAX 显式饱和（as 转换会位重解释成负数，不能用于此处）
        PlcValue::Ulint(u) => SampleValue::Int(i64::try_from(*u).unwrap_or(i64::MAX)),
        PlcValue::Real(f) => SampleValue::Float(*f as f64),
        PlcValue::Lreal(f) => SampleValue::Float(*f),
        PlcValue::String(s) => SampleValue::Str(s.clone()),
        PlcValue::Udt(udt) => {
            SampleValue::Str(serde_json::to_string(udt).unwrap_or_else(|_| format!("{udt:?}")))
        }
    }
}

/// JSON → SampleValue（写命令的 value 字段；与 opcuaClient 同构）。
fn json_to_sample_value(v: &serde_json::Value) -> SampleValue {
    match v {
        serde_json::Value::Bool(b) => SampleValue::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(SampleValue::Int)
            .or_else(|| n.as_f64().map(SampleValue::Float))
            .unwrap_or(SampleValue::Int(0)),
        serde_json::Value::String(s) => SampleValue::Str(s.clone()),
        other => SampleValue::Str(other.to_string()),
    }
}

/// SampleValue + 显式 plc_type → PlcValue。
///
/// CIP 类型化写入（写 DINT tag 传 REAL 会类型不匹配），`plc_type=None` 退回
/// 默认推断：bool→Bool、整数→Dint（AB 最常用）、浮点→Real、串→String。
/// 数值按目标类型截断；Bool 节点接受 0/非零或布尔。
fn plc_with_type(v: &SampleValue, plc_type: Option<&str>) -> PlcValue {
    let default = || match v {
        SampleValue::Bool(b) => PlcValue::Bool(*b),
        SampleValue::Int(i) => PlcValue::Dint(*i as i32),
        SampleValue::Float(f) => PlcValue::Real(*f as f32),
        SampleValue::Str(s) => PlcValue::String(s.clone()),
    };
    let Some(t) = plc_type.map(str::trim).filter(|s| !s.is_empty()) else {
        return default();
    };
    let as_f = || match v {
        SampleValue::Float(f) => *f,
        SampleValue::Int(i) => *i as f64,
        SampleValue::Bool(b) => u8::from(*b) as f64,
        SampleValue::Str(s) => s.parse().unwrap_or(0.0),
    };
    let as_i = || match v {
        SampleValue::Int(i) => *i,
        SampleValue::Float(f) => *f as i64,
        SampleValue::Bool(b) => u8::from(*b) as i64,
        SampleValue::Str(s) => s.parse().unwrap_or(0),
    };
    let as_u = || as_i().max(0) as u64;
    match t.to_lowercase().as_str() {
        "bool" => PlcValue::Bool(match v {
            SampleValue::Bool(b) => *b,
            SampleValue::Int(i) => *i != 0,
            SampleValue::Float(f) => *f != 0.0,
            SampleValue::Str(s) => s.eq_ignore_ascii_case("true"),
        }),
        "sint" => PlcValue::Sint(as_i() as i8),
        "int" => PlcValue::Int(as_i() as i16),
        "dint" => PlcValue::Dint(as_i() as i32),
        "lint" => PlcValue::Lint(as_i()),
        "usint" => PlcValue::Usint(as_u() as u8),
        "uint" => PlcValue::Uint(as_u() as u16),
        "udint" => PlcValue::Udint(as_u() as u32),
        "ulint" => PlcValue::Ulint(as_u()),
        "real" => PlcValue::Real(as_f() as f32),
        "lreal" => PlcValue::Lreal(as_f()),
        "string" => PlcValue::String(match v {
            SampleValue::Str(s) => s.clone(),
            other => format!("{other:?}"),
        }),
        _ => default(),
    }
}

/// 采集目标（预解析）：`(devid, [(field, tag)])`；`string_tags` 单列
/// （走 `read_string_tag`，批量路径只解标准 STRING handle 0x0FCE）。
struct Targets {
    /// 批量读（标量 + 位寻址 tag；`read_tags_batch` 一次网络往返）。
    batch: Vec<(String, Vec<(String, String)>)>,
    /// 逐个 `read_string_tag`（自定义 STRING 类型）。
    strings: Vec<(String, Vec<(String, String)>)>,
}

fn parse_targets(devices: &[crate::types::DeviceSpec]) -> Targets {
    let mut batch = Vec::new();
    let mut strings = Vec::new();
    for dev in devices {
        let mut b = Vec::new();
        let mut s = Vec::new();
        for c in &dev.children {
            if c.value.is_empty() {
                continue;
            }
            let field = if c.label.is_empty() {
                c.value.clone()
            } else {
                c.label.clone()
            };
            if c.r#type.eq_ignore_ascii_case("string") {
                s.push((field, c.value.clone()));
            } else {
                b.push((field, c.value.clone()));
            }
        }
        if !b.is_empty() {
            batch.push((dev.value.clone(), b));
        }
        if !s.is_empty() {
            strings.push((dev.value.clone(), s));
        }
    }
    Targets { batch, strings }
}

#[async_trait]
impl Source for EipClientPlugin {
    fn name(&self) -> &'static str {
        "eipClient"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: SampleSender,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        let inst_id = inst.inst_id.clone();
        let cfg = match parse_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[eipClient] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // M-2 模型驱动装配：绑定展开（模板 addr × devreg params → 绝对 tag 路径）。
        let expanded = match crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("[eipClient] inst {inst_id} 绑定展开失败（设备未注册？）: {e}");
                return;
            }
        };
        let targets = parse_targets(&expanded);

        let period = Duration::from_secs(cfg.period);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase = Duration::from_millis(crate::ticker::phase_ms(inst_id.as_str(), period));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // StatTracker（M4 实例指标）+ flush ticker。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut reconnects: u64 = 0;
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst_id.as_str());
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        // 断线即 None（对标 opcuaClient session=None）；下个 tick 重连（节流）。
        let mut client: Option<EipClient> = None;
        let mut last_connect_attempt = tokio::time::Instant::now()
            .checked_sub(Duration::from_secs(cfg.reconnect_secs))
            .unwrap_or_else(tokio::time::Instant::now);
        let mut last_health_check = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[eipClient] inst {inst_id} stopping");
                    break;
                }
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        if m.cmd == "write" {
                            handle_write(&mut client, &m.data).await;
                        } else {
                            tracing::info!("[eipClient] inst {inst_id} unknown cmd: {}", m.cmd);
                        }
                    }
                }
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                }
                _ = ticker.tick() => {
                    if client.is_none() {
                        let now = tokio::time::Instant::now();
                        if now.duration_since(last_connect_attempt)
                            < Duration::from_secs(cfg.reconnect_secs)
                        {
                            continue; // 重连节流窗口内，跳过本 tick
                        }
                        last_connect_attempt = now;
                        match connect(&cfg).await {
                            Ok(c) => {
                                reconnects += 1;
                                tracker.extra_insert(
                                    "session_reconnect_count",
                                    serde_json::json!(reconnects),
                                );
                                tracing::info!(
                                    "[eipClient] inst {inst_id} connected {}",
                                    cfg.endpoint
                                );
                                client = Some(c);
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[eipClient] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() {
                                    tracker.flush().await;
                                }
                                continue;
                            }
                        }
                    }
                    let Some(c) = client.as_mut() else {
                        continue;
                    };
                    let started = std::time::Instant::now();
                    let mut tick_ok = true;

                    // 可选主动健康检查（keep-alive + 自动重注册，应对 PLC 150s 会话超时）。
                    if cfg.health_check_secs > 0
                        && last_health_check.elapsed()
                            >= Duration::from_secs(cfg.health_check_secs)
                    {
                        last_health_check = std::time::Instant::now();
                        if let Err(e) = c.check_health_detailed().await {
                            tick_ok = false;
                            tracing::warn!("[eipClient] inst {inst_id} health check failed: {e}");
                        }
                    }

                    if tick_ok {
                        tick_ok = collect_once(c, &targets, &ctx, &tx, inst_id.as_str()).await;
                    }
                    if !tick_ok {
                        // 传输级失败（连接断/超时）：拆 session 下 tick 重连。
                        // 单 tag 错误在 collect_once 内 per-tag 容错，不会走到这里。
                        tracker.tick_fail("read failed");
                        client = None;
                        tracing::warn!(
                            "[eipClient] inst {inst_id} transport failure, will reconnect"
                        );
                    } else {
                        tracker.tick_ok(started.elapsed().as_millis() as u64);
                    }
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }

        // 优雅注销 session（best-effort，失败仅 warn）。
        if let Some(mut c) = client {
            if let Err(e) = c.unregister_session().await {
                tracing::warn!("[eipClient] inst {inst_id} unregister failed: {e}");
            }
        }
        tracing::info!("[eipClient] inst {inst_id} exited");
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// 一个采集 tick：批量读标量 tag + 逐个读 string tag → 按 devid 组 Sample 推 hub。
///
/// 返回 false 仅当传输级失败（应拆连接重连）；单 tag 失败（BatchError，
/// 如 tag 不存在）仅 warn + 跳过，不丢整批（对标 pipeline per-devid 容错）。
async fn collect_once(
    client: &mut EipClient,
    targets: &Targets,
    ctx: &HostContext,
    tx: &mpsc::Sender<Sample>,
    inst_id: &str,
) -> bool {
    // devid → BTreeMap 聚合（batch 与 string 两路并到同一 tick 采样）。
    let mut per_dev: BTreeMap<String, BTreeMap<String, SampleValue>> = BTreeMap::new();

    if !targets.batch.is_empty() {
        // 全实例 tag 打平一次批量读（跨 devid 共享 PLC 连接，一次网络往返）。
        // 查找表 tag → (devid, field)：batch 结果带 tag 名但顺序不承诺（打包优化），
        // 按 tag 名回填而非按位置 zip。
        let mut by_tag: std::collections::HashMap<&str, (&str, &str)> =
            std::collections::HashMap::new();
        for (dev, pairs) in &targets.batch {
            for (field, tag) in pairs {
                by_tag.insert(tag.as_str(), (dev.as_str(), field.as_str()));
            }
        }
        let names: Vec<&str> = by_tag.keys().copied().collect();
        match client.read_tags_batch(&names).await {
            Ok(results) => {
                for (tag, res) in results {
                    let Some((dev, field)) = by_tag.get(tag.as_str()) else {
                        continue;
                    };
                    match res {
                        Ok(v) => {
                            per_dev
                                .entry((*dev).to_string())
                                .or_default()
                                .insert((*field).to_string(), plc_to_sample(&v));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[eipClient] inst {inst_id} tag '{tag}' read failed: {e}"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                if e.is_retriable() {
                    return false;
                }
                // 传输看似正常但整批失败（如 PLC 拒绝批量打包）：降级逐 tag 读。
                tracing::warn!(
                    "[eipClient] inst {inst_id} batch read failed: {e}, fallback per-tag"
                );
                for (dev, pairs) in &targets.batch {
                    for (field, tag) in pairs {
                        match client.read_tag(tag).await {
                            Ok(v) => {
                                per_dev
                                    .entry(dev.clone())
                                    .or_default()
                                    .insert(field.clone(), plc_to_sample(&v));
                            }
                            Err(e2) => {
                                if e2.is_retriable() {
                                    return false;
                                }
                                tracing::warn!(
                                    "[eipClient] inst {inst_id} tag '{tag}' read failed: {e2}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    for (dev, pairs) in &targets.strings {
        for (field, tag) in pairs {
            match client.read_string_tag(tag).await {
                Ok(s) => {
                    per_dev
                        .entry(dev.clone())
                        .or_default()
                        .insert(field.clone(), SampleValue::Str(s));
                }
                Err(e) => {
                    if e.is_retriable() {
                        return false;
                    }
                    tracing::warn!("[eipClient] inst {inst_id} string tag '{tag}' failed: {e}");
                }
            }
        }
    }

    let now = chrono::Local::now().timestamp();
    for (devid, mut values) in per_dev {
        if values.is_empty() {
            continue;
        }
        let sample = Sample {
            hwid: ctx.hwid.clone(),
            devid: devid.clone(),
            values: std::mem::take(&mut values),
            timestamp: now,
        };
        if tx.send(sample).await.is_err() {
            break; // hub 关闭
        }
    }
    true
}

/// 处理 write 命令：`data` = JSON `{"tag":"...","value":123,"plc_type":"dint"}`。
///
/// STRING 写走 `write_string_tag`（库处理 unconnected messaging + AB STRING 格式）；
/// 位寻址 tag 名（`MyDint.3`）由库自动走位写。
async fn handle_write(client: &mut Option<EipClient>, data: &str) {
    let Some(c) = client.as_mut() else {
        tracing::warn!("[eipClient] write ignored: not connected");
        return;
    };
    let req: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[eipClient] write bad json: {e}");
            return;
        }
    };
    let tag = match req.get("tag").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            tracing::warn!("[eipClient] write missing tag");
            return;
        }
    };
    let value = match req.get("value") {
        Some(v) => json_to_sample_value(v),
        None => {
            tracing::warn!("[eipClient] write missing value");
            return;
        }
    };
    let plc_type = req.get("plc_type").and_then(|v| v.as_str());
    let is_string = matches!(
        plc_type.map(str::trim).filter(|s| !s.is_empty()),
        Some(t) if t.eq_ignore_ascii_case("string")
    ) || (plc_type.is_none() && matches!(value, SampleValue::Str(_)));

    let result = if is_string {
        let SampleValue::Str(s) = &value else {
            unreachable!("is_string 为 true 时 value 必为 Str")
        };
        c.write_string_tag(tag, s).await
    } else {
        c.write_tag(tag, plc_with_type(&value, plc_type)).await
    };
    match result {
        Ok(()) => tracing::debug!("[eipClient] write '{tag}' ok"),
        Err(e) => {
            if e.is_retriable() {
                // 传输级失败：拆 session，下次 tick 重连。
                *client = None;
            }
            tracing::warn!("[eipClient] write '{tag}' failed: {e}");
        }
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "eipClient",
        create: || Box::new(EipClientPlugin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plc_to_sample_maps_all_variants() {
        assert_eq!(
            plc_to_sample(&PlcValue::Bool(true)),
            SampleValue::Bool(true)
        );
        assert_eq!(plc_to_sample(&PlcValue::Dint(-7)), SampleValue::Int(-7));
        assert_eq!(
            plc_to_sample(&PlcValue::Lint(i64::MIN)),
            SampleValue::Int(i64::MIN)
        );
        assert_eq!(
            plc_to_sample(&PlcValue::Udint(u32::MAX)),
            SampleValue::Int(u32::MAX as i64)
        );
        // u64 饱和：ULint::MAX → i64::MAX
        assert_eq!(
            plc_to_sample(&PlcValue::Ulint(u64::MAX)),
            SampleValue::Int(i64::MAX)
        );
        assert_eq!(plc_to_sample(&PlcValue::Real(1.5)), SampleValue::Float(1.5));
        assert_eq!(
            plc_to_sample(&PlcValue::Lreal(2.25)),
            SampleValue::Float(2.25)
        );
        assert_eq!(
            plc_to_sample(&PlcValue::String("hi".into())),
            SampleValue::Str("hi".into())
        );
        // UDT → JSON 字符串（symbol_id + data 可见）
        let s = plc_to_sample(&PlcValue::Udt(rust_ethernet_ip::UdtData {
            symbol_id: 1,
            data: vec![0x11, 0x22],
        }));
        let SampleValue::Str(json) = s else {
            panic!("udt should map to Str");
        };
        assert!(json.contains("symbol_id"), "udt json: {json}");
    }

    #[test]
    fn plc_with_type_defaults_and_explicit() {
        // 缺省推断：整数→Dint（AB 最常用）
        assert_eq!(
            plc_with_type(&SampleValue::Int(42), None),
            PlcValue::Dint(42)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Float(1.5), None),
            PlcValue::Real(1.5)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Bool(false), None),
            PlcValue::Bool(false)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Str("x".into()), None),
            PlcValue::String("x".into())
        );
        // 显式 plc_type（大小写不敏感）
        assert_eq!(
            plc_with_type(&SampleValue::Int(7), Some("LINT")),
            PlcValue::Lint(7)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Int(7), Some("uint")),
            PlcValue::Uint(7)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Int(1), Some("bool")),
            PlcValue::Bool(true)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Float(2.5), Some("lreal")),
            PlcValue::Lreal(2.5)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Int(5), Some("string")),
            PlcValue::String("Int(5)".into())
        );
        // 未知类型 → 默认推断
        assert_eq!(
            plc_with_type(&SampleValue::Int(9), Some("nope")),
            PlcValue::Dint(9)
        );
        // 空 plc_type → 默认
        assert_eq!(
            plc_with_type(&SampleValue::Int(9), Some("")),
            PlcValue::Dint(9)
        );
    }

    #[test]
    fn json_value_inference() {
        assert_eq!(
            json_to_sample_value(&serde_json::json!(true)),
            SampleValue::Bool(true)
        );
        assert_eq!(
            json_to_sample_value(&serde_json::json!(42)),
            SampleValue::Int(42)
        );
        assert!(matches!(
            json_to_sample_value(&serde_json::json!(1.5)),
            SampleValue::Float(_)
        ));
    }

    #[test]
    fn parse_cfg_defaults_and_validation() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"192.168.1.10:44818"})).unwrap();
        assert_eq!(cfg.endpoint, "192.168.1.10:44818");
        assert_eq!(cfg.period, 3);
        assert!(cfg.slot.is_none());
        assert_eq!(cfg.reconnect_secs, 5);
        assert_eq!(cfg.health_check_secs, 0);
        assert!(cfg.max_packet_size.is_none());
        assert!(cfg.batch_max_operations.is_none());

        let full = parse_cfg(&serde_json::json!({
            "endpoint": "plc:44818", "period": 0, "slot": 3,
            "reconnect_secs": 10, "health_check_secs": 60,
            "max_packet_size": 4000, "batch_max_operations": 20,
            "batch_max_packet_size": 504, "batch_packet_timeout_ms": 2000,
            "batch_continue_on_error": true, "batch_optimize_packing": false
        }))
        .unwrap();
        assert_eq!(full.period, 1, "period 0 clamped to 1");
        assert_eq!(full.slot, Some(3));
        assert_eq!(full.reconnect_secs, 10);
        assert_eq!(full.health_check_secs, 60);
        assert_eq!(full.max_packet_size, Some(4000));
        assert_eq!(full.batch_max_operations, Some(20));
        assert_eq!(full.batch_max_packet_size, Some(504));
        assert_eq!(full.batch_packet_timeout_ms, Some(2000));
        assert_eq!(full.batch_continue_on_error, Some(true));
        assert_eq!(full.batch_optimize_packing, Some(false));

        assert!(
            parse_cfg(&serde_json::json!({})).is_err(),
            "missing endpoint errors"
        );
    }
}
