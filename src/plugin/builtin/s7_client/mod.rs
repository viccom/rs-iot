//! `s7Client`：西门子 S7 南向采集插件（tokio-s7 0.1，纯 Rust，ISO-on-TCP :102）。
//!
//! 周期连接 S7-200/200Smart/300/400/1200/1500/Sinumerik828D PLC，按 `devices` 批量读取
//! 变量，组装 `Sample` 推入 hub；同时消费下行命令（`cmd="write"`）写回 PLC。
//!
//! 配置（`InstConfig::app_config` JSON）：
//! ```json
//! {
//!   "endpoint": "192.168.0.100",
//!   "plc_type": "s1200",
//!   "rack": 0, "slot": 1,
//!   "period": 2,
//!   "timeout_ms": 5000,
//!   "auto_reconnect": true
//! }
//! ```
//! - `endpoint`：PLC IP（端口默认 102，S7 标准 ISO-on-TCP；可用 `port` 覆盖——
//!   非标端口 PLC 真实存在，亦便于测试连进程内 mock server）
//! - `plc_type`：`s200`/`s200smart`/`s300`/`s400`/`s1200`/`s1500`/`sinumerik828d`
//!   （缺省 `s1200`，大小写不敏感）
//! - `rack`/`slot`：可选；缺省按 `plc_type`（`with_plc_type` 自动填默认）
//! - `period`：采集周期秒（缺省 2，最小 1）
//! - `timeout_ms`/`auto_reconnect`：可选（缺省 5000 / true）
//!
//! 变量映射（复用 `devices`，与 opcua/modbus 对齐）：
//! - `DeviceSpec.value` = 设备 ID（`rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = S7 地址串（如 `DB1.DBD0` / `M10.0` / `I0.0` / `V100`）
//! - `ChildSpec.label` = values map 的 key（空则用地址串）
//! - `ChildSpec.type` = **kind**（强制显式，见下表）
//!
//! # kind 与地址语法（重要）
//!
//! tokio-s7 的地址解析器按"位置 + 数字过滤"工作：类型前缀字符 `X/B/W/D/DBX/DBB/DBW/DBD`
//! **纯装饰、被忽略**——`DB1.DBD0` / `DB1.DW0` / `DB1.0` 解析结果完全相同。**变量位宽由
//! `kind` 决定，不是地址前缀**。区首字母仅识别 `I`/`Q`/`M`/`D`/`V`/`T`/`C`（`V`→DB1
//! 固定映射）；位寻址靠点段数（`DB1.X0.0` 3 段 / `M10.5` 2 段）。
//!
//! | kind | 位宽 | 示例地址 | 说明 |
//! |---|---|---|---|
//! | `bool` | 1 bit | `M10.5` / `DB1.X0.0` | 位寻址 |
//! | `byte` | 1 字节 | `DB1.0` / `M10` | 有符号 i8 |
//! | `i16`/`u16` | 2 字节 | `DB1.DBW0` | 大端 |
//! | `i32`/`u32` | 4 字节 | `DB1.DBD0` | 大端 |
//! | `i64`/`u64` | 8 字节 | `DB1.0` | 大端 |
//! | `f32` | 4 字节 | `DB1.DBD0` | IEEE754 大端 |
//! | `f64` | 8 字节 | `DB1.0` | IEEE754 大端 |
//!
//! 位地址（如 `M10.5`）必须配 `kind:"bool"`——配数值 kind 会读到 `M10`（位段被忽略）。
//!
//! # v1 边界
//!
//! - 读写均只支持上表数值/位类型（`bool`/`byte`/`i16`/`u16`/`i32`/`u32`/`i64`/`u64`/`f32`/`f64`）。
//! - **string/wstring 后置**（S7 string 有 2 字节头，批量路径与数值不一致；v1.1 单点支持）。
//! - 小端 `_le` 变体不暴露（S7 原生大端；非标设备后置）。
//! - counter/timer（`C`/`T` 区）后置（需通用 read + area）。

