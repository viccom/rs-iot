//! `opcuaClient`：OPC UA 南向采集插件（async-opcua 0.19，纯 Rust 加密，无 openssl）。
//!
//! 周期连接外部 UA Server，按 `devices` 配置读取 NodeId 值，组装 `Sample` 推入 hub。
//! 同时消费下行命令（`cmd="write"`）写回 Server item。
//!
//! 配置（`InstConfig::app_config` JSON）：
//! ```json
//! {
//!   "endpoint": "opc.tcp://127.0.0.1:49320",
//!   "security_policy": "none",       // "none" | "Basic256Sha256"
//!   "username": null, "password": null,
//!   "period": 3
//! }
//! ```
//! 变量映射（复用 `devices`，与 datasim 对齐）：
//! - `DeviceSpec.value` = 设备 ID（逻辑名，即 `rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = UA NodeId 字符串（如 `ns=2;s=v1`）
//! - `ChildSpec.label` = values map 的 key（显示名；空则用 NodeId）

use crate::host::HostContext;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use opcua::client::{ClientBuilder, IdentityToken, Password, Session};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    AttributeId, DataValue, EndpointDescription, MessageSecurityMode, NodeId, QualifiedName,
    ReadValueId, TimestampsToReturn, UserTokenPolicy, Variant, WriteValue,
};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// UA Server 返回的连接状态码（event_loop.spawn 的 JoinHandle 产出）。
type StatusCode = opcua::types::StatusCode;

/// OPC UA 采集插件。
#[derive(Debug, Default)]
pub struct OpcuaClient;

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
struct OpcuaCfg {
    endpoint: String,
    security_policy: String,
    username: Option<String>,
    password: Option<String>,
    period: u64,
    /// 是否严格校验 server 证书（hostname/application uri/有效期）。
    /// 默认 false：仅检查证书被信任 + 密钥长度，跳过 hostname 校验，
    /// 便于用 IP/loopback 连接（server 证书常不含这些 alt name）。
    /// 生产环境（server 证书与 endpoint 匹配）建议设 true。
    verify_server: bool,
}

fn parse_cfg(app: &serde_json::Value) -> Result<OpcuaCfg, String> {
    let endpoint = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?
        .to_string();
    let security_policy = app
        .get("security_policy")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let username = app
        .get("username")
        .and_then(|v| v.as_str())
        .map(String::from);
    let password = app
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .max(1);
    let verify_server = app
        .get("verify_server")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(OpcuaCfg {
        endpoint,
        security_policy,
        username,
        password,
        period,
        verify_server,
    })
}

/// 安全策略字符串 → (SecurityPolicy, MessageSecurityMode)。
/// v1 支持两种：none（明文）/ Basic256Sha256（签名并加密）。
fn parse_security(s: &str) -> Result<(SecurityPolicy, MessageSecurityMode), String> {
    match s.trim().to_lowercase().as_str() {
        "none" | "" => Ok((SecurityPolicy::None, MessageSecurityMode::None)),
        "basic256sha256" => Ok((
            SecurityPolicy::Basic256Sha256,
            MessageSecurityMode::SignAndEncrypt,
        )),
        other => Err(format!("unsupported security_policy: {other}")),
    }
}

/// 解析 NodeId 字符串（`ns=2;s=v1` / `i=2258` / `ns=0;i=2258`）。
fn parse_node_id(s: &str) -> Result<NodeId, String> {
    s.parse::<NodeId>()
        .map_err(|e| format!("invalid NodeId '{s}': {e}"))
}

