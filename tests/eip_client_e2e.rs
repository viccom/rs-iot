//! eipClient 端到端（CI 内，mock TCP EtherNet/IP server）。
//!
//! mock 协议实现提炼自 rust-ethernet-ip 官方自带模拟器
//! `rust-ethernet-ip/src/bin/plc_sim.rs`（MIT），精简为 DINT/REAL/BOOL 三类型 +
//! RegisterSession/SendRRData/CIP Read/Write/MultipleService 最小子集。
//!
//! 验证三点：
//! ① 多 devid 批量采集入库（read_tags_batch 一次往返，跨 dev 分组）
//! ② 写命令闭环（send_ctl → WriteTag 帧 → mock tag 表更新 → 下轮采集值变化）
//! ③ string tag 独立路径（read_string_tag，标准 STRING handle 0x0FCE）。
#![cfg(feature = "eip")] // 整文件依赖 eipClient 插件：裁剪组合下跳过（CI minimal 组合回归保护）

mod common;

use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::types::{
    AppType, ChildSpec, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, PluginName, Sample,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// ── 协议常量（对齐 plc_sim）────────────────────────────────────────────

const CMD_REGISTER_SESSION: u16 = 0x0065;
const CMD_SEND_RR_DATA: u16 = 0x006F;
const CIP_READ_TAG: u8 = 0x4C;
const CIP_WRITE_TAG: u8 = 0x4D;
const CIP_MULTIPLE_SERVICE_PACKET: u8 = 0x0A;
const CIP_REPLY_READ: u8 = 0xCC;
const CIP_REPLY_WRITE: u8 = 0xCD;
const CIP_REPLY_MULTIPLE_SERVICE_PACKET: u8 = 0x8A;
const CIP_STATUS_SUCCESS: u8 = 0x00;
const CIP_STATUS_PATH_SEGMENT_ERROR: u8 = 0x04;
const CIP_TYPE_DINT: u16 = 0x00C4;
const CIP_TYPE_REAL: u16 = 0x00CA;
const CIP_TYPE_BOOL: u16 = 0x00C1;
const CIP_TYPE_STRUCTURE: u16 = 0x02A0;
const CIP_STANDARD_STRING_HANDLE: u16 = 0x0FCE;
const CIP_STANDARD_STRING_DATA_LEN: usize = 82;
const CIP_STANDARD_STRING_PAD_LEN: usize = 2;

/// mock tag 值（集成测试只需三标量 + STRING）。
#[derive(Clone, Debug)]
enum TagValue {
    Bool(bool),
    Dint(i32),
    Real(f32),
    Str(String),
}

type TagTable = Arc<Mutex<HashMap<String, TagValue>>>;

// ── mock server（提炼自 plc_sim，精简）────────────────────────────────

async fn spawn_mock_server(tags: TagTable) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            if let Ok((sock, _)) = listener.accept().await {
                let tags = tags.clone();
                tokio::spawn(async move {
                    let mut sock = sock;
                    let mut session: u32 = 0;
                    loop {
                        // EIP 头 24B：cmd(2)+len(2)+session(4)+status(4)+ctx(8)+opts(4)
                        let mut hdr = [0u8; 24];
                        if sock.read_exact(&mut hdr).await.is_err() {
                            return;
                        }
                        let cmd = u16::from_le_bytes([hdr[0], hdr[1]]);
                        let len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
                        let mut ctx = [0u8; 8];
                        // ctx 位于 cmd(2)+len(2)+session(4)+status(4) 之后：offset 12..20
                        ctx.copy_from_slice(&hdr[12..20]);
                        let mut body = vec![0u8; len];
                        if len > 0 && sock.read_exact(&mut body).await.is_err() {
                            return;
                        }
                        match cmd {
                            CMD_REGISTER_SESSION => {
                                session = 0x1234_5678;
                                let _ = sock.write_all(&register_session_reply(ctx)).await;
                            }
                            CMD_SEND_RR_DATA => {
                                let cip = build_cip_reply(&body, &tags);
                                let _ = sock.write_all(&send_rr_reply(session, ctx, &cip)).await;
                            }
                            _ => {
                                // 未知命令（含 UnregisterSession 0x66）：不回包。
                            }
                        }
                    }
                });
            }
        }
    });
    addr
}

