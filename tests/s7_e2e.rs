#![cfg(feature = "s7")]
//! s7Client 端到端：起 lux + **进程内最小 S7 mock server** + s7Client 实例，
//! 闭环验证建连/读/写/return_code 行为。CI 友好（无真机、无 docker、无外部 snap7-server）。
//!
//! 构造响应：复用 tokio-s7 的 pub 结构（`S7Data`/`S7Header`/`Cotp`/...）的 `to_be_bytes`，
//! 与 client 的 `from_be_bytes` 对称。解析请求：**裸字节解析**（`handle_raw`）——
//! 避开库 `RequestItem::from_be_bytes_offset` 的 panic bug（`buf.get(9).unwrap()` on
//! cursor-affected BytesMut）。详见 `finalize` / `handle_raw`。

mod common;

use common::test_data_dir;
use futures_util::{SinkExt, StreamExt};
use rs_iot::host::{BridgeRegistry, CtlRegistry, HostContext};
use rs_iot::plugin::instance::InstanceManager;
use rs_iot::plugin::traits::Processor;
use rs_iot::transform::passthrough::Passthrough;
use rs_iot::types::{
    AppType, ChildSpec, CtlMessage, DeviceSpec, HostKind, Hwid, InstConfig, InstId, PluginName,
    Sample,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

// tokio-s7 的 pub 协议结构（rs-iot 依赖，测试可直接 use）。仅用于构造响应字节。
use tokio_s7::cotp::{Cotp, CotpConnection, CotpData};
use tokio_s7::datum::{Datum, ReadWriteDatum};
use tokio_s7::header::S7Header;
use tokio_s7::item::{DataItem, ReturnItem, S7ReturnItem};
use tokio_s7::packet::{S7Data, S7DataCodec};
use tokio_s7::paramter::{ReadWriteParameter, S7Parameter, SetupComParameter};
use tokio_s7::tpkt::TPKT;
use tokio_s7::types::{
    PduType, S7Area, S7DataVariableType, S7ErrorClass, S7FunctionCode, S7MessageType, S7ReturnCode,
};

// ========== mock PLC 内存 ==========

/// mock PLC 内存：按 (area, db, byte_offset) 存字节级状态。
///
/// 读时若区间内任一字节未播种，返回 `None`（mock 以 `InvalidAddress` 应答，
/// 锁住 s7Client 的 return_code 跳过路径）。写时覆盖对应字节。
#[derive(Default)]
struct Memory {
    map: HashMap<(u8, u16, u32), u8>,
}

impl Memory {
    fn read(&self, area: u8, db: u16, byte_addr: u32, count: u16) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as u32 {
            let b = *self.map.get(&(area, db, byte_addr + i))?;
            out.push(b);
        }
        Some(out)
    }

    fn write(&mut self, area: u8, db: u16, byte_addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.map.insert((area, db, byte_addr + i as u32), b);
        }
    }
}

/// 一条写记录（mock 收到 write request 时追加，供测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteEntry {
    area: S7Area,
    db_number: u16,
    byte_address: u32,
    data: Vec<u8>,
}

/// 从 RequestItem 裸字节解析出的关键字段（仅供 mock 用，避免库的 panic 解析路径）。
struct ParsedItem {
    want_bit: bool,
    count: u16,
    db_number: u16,
    area: S7Area,
    byte_address: u32,
    /// 位地址（mock 按字节寻址，目前不使用，保留以备扩展）。
    _bit_address: u32,
}

/// mock server 共享状态：内存 + 写记录。
struct ServerState {
    mem: Mutex<Memory>,
    writes: Mutex<Vec<WriteEntry>>,
}

impl ServerState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mem: Mutex::new(Memory::default()),
            writes: Mutex::new(Vec::new()),
        })
    }
}

// ========== mock server 协议层 ==========

/// 给 `S7Data` 响应收尾：按 parameter/datum 实际字节回填 header 长度字段 + TPKT 总长。
///
/// 必要原因：`S7Data::self_check` 是私有的，手动构造的响应 header 的
/// `parameter_length`/`data_length` 缺省为 0，client 端 `from_be_bytes` 靠这两个字段
/// 定位 datum，必须显式设置。
fn finalize(mut s: S7Data) -> Vec<u8> {
    let param_len = s
        .parameter
        .as_ref()
        .map(|p| p.byte_len() as u16)
        .unwrap_or(0);
    let data_len = s.datum.as_ref().map(|d| d.byte_len() as u16).unwrap_or(0);
    if let Some(h) = &mut s.header {
        h.set_parameter_len(param_len);
        h.set_data_len(data_len);
    }
    s.tpkt.length = s.byte_len() as u16;
    s.to_be_bytes()
}