use crate::host::HostContext;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// 库的 S7Client 与本插件 struct S7Client 同名，用别名区分（S7Conn 表示到 PLC 的连接）。
use tokio_s7::client::{S7Client as S7Conn, S7Config};
use tokio_s7::error::S7Error;
use tokio_s7::item::{DataItem, RequestItem};
use tokio_s7::types::{PlcType, S7ReturnCode};

/// S7 采集插件（注册名 `s7Client`）。
#[derive(Debug, Default)]
pub struct S7Client;

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
struct S7Cfg {
    /// PLC IP 地址。
    endpoint: String,
    /// PLC 端口（缺省 102；可由 `app_config.port` 覆盖，便于非标端口 PLC + 测试）。
    port: u16,
    /// PLC 型号（决定 rack/slot 默认 + PDU）。
    plc_type: PlcType,
    /// 用户显式指定的 rack/slot（缺省 None → 走 plc_type 默认）。
    rack_slot: Option<(u8, u8)>,
    /// 采集周期秒。
    period: u64,
    /// 单次操作超时毫秒。
    timeout_ms: u64,
    /// 是否启用库内置自动重连。
    auto_reconnect: bool,
}

/// S7 标准端口（ISO-on-TCP，RFC 1006）。
const S7_PORT: u16 = 102;

/// 从 `app_config` JSON 解析插件配置。
fn parse_cfg(app: &serde_json::Value) -> Result<S7Cfg, String> {
    let endpoint = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?
        .to_string();
    let port = app
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .unwrap_or(S7_PORT);
    let plc_type = parse_plc_type(
        app.get("plc_type")
            .and_then(|v| v.as_str())
            .unwrap_or("s1200"),
    )?;
    let rack_slot = match (app.get("rack"), app.get("slot")) {
        (Some(r), Some(s)) => {
            let rack = r.as_u64().ok_or("rack must be a number")? as u8;
            let slot = s.as_u64().ok_or("slot must be a number")? as u8;
            Some((rack, slot))
        }
        _ => None,
    };
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .max(1);
    let timeout_ms = app
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(5000);
    let auto_reconnect = app
        .get("auto_reconnect")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    Ok(S7Cfg {
        endpoint,
        port,
        plc_type,
        rack_slot,
        period,
        timeout_ms,
        auto_reconnect,
    })
}

/// `plc_type` 字符串 → `PlcType`（大小写不敏感）。
fn parse_plc_type(s: &str) -> Result<PlcType, String> {
    match s.trim().to_lowercase().as_str() {
        "s200" => Ok(PlcType::S200),
        "s200smart" => Ok(PlcType::S200Smart),
        "s300" => Ok(PlcType::S300),
        "s400" => Ok(PlcType::S400),
        "s1200" => Ok(PlcType::S1200),
        "s1500" => Ok(PlcType::S1500),
        "sinumerik828d" => Ok(PlcType::Sinumerik828D),
        other => Err(format!("unsupported plc_type: {other}")),
    }
}

/// 变量位宽类型（kind）。地址前缀不可靠，位宽必须由此显式指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 位。
    Bool,
    /// 有符号字节（i8）。
    Byte,
    /// 有符号 16 位。
    I16,
    /// 无符号 16 位。
    U16,
    /// 有符号 32 位。
    I32,
    /// 无符号 32 位。
    U32,
    /// 有符号 64 位。
    I64,
    /// 无符号 64 位。
    U64,
    /// 32 位浮点（IEEE754 大端）。
    F32,
    /// 64 位浮点（IEEE754 大端）。
    F64,
}

/// kind 字符串 → `Kind`（大小写不敏感；接受常见别名）。
fn parse_kind(s: &str) -> Result<Kind, String> {
    match s.trim().to_lowercase().as_str() {
        "bool" | "bit" => Ok(Kind::Bool),
        "byte" | "i8" => Ok(Kind::Byte),
        "i16" | "int16" => Ok(Kind::I16),
        "u16" | "uint16" => Ok(Kind::U16),
        "i32" | "int32" => Ok(Kind::I32),
        "u32" | "uint32" => Ok(Kind::U32),
        "i64" | "int64" => Ok(Kind::I64),
        "u64" | "uint64" => Ok(Kind::U64),
        "f32" | "float" | "float32" => Ok(Kind::F32),
        "f64" | "double" | "float64" => Ok(Kind::F64),
        other => Err(format!("unsupported kind: {other}")),
    }
}

