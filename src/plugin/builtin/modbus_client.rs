//! `modbusClient`：Modbus 南向采集插件（tokio-modbus 0.17），支持 **TCP / RTU（serial /
//! over-tcp / over-udp）** 四种传输，统一产出 `Context`，采集/写逻辑同构。
//!
//! 周期连接外部 Modbus 从站，按 `devices` 配置批量读，组装 `Sample` 推入 hub。
//! 同时消费下行命令（`cmd="write"`）写回从站。满足现场三类需求：
//!
//! 1. **布尔**：独立 coil/discrete（FC01/FC02，`coil:0:bool`）或 holding/input 寄存器位
//!    解析（`holding:0:bit:3`，多个布尔打包进一个寄存器）。
//! 2. **混合类型批量读 + 偏移转换**（一次 FC03/FC04 读连续区间，各点按 offset 解码）。
//! 3. **大块读取部分使用**（`gap_threshold`/`max_regs` 控制区间合并）。
//!
//! 配置（`InstConfig::app_config` JSON，扁平 `kind` 决定传输；`kind` 缺省即 tcp，向后兼容）：
//!
//! **TCP**（默认）：
//! ```json
//! { "kind":"tcp", "endpoint":"192.168.1.5:502", "slave_id":1, "period":3,
//!   "gap_threshold":16, "max_regs":100, "reconnect_secs":2 }
//! ```
//! **RTU over serial**（串口直连 RS-485/RS-232；`serial_port` 必填，串口参数缺省 9600/8N1/NONE）：
//! ```json
//! { "kind":"rtu_serial", "serial_port":"COM3", "baud_rate":9600,
//!   "data_bits":8, "parity":"none", "stop_bits":1, "flow_control":"none",
//!   "slave_id":1, "period":3 }
//! ```
//! **RTU over TCP**（串口服务器/网关透传，RTU 帧+CRC 走 TCP，无 MBAP 头）：
//! ```json
//! { "kind":"rtu_tcp", "endpoint":"192.168.1.5:502", "slave_id":1, "period":3 }
//! ```
//! **RTU over UDP**（部分 DTU 用 UDP datagram 透传 RTU 帧；内部 connected socket，
//! datagram 边界天然保帧，无流式粘包）：
//! ```json
//! { "kind":"rtu_udp", "endpoint":"10.0.0.7:502", "slave_id":1, "period":3 }
//! ```
//!
//! 串口参数（仅 rtu_serial）：`baud_rate`(默认 9600) / `data_bits`(5/6/7/8，默认 8) /
//! `parity`(none/even/odd，默认 none) / `stop_bits`(1/2，默认 1) /
//! `flow_control`(none/software/hardware，默认 none)。
//!
//! 变量映射（复用 `devices`，与 opcua/datasim 对齐）：
//! - `DeviceSpec.value` = 设备 ID（`rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = 地址串（如 `holding:0:f32:ABCD`）
//! - `ChildSpec.label` = values map key（显示名；空则用地址串）
//!
//! 地址串语法 `<func>:<addr>:<type>[:<byte_order>][:<bit>]`：
//! - `func`：`coil`(FC01) / `discrete`(FC02，只读) / `holding`(FC03) / `input`(FC04，只读)
//! - `addr`：0-based 地址（u16；与 wire 一致，**不做 40001 自动转换**——各厂商减 1/减 0
//!   不一致。PLC tag 换算：holding=tag-40001，input=tag-30001，coil=tag-1，discrete=tag-10001）
//! - `type`：
//!   - `coil`/`discrete`：仅 `bool`（直接布尔，1 bit/addr，FC01/FC02 返回 `Vec<bool>`）
//!   - `holding`/`input`：`bit`（寄存器位，需位号）/ `i16`/`u16`/`i32`/`u32`/`f32`/`f64`
//! - `byte_order`（holding/input 的 i32/u32/f32/f64 可选，默认 `ABCD`）：`ABCD`(大端)/
//!   `CDAB`(字交换)/`BADC`(字内字节交换)/`DCBA`(小端)。`i16`/`u16` 单寄存器无字节序，带则报错。
//! - `bit`（holding/input 的 `bit` 类型必填）：0-15
//!
//! 示例：`coil:0:bool`、`discrete:5:bool`、`holding:0:f32:ABCD`、`holding:10:bit:3`、`input:5:u16`。
//!
//! **字节序**：以 32 位 4 字节 `[A,B,C,D]`（`A=reg0 高字节`）为基准，置换后统一
//! `from_be_bytes`。4 种记法均为对合置换（`perm4[perm4[i]]==i`），故 `encode` 是 `decode`
//! 的逆，单测以 round-trip 为金标准。
//!
//! **批量合并**：`gap_threshold` 约束相邻两点**最大单空洞**（非未用总数）；`max_regs`
//! 约束单区间最大单元数（coil/discrete 为 bit 数，holding/input 为寄存器数；协议上限 125，
//! 真实设备常更低，遇 IllegalDataAddress 往下调）。
//!
//! **写命令**（`data` JSON `{"address":"holding:0:f32:ABCD","value":12.34}`）：
//! - coil → FC05（单）/ FC0F（多）
//! - holding u16/i16/bit → FC06；i32/u32/f32/f64 → FC16；bit 走 RMW（读-改-写，单 task 内
//!   原子但非事务，部分失败仅 warn）
//! - discrete/input 只读，写拒绝
//!
//! **传输边界**：TCP / RTU(serial / over-tcp / over-udp) 四种均产出统一 `Context`，
//! 采集/写逻辑同构；ASCII 暂不支持（tokio-modbus 无，未来可借 `rmodbus` codec 后置）。
//! 实例级 slave_id（多 RTU 从站网关需每 slave 建一实例，共享 endpoint）。RTU over UDP 用
//! connected socket（`connect(peer)` 后 `send`/`recv` 无需 addr），datagram 边界天然消除
//! 流式粘包/分帧歧义。

use crate::host::HostContext;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_modbus::client::{rtu, tcp, Context, Reader, Writer};
use tokio_modbus::prelude::SlaveContext;
use tokio_modbus::Slave;
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, StopBits};
use tokio_util::sync::CancellationToken;

/// Modbus 采集插件（无状态工厂标记，每个实例 task 内持有自己的 `Context`）。
/// 支持 TCP / RTU（serial / over-tcp / over-udp）四种传输，统一产出 `Context`，采集/写逻辑同构。
#[derive(Debug, Default)]
pub struct ModbusClient;

/// 传输模式（扁平 `kind` 映射）。RTU 三种均经 `rtu::attach_slave` 产 `Context`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// Modbus TCP（MBAP 头）。
    Tcp,
    /// RTU over serial（RS-485/RS-232 串口直连）。
    RtuSerial,
    /// RTU over TCP（串口服务器/网关透传，RTU 帧+CRC 走 TCP，无 MBAP 头）。
    RtuTcp,
    /// RTU over UDP（部分 DTU 用 UDP datagram 透传 RTU 帧）。
    RtuUdp,
}

/// 串口校验位（映射 `serialport::Parity`，工业默认 None）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ParityKind {
    #[default]
    None,
    Even,
    Odd,
}

/// 串口流控（映射 `serialport::FlowControl`，工业默认 None）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FlowKind {
    #[default]
    None,
    Software,
    Hardware,
}