/// Variant → SampleValue（数值类归一为 Float/Int，Bool→Bool，其余→Str）。
fn variant_to_sample(v: &Variant) -> Option<SampleValue> {
    match v {
        Variant::Boolean(b) => Some(SampleValue::Bool(*b)),
        Variant::SByte(i) => Some(SampleValue::Int(*i as i64)),
        Variant::Byte(u) => Some(SampleValue::Int(*u as i64)),
        Variant::Int16(i) => Some(SampleValue::Int(*i as i64)),
        Variant::UInt16(u) => Some(SampleValue::Int(*u as i64)),
        Variant::Int32(i) => Some(SampleValue::Int(*i as i64)),
        Variant::UInt32(u) => Some(SampleValue::Int(*u as i64)),
        Variant::Int64(i) => Some(SampleValue::Int(*i)),
        Variant::UInt64(u) => Some(SampleValue::Int(*u as i64)),
        Variant::Float(f) => Some(SampleValue::Float(*f as f64)),
        Variant::Double(f) => Some(SampleValue::Float(*f)),
        Variant::String(s) => Some(SampleValue::Str(s.as_ref().to_string())),
        Variant::DateTime(dt) => Some(SampleValue::Str(dt.as_chrono().to_rfc3339())),
        _ => Some(SampleValue::Str(format!("{v:?}"))),
    }
}

/// SampleValue → Variant（写方向，默认映射：Float→Double、Int→Int64）。
fn sample_value_to_variant(v: &SampleValue) -> Variant {
    match v {
        SampleValue::Float(f) => Variant::Double(*f),
        SampleValue::Int(i) => Variant::Int64(*i),
        SampleValue::Bool(b) => Variant::Boolean(*b),
        SampleValue::Str(s) => Variant::String(s.as_str().into()),
    }
}

