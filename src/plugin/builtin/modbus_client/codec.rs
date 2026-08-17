//! 字节序解码/编码（纯函数），仅 holding/input 的多字节类型用。

use super::address::{Func, Order, PointSpec, Type};
use crate::types::SampleValue;

/// 32 位（4 字节 `[A,B,C,D]`，`A=reg0 高字节`）的字节置换索引。4 种均为对合置换。
pub(super) fn perm4(o: Order) -> [usize; 4] {
    match o {
        Order::Abcd => [0, 1, 2, 3],
        Order::Cdab => [2, 3, 0, 1],
        Order::Badc => [1, 0, 3, 2],
        Order::Dcba => [3, 2, 1, 0],
    }
}

/// 64 位（8 字节 `[A..H]`）的字节置换索引。
/// ABCD/DCBA 严格定义；CDAB/BADC 按"字反序/字内字节反序"扩展，单测 round-trip 锁定。
pub(super) fn perm8(o: Order) -> [usize; 8] {
    match o {
        Order::Abcd => [0, 1, 2, 3, 4, 5, 6, 7],
        Order::Dcba => [7, 6, 5, 4, 3, 2, 1, 0],
        Order::Cdab => [6, 7, 4, 5, 2, 3, 0, 1],
        Order::Badc => [1, 0, 3, 2, 5, 4, 7, 6],
    }
}

pub(super) fn decode_u32(regs: [u16; 2], o: Order) -> u32 {
    let bytes: [u8; 4] = std::array::from_fn(|i| {
        let b = regs[i / 2].to_be_bytes();
        b[i % 2]
    });
    let idx = perm4(o);
    u32::from_be_bytes(std::array::from_fn(|i| bytes[idx[i]]))
}

pub(super) fn decode_u64(regs: [u16; 4], o: Order) -> u64 {
    let bytes: [u8; 8] = std::array::from_fn(|i| {
        let b = regs[i / 2].to_be_bytes();
        b[i % 2]
    });
    let idx = perm8(o);
    u64::from_be_bytes(std::array::from_fn(|i| bytes[idx[i]]))
}

pub(super) fn decode_f32(regs: [u16; 2], o: Order) -> f32 {
    f32::from_bits(decode_u32(regs, o))
}

pub(super) fn decode_f64(regs: [u16; 4], o: Order) -> f64 {
    f64::from_bits(decode_u64(regs, o))
}

/// 从 holding 寄存器值取指定位。
pub(super) fn extract_bit(reg: u16, bit: u8) -> bool {
    (reg >> bit) & 1 == 1
}

/// 一次读取的返回缓冲（coil/discrete 为布尔序列，holding/input 为寄存器序列）。
#[derive(Debug)]
pub(super) enum Buf {
    /// coil/discrete（FC01/02）。
    Bits(Vec<bool>),
    /// holding/input（FC03/04）。
    Regs(Vec<u16>),
}

/// 按点位规约从区间缓冲解码一个值。`base` 为区间起始地址。
/// 缓冲区不足、bit 点缺位号、或 holding/input 误配 bool 时返回 None。
pub(super) fn decode_point(p: &PointSpec, buf: &Buf, base: u16) -> Option<SampleValue> {
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

pub(super) fn encode_u32(v: u32, o: Order) -> [u16; 2] {
    let vp = v.to_be_bytes();
    let idx = perm4(o);
    let b: [u8; 4] = std::array::from_fn(|i| vp[idx[i]]);
    [
        u16::from_be_bytes([b[0], b[1]]),
        u16::from_be_bytes([b[2], b[3]]),
    ]
}

pub(super) fn encode_u64(v: u64, o: Order) -> [u16; 4] {
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
pub(super) enum WritePayload {
    /// coil（FC05/0F）。
    Coils(Vec<bool>),
    /// holding 寄存器（FC06/16）。
    Regs(Vec<u16>),
}

/// 将写值按点位规约编码为待写载荷。
///
/// coil → `Coils`；holding bit → `Regs`（单寄存器，RMW：`current_reg` 为调用方先读到的当前值）；
/// holding 多字节 → `Regs`。discrete/input 在此之前应被拒绝（只读）。
pub(super) fn encode_value(
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
