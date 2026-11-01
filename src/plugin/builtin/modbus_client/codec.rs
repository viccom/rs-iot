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
    // C-4（2026-11-01 审查）：非数字字符串此前 `unwrap_or(0)` 静默向 PLC 写 0
    // （工业写危险）——改为显式 Err 拒写。bool 分支的 "true"/"false" 串不受影响
    // （as_b 自行匹配，不走数值路径）。
    let as_f = || -> Result<f64, String> {
        match sv {
            SampleValue::Float(f) => Ok(*f),
            SampleValue::Int(i) => Ok(*i as f64),
            SampleValue::UInt(u) => Ok(*u as f64),
            SampleValue::Bool(b) => Ok(u8::from(*b) as f64),
            SampleValue::Str(s) => s
                .parse()
                .map_err(|_| format!("value 非数字字符串 {s:?}（拒写，不做 0 兜底）")),
            SampleValue::Null => Err("value 为 Null（坏质量/无值），拒写".to_string()),
        }
    };
    let as_i = || -> Result<i64, String> {
        match sv {
            SampleValue::Int(i) => Ok(*i),
            SampleValue::UInt(u) => {
                i64::try_from(*u).map_err(|_| format!("value {u} 超出 i64 范围"))
            }
            // MB-1 续：`f as i64` 对 NaN 饱和成 0——等于把"无值"静默写成 0，与
            // `unwrap_or(0)` 同类危险。非有限浮点先拒。
            SampleValue::Float(f) => {
                if f.is_finite() {
                    Ok(*f as i64)
                } else {
                    Err(format!("value {f} 不是有限数（拒写 NaN/inf，不做 0 兜底）"))
                }
            }
            SampleValue::Bool(b) => Ok(u8::from(*b) as i64),
            SampleValue::Str(s) => s
                .parse()
                .map_err(|_| format!("value 非数字字符串 {s:?}（拒写，不做 0 兜底）")),
            SampleValue::Null => Err("value 为 Null（坏质量/无值），拒写".to_string()),
        }
    };
    // MB-2（2026-11-01 协议语义审查）：此前只认 "true"，"1"/"on"/"yes" 静默写
    // false（HMI 推按钮传 number 无碍，MQTT/外部 API 传字符串即触发）。现按
    // trim + 忽略大小写接受常见布尔写法，其余串显式 Err（不猜）。
    let as_b = || -> Result<bool, String> {
        match sv {
            SampleValue::Bool(b) => Ok(*b),
            SampleValue::Int(i) => Ok(*i != 0),
            SampleValue::UInt(u) => Ok(*u != 0),
            SampleValue::Float(f) => Ok(*f != 0.0),
            SampleValue::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => Ok(true),
                "false" | "0" | "off" | "no" => Ok(false),
                _ => Err(format!(
                    "value 字符串 {s:?} 不是布尔（true/false/1/0/on/off/yes/no）"
                )),
            },
            SampleValue::Null => Err("value 为 Null（坏质量/无值），拒写".to_string()),
        }
    };
    // MB-1（2026-11-01 协议语义审查）：`as` 回绕会把越界写值静默写给 PLC——
    // 实证 `-1 as u16 = 65535`、`70000 as u16 = 4464`、`40000 as i16 = -25536`、
    // `-5 as u32 = 4294967291`。全部改 checked 转换：越界 Err 并给出合法范围。
    let to_i16 = || -> Result<i16, String> {
        let i = as_i()?;
        i16::try_from(i)
            .map_err(|_| format!("value {i} 超出 i16 范围 -32768..=32767（拒写，不回绕）"))
    };
    let to_u16 = || -> Result<u16, String> {
        let i = as_i()?;
        u16::try_from(i).map_err(|_| format!("value {i} 超出 u16 范围 0..=65535（拒写，不回绕）"))
    };
    let to_i32 = || -> Result<i32, String> {
        let i = as_i()?;
        i32::try_from(i).map_err(|_| {
            format!("value {i} 超出 i32 范围 -2147483648..=2147483647（拒写，不回绕）")
        })
    };
    let to_u32 = || -> Result<u32, String> {
        let i = as_i()?;
        u32::try_from(i)
            .map_err(|_| format!("value {i} 超出 u32 范围 0..=4294967295（拒写，不回绕）"))
    };
    let to_f32 = || -> Result<f32, String> {
        let f = as_f()?;
        let v = f as f32;
        if v.is_finite() {
            Ok(v)
        } else {
            Err(format!(
                "value {f} 超出 f32 有限范围（约 ±3.4e38，拒写，不回绕为 inf）"
            ))
        }
    };
    let to_f64 = || -> Result<f64, String> {
        let f = as_f()?;
        if f.is_finite() {
            Ok(f)
        } else {
            Err(format!("value {f} 不是有限数（拒写 NaN/inf）"))
        }
    };
    match (p.func, p.typ) {
        (Func::Coil, Type::Bool) => Ok(WritePayload::Coils(vec![as_b()?])),
        (Func::Discrete | Func::Input, _) => Err("discrete/input 只读，不可写".to_string()),
        (Func::Coil, _) => Err("coil 只接受 bool 类型".to_string()),
        (Func::Holding, Type::Bool) => Err("holding 用 bit（寄存器位）而非 bool".to_string()),
        (Func::Holding, Type::Bit) => {
            let cur = current_reg.ok_or_else(|| "bit 写需要当前寄存器值（RMW）".to_string())?;
            let b = p.bit.ok_or_else(|| "bit 点缺少 bit 号".to_string())?;
            Ok(WritePayload::Regs(vec![if as_b()? {
                cur | (1 << b)
            } else {
                cur & !(1 << b)
            }]))
        }
        (Func::Holding, Type::I16) => Ok(WritePayload::Regs(vec![to_i16()? as u16])),
        (Func::Holding, Type::U16) => Ok(WritePayload::Regs(vec![to_u16()?])),
        (Func::Holding, Type::I32) => Ok(WritePayload::Regs(
            encode_u32(to_i32()? as u32, p.order).to_vec(),
        )),
        (Func::Holding, Type::U32) => {
            Ok(WritePayload::Regs(encode_u32(to_u32()?, p.order).to_vec()))
        }
        (Func::Holding, Type::F32) => Ok(WritePayload::Regs(
            encode_u32(to_f32()?.to_bits(), p.order).to_vec(),
        )),
        (Func::Holding, Type::F64) => Ok(WritePayload::Regs(
            encode_u64(to_f64()?.to_bits(), p.order).to_vec(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::builtin::modbus_client::address::parse_point;

    /// MB-1（2026-11-01 协议语义审查）：写值越界必须 Err——此前 `as` 回绕会把
    /// -1 写成 65535、70000 写成 4464、40000 写成 -25536、-5 写成 4294967291。
    #[test]
    fn encode_value_rejects_out_of_range_numbers() {
        let u16p = parse_point("holding:0:u16").unwrap();
        let i16p = parse_point("holding:0:i16").unwrap();
        let i32p = parse_point("holding:0:i32").unwrap();
        let u32p = parse_point("holding:0:u32").unwrap();
        for (label, p, v) in [
            ("-1→U16", &u16p, -1i64),
            ("70000→U16", &u16p, 70000),
            ("40000→I16", &i16p, 40000),
            ("-5→U32", &u32p, -5),
            ("-2147483649→I32", &i32p, -2_147_483_649),
            ("4294967296→U32", &u32p, 4_294_967_296),
        ] {
            let err = encode_value(&SampleValue::Int(v), p, None)
                .expect_err(&format!("{label} 必须 Err（不得回绕）"));
            assert!(
                err.contains("范围"),
                "{label} 错误信息应给出合法范围：{err}"
            );
        }
        // 边界内照常编码（不得被 checked 转换误拒）
        assert!(encode_value(&SampleValue::Int(65535), &u16p, None).is_ok());
        assert!(encode_value(&SampleValue::Int(-32768), &i16p, None).is_ok());
        assert!(encode_value(&SampleValue::Int(i64::from(i32::MIN)), &i32p, None).is_ok());
        assert!(encode_value(&SampleValue::Int(i64::from(u32::MAX)), &u32p, None).is_ok());
    }

    /// MB-1 续：浮点写值必须有限且在 f32 可表示范围内（NaN/inf/1e40 全拒），
    /// 且 NaN 不得经 `as i64` 饱和成 0 静默写进整数点。
    #[test]
    fn encode_value_rejects_non_finite_float() {
        let f32p = parse_point("holding:0:f32").unwrap();
        let f64p = parse_point("holding:0:f64").unwrap();
        let u16p = parse_point("holding:0:u16").unwrap();
        // f32 点：超范围（1e40 > f32::MAX）与 NaN/inf 都拒
        for (label, v) in [
            ("1e40", 1e40f64),
            ("NaN", f64::NAN),
            ("+inf", f64::INFINITY),
            ("-inf", f64::NEG_INFINITY),
        ] {
            assert!(
                encode_value(&SampleValue::Float(v), &f32p, None).is_err(),
                "f32 点应拒写 {label}"
            );
        }
        // f64 点：只拒非有限值（1e40 在 f64 中是合法有限数）
        for (label, v) in [
            ("NaN", f64::NAN),
            ("+inf", f64::INFINITY),
            ("-inf", f64::NEG_INFINITY),
        ] {
            assert!(
                encode_value(&SampleValue::Float(v), &f64p, None).is_err(),
                "f64 点应拒写 {label}"
            );
            assert!(
                encode_value(&SampleValue::Float(v), &u16p, None).is_err(),
                "整数点应拒写 {label}（不得饱和成 0）"
            );
        }
        assert!(encode_value(&SampleValue::Float(1e40), &f64p, None).is_ok());
        assert!(encode_value(&SampleValue::Float(f64::from(f32::MAX)), &f32p, None).is_ok());
        assert!(encode_value(&SampleValue::Float(-1.5), &f64p, None).is_ok());
    }

    /// MB-2：布尔串只认 "true" 是协议语义缺陷（"1"/"on" 静默写 false）。
    /// 现接受 true/false/1/0/on/off/yes/no（trim + 忽略大小写），其余串 Err。
    #[test]
    fn encode_value_bool_strings_accept_common_spellings() {
        let p = parse_point("coil:0:bool").unwrap();
        for s in ["1", "on", "ON", " yes ", "TRUE", "True"] {
            assert_eq!(
                encode_value(&SampleValue::Str(s.into()), &p, None).unwrap(),
                WritePayload::Coils(vec![true]),
                "{s:?} 应解析为 true"
            );
        }
        for s in ["0", "off", "NO", "false", "False"] {
            assert_eq!(
                encode_value(&SampleValue::Str(s.into()), &p, None).unwrap(),
                WritePayload::Coils(vec![false]),
                "{s:?} 应解析为 false"
            );
        }
        let err = encode_value(&SampleValue::Str("maybe".into()), &p, None)
            .expect_err("无法识别的布尔串必须 Err（不得静默写 false）");
        assert!(err.contains("布尔"), "错误信息应说明布尔取值：{err}");
        // 数值/布尔路径不变（非零即真）
        assert_eq!(
            encode_value(&SampleValue::Int(1), &p, None).unwrap(),
            WritePayload::Coils(vec![true])
        );
        assert_eq!(
            encode_value(&SampleValue::Int(0), &p, None).unwrap(),
            WritePayload::Coils(vec![false])
        );
    }
}