/// 从 `app_config` 解析的插件配置。`transport` 决定必填字段（rtu_serial 需 serial_port，余需 endpoint）。
#[derive(Debug)]
struct ModbusCfg {
    transport: Transport,
    /// tcp/rtu_tcp/rtu_udp 的 `ip:port`（rtu_serial 时为空）。
    endpoint: String,
    /// rtu_serial 串口名（COM3 / /dev/ttyUSB0）。
    serial_port: Option<String>,
    /// rtu_serial 波特率（默认 9600）。
    baud_rate: u32,
    /// rtu_serial 数据位（5/6/7/8，默认 8）。
    data_bits: u8,
    /// rtu_serial 校验位（默认 None）。
    parity: ParityKind,
    /// rtu_serial 停止位（1/2，默认 1）。
    stop_bits: u8,
    /// rtu_serial 流控（默认 None）。
    flow_control: FlowKind,
    slave_id: u8,
    period: u64,
    /// 相邻两点最大单空洞（超过则分裂成两次读）。
    gap_threshold: u16,
    /// 单次请求最大单元数（coil/discrete 为 bit 数，holding/input 为寄存器数；协议上限 125）。
    max_regs: u16,
    reconnect_secs: u64,
}

/// 解析 `app_config`。`kind` 扁平：tcp（默认，向后兼容）/ rtu_serial / rtu_tcp / rtu_udp。
/// rtu_serial 必须有 `serial_port`，其余必须有 `endpoint`。串口参数缺省 9600/8N1/NONE。
fn parse_cfg(app: &serde_json::Value) -> Result<ModbusCfg, String> {
    let kind = app.get("kind").and_then(|v| v.as_str()).unwrap_or("tcp");
    let transport = match kind {
        "tcp" => Transport::Tcp,
        "rtu_serial" => Transport::RtuSerial,
        "rtu_tcp" => Transport::RtuTcp,
        "rtu_udp" => Transport::RtuUdp,
        other => {
            return Err(format!(
                "unsupported kind '{other}': 仅支持 tcp / rtu_serial / rtu_tcp / rtu_udp（ASCII 见文档后置）"
            ));
        }
    };
    // rtu_serial 必须有 serial_port；其余必须有 endpoint。
    let (endpoint, serial_port) = match transport {
        Transport::RtuSerial => {
            let sp = app
                .get("serial_port")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "rtu_serial 需要 serial_port（如 COM3 / /dev/ttyUSB0）".to_string())?
                .to_string();
            (String::new(), Some(sp))
        }
        _ => {
            let ep = app
                .get("endpoint")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "app_config.endpoint missing".to_string())?
                .to_string();
            (ep, None)
        }
    };
    let baud_rate = app
        .get("baud_rate")
        .and_then(|v| v.as_u64())
        .unwrap_or(9600) as u32;
    let data_bits = app.get("data_bits").and_then(|v| v.as_u64()).unwrap_or(8) as u8;
    if !matches!(data_bits, 5..=8) {
        return Err(format!("unsupported data_bits {data_bits}（5/6/7/8）"));
    }
    let parity = match app.get("parity").and_then(|v| v.as_str()) {
        Some("even") => ParityKind::Even,
        Some("odd") => ParityKind::Odd,
        Some("none") | None => ParityKind::None,
        Some(other) => return Err(format!("unsupported parity '{other}'（none/even/odd）")),
    };
    let stop_bits = app.get("stop_bits").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
    if !matches!(stop_bits, 1 | 2) {
        return Err(format!("unsupported stop_bits {stop_bits}（1/2）"));
    }
    let flow_control = match app.get("flow_control").and_then(|v| v.as_str()) {
        Some("software") => FlowKind::Software,
        Some("hardware") => FlowKind::Hardware,
        Some("none") | None => FlowKind::None,
        Some(other) => {
            return Err(format!(
                "unsupported flow_control '{other}'（none/software/hardware）"
            ))
        }
    };
    let slave_id = app.get("slave_id").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .max(1);
    let gap_threshold = app
        .get("gap_threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(16) as u16;
    let max_regs = app
        .get("max_regs")
        .and_then(|v| v.as_u64())
        .unwrap_or(100)
        .clamp(1, 125) as u16;
    let reconnect_secs = app
        .get("reconnect_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(2)
        .max(1);
    Ok(ModbusCfg {
        transport,
        endpoint,
        serial_port,
        baud_rate,
        data_bits,
        parity,
        stop_bits,
        flow_control,
        slave_id,
        period,
        gap_threshold,
        max_regs,
        reconnect_secs,
    })
}

// ========== 地址模型 ==========

/// 功能码（4 种读各对应独立地址空间，合并时按 func 分组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    /// FC01 coil（可读可写，1 bit/addr）。
    Coil,
    /// FC02 discrete input（只读，1 bit/addr）。
    Discrete,
    /// FC03 holding register（可读可写，16 bit/addr）。
    Holding,
    /// FC04 input register（只读，16 bit/addr）。
    Input,
}

impl Func {
    /// 是否只读（discrete/input）。
    fn is_readonly(self) -> bool {
        matches!(self, Func::Discrete | Func::Input)
    }
}

/// 数据类型。`Bool` 仅配 coil/discrete（直接布尔）；`Bit` 仅配 holding/input（寄存器位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    /// coil/discrete 直接布尔（FC01/FC02 返回 `Vec<bool>`）。
    Bool,
    /// holding/input 寄存器位（需位号 0-15）。
    Bit,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

/// 32/64 位浮点/整型的字节序（Modicon 标准记法）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// 大端（默认，ABCD）。
    Abcd,
    /// 字交换（CDAB，low word first）。
    Cdab,
    /// 字内字节交换（BADC）。
    Badc,
    /// 小端（DCBA）。
    Dcba,
}

/// 一个点位规约（解析自地址串）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PointSpec {
    func: Func,
    addr: u16,
    typ: Type,
    order: Order,
    bit: Option<u8>,
}

impl PointSpec {
    /// 该点占用的地址单元数（coil/discrete bool=1 bit 地址；holding/input bit/i16/u16=1 寄存器，
    /// i32/u32/f32=2，f64=4）。
    fn width(&self) -> u16 {
        match self.typ {
            Type::Bool | Type::Bit | Type::I16 | Type::U16 => 1,
            Type::I32 | Type::U32 | Type::F32 => 2,
            Type::F64 => 4,
        }
    }
}

fn parse_func(s: &str) -> Result<Func, String> {
    match s.to_lowercase().as_str() {
        "coil" | "coils" | "fc01" => Ok(Func::Coil),
        "discrete" | "discretes" | "discrete_input" | "fc02" => Ok(Func::Discrete),
        "holding" | "holdings" | "fc03" => Ok(Func::Holding),
        "input" | "inputs" | "fc04" => Ok(Func::Input),
        other => Err(format!(
            "unsupported func '{other}'（coil/discrete/holding/input）"
        )),
    }
}

fn parse_type(s: &str) -> Result<Type, String> {
    match s.to_lowercase().as_str() {
        "bool" | "boolean" => Ok(Type::Bool),
        "bit" => Ok(Type::Bit),
        "i16" | "int16" => Ok(Type::I16),
        "u16" | "uint16" => Ok(Type::U16),
        "i32" | "int32" => Ok(Type::I32),
        "u32" | "uint32" => Ok(Type::U32),
        "f32" | "float" | "float32" => Ok(Type::F32),
        "f64" | "double" | "float64" => Ok(Type::F64),
        other => Err(format!("unsupported type '{other}'")),
    }
}

fn parse_order(s: &str) -> Result<Order, String> {
    match s.to_uppercase().as_str() {
        "ABCD" | "BE" | "BIG" => Ok(Order::Abcd),
        "CDAB" | "SWAP" | "WORDSWAP" => Ok(Order::Cdab),
        "BADC" | "BYTESWAP" => Ok(Order::Badc),
        "DCBA" | "LE" | "LITTLE" => Ok(Order::Dcba),
        other => Err(format!(
            "unsupported byte_order '{other}'（ABCD/CDAB/BADC/DCBA）"
        )),
    }
}