/// kind → 批量 read 的 `RequestItem`（bool 走 `parse_bit`，其余 `parse_byte(count)`）。
fn build_request(addr: &str, kind: Kind) -> Result<RequestItem, S7Error> {
    match kind {
        Kind::Bool => RequestItem::parse_bit(addr),
        Kind::Byte => RequestItem::parse_byte(addr, 1),
        Kind::I16 | Kind::U16 => RequestItem::parse_byte(addr, 2),
        Kind::I32 | Kind::U32 | Kind::F32 => RequestItem::parse_byte(addr, 4),
        Kind::I64 | Kind::U64 | Kind::F64 => RequestItem::parse_byte(addr, 8),
    }
}

/// kind 所需的最小字节数（bool/byte=1，i16/u16=2，i32/u32/f32=4，i64/u64/f64=8）。
fn kind_byte_count(kind: Kind) -> usize {
    match kind {
        Kind::Bool | Kind::Byte => 1,
        Kind::I16 | Kind::U16 => 2,
        Kind::I32 | Kind::U32 | Kind::F32 => 4,
        Kind::I64 | Kind::U64 | Kind::F64 => 8,
    }
}

/// `DataItem` → `SampleValue`（检查 `return_code`；按 kind 大端解码 `data`）。
///
/// 单项失败（`return_code != Success`）返回 `None`，由调用方 warn 跳过——批量 read 不
/// 因单点失败报 `Err`，部分失败体现在每项的 `return_code`（对标 modbus exception）。
fn decode_item(kind: Kind, item: &DataItem) -> Option<SampleValue> {
    if item.return_code != S7ReturnCode::Success {
        tracing::warn!(
            "[s7Client] read item return_code {:?} ({})",
            item.return_code,
            item.return_code.description()
        );
        return None;
    }
    let d = &item.data;
    let need = kind_byte_count(kind);
    if d.len() < need {
        tracing::warn!(
            "[s7Client] decode data too short: need {need} bytes, have {}",
            d.len()
        );
        return None;
    }
    Some(match kind {
        Kind::Bool => SampleValue::Bool(d[0] != 0),
        Kind::Byte => SampleValue::Int(i8::from_be_bytes([d[0]]) as i64),
        Kind::I16 => SampleValue::Int(i16::from_be_bytes([d[0], d[1]]) as i64),
        Kind::U16 => SampleValue::Int(u16::from_be_bytes([d[0], d[1]]) as i64),
        Kind::I32 => SampleValue::Int(i32::from_be_bytes([d[0], d[1], d[2], d[3]]) as i64),
        Kind::U32 => SampleValue::Int(u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as i64),
        Kind::I64 => SampleValue::Int(i64::from_be_bytes([
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        ])),
        Kind::U64 => {
            SampleValue::Int(
                u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]) as i64,
            )
        }
        Kind::F32 => SampleValue::Float(f32::from_be_bytes([d[0], d[1], d[2], d[3]]) as f64),
        Kind::F64 => SampleValue::Float(f64::from_be_bytes([
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        ])),
    })
}

/// 建立到 PLC 的 S7 连接（`with_plc_type` 自动填 rack/slot 默认；用户显式配则覆盖）。
async fn connect(cfg: &S7Cfg) -> Result<S7Conn, String> {
    let mut s7 = S7Config::new(&cfg.endpoint)
        .with_plc_type(cfg.plc_type)
        .with_port(cfg.port)
        .with_timeout(cfg.timeout_ms)
        .with_auto_reconnect(cfg.auto_reconnect);
    if let Some((rack, slot)) = cfg.rack_slot {
        s7 = s7.with_rack_slot(rack, slot);
    }
    let mut client = S7Conn::new(s7);
    client
        .connect()
        .await
        .map_err(|e| format!("connect {}: {e}", cfg.endpoint))?;
    Ok(client)
}