/// SampleValue + 显式 OPC UA 类型 → Variant。
///
/// OPC UA server 对 write 的类型严格匹配（Int32 节点写 Int64 会 BadTypeMismatch），
/// 默认映射在真实场景常失败。`opc_type` 让调用方按节点实际类型构造 Variant。
/// `opc_type = None` 退回 `sample_value_to_variant`（向后兼容）。
///
/// 支持类型（不区分大小写）：Boolean/SByte/Byte/Int16/UInt16/Int32/UInt32/
/// Int64/UInt64/Float/Double/String。数值按目标类型截断；Bool 节点接受 0/非零或布尔。
fn variant_with_type(v: &SampleValue, opc_type: Option<&str>) -> Variant {
    let Some(t) = opc_type.map(str::trim).filter(|s| !s.is_empty()) else {
        return sample_value_to_variant(v);
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
    let as_b = || match v {
        SampleValue::Bool(b) => *b,
        SampleValue::Int(i) => *i != 0,
        SampleValue::Float(f) => *f != 0.0,
        SampleValue::Str(s) => s.eq_ignore_ascii_case("true"),
    };
    match t.to_lowercase().as_str() {
        "boolean" => Variant::Boolean(as_b()),
        "sbyte" => Variant::SByte(as_i() as i8),
        "byte" => Variant::Byte(as_i() as u8),
        "int16" => Variant::Int16(as_i() as i16),
        "uint16" => Variant::UInt16(as_i() as u16),
        "int32" => Variant::Int32(as_i() as i32),
        "uint32" => Variant::UInt32(as_i() as u32),
        "int64" => Variant::Int64(as_i()),
        "uint64" => Variant::UInt64(as_i() as u64),
        "float" => Variant::Float(as_f() as f32),
        "double" => Variant::Double(as_f()),
        "string" => match v {
            SampleValue::Str(s) => Variant::String(s.as_str().into()),
            _ => Variant::String(format!("{v:?}").as_str().into()),
        },
        _ => sample_value_to_variant(v),
    }
}

/// JSON → SampleValue（写命令的 value 字段）。
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

fn read_value_id(node_id: NodeId) -> ReadValueId {
    ReadValueId {
        node_id,
        attribute_id: AttributeId::Value as u32,
        index_range: Default::default(),
        data_encoding: QualifiedName::null(),
    }
}

fn write_value(node_id: NodeId, variant: Variant) -> WriteValue {
    WriteValue {
        node_id,
        attribute_id: AttributeId::Value as u32,
        index_range: Default::default(),
        value: DataValue {
            value: Some(variant),
            ..Default::default()
        },
    }
}

/// 建立到 UA Server 的会话。
async fn connect(
    cfg: &OpcuaCfg,
) -> Result<(std::sync::Arc<Session>, JoinHandle<StatusCode>), String> {
    let mut client = ClientBuilder::new()
        .application_name("rs-iot OPC UA client")
        .application_uri("urn:rs-iot:opcua-client")
        .create_sample_keypair(true)
        .trust_server_certs(true)
        .verify_server_certs(cfg.verify_server)
        .session_retry_limit(3)
        .client()
        .map_err(|e| format!("build opcua client: {e:?}"))?;

    let (policy, mode) = parse_security(&cfg.security_policy)?;
    let endpoint: EndpointDescription = (
        cfg.endpoint.as_str(),
        policy.to_str(),
        mode,
        UserTokenPolicy::anonymous(),
    )
        .into();
    let token = match (&cfg.username, &cfg.password) {
        (Some(u), Some(p)) => IdentityToken::UserName(u.clone(), Password(p.clone())),
        _ => IdentityToken::Anonymous,
    };

    let (session, event_loop) = client
        .connect_to_matching_endpoint(endpoint, token)
        .await
        .map_err(|e| format!("connect {}: {e:?}", cfg.endpoint))?;
    let handle = event_loop.spawn();
    session.wait_for_connection().await;
    Ok((session, handle))
}

#[async_trait]
impl Source for OpcuaClient {
    fn name(&self) -> &'static str {
        "opcuaClient"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: mpsc::Sender<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        let inst_id = inst.inst_id.clone();
        let cfg = match parse_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[opcuaClient] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // 预解析所有设备的 (devid, [(field, NodeId)])，采集循环内不再重复 parse。
        let devices: Vec<(String, Vec<(String, NodeId)>)> = inst
            .devices
            .iter()
            .map(|dev| {
                let pairs: Vec<(String, NodeId)> = dev
                    .children
                    .iter()
                    .filter_map(|c| {
                        parse_node_id(&c.value)
                            .map(|nid| {
                                let field = if c.label.is_empty() {
                                    c.value.clone()
                                } else {
                                    c.label.clone()
                                };
                                (field, nid)
                            })
                            .map_err(|e| {
                                tracing::warn!(
                                    "[opcuaClient] inst {inst_id} dev {} bad NodeId '{}': {}",
                                    dev.value,
                                    c.value,
                                    e
                                );
                                e
                            })
                            .ok()
                    })
                    .collect();
                (dev.value.clone(), pairs)
            })
            .collect();

        let period = Duration::from_secs(cfg.period);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase = std::time::Duration::from_millis(crate::plugin::ticker::phase_ms(
            inst_id.as_str(),
            period,
        ));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // StatTracker（M4 实例指标）+ flush ticker（周期落 lux stat:<inst>）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut reconnects: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));

        let mut session: Option<std::sync::Arc<Session>> = None;
        let mut ev_handle: Option<JoinHandle<StatusCode>> = None;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[opcuaClient] inst {inst_id} stopping");
                    break;
                }
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        if m.cmd == "write" {
                            handle_write(&session, &m.data).await;
                        } else {
                            tracing::info!("[opcuaClient] inst {inst_id} unknown cmd: {}", m.cmd);
                        }
                    }
                }
                _ = flush_ticker.tick() => {
                    tracker.flush().await;
                }
                _ = ticker.tick() => {
                    if session.is_none() {
                        match connect(&cfg).await {
                            Ok((s, h)) => {
                                reconnects += 1;
                                tracker.extra_insert(
                                    "session_reconnect_count",
                                    serde_json::json!(reconnects),
                                );
                                tracing::info!("[opcuaClient] inst {inst_id} connected {}", cfg.endpoint);
                                session = Some(s);
                                ev_handle = Some(h);
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[opcuaClient] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() {
                                    tracker.flush().await;
                                }
                                continue;
                            }
                        }
                    }
                    let s = match &session {
                        Some(s) => s.clone(),
                        None => continue,
                    };
                    let started = std::time::Instant::now();
                    let mut tick_ok = true;
                    for (devid, pairs) in &devices {
                        if pairs.is_empty() {
                            continue;
                        }
                        let nodes: Vec<ReadValueId> =
                            pairs.iter().map(|(_, nid)| read_value_id(nid.clone())).collect();
                        match s.read(&nodes, TimestampsToReturn::Both, 0.0).await {
                            Ok(results) => {
                                let mut values = BTreeMap::new();
                                for ((field, _), dv) in pairs.iter().zip(results.iter()) {
                                    let sv = data_value_to_sample(dv);
                                    if let Some(v) = sv {
                                        values.insert(field.clone(), v);
                                    }
                                }
                                let sample = Sample {
                                    hwid: ctx.hwid.clone(),
                                    devid: devid.clone(),
                                    values,
                                    timestamp: chrono::Local::now().timestamp(),
                                };
                                if tx.send(sample).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tick_ok = false;
                                tracker.tick_fail(&format!("read failed: {e:?}"));
                                tracing::warn!("[opcuaClient] inst {inst_id} read failed: {e:?}");
                                // 读失败可能因断连，下次 tick 重连
                                session = None;
                                if let Some(h) = ev_handle.take() {
                                    h.abort();
                                }
                                break;
                            }
                        }
                    }
                    if tick_ok {
                        tracker.tick_ok(started.elapsed().as_millis() as u64);
                    }
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }

        if let Some(s) = session {
            let _ = s.disconnect().await;
        }
        if let Some(h) = ev_handle {
            h.abort();
        }
        tracing::info!("[opcuaClient] inst {inst_id} exited");

        // 统一退出处理（cancelled 触发；stat 落 stopped）。
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// DataValue → SampleValue（取 value Variant；status 非 Good 时记 warn 返回 None）。
fn data_value_to_sample(dv: &DataValue) -> Option<SampleValue> {
    if let Some(ref v) = dv.value {
        if let Some(ref st) = dv.status {
            if !st.is_good() {
                tracing::warn!("[opcuaClient] read status bad: {st:?}");
                return None;
            }
        }
        variant_to_sample(v)
    } else {
        None
    }
}