/// 解析地址串 `<func>:<addr>:<type>[:<byte_order>][:<bit>]`。
///
/// func 与 type 组合校验：coil/discrete 仅 bool（直接布尔，无后续段）；holding/input 用
/// bit（需位号）/i16/u16（无字节序）/i32/u32/f32/f64（可选字节序），不接受 bool。
fn parse_point(s: &str) -> Result<PointSpec, String> {
    let parts: Vec<&str> = s.split(':').collect();
    let func = parse_func(parts.first().copied().unwrap_or(""))?;
    let addr = parts
        .get(1)
        .ok_or_else(|| "missing addr".to_string())
        .and_then(|a| a.parse::<u16>().map_err(|_| format!("invalid addr '{a}'")))?;
    let typ = parse_type(parts.get(2).ok_or_else(|| "missing type".to_string())?)?;
    let mut order = Order::Abcd;
    let mut bit = None;
    match (func, typ) {
        (Func::Coil | Func::Discrete, Type::Bool) => {
            if parts.len() > 3 {
                return Err("coil/discrete 不支持字节序/位号段（直接布尔）".to_string());
            }
        }
        (Func::Coil | Func::Discrete, _) => {
            return Err("coil/discrete 只接受 bool 类型（FC01/FC02 返回布尔）".to_string());
        }
        (Func::Holding | Func::Input, Type::Bool) => {
            return Err("holding/input 用 bit（寄存器位解析）而非 bool".to_string());
        }
        (Func::Holding | Func::Input, Type::Bit) => {
            let b = parts
                .get(3)
                .ok_or_else(|| "bit type requires bit number (0-15)".to_string())
                .and_then(|x| x.parse::<u8>().map_err(|_| format!("invalid bit '{x}'")))?;
            if b > 15 {
                return Err(format!("bit {b} out of range (0-15)"));
            }
            bit = Some(b);
        }
        (Func::Holding | Func::Input, Type::I16 | Type::U16) => {
            if parts.len() > 3 {
                return Err(format!(
                    "{} 不支持字节序段（16 位单寄存器）",
                    if typ == Type::I16 { "i16" } else { "u16" }
                ));
            }
        }
        (Func::Holding | Func::Input, Type::I32 | Type::U32 | Type::F32 | Type::F64) => {
            if let Some(o) = parts.get(3) {
                order = parse_order(o)?;
            }
        }
    }
    Ok(PointSpec {
        func,
        addr,
        typ,
        order,
        bit,
    })
}

// ========== 字节序解码（纯函数，仅 holding/input 的多字节类型用） ==========

/// 32 位（4 字节 `[A,B,C,D]`，`A=reg0 高字节`）的字节置换索引。4 种均为对合置换。
fn perm4(o: Order) -> [usize; 4] {
    match o {
        Order::Abcd => [0, 1, 2, 3],
        Order::Cdab => [2, 3, 0, 1],
        Order::Badc => [1, 0, 3, 2],
        Order::Dcba => [3, 2, 1, 0],
    }
}

/// 64 位（8 字节 `[A..H]`）的字节置换索引。
/// ABCD/DCBA 严格定义；CDAB/BADC 按"字反序/字内字节反序"扩展，单测 round-trip 锁定。
fn perm8(o: Order) -> [usize; 8] {
    match o {
        Order::Abcd => [0, 1, 2, 3, 4, 5, 6, 7],
        Order::Dcba => [7, 6, 5, 4, 3, 2, 1, 0],
        Order::Cdab => [6, 7, 4, 5, 2, 3, 0, 1],
        Order::Badc => [1, 0, 3, 2, 5, 4, 7, 6],
    }
}

fn decode_u32(regs: [u16; 2], o: Order) -> u32 {
    let bytes: [u8; 4] = std::array::from_fn(|i| {
        let b = regs[i / 2].to_be_bytes();
        b[i % 2]
    });
    let idx = perm4(o);
    u32::from_be_bytes(std::array::from_fn(|i| bytes[idx[i]]))
}

fn decode_u64(regs: [u16; 4], o: Order) -> u64 {
    let bytes: [u8; 8] = std::array::from_fn(|i| {
        let b = regs[i / 2].to_be_bytes();
        b[i % 2]
    });
    let idx = perm8(o);
    u64::from_be_bytes(std::array::from_fn(|i| bytes[idx[i]]))
}

fn decode_f32(regs: [u16; 2], o: Order) -> f32 {
    f32::from_bits(decode_u32(regs, o))
}

fn decode_f64(regs: [u16; 4], o: Order) -> f64 {
    f64::from_bits(decode_u64(regs, o))
}

/// 从 holding 寄存器值取指定位。
fn extract_bit(reg: u16, bit: u8) -> bool {
    (reg >> bit) & 1 == 1
}

/// 一次读取的返回缓冲（coil/discrete 为布尔序列，holding/input 为寄存器序列）。
#[derive(Debug)]
enum Buf {
    /// coil/discrete（FC01/02）。
    Bits(Vec<bool>),
    /// holding/input（FC03/04）。
    Regs(Vec<u16>),
}

/// 按点位规约从区间缓冲解码一个值。`base` 为区间起始地址。
/// 缓冲区不足、bit 点缺位号、或 holding/input 误配 bool 时返回 None。
fn decode_point(p: &PointSpec, buf: &Buf, base: u16) -> Option<SampleValue> {
    let off = (p.addr - base) as usize;
    match buf {
        Buf::Bits(bits) => bits.get(off).copied().map(SampleValue::Bool),
        Buf::Regs(regs) => {
            let w = p.width() as usize;
            if off + w > regs.len() {
                return None;
            }
            Some(match p.typ {
                Type::Bit => {
                    let b = p.bit?;
                    SampleValue::Bool(extract_bit(regs[off], b))
                }
                // parse_point 已保证 holding/input 不配 bool；此分支不可达，防御返回 None。
                Type::Bool => return None,
                Type::I16 => SampleValue::Int(regs[off] as i16 as i64),
                Type::U16 => SampleValue::Int(regs[off] as i64),
                Type::I32 => {
                    SampleValue::Int(decode_u32([regs[off], regs[off + 1]], p.order) as i32 as i64)
                }
                Type::U32 => {
                    SampleValue::Int(decode_u32([regs[off], regs[off + 1]], p.order) as i64)
                }
                Type::F32 => {
                    SampleValue::Float(decode_f32([regs[off], regs[off + 1]], p.order) as f64)
                }
                Type::F64 => SampleValue::Float(decode_f64(
                    [regs[off], regs[off + 1], regs[off + 2], regs[off + 3]],
                    p.order,
                )),
            })
        }
    }
}

// ========== 写方向编码（纯函数） ==========

fn encode_u32(v: u32, o: Order) -> [u16; 2] {
    let vp = v.to_be_bytes();
    let idx = perm4(o);
    let b: [u8; 4] = std::array::from_fn(|i| vp[idx[i]]);
    [
        u16::from_be_bytes([b[0], b[1]]),
        u16::from_be_bytes([b[2], b[3]]),
    ]
}

fn encode_u64(v: u64, o: Order) -> [u16; 4] {
    let vp = v.to_be_bytes();
    let idx = perm8(o);
    let b: [u8; 8] = std::array::from_fn(|i| vp[idx[i]]);
    [
        u16::from_be_bytes([b[0], b[1]]),
        u16::from_be_bytes([b[2], b[3]]),
        u16::from_be_bytes([b[4], b[5]]),
        u16::from_be_bytes([b[6], b[7]]),
    ]
}

/// 写载荷（coil 为布尔序列，holding 为寄存器序列）。
#[derive(Debug, PartialEq, Eq)]
enum WritePayload {
    /// coil（FC05/0F）。
    Coils(Vec<bool>),
    /// holding 寄存器（FC06/16）。
    Regs(Vec<u16>),
}