/// JSON → `SampleValue`（写命令的 value 字段）。
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

/// 按 `SampleValue` 推断 kind（写命令未显式指定时）。
fn infer_kind(v: &SampleValue) -> Kind {
    match v {
        SampleValue::Bool(_) => Kind::Bool,
        SampleValue::Int(_) => Kind::I64,
        SampleValue::Float(_) => Kind::F64,
        SampleValue::Str(_) => Kind::Byte,
    }
}

#[async_trait]
impl Source for S7Client {
    fn name(&self) -> &'static str {
        "s7Client"
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
                tracing::warn!("[s7Client] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // 预解析所有设备的 (devid, [(field, addr, kind)])，采集循环内不再重复 parse。
        let devices: Vec<(String, Vec<(String, String, Kind)>)> = inst
            .devices
            .iter()
            .map(|dev| {
                let pts: Vec<(String, String, Kind)> = dev
                    .children
                    .iter()
                    .filter_map(|c| match parse_kind(&c.r#type) {
                        Ok(kind) => {
                            let field = if c.label.is_empty() {
                                c.value.clone()
                            } else {
                                c.label.clone()
                            };
                            Some((field, c.value.clone(), kind))
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[s7Client] inst {inst_id} dev {} bad kind '{}': {}",
                                dev.value,
                                c.r#type,
                                e
                            );
                            None
                        }
                    })
                    .collect();
                (dev.value.clone(), pts)
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

        let mut client: Option<S7Conn> = None;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[s7Client] inst {inst_id} stopping");
                    break;
                }
                msg = cmd_rx.recv() => {
                    if let Some(m) = msg {
                        if m.cmd == "write" {
                            handle_write(&client, &m.data).await;
                        } else {
                            tracing::info!("[s7Client] inst {inst_id} unknown cmd: {}", m.cmd);
                        }
                    }
                }
                _ = flush_ticker.tick() => {
                    tracker.flush().await;
                }
                _ = ticker.tick() => {
                    if client.is_none() {
                        match connect(&cfg).await {
                            Ok(c) => {
                                reconnects += 1;
                                tracker.extra_insert(
                                    "reconnect_count",
                                    serde_json::json!(reconnects),
                                );
                                tracing::info!("[s7Client] inst {inst_id} connected {}", cfg.endpoint);
                                client = Some(c);
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[s7Client] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() {
                                    tracker.flush().await;
                                }
                                continue;
                            }
                        }
                    }
                    let c = match &client {
                        Some(c) => c,
                        None => continue,
                    };
                    let started = std::time::Instant::now();
                    let mut tick_ok = true;
                    for (devid, pts) in &devices {
                        if pts.is_empty() {
                            continue;
                        }
                        // 构造 Vec<RequestItem>：地址解析失败的点 warn 跳过，不计入。
                        let mut items: Vec<RequestItem> = Vec::with_capacity(pts.len());
                        let mut ok_pts: Vec<&(String, String, Kind)> = Vec::with_capacity(pts.len());
                        for p in pts {
                            match build_request(&p.1, p.2) {
                                Ok(it) => {
                                    items.push(it);
                                    ok_pts.push(p);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "[s7Client] inst {inst_id} bad address '{}': {e}",
                                        p.1
                                    );
                                }
                            }
                        }
                        if items.is_empty() {
                            continue;
                        }
                        match c.read(items).await {
                            Ok(data_items) => {
                                let mut values = BTreeMap::new();
                                for (p, di) in ok_pts.iter().zip(data_items.iter()) {
                                    if let Some(v) = decode_item(p.2, di) {
                                        values.insert(p.0.clone(), v);
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
                                tracker.tick_fail(&format!("read failed: {e}"));
                                tracing::warn!("[s7Client] inst {inst_id} read failed: {e}");
                                // 连接类错误：drop 旧 client，下个 tick 重连（库的
                                // auto_reconnect 也可能在后台恢复，此处兜底）。
                                if e.is_connection_error() {
                                    client = None;
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

        if let Some(mut c) = client {
            let _ = c.disconnect().await;
        }
        tracing::info!("[s7Client] inst {inst_id} exited");

        // 统一退出处理（cancelled 触发；stat 落 stopped）。
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// 处理 write 命令：`data` = JSON `{"address":"DB1.DBD0","value":42.5,"kind":"f32"}`。
///
/// `kind` 缺省时按 value 类型推断（bool→Bool、浮点→F64、整数→I64）。写方向用库 typed
/// 单点方法（库内部处理大端编码），不批量——写命令单点语义清晰。
async fn handle_write(client: &Option<S7Conn>, data: &str) {
    let c = match client {
        Some(c) => c,
        None => {
            tracing::warn!("[s7Client] write ignored: not connected");
            return;
        }
    };
    let req: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[s7Client] write bad json: {e}");
            return;
        }
    };
    let address = match req.get("address").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => {
            tracing::warn!("[s7Client] write missing address");
            return;
        }
    };
    let value = match req.get("value") {
        Some(v) => json_to_sample_value(v),
        None => {
            tracing::warn!("[s7Client] write missing value");
            return;
        }
    };
    let kind = match req.get("kind").and_then(|v| v.as_str()) {
        Some(k) => match parse_kind(k) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("[s7Client] write bad kind: {e}");
                return;
            }
        },
        None => infer_kind(&value),
    };
    if let Err(e) = write_typed(c, address, kind, &value).await {
        tracing::warn!("[s7Client] write failed: {e}");
    }
}

/// 按 kind 调库 typed write_* 方法。数值按目标类型截断；bool 接受 0/非零或布尔。
async fn write_typed(c: &S7Conn, addr: &str, kind: Kind, v: &SampleValue) -> Result<(), S7Error> {
    let as_i = || match v {
        SampleValue::Int(i) => *i,
        SampleValue::Float(f) => *f as i64,
        SampleValue::Bool(b) => i64::from(*b),
        SampleValue::Str(s) => s.parse().unwrap_or(0),
    };
    let as_u = || as_i() as u64;
    let as_f = || match v {
        SampleValue::Float(f) => *f,
        SampleValue::Int(i) => *i as f64,
        SampleValue::Bool(b) => u8::from(*b) as f64,
        SampleValue::Str(s) => s.parse().unwrap_or(0.0),
    };
    let as_b = || match v {
        SampleValue::Bool(b) => *b,
        SampleValue::Int(i) => *i != 0,
        SampleValue::Float(f) => *f != 0.0,
        SampleValue::Str(s) => s.eq_ignore_ascii_case("true"),
    };
    match kind {
        Kind::Bool => c.write_bool(addr, as_b()).await,
        Kind::Byte => c.write_byte(addr, as_u() as u8).await,
        Kind::I16 => c.write_int16(addr, as_i() as i16).await,
        Kind::U16 => c.write_uint16(addr, as_u() as u16).await,
        Kind::I32 => c.write_int32(addr, as_i() as i32).await,
        Kind::U32 => c.write_uint32(addr, as_u() as u32).await,
        Kind::I64 => c.write_int64(addr, as_i()).await,
        Kind::U64 => c.write_uint64(addr, as_u()).await,
        Kind::F32 => c.write_float32(addr, as_f() as f32).await,
        Kind::F64 => c.write_float64(addr, as_f()).await,
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "s7Client",
        create: || Box::new(S7Client),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_s7::types::{S7DataVariableType, S7ParamVariableType};

    #[test]
    fn parse_plc_type_all_variants_case_insensitive() {
        assert_eq!(parse_plc_type("s200").unwrap(), PlcType::S200);
        assert_eq!(parse_plc_type("S200SMART").unwrap(), PlcType::S200Smart);
        assert_eq!(parse_plc_type("s300").unwrap(), PlcType::S300);
        assert_eq!(parse_plc_type("s400").unwrap(), PlcType::S400);
        assert_eq!(parse_plc_type("s1200").unwrap(), PlcType::S1200);
        assert_eq!(parse_plc_type("s1500").unwrap(), PlcType::S1500);
        assert_eq!(
            parse_plc_type("sinumerik828d").unwrap(),
            PlcType::Sinumerik828D
        );
        assert!(parse_plc_type("s7-1500").is_err());
        assert!(parse_plc_type("").is_err());
    }

    #[test]
    fn parse_kind_accepts_aliases() {
        assert_eq!(parse_kind("bool").unwrap(), Kind::Bool);
        assert_eq!(parse_kind("bit").unwrap(), Kind::Bool);
        assert_eq!(parse_kind("byte").unwrap(), Kind::Byte);
        assert_eq!(parse_kind("i16").unwrap(), Kind::I16);
        assert_eq!(parse_kind("uint16").unwrap(), Kind::U16);
        assert_eq!(parse_kind("int32").unwrap(), Kind::I32);
        assert_eq!(parse_kind("f32").unwrap(), Kind::F32);
        assert_eq!(parse_kind("float").unwrap(), Kind::F32);
        assert_eq!(parse_kind("f64").unwrap(), Kind::F64);
        assert_eq!(parse_kind("double").unwrap(), Kind::F64);
        // string v1 不支持
        assert!(parse_kind("string").is_err());
        assert!(parse_kind("").is_err());
    }

    #[test]
    fn parse_cfg_defaults_and_validation() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"192.168.0.1"})).unwrap();
        assert_eq!(cfg.endpoint, "192.168.0.1");
        assert_eq!(cfg.port, 102, "port defaults to 102 (S7 standard)");
        assert_eq!(cfg.plc_type, PlcType::S1200);
        assert!(cfg.rack_slot.is_none(), "rack/slot default to plc_type");
        assert_eq!(cfg.period, 2);
        assert_eq!(cfg.timeout_ms, 5000);
        assert!(cfg.auto_reconnect);

        let cfg2 = parse_cfg(
            &serde_json::json!({"endpoint":"x","port":1025,"plc_type":"s300","rack":0,"slot":2,"period":0,"timeout_ms":1000,"auto_reconnect":false}),
        )
        .unwrap();
        assert_eq!(cfg2.plc_type, PlcType::S300);
        assert_eq!(cfg2.port, 1025, "port overridable (非标端口 PLC / 测试)");
        assert_eq!(cfg2.rack_slot, Some((0, 2)));
        assert_eq!(cfg2.period, 1, "period 0 clamped to 1");
        assert_eq!(cfg2.timeout_ms, 1000);
        assert!(!cfg2.auto_reconnect);

        assert!(
            parse_cfg(&serde_json::json!({})).is_err(),
            "missing endpoint errors"
        );
        assert!(parse_cfg(&serde_json::json!({"endpoint":"x","plc_type":"bad"})).is_err());
    }