/// 处理 write 命令：`data` = JSON `{"node_id":"ns=2;s=v1","value":123.45,"opc_type":"Int32"}`。
///
/// `opc_type` 可选，指定节点实际 OPC UA 类型（OPC UA write 严格类型匹配，
/// 如 Int32 节点须写 Int32 而非 Int64，否则 BadTypeMismatch）。缺省用默认映射。
async fn handle_write(session: &Option<std::sync::Arc<Session>>, data: &str) {
    let s = match session {
        Some(s) => s.clone(),
        None => {
            tracing::warn!("[opcuaClient] write ignored: not connected");
            return;
        }
    };
    let req: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[opcuaClient] write bad json: {e}");
            return;
        }
    };
    let node_id = match req.get("node_id").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            tracing::warn!("[opcuaClient] write missing node_id");
            return;
        }
    };
    let nid = match parse_node_id(node_id) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("[opcuaClient] write bad node_id: {e}");
            return;
        }
    };
    let value = match req.get("value") {
        Some(v) => json_to_sample_value(v),
        None => {
            tracing::warn!("[opcuaClient] write missing value");
            return;
        }
    };
    let opc_type = req.get("opc_type").and_then(|v| v.as_str());
    let wv = write_value(nid, variant_with_type(&value, opc_type));
    match s.write(&[wv]).await {
        Ok(results) => {
            for st in results {
                if !st.is_good() {
                    tracing::warn!("[opcuaClient] write status: {st:?}");
                }
            }
        }
        Err(e) => tracing::warn!("[opcuaClient] write failed: {e:?}"),
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "opcuaClient",
        create: || Box::new(OpcuaClient),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_security_supports_none_and_basic256sha256() {
        let (p, m) = parse_security("none").unwrap();
        assert_eq!(p, SecurityPolicy::None);
        assert_eq!(m, MessageSecurityMode::None);
        let (p, m) = parse_security("Basic256Sha256").unwrap();
        assert_eq!(p, SecurityPolicy::Basic256Sha256);
        assert_eq!(m, MessageSecurityMode::SignAndEncrypt);
        // 空串等价 none
        assert!(parse_security("").unwrap().0 == SecurityPolicy::None);
        // 大小写不敏感
        assert!(parse_security("BASIC256SHA256").is_ok());
        assert!(parse_security("unknown").is_err());
    }

    #[test]
    fn parse_node_id_accepts_standard_forms() {
        assert!(parse_node_id("ns=2;s=v1").is_ok());
        assert!(parse_node_id("i=2258").is_ok());
        assert!(parse_node_id("ns=0;i=2258").is_ok());
        assert!(parse_node_id("not a node").is_err());
    }

    #[test]
    fn variant_round_trips_to_sample() {
        assert_eq!(
            variant_to_sample(&Variant::Double(1.5)),
            Some(SampleValue::Float(1.5))
        );
        assert_eq!(
            variant_to_sample(&Variant::Int64(-7)),
            Some(SampleValue::Int(-7))
        );
        assert_eq!(
            variant_to_sample(&Variant::Boolean(true)),
            Some(SampleValue::Bool(true))
        );
        let s = variant_to_sample(&Variant::String("hi".into()));
        assert_eq!(s, Some(SampleValue::Str("hi".into())));
    }

    #[test]
    fn sample_value_maps_back_to_variant() {
        assert_eq!(
            sample_value_to_variant(&SampleValue::Float(2.0)),
            Variant::Double(2.0)
        );
        assert_eq!(
            sample_value_to_variant(&SampleValue::Int(9)),
            Variant::Int64(9)
        );
        assert_eq!(
            sample_value_to_variant(&SampleValue::Bool(false)),
            Variant::Boolean(false)
        );
    }

    #[test]
    fn json_to_sample_value_handles_types() {
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
        assert_eq!(
            json_to_sample_value(&serde_json::json!("x")),
            SampleValue::Str("x".into())
        );
    }

    #[test]
    fn variant_with_type_matches_server_node_type() {
        // Int + Int32 → Int32（v1 默认 Int64 会 BadTypeMismatch，显式 Int32 解决）
        assert_eq!(
            variant_with_type(&SampleValue::Int(42), Some("Int32")),
            Variant::Int32(42)
        );
        // 大小写不敏感
        assert_eq!(
            variant_with_type(&SampleValue::Int(7), Some("uint16")),
            Variant::UInt16(7)
        );
        // Float + Float → Float(f32)
        assert_eq!(
            variant_with_type(&SampleValue::Float(1.5), Some("Float")),
            Variant::Float(1.5_f32)
        );
        // Bool from int
        assert_eq!(
            variant_with_type(&SampleValue::Int(1), Some("Boolean")),
            Variant::Boolean(true)
        );
        // 无 opc_type → 默认 Int64
        assert_eq!(
            variant_with_type(&SampleValue::Int(9), None),
            Variant::Int64(9)
        );
        // 空 opc_type → 默认 Double
        assert_eq!(
            variant_with_type(&SampleValue::Float(2.0), Some("")),
            Variant::Double(2.0)
        );
        // 未知 opc_type → 默认
        assert_eq!(
            variant_with_type(&SampleValue::Int(1), Some("unknown")),
            Variant::Int64(1)
        );
    }

    #[test]
    fn parse_cfg_defaults_period_and_policy() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"opc.tcp://x:1234"})).unwrap();
        assert_eq!(cfg.endpoint, "opc.tcp://x:1234");
        assert_eq!(cfg.security_policy, "none");
        assert_eq!(cfg.period, 3);
        assert!(cfg.username.is_none());
        assert!(!cfg.verify_server, "verify_server defaults to false");

        let cfg2 = parse_cfg(&serde_json::json!({"endpoint":"x","period":0})).unwrap();
        assert_eq!(cfg2.period, 1, "period 0 clamped to 1");
        assert!(
            parse_cfg(&serde_json::json!({})).is_err(),
            "missing endpoint errors"
        );
    }
}