/// 将写值按点位规约编码为待写载荷。
///
/// coil → `Coils`；holding bit → `Regs`（单寄存器，RMW：`current_reg` 为调用方先读到的当前值）；
/// holding 多字节 → `Regs`。discrete/input 在此之前应被拒绝（只读）。
fn encode_value(
    sv: &SampleValue,
    p: &PointSpec,
    current_reg: Option<u16>,
) -> Result<WritePayload, String> {
    let as_f = || match sv {
        SampleValue::Float(f) => *f,
        SampleValue::Int(i) => *i as f64,
        SampleValue::Bool(b) => u8::from(*b) as f64,
        SampleValue::Str(s) => s.parse().unwrap_or(0.0),
    };
    let as_i = || match sv {
        SampleValue::Int(i) => *i,
        SampleValue::Float(f) => *f as i64,
        SampleValue::Bool(b) => u8::from(*b) as i64,
        SampleValue::Str(s) => s.parse().unwrap_or(0),
    };
    let as_b = || match sv {
        SampleValue::Bool(b) => *b,
        SampleValue::Int(i) => *i != 0,
        SampleValue::Float(f) => *f != 0.0,
        SampleValue::Str(s) => s.eq_ignore_ascii_case("true"),
    };
    match (p.func, p.typ) {
        (Func::Coil, Type::Bool) => Ok(WritePayload::Coils(vec![as_b()])),
        (Func::Discrete | Func::Input, _) => Err("discrete/input 只读，不可写".to_string()),
        (Func::Coil, _) => Err("coil 只接受 bool 类型".to_string()),
        (Func::Holding, Type::Bool) => Err("holding 用 bit（寄存器位）而非 bool".to_string()),
        (Func::Holding, Type::Bit) => {
            let cur = current_reg.ok_or_else(|| "bit 写需要当前寄存器值（RMW）".to_string())?;
            let b = p.bit.ok_or_else(|| "bit 点缺少 bit 号".to_string())?;
            Ok(WritePayload::Regs(vec![if as_b() {
                cur | (1 << b)
            } else {
                cur & !(1 << b)
            }]))
        }
        (Func::Holding, Type::I16) => Ok(WritePayload::Regs(vec![as_i() as i16 as u16])),
        (Func::Holding, Type::U16) => Ok(WritePayload::Regs(vec![as_i() as u16])),
        (Func::Holding, Type::I32) => Ok(WritePayload::Regs(
            encode_u32(as_i() as i32 as u32, p.order).to_vec(),
        )),
        (Func::Holding, Type::U32) => Ok(WritePayload::Regs(
            encode_u32(as_i() as u32, p.order).to_vec(),
        )),
        (Func::Holding, Type::F32) => Ok(WritePayload::Regs(
            encode_u32((as_f() as f32).to_bits(), p.order).to_vec(),
        )),
        (Func::Holding, Type::F64) => Ok(WritePayload::Regs(
            encode_u64(as_f().to_bits(), p.order).to_vec(),
        )),
    }
}

// ========== 批量合并（纯函数） ==========

/// 合并后的连续读取区间。
#[derive(Debug)]
struct Region<'a> {
    func: Func,
    start: u16,
    end: u16,
    points: Vec<(&'a str, &'a PointSpec)>,
}

impl Region<'_> {
    /// 区间地址单元数（`end - start + 1`；coil/discrete 为 bit 数，holding/input 为寄存器数）。
    fn len(&self) -> u16 {
        self.end - self.start + 1
    }
}

/// 按 func 分组 + 组内按 addr 排序 + 贪心合并相邻/近邻点为读取区间。
///
/// 4 种 func 各自独立地址空间，必分 4 组。组内合并条件：`addr <= end + gap_threshold + 1`
/// （最大单空洞）且新区间长 < `max_regs`。同 addr 多 bit/多 bool 折叠进同一区间。
fn merge_regions<'a>(pairs: &'a [(String, PointSpec)], gap: u16, max_regs: u16) -> Vec<Region<'a>> {
    let mut out = Vec::new();
    for func in [Func::Coil, Func::Discrete, Func::Holding, Func::Input] {
        let mut group: Vec<&(String, PointSpec)> =
            pairs.iter().filter(|(_, p)| p.func == func).collect();
        group.sort_by_key(|(_, p)| p.addr);
        let mut cur: Option<Region<'a>> = None;
        for (field, p) in group {
            let end_p = p.addr.saturating_add(p.width() - 1);
            let merged = matches!(&cur, Some(r)
                if p.addr <= r.end.saturating_add(gap).saturating_add(1)
                    && end_p.saturating_sub(r.start) < max_regs);
            if merged {
                let r = cur.as_mut().expect("checked Some above");
                r.end = r.end.max(end_p);
                r.points.push((field.as_str(), p));
            } else {
                if let Some(r) = cur.take() {
                    out.push(r);
                }
                cur = Some(Region {
                    func,
                    start: p.addr,
                    end: end_p,
                    points: vec![(field.as_str(), p)],
                });
            }
        }
        if let Some(r) = cur.take() {
            out.push(r);
        }
    }
    out
}

/// JSON → SampleValue（写命令 value 字段）。与 opcua_client 同实现。
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

// ========== 连接 ==========

/// 建立到 Modbus 从站的连接（按 transport 选 TCP / RTU(serial/over-tcp/over-udp)）。
/// 四种传输统一返回 `Context`，下游采集/写逻辑同构。每次新建 Context 都 `set_slave`（drop 后丢失）。
async fn connect(cfg: &ModbusCfg) -> Result<Context, String> {
    let mut ctx = match cfg.transport {
        Transport::Tcp => {
            let addr = parse_addr(&cfg.endpoint)?;
            tcp::connect(addr)
                .await
                .map_err(|e| format!("connect tcp {}: {e}", cfg.endpoint))?
        }
        Transport::RtuTcp => {
            let addr = parse_addr(&cfg.endpoint)?;
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| format!("connect rtu-tcp {}: {e}", cfg.endpoint))?;
            rtu::attach_slave(stream, Slave(cfg.slave_id))
        }
        Transport::RtuUdp => {
            let addr = parse_addr(&cfg.endpoint)?;
            let sock = UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|e| format!("bind rtu-udp: {e}"))?;
            sock.connect(addr)
                .await
                .map_err(|e| format!("connect rtu-udp {}: {e}", cfg.endpoint))?;
            rtu::attach_slave(UdpIo::new(sock), Slave(cfg.slave_id))
        }
        Transport::RtuSerial => {
            let port = cfg
                .serial_port
                .as_ref()
                .ok_or_else(|| "rtu_serial 缺 serial_port".to_string())?;
            let builder = tokio_serial::new(port, cfg.baud_rate)
                .data_bits(map_data_bits(cfg.data_bits))
                .parity(map_parity(cfg.parity))
                .stop_bits(map_stop_bits(cfg.stop_bits))
                .flow_control(map_flow(cfg.flow_control));
            let stream = builder
                .open_native_async()
                .map_err(|e| format!("open serial {port}: {e}"))?;
            rtu::attach_slave(stream, Slave(cfg.slave_id))
        }
    };
    // 统一 set_slave：RTU 在 attach_slave 已带 slave，再设无害；TCP 必须设。
    ctx.set_slave(Slave(cfg.slave_id));
    Ok(ctx)
}

/// 解析 `ip:port` → `SocketAddr`（预解析避免 DNS 边界，与原 TCP 实现一致）。
fn parse_addr(endpoint: &str) -> Result<SocketAddr, String> {
    endpoint
        .parse()
        .map_err(|e| format!("invalid endpoint '{endpoint}': {e}"))
}