    #[test]
    fn build_request_count_matches_kind() {
        // bool → parse_bit（variable_type = Bit）
        let r = build_request("M10.0", Kind::Bool).unwrap();
        assert_eq!(r.variable_type, S7ParamVariableType::Bit);
        // 数值 → parse_byte(count)，count 按字节
        assert_eq!(build_request("DB1.0", Kind::Byte).unwrap().count, 1);
        assert_eq!(build_request("DB1.0", Kind::I16).unwrap().count, 2);
        assert_eq!(build_request("DB1.0", Kind::U16).unwrap().count, 2);
        assert_eq!(build_request("DB1.0", Kind::I32).unwrap().count, 4);
        assert_eq!(build_request("DB1.0", Kind::U32).unwrap().count, 4);
        assert_eq!(build_request("DB1.0", Kind::F32).unwrap().count, 4);
        assert_eq!(build_request("DB1.0", Kind::I64).unwrap().count, 8);
        assert_eq!(build_request("DB1.0", Kind::U64).unwrap().count, 8);
        assert_eq!(build_request("DB1.0", Kind::F64).unwrap().count, 8);
    }

    #[test]
    fn build_request_rejects_bad_address() {
        // 未知区前缀
        assert!(build_request("X10.0", Kind::Bool).is_err());
        // 位号超 7
        assert!(build_request("M10.8", Kind::Bool).is_err());
        // 空地址
        assert!(build_request("", Kind::I16).is_err());
    }