/// 构造 AckData 响应头（**必须 12 字节**）。
///
/// tokio-s7 的 `S7Header::from_be_bytes` 对 AckData 有个 quirk：只要 trailing 字节 >= 2，
/// 它就按 12 字节头解析、并连读 3 字节 error 区（byte10 丢弃、byte11 当 error_class、
/// byte12 当 error_code），`byte_len()` 恒返回 12。因此响应头必须真的 12 字节，否则
/// `header_len=12` 与实际 10 字节不符 → parameter 切片越界 panic。
///
/// 设 `error_class=NoError`（byte10=0x00 被丢弃）、`error_code=0x00`（byte11 被库当
/// error_class 读成 NoError）→ `check_response` 放行。byte12 读到的 error_code 不被检查。
fn ack_header() -> S7Header {
    let mut h = S7Header::new(S7MessageType::AckData);
    h.error_class = Some(S7ErrorClass::NoError);
    h.error_code = Some(0x00);
    h
}

/// COTP CR 的应答：ConnectConfirm（pdu_type 0xD0）。client 仅校验 pdu_type。
fn build_cotp_cc() -> Vec<u8> {
    let cc = CotpConnection {
        pdu_type: PduType::ConnectConfirm,
        ..CotpConnection::default()
    };
    finalize(S7Data {
        tpkt: TPKT::default(),
        cotp: Some(Cotp::CotpConnection(cc)),
        header: None,
        parameter: None,
        datum: None,
    })
}

/// Setup Communication 应答：AckData(0x03) + SetupParameter(pdu=480)。
///
/// header 用 `ack_header()`（12 字节，详见其注释）——满足库 `S7Header::from_be_bytes`
/// 对 AckData 恒按 12 字节解析的 quirk。协商 PDU=480（s1200 默认量级，任意合理值即可，
/// s7Client 只透传，不校验具体数值）。
fn build_setup_response() -> Vec<u8> {
    finalize(S7Data {
        tpkt: TPKT::default(),
        cotp: Some(Cotp::CotpData(CotpData::default())),
        header: Some(ack_header()),
        parameter: Some(S7Parameter::SetupParameter(SetupComParameter::new(480))),
        datum: None,
    })
}

/// Read 应答：每个 `ParsedItem` 一个 DataItem，按 area/db/addr 从内存取字节。
/// 未播种的地址给 `InvalidAddress`（return_code 0x05），s7Client 跳过该点。
fn build_read_response(items: &[ParsedItem], item_count: u8, state: &ServerState) -> Vec<u8> {
    let mem = state.mem.lock().expect("mem lock");
    let mut data_items = Vec::with_capacity(items.len());
    for it in items {
        // bit 的 count=1；其余按 it.count 字节。mem.read 返回 None → InvalidAddress。
        match mem.read(it.area.code(), it.db_number, it.byte_address, it.count) {
            Some(data) => {
                let (dvt, cnt) = if it.want_bit {
                    (S7DataVariableType::Bit, 1u16)
                } else {
                    (S7DataVariableType::ByteWordDword, it.count)
                };
                data_items.push(DataItem {
                    return_code: S7ReturnCode::Success,
                    data_variable_type: dvt,
                    count: cnt,
                    data,
                });
            }
            None => {
                // return_code != Success：s7Client decode_item 返回 None 跳过。data 留空。
                data_items.push(DataItem {
                    return_code: S7ReturnCode::InvalidAddress,
                    data_variable_type: S7DataVariableType::ByteWordDword,
                    count: 0,
                    data: Vec::new(),
                });
            }
        }
    }
    drop(mem);
    // 响应参数只发 function_code + item_count（2B），不发 request_items——
    // 库 from_be_bytes 在 data.len()==2 时按空 items 解析，足够提供 function_code。
    let resp_param = ReadWriteParameter {
        function_code: S7FunctionCode::ReadVariable,
        item_count,
        request_items: Vec::new(),
    };
    finalize(S7Data {
        tpkt: TPKT::default(),
        cotp: Some(Cotp::CotpData(CotpData::default())),
        header: Some(ack_header()),
        parameter: Some(S7Parameter::ReadWriteParameter(resp_param)),
        datum: Some(Datum::ReadWrite(ReadWriteDatum::new(data_items))),
    })
}