/// `data_bits`(u8) → serialport `DataBits`（parse_cfg 已校验 5/6/7/8）。
fn map_data_bits(b: u8) -> DataBits {
    match b {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    }
}

/// `ParityKind` → serialport `Parity`。
fn map_parity(p: ParityKind) -> Parity {
    match p {
        ParityKind::None => Parity::None,
        ParityKind::Even => Parity::Even,
        ParityKind::Odd => Parity::Odd,
    }
}

/// `stop_bits`(u8) → serialport `StopBits`（parse_cfg 已校验 1/2）。
fn map_stop_bits(b: u8) -> StopBits {
    match b {
        2 => StopBits::Two,
        _ => StopBits::One,
    }
}

/// `FlowKind` → serialport `FlowControl`。
fn map_flow(f: FlowKind) -> FlowControl {
    match f {
        FlowKind::None => FlowControl::None,
        FlowKind::Software => FlowControl::Software,
        FlowKind::Hardware => FlowControl::Hardware,
    }
}

/// UDP RTU 传输适配器：把 connected `UdpSocket` 包成 `AsyncRead + AsyncWrite`，供 `rtu::attach_slave`。
///
/// RTU client "写一帧→读一帧" 的交互正好对应 UDP "攒帧→发一个 datagram→收一个 datagram"，
/// 1:1 请求-响应，datagram 边界天然保帧，无流式粘包/分帧歧义。
///
/// - `poll_write`：攒字节到 `write_buf`（不实际发，等 flush）——对应 RTU client 编码一帧的多次 write；
/// - `poll_flush`：`write_buf` 整体作为一个 datagram `poll_send`——一个 RTU 帧 = 一个 UDP 包；
/// - `poll_read`：先消费 `read_buf` 剩余（上次 recv 未读完），否则 `poll_recv` 收一个整 datagram 入队再按容量吐出。
///
/// 内部用 `UdpSocket::poll_send`/`poll_recv`（cancel-safe），`UdpIo` 字段均为非自引用故 `Unpin`。
struct UdpIo {
    /// 已 connect(peer) 的 UDP socket。
    sock: UdpSocket,
    /// 待 flush 发送的积压字节（攒一帧）。
    write_buf: Vec<u8>,
    /// 上次 recv 到但未消费完的字节（UDP 一次 recv = 整 datagram，poll_read 分批吐出）。
    read_buf: VecDeque<u8>,
}

impl UdpIo {
    /// 包一个已 `connect(peer)` 的 `UdpSocket`。
    fn new(sock: UdpSocket) -> Self {
        Self {
            sock,
            write_buf: Vec::new(),
            read_buf: VecDeque::new(),
        }
    }
}

impl AsyncRead for UdpIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // 先消费 read_buf 剩余（上次 recv 的 datagram 未读完部分）。
        if !this.read_buf.is_empty() {
            drain_into(this, buf);
            return Poll::Ready(Ok(()));
        }
        // read_buf 空：recv 一个新 datagram（以太网 MTU 1500 足够单 RTU 帧 ≤256B）。
        const MAX_DGRAM: usize = 1500;
        let mut tmp = [0u8; MAX_DGRAM];
        let mut rb = ReadBuf::new(&mut tmp);
        match this.sock.poll_recv(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                this.read_buf.extend(rb.filled().iter().copied());
                drain_into(this, buf);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for UdpIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.write_buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        while !this.write_buf.is_empty() {
            // poll_send 理论整包发（UDP），但 API 返回字节数，循环 drain 保险。
            match this.sock.poll_send(cx, this.write_buf.as_slice()) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::other("udp poll_send wrote 0 bytes")));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_buf.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        // UDP 无连接态可关；清空 write_buf（丢弃未发数据，失联重连语义）。
        self.get_mut().write_buf.clear();
        Poll::Ready(Ok(()))
    }
}

/// 从 `read_buf` 队首按 `buf` 剩余容量吐字节（UdpIo::poll_read 复用）。
fn drain_into(this: &mut UdpIo, buf: &mut ReadBuf<'_>) {
    let n = buf.remaining().min(this.read_buf.len());
    // VecDeque 非连续，drain 收集成切片喂 `put_slice`（ReadBuf 无逐字节 put_u8）。
    let chunk: Vec<u8> = this.read_buf.drain(..n).collect();
    buf.put_slice(&chunk);
}

/// `select!` 事件。把事件类型与 `Context` 的 `&mut` 借用解耦：modbus 的 read/write 取
/// `&mut self`（不像 opcua 的 `Arc<Session>` 不可变借用），若在 `select!` 分支内直接
/// `&mut session`，两分支会编译期 borrow 冲突。故 select 只产事件，`&mut session` 在
/// `match evt` 单分支内完成。
enum Evt {
    Tick,
    Cmd(Option<CtlMessage>),
}