fn register_session_reply(ctx: [u8; 8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(28);
    r.extend_from_slice(&CMD_REGISTER_SESSION.to_le_bytes());
    r.extend_from_slice(&4u16.to_le_bytes());
    r.extend_from_slice(&0x1234_5678_u32.to_le_bytes());
    r.extend_from_slice(&0u32.to_le_bytes());
    r.extend_from_slice(&ctx);
    r.extend_from_slice(&0u32.to_le_bytes());
    r.extend_from_slice(&[0u8; 4]);
    r
}

fn send_rr_reply(session: u32, ctx: [u8; 8], cip: &[u8]) -> Vec<u8> {
    // CPF：null item + unconnected data item(0x00B2)
    let mut data = Vec::new();
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0x00B2_u16.to_le_bytes());
    data.extend_from_slice(&(cip.len() as u16).to_le_bytes());
    data.extend_from_slice(cip);
    let mut r = Vec::with_capacity(24 + data.len());
    r.extend_from_slice(&CMD_SEND_RR_DATA.to_le_bytes());
    r.extend_from_slice(&(data.len() as u16).to_le_bytes());
    r.extend_from_slice(&session.to_le_bytes());
    r.extend_from_slice(&0u32.to_le_bytes());
    r.extend_from_slice(&ctx);
    r.extend_from_slice(&0u32.to_le_bytes());
    r.extend_from_slice(&data);
    r
}

/// 从 SendRRData body（CPF items）提取 UCMM 内嵌 CIP 请求。
fn extract_cip_request(payload: &[u8]) -> Vec<u8> {
    if payload.len() < 8 {
        return Vec::new();
    }
    let item_count = u16::from_le_bytes([payload[6], payload[7]]);
    let mut pos = 8;
    for _ in 0..item_count {
        if pos + 4 > payload.len() {
            break;
        }
        let item_type = u16::from_le_bytes([payload[pos], payload[pos + 1]]);
        let item_len = u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
        pos += 4;
        if pos + item_len > payload.len() {
            break;
        }
        if item_type == 0x00B2 {
            let ucmm = &payload[pos..pos + item_len];
            if ucmm.len() < 10 {
                return Vec::new();
            }
            // UCMM(0x52)：10B 头（service+path+timeout）+ 2B len + CIP
            let msg_len = u16::from_le_bytes([ucmm[8], ucmm[9]]) as usize;
            let end = usize::min(10 + msg_len, ucmm.len());
            return ucmm[10..end].to_vec();
        }
        pos += item_len;
    }
    Vec::new()
}

fn build_cip_reply(payload: &[u8], tags: &TagTable) -> Vec<u8> {
    let req = extract_cip_request(payload);
    match req.first().copied().unwrap_or(0) {
        CIP_READ_TAG => build_read_reply(&req, tags),
        CIP_WRITE_TAG => handle_write(&req, tags),
        CIP_MULTIPLE_SERVICE_PACKET => build_multiple_service_reply(&req, tags),
        _ => vec![CIP_REPLY_READ, 0x00, 0x01, 0x00],
    }
}

/// 解析 CIP 请求 tag 路径：只处理 0x91（ANSI Extended Symbol）首段——
/// 集成测试 tag 全用简单名（无 Program:/数组/点成员）。
fn parse_tag(cip_request: &[u8]) -> Option<String> {
    if cip_request.len() < 2 {
        return None;
    }
    let path_bytes = cip_request[1] as usize * 2;
    let path = cip_request.get(2..2 + path_bytes)?;
    if path.first() != Some(&0x91) || path.len() < 2 {
        return None;
    }
    let len = path[1] as usize;
    let name = path.get(2..2 + len)?;
    Some(String::from_utf8_lossy(name).to_string())
}