/// Write 应答：AckData + 每项一个 ReturnItem（1B, Success）。
fn build_write_response(item_count: u8) -> Vec<u8> {
    let return_items = (0..item_count)
        .map(|_| S7ReturnItem::Return(ReturnItem::new(S7ReturnCode::Success)))
        .collect::<Vec<_>>();
    let resp_param = ReadWriteParameter {
        function_code: S7FunctionCode::WriteVariable,
        item_count,
        request_items: Vec::new(),
    };
    finalize(S7Data {
        tpkt: TPKT::default(),
        cotp: Some(Cotp::CotpData(CotpData::default())),
        header: Some(ack_header()),
        parameter: Some(S7Parameter::ReadWriteParameter(resp_param)),
        datum: Some(Datum::ReadWrite(ReadWriteDatum { return_items })),
    })
}

/// 落内存 + 记录写。`items` 与 `datas` 按 request 顺序一一对应。
fn record_write(items: &[ParsedItem], datas: &[Vec<u8>], state: &ServerState) {
    let entries: Vec<WriteEntry> = items
        .iter()
        .zip(datas.iter())
        .map(|(it, data)| WriteEntry {
            area: it.area,
            db_number: it.db_number,
            byte_address: it.byte_address,
            data: data.clone(),
        })
        .collect();
    {
        let mut mem = state.mem.lock().expect("mem lock");
        for e in &entries {
            mem.write(e.area.code(), e.db_number, e.byte_address, &e.data);
        }
    }
    {
        let mut writes = state.writes.lock().expect("writes lock");
        writes.extend(entries);
    }
}

// ---------- 裸字节解析请求 ----------

/// 从 12 字节 RequestItem 原始字节解析出 mock 关心的字段。
///
/// 布局：[spec(0), len(1), syntax(2), vtype(3), count(4-5), db(6-7), area(8), addr(9-11)]。
/// 不用库的 `from_be_bytes_offset`（其 `buf.get(9).unwrap()` 在 cursor 推进后会 None panic）。
fn parse_item(b: &[u8]) -> Option<ParsedItem> {
    if b.len() < 12 {
        return None;
    }
    let variable_type = b[3];
    let count = u16::from_be_bytes([b[4], b[5]]);
    let db_number = u16::from_be_bytes([b[6], b[7]]);
    let area = S7Area::from(b[8])?;
    let addr = ((b[9] as u32) << 16) | ((b[10] as u32) << 8) | (b[11] as u32);
    let (byte_address, bit_address) = if area == S7Area::S7Timers || area == S7Area::S7Counters {
        (addr >> 2, 0)
    } else {
        (addr >> 3, addr & 0x07)
    };
    Some(ParsedItem {
        want_bit: variable_type == S7_PARAM_VTYPE_BIT,
        count,
        db_number,
        area,
        byte_address,
        _bit_address: bit_address,
    })
}

/// `S7ParamVariableType::Bit` 的 code（0x01）——裸字节比对用（避免 import 该枚举）。
const S7_PARAM_VTYPE_BIT: u8 = 0x01;

/// 从 write request 的 datum 区逐个解析 DataItem 的 data 字段。
///
/// DataItem 布局：[return_code(1), dvt(1), count(2), data(...)]。ByteWordDword(0x04) 的
/// count 字段单位是 bit → 字节数 = count/8；Bit(0x03) 的 count 直接是字节数。非末项且
/// 项长为奇时 S7 协议补 1 字节对齐（与库 `ReadWriteDatum::byte_len` 对称）。
fn parse_write_data(data: &[u8], item_count: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(item_count);
    let mut off = 0;
    for i in 0..item_count {
        if data.len() < off + 4 {
            break;
        }
        let dvt = data[off + 1];
        let cnt = u16::from_be_bytes([data[off + 2], data[off + 3]]);
        let nbytes = if dvt == 0x04 {
            (cnt / 8) as usize
        } else {
            cnt as usize
        };
        let end = off + 4 + nbytes;
        if data.len() < end {
            break;
        }
        out.push(data[off + 4..end].to_vec());
        let item_len = 4 + nbytes;
        off = end;
        // 非末项 + 项长奇 → 跳过 1 字节填充。
        if i + 1 < item_count && item_len % 2 == 1 {
            off += 1;
        }
    }
    out
}