#[async_trait]
impl Source for ModbusClient {
    fn name(&self) -> &'static str {
        "modbusClient"
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
                tracing::warn!("[modbusClient] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // 预解析所有设备的 (devid, [(field, PointSpec)])，采集循环内不再重复 parse。
        // 解析失败的点位 warn 丢弃，不阻断其余点位。
        let devices: Vec<(String, Vec<(String, PointSpec)>)> = inst
            .devices
            .iter()
            .map(|dev| {
                let pairs: Vec<(String, PointSpec)> = dev
                    .children
                    .iter()
                    .filter_map(|c| {
                        parse_point(&c.value)
                            .map(|p| {
                                let field = if c.label.is_empty() {
                                    c.value.clone()
                                } else {
                                    c.label.clone()
                                };
                                (field, p)
                            })
                            .map_err(|e| {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} dev {} bad address '{}': {}",
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
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut session: Option<Context> = None;

        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    if m.cmd == "write" {
                        handle_write(&mut session, &m.data).await;
                    } else {
                        tracing::info!("[modbusClient] inst {inst_id} unknown cmd: {}", m.cmd);
                    }
                }
                Evt::Cmd(None) => break,
                Evt::Tick => {
                    if session.is_none() {
                        match connect(&cfg).await {
                            Ok(c) => {
                                tracing::info!(
                                    "[modbusClient] inst {inst_id} connected {} slave {}",
                                    cfg.endpoint,
                                    cfg.slave_id
                                );
                                session = Some(c);
                            }
                            Err(e) => {
                                tracing::warn!("[modbusClient] inst {inst_id} connect failed: {e}");
                                tokio::time::sleep(Duration::from_secs(cfg.reconnect_secs)).await;
                                continue;
                            }
                        }
                    }
                    let sess = match &mut session {
                        Some(s) => s,
                        None => continue,
                    };
                    for (devid, pairs) in &devices {
                        if pairs.is_empty() {
                            continue;
                        }
                        let regions = merge_regions(pairs, cfg.gap_threshold, cfg.max_regs);
                        let mut values = BTreeMap::new();
                        let mut read_failed = false;
                        for reg in &regions {
                            let len = reg.len();
                            // 双层 Result：外层 transport Error / 内层 ExceptionCode（协议异常）。
                            let res = match reg.func {
                                Func::Coil => sess
                                    .read_coils(reg.start, len)
                                    .await
                                    .map(|r| r.map(Buf::Bits)),
                                Func::Discrete => sess
                                    .read_discrete_inputs(reg.start, len)
                                    .await
                                    .map(|r| r.map(Buf::Bits)),
                                Func::Holding => sess
                                    .read_holding_registers(reg.start, len)
                                    .await
                                    .map(|r| r.map(Buf::Regs)),
                                Func::Input => sess
                                    .read_input_registers(reg.start, len)
                                    .await
                                    .map(|r| r.map(Buf::Regs)),
                            };
                            match res {
                                Ok(Ok(buf)) => {
                                    for (field, p) in &reg.points {
                                        if let Some(v) = decode_point(p, &buf, reg.start) {
                                            values.insert((*field).to_string(), v);
                                        }
                                    }
                                }
                                Ok(Err(exc)) => {
                                    // 协议异常（连接正常，如 IllegalDataAddress）：本次 sample
                                    // 不完整，跳过此设备；不拆 session。
                                    tracing::warn!(
                                        "[modbusClient] inst {inst_id} read {}+{} exception: {exc:?}",
                                        reg.start,
                                        len
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "[modbusClient] inst {inst_id} read {}+{} failed: {e:?}",
                                        reg.start,
                                        len
                                    );
                                    read_failed = true;
                                    break;
                                }
                            }
                        }
                        if read_failed {
                            // 读失败可能因断连：drop Context，下 tick 重连。
                            session = None;
                            break;
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
                }
            }
        }
        // Context drop 即关连接（tokio-modbus 无显式 disconnect API）。
        tracing::info!("[modbusClient] inst {inst_id} exited");
    }
}

/// 处理 write 命令：`data` = JSON `{"address":"holding:0:f32:ABCD","value":12.34}`。
///
/// coil → FC05/0F；holding u16/i16/bit → FC06，i32/u32/f32/f64 → FC16，bit 走 RMW；
/// discrete/input 只读拒绝。写失败仅 warn，不拆 session（读 tick 兜底发现断连）。
async fn handle_write(session: &mut Option<Context>, data: &str) {
    let req: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[modbusClient] write bad json: {e}");
            return;
        }
    };
    let addr = match req.get("address").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => {
            tracing::warn!("[modbusClient] write missing address");
            return;
        }
    };
    let p = match parse_point(addr) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[modbusClient] write bad address '{addr}': {e}");
            return;
        }
    };
    if p.func.is_readonly() {
        tracing::warn!(
            "[modbusClient] write rejected: {}(FC0{}) 只读",
            addr_func_name(p.func),
            addr_fc(p.func)
        );
        return;
    }
    let value = match req.get("value") {
        Some(v) => json_to_sample_value(v),
        None => {
            tracing::warn!("[modbusClient] write missing value");
            return;
        }
    };
    let sess = match session {
        Some(s) => s,
        None => {
            tracing::warn!("[modbusClient] write ignored: not connected");
            return;
        }
    };
    // 仅 holding bit 写需 RMW（coil 直接写单 coil，无需读当前值）。
    let current_reg = if p.func == Func::Holding && p.typ == Type::Bit {
        match sess.read_holding_registers(p.addr, 1).await {
            Ok(Ok(buf)) => buf.first().copied(),
            Ok(Err(exc)) => {
                tracing::warn!("[modbusClient] bit write RMW read exception: {exc:?}");
                return;
            }
            Err(e) => {
                tracing::warn!("[modbusClient] bit write RMW read failed: {e:?}");
                return;
            }
        }
    } else {
        None
    };
    let payload = match encode_value(&value, &p, current_reg) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[modbusClient] write encode failed: {e}");
            return;
        }
    };
    let result = match payload {
        WritePayload::Coils(c) => {
            if c.len() == 1 {
                sess.write_single_coil(p.addr, c[0]).await
            } else {
                sess.write_multiple_coils(p.addr, &c).await
            }
        }
        WritePayload::Regs(r) => {
            if r.len() == 1 {
                sess.write_single_register(p.addr, r[0]).await
            } else {
                sess.write_multiple_registers(p.addr, &r).await
            }
        }
    };
    match result {
        Ok(Ok(())) => {}
        Ok(Err(exc)) => tracing::warn!("[modbusClient] write exception: {exc:?}"),
        Err(e) => tracing::warn!("[modbusClient] write failed: {e:?}"),
    }
}

/// func 的可读名（日志用）。
fn addr_func_name(f: Func) -> &'static str {
    match f {
        Func::Coil => "coil",
        Func::Discrete => "discrete",
        Func::Holding => "holding",
        Func::Input => "input",
    }
}