fn read_count(cip_request: &[u8]) -> usize {
    let path_bytes = cip_request[1] as usize * 2;
    cip_request
        .get(2 + path_bytes..2 + path_bytes + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
        .unwrap_or(1)
}

fn build_read_reply(cip_request: &[u8], tags: &TagTable) -> Vec<u8> {
    let tag = parse_tag(cip_request).unwrap_or_else(|| "DINT_TAG".into());
    let _count = read_count(cip_request);
    let guard = tags.lock().expect("tag lock");
    let value = guard.get(&tag).cloned().unwrap_or(TagValue::Dint(0));
    match value {
        TagValue::Bool(v) => {
            let mut r = vec![CIP_REPLY_READ, 0x00, 0x00, 0x00];
            r.extend_from_slice(&CIP_TYPE_BOOL.to_le_bytes());
            r.push(u8::from(v));
            r
        }
        TagValue::Dint(v) => {
            let mut r = vec![CIP_REPLY_READ, 0x00, 0x00, 0x00];
            r.extend_from_slice(&CIP_TYPE_DINT.to_le_bytes());
            r.extend_from_slice(&v.to_le_bytes());
            r
        }
        TagValue::Real(v) => {
            let mut r = vec![CIP_REPLY_READ, 0x00, 0x00, 0x00];
            r.extend_from_slice(&CIP_TYPE_REAL.to_le_bytes());
            r.extend_from_slice(&v.to_le_bytes());
            r
        }
        TagValue::Str(s) => {
            // 标准 STRING：STRUCTURE 类型 + 0x0FCE handle + len(4B) + 82B + pad(2B)
            let mut r = vec![CIP_REPLY_READ, 0x00, 0x00, 0x00];
            r.extend_from_slice(&CIP_TYPE_STRUCTURE.to_le_bytes());
            r.extend_from_slice(&CIP_STANDARD_STRING_HANDLE.to_le_bytes());
            let bytes = s.as_bytes();
            let n = bytes.len().min(CIP_STANDARD_STRING_DATA_LEN);
            r.extend_from_slice(&(n as u32).to_le_bytes());
            r.extend_from_slice(&bytes[..n]);
            r.resize(r.len() + (CIP_STANDARD_STRING_DATA_LEN - n), 0);
            r.resize(r.len() + CIP_STANDARD_STRING_PAD_LEN, 0);
            r
        }
    }
}

fn handle_write(cip_request: &[u8], tags: &TagTable) -> Vec<u8> {
    let Some(tag) = parse_tag(cip_request) else {
        return vec![CIP_REPLY_WRITE, 0x00, CIP_STATUS_PATH_SEGMENT_ERROR, 0x00];
    };
    let path_bytes = cip_request[1] as usize * 2;
    let pos = 2 + path_bytes;
    if cip_request.len() < pos + 4 {
        return vec![CIP_REPLY_WRITE, 0x00, CIP_STATUS_PATH_SEGMENT_ERROR, 0x00];
    }
    let data_type = u16::from_le_bytes([cip_request[pos], cip_request[pos + 1]]);
    let data = &cip_request[pos + 4..];
    let value = match data_type {
        CIP_TYPE_BOOL => data.first().map(|b| TagValue::Bool(*b != 0)),
        CIP_TYPE_DINT => data
            .get(..4)
            .map(|b| TagValue::Dint(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
        CIP_TYPE_REAL => data
            .get(..4)
            .map(|b| TagValue::Real(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
        _ => None,
    };
    match value {
        Some(v) => {
            tags.lock().expect("tag lock").insert(tag, v);
            vec![CIP_REPLY_WRITE, 0x00, CIP_STATUS_SUCCESS, 0x00]
        }
        None => vec![CIP_REPLY_WRITE, 0x00, CIP_STATUS_PATH_SEGMENT_ERROR, 0x00],
    }
}

fn build_multiple_service_reply(req: &[u8], tags: &TagTable) -> Vec<u8> {
    if req.len() < 8 {
        return vec![
            CIP_REPLY_MULTIPLE_SERVICE_PACKET,
            0x00,
            CIP_STATUS_PATH_SEGMENT_ERROR,
            0x00,
        ];
    }
    let count = u16::from_le_bytes([req[6], req[7]]) as usize;
    let mut offsets = Vec::with_capacity(count);
    for i in 0..count {
        let p = 8 + i * 2;
        offsets.push(u16::from_le_bytes([req[p], req[p + 1]]) as usize);
    }
    let mut replies = Vec::with_capacity(count);
    for i in 0..count {
        let start = 6 + offsets[i];
        let end = if i + 1 < count {
            6 + offsets[i + 1]
        } else {
            req.len()
        };
        let sub = req.get(start..end).unwrap_or(&[]);
        let reply = match sub.first().copied() {
            Some(CIP_READ_TAG) => build_read_reply(sub, tags),
            Some(CIP_WRITE_TAG) => handle_write(sub, tags),
            _ => vec![CIP_REPLY_READ, 0x00, CIP_STATUS_PATH_SEGMENT_ERROR, 0x00],
        };
        replies.push(reply);
    }
    let mut r = vec![
        CIP_REPLY_MULTIPLE_SERVICE_PACKET,
        0x00,
        CIP_STATUS_SUCCESS,
        0x00,
    ];
    r.extend_from_slice(&(count as u16).to_le_bytes());
    let mut off = 2 + count * 2;
    for reply in &replies {
        r.extend_from_slice(&(off as u16).to_le_bytes());
        off += reply.len();
    }
    for reply in replies {
        r.extend_from_slice(&reply);
    }
    r
}

// ── 测试编排（照 modbus_multi_slave_e2e 模式）─────────────────────────

fn mk_child(label: &str, tag: &str, ty: &str) -> ChildSpec {
    ChildSpec {
        label: label.into(),
        value: tag.into(),
        r#type: ty.into(),
        props: None,
    }
}

/// 起 lux + InstanceManager + eipClient 实例；hub_rx 收 Sample。
/// 返回 (mgr, hub_rx, shutdown, lux handle)——用例自行收数/发命令后调 teardown。
struct Harness {
    mgr: Arc<InstanceManager>,
    hub_rx: tokio::sync::mpsc::Receiver<Sample>,
    shutdown: CancellationToken,
    lux: lux::ServerHandle,
}

async fn start_instance(
    inst_id: &str,
    app: serde_json::Value,
    devices: Vec<DeviceSpec>,
) -> Harness {
    let data_dir = common::test_data_dir(&format!("eip-{inst_id}"));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    let shutdown = CancellationToken::new();
    let ctx = HostContext {
        lux: client,
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(Arc::new(rs_iot::process_metrics::ProcessMetrics::new())),
        bridge_registry: BridgeRegistry::new(),
        metrics: Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx, hub_tx));
    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "it eip".into(),
        app_code: PluginName::new("eipClient"),
        app_type: AppType::South,
        auto_start: false,
        app_config: app,
        devices,
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start eip");
    Harness {
        mgr,
        hub_rx,
        shutdown,
        lux: handle,
    }
}

/// 收 Sample 值到 `want` 全出现或 deadline。
async fn collect_until(
    hub_rx: &mut tokio::sync::mpsc::Receiver<Sample>,
    want: &HashMap<String, String>,
    deadline_secs: u64,
) -> HashMap<String, String> {
    let mut got: HashMap<String, String> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        if want.iter().all(|(k, v)| got.get(k) == Some(v)) {
            break;
        }
        match tokio::time::timeout_at(deadline, hub_rx.recv()).await {
            Ok(Some(s)) => {
                for (k, v) in &s.values {
                    got.insert(format!("{}.{}", s.devid, k), format!("{v:?}"));
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    got
}

/// ① 多 devid 批量采集：DINT/REAL/BOOL/STRING 全链路入库。
#[tokio::test]
async fn eip_collects_multi_type_tags() {
    let tags: TagTable = Arc::new(Mutex::new(HashMap::from([
        ("DINT_TAG".to_string(), TagValue::Dint(4242)),
        ("REAL_TAG".to_string(), TagValue::Real(2.5)),
        ("BOOL_TAG".to_string(), TagValue::Bool(true)),
        ("MSG_TAG".to_string(), TagValue::Str("hello-eip".into())),
    ])));
    let addr = spawn_mock_server(tags).await;
    let app = serde_json::json!({ "endpoint": addr.to_string(), "period": 1 });
    let devices = vec![
        DeviceSpec {
            label: "plcA".into(),
            value: "plcA".into(),
            children: vec![
                mk_child("dint", "DINT_TAG", ""),
                mk_child("real", "REAL_TAG", ""),
            ],
            slave_id: None,
        },
        DeviceSpec {
            label: "plcB".into(),
            value: "plcB".into(),
            children: vec![
                mk_child("flag", "BOOL_TAG", ""),
                mk_child("msg", "MSG_TAG", "string"),
            ],
            slave_id: None,
        },
    ];
    let mut h = start_instance("eip1", app, devices).await;
    let want: HashMap<String, String> = HashMap::from([
        ("plcA.dint".to_string(), "Int(4242)".to_string()),
        ("plcA.real".to_string(), "Float(2.5)".to_string()),
        ("plcB.flag".to_string(), "Bool(true)".to_string()),
        ("plcB.msg".to_string(), "Str(\"hello-eip\")".to_string()),
    ]);
    let got = collect_until(&mut h.hub_rx, &want, 15).await;
    h.shutdown.cancel();
    h.mgr.stop_all().await;
    common::stop_lux(h.lux).await;
    for (k, v) in &want {
        assert_eq!(got.get(k), Some(v), "tag {k} 未按预期入库，got: {got:?}");
    }
}

/// ② 写命令闭环：send_ctl 写 DINT_TAG=777 → 下轮采集读到新值（mock tag 表被更新）。
#[tokio::test]
async fn eip_write_command_round_trip() {
    let tags: TagTable = Arc::new(Mutex::new(HashMap::from([(
        "SP_TAG".to_string(),
        TagValue::Dint(100),
    )])));
    let addr = spawn_mock_server(tags).await;
    let app = serde_json::json!({ "endpoint": addr.to_string(), "period": 1 });
    let devices = vec![DeviceSpec {
        label: "plc".into(),
        value: "plc".into(),
        children: vec![mk_child("sp", "SP_TAG", "")],
        slave_id: None,
    }];
    let mut h = start_instance("eip2", app, devices).await;

    // 先等到初始值 100（确认链路通）
    let want0: HashMap<String, String> =
        HashMap::from([("plc.sp".to_string(), "Int(100)".to_string())]);
    let got = collect_until(&mut h.hub_rx, &want0, 15).await;
    assert_eq!(
        got.get("plc.sp"),
        Some(&"Int(100)".to_string()),
        "初始值未到: {got:?}"
    );

    // 发写命令（plc_type 显式 dint——CIP 类型化写入）
    let ok = h.mgr.send_ctl(
        "eip2",
        CtlMessage {
            instance: "eip2".into(),
            devid: "plc".into(),
            cmd: "write".into(),
            data: r#"{"tag":"SP_TAG","value":777,"plc_type":"dint"}"#.into(),
        },
    );
    assert!(ok, "send_ctl 应投递成功（实例运行中）");

    // 下轮采集应读到 777（写→mock 表更新→读回）
    let want1: HashMap<String, String> =
        HashMap::from([("plc.sp".to_string(), "Int(777)".to_string())]);
    let got = collect_until(&mut h.hub_rx, &want1, 15).await;
    h.shutdown.cancel();
    h.mgr.stop_all().await;
    common::stop_lux(h.lux).await;
    assert_eq!(
        got.get("plc.sp"),
        Some(&"Int(777)".to_string()),
        "写命令闭环失败（未读到写入值）: {got:?}"
    );
}