/// 从 read/write request 参数区解析 N 个 RequestItem（每个 12B）。
fn parse_items(param: &[u8], item_count: usize) -> Vec<ParsedItem> {
    let mut out = Vec::with_capacity(item_count);
    for i in 0..item_count {
        let off = 2 + i * 12;
        if param.len() < off + 12 {
            break;
        }
        if let Some(it) = parse_item(&param[off..off + 12]) {
            out.push(it);
        }
    }
    out
}

/// 分发单个 S7 请求（裸字节）→ 响应字节（或 None 表示不回包、关闭连接）。
///
/// 帧布局：TPKT(4) + COTP + [S7Header + param + datum]。COTP 首字节=长度，次字节=pdu_type：
/// 0xE0=CR / 0x80|0xC0=DR|DC / 0xF0=DtData（含 S7 payload）。DtData 的 S7 payload 起始于
/// 帧 byte 7（COTP DT 固定 3B）。
fn handle_raw(raw: &[u8], state: &ServerState) -> Option<Vec<u8>> {
    if raw.len() < 6 {
        return None;
    }
    match raw[5] {
        // COTP CR → CC。
        0xE0 => Some(build_cotp_cc()),
        // COTP DR/DC → 关闭。
        0x80 | 0xC0 => None,
        // COTP DtData → S7 payload。
        0xF0 => handle_dtdata(raw, state),
        _ => None,
    }
}

/// 处理 DtData 帧（含 S7Header + param + datum）。function_code 在 param 首字节。
fn handle_dtdata(raw: &[u8], state: &ServerState) -> Option<Vec<u8>> {
    // S7Header 起 on byte 7：protocol_id(7), msg_type(8), reserved(9-10), pdu_ref(11-12),
    // param_len(13-14), data_len(15-16)。Job 无 error 字段 → header 10B，param 起 on byte 17。
    if raw.len() < 17 {
        return None;
    }
    let param_len = u16::from_be_bytes([raw[13], raw[14]]) as usize;
    let _data_len = u16::from_be_bytes([raw[15], raw[16]]) as usize;
    let param_start = 17;
    if raw.len() < param_start + 2 {
        return None;
    }
    let fc = raw[param_start];
    match fc {
        // SetupCommunication → AckData + SetupParameter(480)。
        0xF0 => Some(build_setup_response()),
        // ReadVariable：按 item 取内存。
        0x04 => {
            let item_count = raw[param_start + 1] as usize;
            let items = parse_items(&raw[param_start..param_start + param_len], item_count);
            Some(build_read_response(
                &items,
                item_count.min(u8::MAX as usize) as u8,
                state,
            ))
        }
        // WriteVariable：记录 + 回 AckData。
        0x05 => {
            let item_count = raw[param_start + 1] as usize;
            let items = parse_items(&raw[param_start..param_start + param_len], item_count);
            let data_start = param_start + param_len;
            let datas = if data_start <= raw.len() {
                parse_write_data(&raw[data_start..], item_count)
            } else {
                Vec::new()
            };
            record_write(&items, &datas, state);
            Some(build_write_response(item_count.min(u8::MAX as usize) as u8))
        }
        _ => None,
    }
}

/// 单连接服务循环：Framed<S7DataCodec> 帧帧处理（裸字节分发），直到 client 关闭或 cancel。
async fn serve_s7(stream: TcpStream, state: Arc<ServerState>, token: CancellationToken) {
    let mut framed = Framed::new(stream, S7DataCodec);
    loop {
        let raw = tokio::select! {
            biased;
            _ = token.cancelled() => break,
            res = framed.next() => match res {
                Some(Ok(b)) => b,
                _ => break,
            },
        };
        match handle_raw(&raw, &state) {
            Some(bytes) => {
                if framed.send(bytes).await.is_err() {
                    break;
                }
            }
            None => break,
        }
    }
}

/// mock server acceptor：接受连接，每连接 spawn 一个 `serve_s7` task。
async fn run_server(listener: TcpListener, state: Arc<ServerState>, token: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => break,
            res = listener.accept() => match res {
                Ok((stream, _)) => {
                    let st = state.clone();
                    let ct = token.child_token();
                    tokio::spawn(async move { serve_s7(stream, st, ct).await });
                }
                Err(_) => break,
            },
        }
    }
}

// ========== 测试环境 ==========