/// func 的功能码号（日志用）。
fn addr_fc(f: Func) -> u8 {
    match f {
        Func::Coil => 1,
        Func::Discrete => 2,
        Func::Holding => 3,
        Func::Input => 4,
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "modbusClient",
        create: || Box::new(ModbusClient),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_point_accepts_standard_forms() {
        assert_eq!(
            parse_point("holding:0:f32:ABCD").unwrap(),
            PointSpec {
                func: Func::Holding,
                addr: 0,
                typ: Type::F32,
                order: Order::Abcd,
                bit: None,
            }
        );
        assert_eq!(
            parse_point("holding:10:bit:3").unwrap(),
            PointSpec {
                func: Func::Holding,
                addr: 10,
                typ: Type::Bit,
                order: Order::Abcd,
                bit: Some(3),
            }
        );
        assert_eq!(parse_point("input:5:u16").unwrap().typ, Type::U16);
        assert_eq!(parse_point("holding:0:f32").unwrap().order, Order::Abcd);
        assert_eq!(
            parse_point("holding:0:f32:cdab").unwrap().order,
            Order::Cdab
        );
        assert_eq!(parse_point("holding:0:f32:le").unwrap().order, Order::Dcba);
        // coil/discrete bool
        assert_eq!(parse_point("coil:0:bool").unwrap().func, Func::Coil);
        assert_eq!(parse_point("discrete:5:bool").unwrap().func, Func::Discrete);
        assert_eq!(parse_point("fc01:0:bool").unwrap().func, Func::Coil);
    }

    #[test]
    fn parse_point_rejects_invalid() {
        assert!(parse_point("holding:0:bit").is_err(), "bit 缺位号");
        assert!(parse_point("holding:0:bit:16").is_err(), "bit 越界");
        assert!(
            parse_point("holding:0:u16:ABCD").is_err(),
            "u16 不支持字节序"
        );
        assert!(
            parse_point("holding:0:i16:ABCD").is_err(),
            "i16 不支持字节序"
        );
        assert!(parse_point("holding:0:f32:XYZW").is_err(), "非法 order");
        assert!(parse_point("holding:0:badtype").is_err(), "非法 type");
        assert!(parse_point("holding:x:u16").is_err(), "非法 addr");
        assert!(parse_point("holding:0").is_err(), "缺 type");
        // func-type 组合校验
        assert!(parse_point("coil:0:i16").is_err(), "coil 不接受 i16");
        assert!(
            parse_point("coil:0:bool:ABCD").is_err(),
            "coil 不支持字节序段"
        );
        assert!(
            parse_point("holding:0:bool").is_err(),
            "holding 不接受 bool（用 bit）"
        );
        assert!(parse_point("input:0:bool").is_err(), "input 不接受 bool");
    }

    #[test]
    fn width_matches_type() {
        assert_eq!(parse_point("coil:0:bool").unwrap().width(), 1);
        assert_eq!(parse_point("holding:0:bit:0").unwrap().width(), 1);
        assert_eq!(parse_point("holding:0:u16").unwrap().width(), 1);
        assert_eq!(parse_point("holding:0:f32").unwrap().width(), 2);
        assert_eq!(parse_point("holding:0:f64").unwrap().width(), 4);
    }

    #[test]
    fn parse_cfg_tcp_defaults_and_clamps() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.transport, Transport::Tcp);
        assert_eq!(cfg.endpoint, "127.0.0.1:502");
        assert_eq!(cfg.serial_port, None);
        assert_eq!(cfg.slave_id, 1);
        assert_eq!(cfg.period, 3);
        assert_eq!(cfg.gap_threshold, 16);
        assert_eq!(cfg.max_regs, 100);
        assert_eq!(cfg.reconnect_secs, 2);

        let cfg2 =
            parse_cfg(&serde_json::json!({"endpoint":"x","period":0,"max_regs":9999})).unwrap();
        assert_eq!(cfg2.period, 1, "period 0 clamp 1");
        assert_eq!(cfg2.max_regs, 125, "max_regs clamp 125");
    }

    #[test]
    fn parse_cfg_rtu_serial_defaults_and_params() {
        let cfg =
            parse_cfg(&serde_json::json!({"kind":"rtu_serial","serial_port":"COM3"})).unwrap();
        assert_eq!(cfg.transport, Transport::RtuSerial);
        assert_eq!(cfg.serial_port.as_deref(), Some("COM3"));
        assert_eq!(cfg.endpoint, "", "rtu_serial 无 endpoint");
        // 工业默认 9600/8N1/NONE。
        assert_eq!(cfg.baud_rate, 9600);
        assert_eq!(cfg.data_bits, 8);
        assert_eq!(cfg.parity, ParityKind::None);
        assert_eq!(cfg.stop_bits, 1);
        assert_eq!(cfg.flow_control, FlowKind::None);

        let cfg3 = parse_cfg(&serde_json::json!({
            "kind":"rtu_serial","serial_port":"/dev/ttyUSB0","baud_rate":115200,
            "data_bits":7,"parity":"even","stop_bits":2,"flow_control":"hardware"
        }))
        .unwrap();
        assert_eq!(cfg3.baud_rate, 115200);
        assert_eq!(cfg3.data_bits, 7);
        assert_eq!(cfg3.parity, ParityKind::Even);
        assert_eq!(cfg3.stop_bits, 2);
        assert_eq!(cfg3.flow_control, FlowKind::Hardware);
    }

    #[test]
    fn parse_cfg_rtu_tcp_and_rtu_udp() {
        let cfg =
            parse_cfg(&serde_json::json!({"kind":"rtu_tcp","endpoint":"192.168.1.5:502"})).unwrap();
        assert_eq!(cfg.transport, Transport::RtuTcp);
        assert_eq!(cfg.endpoint, "192.168.1.5:502");

        let cfg = parse_cfg(
            &serde_json::json!({"kind":"rtu_udp","endpoint":"10.0.0.7:502","slave_id":2}),
        )
        .unwrap();
        assert_eq!(cfg.transport, Transport::RtuUdp);
        assert_eq!(cfg.slave_id, 2);
    }

    #[test]
    fn parse_cfg_rejects_invalid_kind_and_params() {
        assert!(
            parse_cfg(&serde_json::json!({})).is_err(),
            "tcp 缺 endpoint"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_tcp"})).is_err(),
            "rtu_tcp 缺 endpoint"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_serial"})).is_err(),
            "rtu_serial 缺 serial_port"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"ascii","endpoint":"x"})).is_err(),
            "非法 kind（ASCII 后置）"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","data_bits":9}))
                .is_err(),
            "非法 data_bits"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","stop_bits":3}))
                .is_err(),
            "非法 stop_bits"
        );
        assert!(
            parse_cfg(
                &serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","parity":"mark"})
            )
            .is_err(),
            "非法 parity"
        );
        assert!(
            parse_cfg(
                &serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","flow_control":"auto"})
            )
            .is_err(),
            "非法 flow_control"
        );
    }

    #[tokio::test]
    async fn udpio_keeps_datagram_boundary() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 一对 loopback UDP：UdpIo(client, connected) → server。
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server_addr).await.unwrap();
        let mut io = UdpIo::new(client);

        // 两帧分别写 + flush：每帧应是独立 datagram，不粘包。
        let frame1: &[u8] = b"\x01\x03\x00\x00\x00\x01";
        let frame2: &[u8] = b"\x01\x03\x02\x12\x34";
        io.write_all(frame1).await.unwrap();
        io.flush().await.unwrap();
        io.write_all(frame2).await.unwrap();
        io.flush().await.unwrap();

        let mut buf1 = [0u8; 64];
        let (n1, client_addr) = server.recv_from(&mut buf1).await.unwrap();
        assert_eq!(&buf1[..n1], frame1, "第一帧独立 datagram");
        let mut buf2 = [0u8; 64];
        let (n2, _) = server.recv_from(&mut buf2).await.unwrap();
        assert_eq!(&buf2[..n2], frame2, "第二帧独立 datagram，未与第一帧粘包");

        // server 回一个 datagram，UdpIo 应读回完整字节。
        let resp: &[u8] = b"\x01\x03\x02\xAB\xCD";
        server.send_to(resp, client_addr).await.unwrap();
        let mut rbuf = [0u8; 64];
        let rn = io.read(&mut rbuf).await.unwrap();
        assert_eq!(&rbuf[..rn], resp, "回包完整读取");
    }

    #[tokio::test]
    async fn udpio_read_chunked_from_one_datagram() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 验证 read_buf：一个 datagram 被多次小容量 read 分块消费，顺序保持。
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server_addr).await.unwrap();
        let mut io = UdpIo::new(client);

        // 先握手让 server 拿到 client_addr。
        io.write_all(b"hi").await.unwrap();
        io.flush().await.unwrap();
        let mut tmp = [0u8; 16];
        let (_, client_addr) = server.recv_from(&mut tmp).await.unwrap();

        // server 发 8 字节 datagram；UdpIo poll_read 整包入 read_buf，再分批吐出。
        server.send_to(b"01234567", client_addr).await.unwrap();
        let mut a = [0u8; 2];
        assert_eq!(io.read(&mut a).await.unwrap(), 2);
        assert_eq!(&a, b"01");
        assert_eq!(io.read(&mut a).await.unwrap(), 2);
        assert_eq!(&a, b"23");
        let mut b4 = [0u8; 4];
        assert_eq!(io.read(&mut b4).await.unwrap(), 4);
        assert_eq!(&b4, b"4567");
    }

    #[test]
    fn extract_bit_works() {
        let reg = 0b0000_0000_0000_1010u16; // bit1, bit3
        assert!(!extract_bit(reg, 0));
        assert!(extract_bit(reg, 1));
        assert!(!extract_bit(reg, 2));
        assert!(extract_bit(reg, 3));
    }

    #[test]
    fn decode_u32_roundtrip_all_orders() {
        let orders = [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba];
        for o in orders {
            for v in [
                0x00000000u32,
                0x41480000,
                0xFFFFFFFF,
                0x12345678,
                0x80000001,
            ] {
                let regs = encode_u32(v, o);
                assert_eq!(decode_u32(regs, o), v, "order {o:?} v {v:#010x} round-trip");
            }
        }
    }

    #[test]
    fn decode_f32_known_value_abcd() {
        // f32 12.5 = 0x41480000，ABCD → regs [0x4148, 0x0000]
        assert_eq!(
            encode_u32(12.5_f32.to_bits(), Order::Abcd),
            [0x4148, 0x0000]
        );
        assert!((decode_f32([0x4148, 0x0000], Order::Abcd) - 12.5_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn decode_i32_negative() {
        let v: i32 = -1;
        for o in [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba] {
            let regs = encode_u32(v as u32, o);
            assert_eq!(decode_u32(regs, o) as i32, -1, "i32 -1 order {o:?}");
        }
    }

    #[test]
    fn decode_u64_roundtrip_all_orders() {
        let orders = [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba];
        for o in orders {
            for v in [0u64, 0x4024000000000000, u64::MAX, 0x123456789ABCDEF0] {
                let regs = encode_u64(v, o);
                assert_eq!(decode_u64(regs, o), v, "f64 order {o:?} v {v:#018x}");
            }
        }
    }

    #[test]
    fn decode_point_from_regs_buf() {
        let p = parse_point("holding:0:f32:ABCD").unwrap();
        let buf = Buf::Regs(vec![0x4148, 0x0000]);
        assert!(
            matches!(decode_point(&p, &buf, 0), Some(SampleValue::Float(f)) if (f-12.5).abs() < 1e-6)
        );
        // bit@1:bit3 从 reg1=0x000A 取 bit3
        let pb = parse_point("holding:1:bit:3").unwrap();
        assert_eq!(
            decode_point(&pb, &Buf::Regs(vec![0x0000, 0x000A]), 0),
            Some(SampleValue::Bool(true))
        );
        // 缓冲区不足 → None
        assert_eq!(decode_point(&p, &Buf::Regs(vec![0x4148]), 0), None);
    }

    #[test]
    fn decode_point_from_bits_buf() {
        // coil/discrete 从 Bits buf 按 offset 取
        let p = parse_point("coil:2:bool").unwrap();
        assert_eq!(
            decode_point(&p, &Buf::Bits(vec![false, false, true, false]), 0),
            Some(SampleValue::Bool(true))
        );
        let pd = parse_point("discrete:0:bool").unwrap();
        assert_eq!(
            decode_point(&pd, &Buf::Bits(vec![true, false]), 0),
            Some(SampleValue::Bool(true))
        );
    }

    #[test]
    fn encode_value_coil_bool() {
        let p = parse_point("coil:0:bool").unwrap();
        assert_eq!(
            encode_value(&SampleValue::Bool(true), &p, None).unwrap(),
            WritePayload::Coils(vec![true])
        );
        assert_eq!(
            encode_value(&SampleValue::Int(1), &p, None).unwrap(),
            WritePayload::Coils(vec![true])
        );
        assert_eq!(
            encode_value(&SampleValue::Int(0), &p, None).unwrap(),
            WritePayload::Coils(vec![false])
        );
    }

    #[test]
    fn encode_value_holding_bit_rmw() {
        let p = parse_point("holding:0:bit:1").unwrap();
        // 当前 reg=0x0001（bit0=1），写 bit1=true → 0x0003
        let r = encode_value(&SampleValue::Bool(true), &p, Some(0x0001)).unwrap();
        assert_eq!(r, WritePayload::Regs(vec![0x0003]));
        // 写 bit1=false → 0x0001（bit1 清，bit0 不变）
        let r = encode_value(&SampleValue::Bool(false), &p, Some(0x0003)).unwrap();
        assert_eq!(r, WritePayload::Regs(vec![0x0001]));
        // bit 写缺 current_reg → Err
        assert!(encode_value(&SampleValue::Bool(true), &p, None).is_err());
    }

    #[test]
    fn encode_value_holding_f32_abcd() {
        let p = parse_point("holding:0:f32:ABCD").unwrap();
        let r = encode_value(&SampleValue::Float(12.5), &p, None).unwrap();
        assert_eq!(r, WritePayload::Regs(vec![0x4148, 0x0000]));
    }

    #[test]
    fn encode_value_rejects_readonly_and_bad_combo() {
        let pd = parse_point("discrete:0:bool").unwrap();
        assert!(
            encode_value(&SampleValue::Bool(true), &pd, None).is_err(),
            "discrete 只读"
        );
        let pi = parse_point("input:0:u16").unwrap();
        assert!(
            encode_value(&SampleValue::Int(1), &pi, None).is_err(),
            "input 只读"
        );
    }

    #[test]
    fn merge_regions_empty() {
        assert!(merge_regions(&[], 16, 100).is_empty());
    }

    #[test]
    fn merge_regions_single() {
        let pairs = vec![("a".into(), parse_point("holding:0:f32").unwrap())];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, 0);
        assert_eq!(r[0].len(), 2);
    }

    #[test]
    fn merge_regions_adjacent_and_gap() {
        // 3×f32（0-5）+ 4×u16（6-9）连续 → 一个区间 0-9（len=10）
        let pairs = vec![
            ("a".into(), parse_point("holding:0:f32").unwrap()),
            ("b".into(), parse_point("holding:2:f32").unwrap()),
            ("c".into(), parse_point("holding:4:f32").unwrap()),
            ("d".into(), parse_point("holding:6:u16").unwrap()),
            ("e".into(), parse_point("holding:7:u16").unwrap()),
            ("f".into(), parse_point("holding:8:u16").unwrap()),
            ("g".into(), parse_point("holding:9:u16").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1, "连续应合并为 1 个区间");
        assert_eq!(r[0].len(), 10);
    }

    #[test]
    fn merge_regions_gap_split() {
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:100:u16").unwrap()),
        ];
        assert_eq!(merge_regions(&pairs, 16, 100).len(), 2);
        let r = merge_regions(&pairs, 100, 125);
        assert_eq!(r.len(), 1, "gap=100 应合并");
        assert_eq!(r[0].len(), 101);
    }

    #[test]
    fn merge_regions_max_regs_split() {
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:5:u16").unwrap()),
        ];
        assert_eq!(merge_regions(&pairs, 16, 10).len(), 1, "0-6 len=6 <10 合并");
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:50:u16").unwrap()),
        ];
        assert_eq!(
            merge_regions(&pairs, 100, 10).len(),
            2,
            "合并后 len=51>10 分裂"
        );
    }

    #[test]
    fn merge_regions_bit_and_f32_coexist() {
        let pairs = vec![
            ("f".into(), parse_point("holding:4:f32:ABCD").unwrap()),
            ("b".into(), parse_point("holding:5:bit:2").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].points.len(), 2);
    }

    #[test]
    fn merge_regions_multi_bit_fold() {
        let pairs = vec![
            ("b0".into(), parse_point("holding:5:bit:0").unwrap()),
            ("b1".into(), parse_point("holding:5:bit:1").unwrap()),
            ("b2".into(), parse_point("holding:5:bit:2").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 1, "3 bit 折叠为 1 寄存器");
        assert_eq!(r[0].points.len(), 3);
    }

    #[test]
    fn merge_regions_four_funcs_separate() {
        // 4 种 func 各自独立地址空间，必分 4 组
        let pairs = vec![
            ("c".into(), parse_point("coil:0:bool").unwrap()),
            ("d".into(), parse_point("discrete:0:bool").unwrap()),
            ("h".into(), parse_point("holding:0:u16").unwrap()),
            ("i".into(), parse_point("input:0:u16").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 4, "4 func 各一组");
        assert!(r.iter().any(|r| r.func == Func::Coil));
        assert!(r.iter().any(|r| r.func == Func::Discrete));
        assert!(r.iter().any(|r| r.func == Func::Holding));
        assert!(r.iter().any(|r| r.func == Func::Input));
    }

    #[test]
    fn merge_regions_coils_batch() {
        // 需求①变体：10 个连续 coil → 1 个区间（一次 read_coils(0,10)）
        let pairs: Vec<(String, PointSpec)> = (0..10)
            .map(|i| {
                (
                    format!("c{i}"),
                    parse_point(&format!("coil:{i}:bool")).unwrap(),
                )
            })
            .collect();
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].func, Func::Coil);
        assert_eq!(r[0].len(), 10);
        assert_eq!(r[0].points.len(), 10);
    }

    #[test]
    fn merge_regions_unsorted_input() {
        let pairs = vec![
            ("b".into(), parse_point("holding:5:u16").unwrap()),
            ("a".into(), parse_point("holding:0:u16").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, 0);
        assert_eq!(r[0].end, 5);
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
    fn func_is_readonly() {
        assert!(Func::Discrete.is_readonly());
        assert!(Func::Input.is_readonly());
        assert!(!Func::Coil.is_readonly());
        assert!(!Func::Holding.is_readonly());
    }
}