    /// 构造测试用 DataItem（return_code + 原始 data 字节）。
    fn data_item(code: S7ReturnCode, data: Vec<u8>) -> DataItem {
        DataItem {
            return_code: code,
            data_variable_type: S7DataVariableType::ByteWordDword,
            count: data.len() as u16,
            data,
        }
    }

    #[test]
    fn decode_item_big_endian_roundtrip() {
        // i16 = 32 → 0x0020
        assert_eq!(
            decode_item(
                Kind::I16,
                &data_item(S7ReturnCode::Success, vec![0x00, 0x20])
            ),
            Some(SampleValue::Int(32))
        );
        // u16 = 61472 → 0xF020
        assert_eq!(
            decode_item(
                Kind::U16,
                &data_item(S7ReturnCode::Success, vec![0xF0, 0x20])
            ),
            Some(SampleValue::Int(61472))
        );
        // i32 = 1 → 0x00000001
        assert_eq!(
            decode_item(
                Kind::I32,
                &data_item(S7ReturnCode::Success, vec![0, 0, 0, 1])
            ),
            Some(SampleValue::Int(1))
        );
        // u32 = 0xFFFFFFFF = 4294967295
        assert_eq!(
            decode_item(
                Kind::U32,
                &data_item(S7ReturnCode::Success, vec![0xFF, 0xFF, 0xFF, 0xFF])
            ),
            Some(SampleValue::Int(4_294_967_295))
        );
        // i64 = -1 → 8 字节 0xFF
        assert_eq!(
            decode_item(Kind::I64, &data_item(S7ReturnCode::Success, vec![0xFF; 8])),
            Some(SampleValue::Int(-1))
        );
        // f32 = 12.5 → 0x41480000
        let f = decode_item(
            Kind::F32,
            &data_item(S7ReturnCode::Success, vec![0x41, 0x48, 0x00, 0x00]),
        );
        assert!(matches!(f, Some(SampleValue::Float(v)) if (v - 12.5).abs() < 1e-6));
        // f64 = 1.0 → 0x3FF0000000000000
        let f = decode_item(
            Kind::F64,
            &data_item(S7ReturnCode::Success, vec![0x3F, 0xF0, 0, 0, 0, 0, 0, 0]),
        );
        assert!(matches!(f, Some(SampleValue::Float(v)) if v == 1.0));
        // bool true → 0x01
        assert_eq!(
            decode_item(Kind::Bool, &data_item(S7ReturnCode::Success, vec![0x01])),
            Some(SampleValue::Bool(true))
        );
        // byte (i8) -1 → 0xFF
        assert_eq!(
            decode_item(Kind::Byte, &data_item(S7ReturnCode::Success, vec![0xFF])),
            Some(SampleValue::Int(-1))
        );
    }

    #[test]
    fn decode_item_rejects_bad_return_code() {
        assert_eq!(
            decode_item(
                Kind::I16,
                &data_item(S7ReturnCode::InvalidAddress, vec![0, 0])
            ),
            None
        );
        assert_eq!(
            decode_item(
                Kind::Bool,
                &data_item(S7ReturnCode::ObjectDoesNotExist, vec![0])
            ),
            None
        );
    }

    #[test]
    fn decode_item_rejects_short_data() {
        // i16 需 2 字节，给 1 字节
        assert_eq!(
            decode_item(Kind::I16, &data_item(S7ReturnCode::Success, vec![0])),
            None
        );
        // f64 需 8 字节，给 4 字节
        assert_eq!(
            decode_item(Kind::F64, &data_item(S7ReturnCode::Success, vec![0; 4])),
            None
        );
    }

    #[test]
    fn infer_kind_from_sample_value() {
        assert_eq!(infer_kind(&SampleValue::Bool(true)), Kind::Bool);
        assert_eq!(infer_kind(&SampleValue::Int(5)), Kind::I64);
        assert_eq!(infer_kind(&SampleValue::Float(1.5)), Kind::F64);
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
}