struct Env {
    handle: lux::ServerHandle,
    client: lux::EmbeddedClient,
    shutdown: CancellationToken,
    mgr: Arc<InstanceManager>,
    state: Arc<ServerState>,
}

/// 起 lux + identity + mock S7 server + pipeline + s7Client 实例。
async fn setup(inst_id: &str, devices: Vec<DeviceSpec>, state: Arc<ServerState>) -> Env {
    let data_dir = test_data_dir(&format!("s7-{inst_id}"));
    let (handle, client) = common::start_lux(data_dir.to_str().expect("utf8")).await;
    rs_iot::db::identity::bootstrap(&client)
        .await
        .expect("bootstrap");

    // mock S7 server（127.0.0.1:0，OS 分配端口）。
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let shutdown = CancellationToken::new();
    let sv_state = state.clone();
    let sv_token = shutdown.child_token();
    tokio::spawn(async move { run_server(listener, sv_state, sv_token).await });

    // HostContext + hub + pipeline（Passthrough 透传，落 rtdata）。
    let ctx = HostContext {
        lux: client.clone(),
        hwid: Hwid::new("it-hwid"),
        shutdown: shutdown.clone(),
        ctl_registry: CtlRegistry::new(std::sync::Arc::new(
            rs_iot::process_metrics::ProcessMetrics::new(),
        )),
        bridge_registry: BridgeRegistry::new(),
        metrics: std::sync::Arc::new(rs_iot::process_metrics::ProcessMetrics::new()),
        trace_registry: rs_iot::io_trace::TraceRegistry::new(),
        event_tx: rs_iot::event_bus::EventBus::dummy(),
    };
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Sample>(64);
    let mgr = Arc::new(InstanceManager::new(ctx.clone(), hub_tx));
    let proc: Arc<dyn Processor> = Arc::new(Passthrough);
    let pt = shutdown.child_token();
    let _pipeline = tokio::spawn(rs_iot::pipeline::run(ctx, hub_rx, proc, 1, pt));

    // s7Client 实例：port 指向 mock server，period=1s 加快采集。
    let inst = InstConfig {
        inst_id: inst_id.into(),
        inst_name: "it s7".into(),
        app_code: PluginName::new("s7Client"),
        app_type: AppType::South,
        auto_start: true,
        app_config: json!({
            "endpoint": "127.0.0.1",
            "port": port,
            "plc_type": "s1200",
            "period": 1,
        }),
        devices,
        host_kind: HostKind::Builtin,
    };
    mgr.start_source(inst).await.expect("start s7");

    Env {
        handle,
        client,
        shutdown,
        mgr,
        state,
    }
}

async fn teardown(env: Env) {
    env.shutdown.cancel();
    env.mgr.stop_all().await;
    common::stop_lux(env.handle).await;
}

/// 取 lux Hash 单 field 的字符串值（HGET）。
async fn hget_cell(client: &lux::EmbeddedClient, key: &str, field: &str) -> String {
    use lux::EmbeddedValue;
    match client.execute_value("HGET", &[key, field]).await {
        Ok(EmbeddedValue::Bulk(b)) => String::from_utf8_lossy(&b).to_string(),
        Ok(EmbeddedValue::Simple(s)) => s,
        _ => String::new(),
    }
}

// ========== 测试用例 ==========

#[tokio::test]
async fn s7_read_roundtrip_to_rtdata() {
    let devices = vec![DeviceSpec {
        label: "plc".into(),
        value: "devS7".into(),
        children: vec![
            ChildSpec {
                label: "temp".into(),
                value: "DB1.DBD0".into(),
                r#type: "f32".into(),
            },
            ChildSpec {
                label: "speed".into(),
                value: "DB1.DBW4".into(),
                r#type: "i16".into(),
            },
            ChildSpec {
                label: "motor_on".into(),
                value: "M10.0".into(),
                r#type: "bool".into(),
            },
        ],
        slave_id: None,
    }];
    let state = ServerState::new();
    // 预播种 mock PLC 内存（大端编码）：
    // DB1.DBD0 f32 = 12.5 → 0x41480000；DB1.DBW4 i16 = 32000 → 0x7D00；M10.0 bool = true → 0x01。
    {
        let mut mem = state.mem.lock().expect("mem lock");
        mem.write(S7Area::DataBlocks.code(), 1, 0, &[0x41, 0x48, 0x00, 0x00]);
        mem.write(S7Area::DataBlocks.code(), 1, 4, &[0x7D, 0x00]);
        mem.write(S7Area::Flags.code(), 0, 10, &[0x01]);
    }
    let env = setup("s7read", devices, state).await;

    // period=1s，等 2.5s 保证至少两轮采集 + pipeline 落库。
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let temp = hget_cell(&env.client, "rtdata:devS7", "devS7.temp").await;
    assert!(
        temp.contains("12.5"),
        "f32 DB1.DBD0 应解码为 12.5，得到 '{temp}'"
    );
    let speed = hget_cell(&env.client, "rtdata:devS7", "devS7.speed").await;
    assert!(
        speed.contains("32000"),
        "i16 DB1.DBW4 应解码为 32000，得到 '{speed}'"
    );
    let motor = hget_cell(&env.client, "rtdata:devS7", "devS7.motor_on").await;
    assert!(
        motor.contains("true"),
        "bool M10.0 应解码为 true，得到 '{motor}'"
    );

    teardown(env).await;
}

#[tokio::test]
async fn s7_write_roundtrip() {
    let devices = vec![DeviceSpec {
        label: "plc".into(),
        value: "devS7".into(),
        children: vec![ChildSpec {
            label: "temp".into(),
            value: "DB1.DBD0".into(),
            r#type: "f32".into(),
        }],
        slave_id: None,
    }];
    let env = setup("s7write", devices, ServerState::new()).await;

    // 等连接建立 + 至少一次 tick（确保 s7Client client 非 None）。
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 经 InstanceManager 投递写命令（与 REST/MQTT 同通道）。
    let msg = CtlMessage {
        instance: InstId::new("s7write"),
        devid: "devS7".into(),
        cmd: "write".into(),
        data: json!({"address":"DB1.DBD0","value":42.5,"kind":"f32"}).to_string(),
    };
    assert!(
        env.mgr.send_ctl("s7write", msg),
        "send_ctl 应找到运行中的实例"
    );

    // 等写 round-trip 完成。
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 在 teardown 前 drop writes guard，避免跨 await 持有锁。
    let recorded: Vec<WriteEntry> = {
        let writes = env.state.writes.lock().expect("writes lock");
        writes.iter().cloned().collect::<Vec<_>>()
    };
    assert_eq!(
        recorded.len(),
        1,
        "mock 应收到恰好 1 条 write，实际 {recorded:?}"
    );
    let w = &recorded[0];
    assert_eq!(w.area, S7Area::DataBlocks, "写区应为 DB");
    assert_eq!(w.db_number, 1, "写 DB 号应为 1");
    assert_eq!(w.byte_address, 0, "写字节地址应为 0");
    assert_eq!(w.data.len(), 4, "f32 写应为 4 字节");
    let f = f32::from_be_bytes([w.data[0], w.data[1], w.data[2], w.data[3]]);
    assert!((f - 42.5).abs() < 1e-6, "写入值应解码为 42.5，实际 {f}");

    teardown(env).await;
}

#[tokio::test]
async fn s7_return_code_partial_failure() {
    // 两点同 device：ok_temp 已播种；bad_addr（DB1.DBD100）不播种 → mock 回 InvalidAddress。
    let devices = vec![DeviceSpec {
        label: "plc".into(),
        value: "devS7".into(),
        children: vec![
            ChildSpec {
                label: "ok_temp".into(),
                value: "DB1.DBD0".into(),
                r#type: "f32".into(),
            },
            ChildSpec {
                label: "bad_addr".into(),
                value: "DB1.DBD100".into(),
                r#type: "i16".into(),
            },
        ],
        slave_id: None,
    }];
    let state = ServerState::new();
    {
        let mut mem = state.mem.lock().expect("mem lock");
        mem.write(S7Area::DataBlocks.code(), 1, 0, &[0x41, 0x48, 0x00, 0x00]); // 12.5
    }
    let env = setup("s7partial", devices, state).await;

    tokio::time::sleep(Duration::from_millis(2500)).await;

    let ok = hget_cell(&env.client, "rtdata:devS7", "devS7.ok_temp").await;
    assert!(
        ok.contains("12.5"),
        "成功点 ok_temp 应落库为 12.5，得到 '{ok}'"
    );
    let bad = hget_cell(&env.client, "rtdata:devS7", "devS7.bad_addr").await;
    assert!(
        bad.is_empty(),
        "return_code=0x05 的点应被跳过、不落库，得到 '{bad}'"
    );

    teardown(env).await;
}
