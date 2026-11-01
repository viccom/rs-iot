//! 值语义：`Variant` ↔ [`SampleValue`]（读写两向）与写值类型校验。
//!
//! 审查对应：OPC-1（坏质量 → `Null`）、OPC-4（非标量用库 `Display`）、
//! OPC-6（逐类型 checked 范围、任一节点失败即 `Err`）、OPC-7（点表类型词表别名）、
//! OPC-17（JSON null / >i64 整数 / 写 Null 拒写）。
//!
//! 约定：`Null` 是**坏质量标记**而非数值——任何写路径都拒写（不发 `Variant::Empty`
//! 给服务端冒充有效值）。

use crate::types::SampleValue;
use opcua::types::{DataValue, StatusCode, Variant};

/// 读方向：`DataValue` → `SampleValue`。坏质量/缺值统一 `Null`（OPC-1）。
///
/// 此前坏状态 `warn + None` 被调用方静默跳过（保留旧值，下游误以为新鲜）；
/// 现在显式投递 `Null`：rtdata 跳过该 field（保留上次有效值）、历史跳过该行，
/// 而"本拍无有效值"这件事不再丢失。
pub(super) fn data_value_to_sample(dv: &DataValue) -> SampleValue {
    if data_value_bad_status(dv).is_some() {
        return SampleValue::Null;
    }
    match &dv.value {
        Some(v) => variant_to_sample(v),
        None => SampleValue::Null,
    }
}

/// 坏质量状态（`None` = Good 或未提供 status，按 OPC UA 规范视为 Good）。
pub(super) fn data_value_bad_status(dv: &DataValue) -> Option<StatusCode> {
    dv.status.filter(|st| !st.is_good())
}

/// `Variant` → `SampleValue`（数值类归一 Float/Int/UInt，Bool→Bool，字符串类→Str）。
///
/// 非标量走库的 `Display`（OPC-4）：`Array`/`ExtensionObject`/`ByteString` 等
/// 得到可读形式，而不是 `format!("{v:?}")` 的 `Variant::…` 调试噪声。
pub(super) fn variant_to_sample(v: &Variant) -> SampleValue {
    match v {
        Variant::Boolean(b) => SampleValue::Bool(*b),
        Variant::SByte(i) => SampleValue::Int(*i as i64),
        Variant::Byte(u) => SampleValue::Int(*u as i64),
        Variant::Int16(i) => SampleValue::Int(*i as i64),
        Variant::UInt16(u) => SampleValue::Int(*u as i64),
        Variant::Int32(i) => SampleValue::Int(*i as i64),
        Variant::UInt32(u) => SampleValue::Int(*u as i64),
        Variant::Int64(i) => SampleValue::Int(*i),
        // F-5：UInt64 不再 `as i64`（超 2^63 符号翻转），独立无符号变体承载。
        Variant::UInt64(u) => SampleValue::UInt(*u),
        Variant::Float(f) => SampleValue::Float(*f as f64),
        Variant::Double(f) => SampleValue::Float(*f),
        Variant::String(s) => SampleValue::Str(s.as_ref().to_string()),
        Variant::DateTime(dt) => SampleValue::Str(dt.as_chrono().to_rfc3339()),
        other => SampleValue::Str(other.to_string()),
    }
}

/// `SampleValue` → `Variant`（写方向默认映射：Float→Double、Int→Int64、UInt→UInt64）。
///
/// `Null` 由 [`variant_with_type`] 提前拒绝；这里的 `Empty` 仅为防御性兜底
///（正常写路径不可达）。
pub(super) fn sample_value_to_variant(v: &SampleValue) -> Variant {
    match v {
        SampleValue::Float(f) => Variant::Double(*f),
        SampleValue::Int(i) => Variant::Int64(*i),
        SampleValue::UInt(u) => Variant::UInt64(*u),
        SampleValue::Bool(b) => Variant::Boolean(*b),
        SampleValue::Str(s) => Variant::String(s.as_str().into()),
        SampleValue::Null => Variant::Empty,
    }
}

/// OPC UA 标量类型（`opc_type` / 点表类型的归一结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpcType {
    /// Boolean
    Boolean,
    /// SByte（i8）
    SByte,
    /// Byte（u8）
    Byte,
    /// Int16
    Int16,
    /// UInt16
    UInt16,
    /// Int32
    Int32,
    /// UInt32
    UInt32,
    /// Int64
    Int64,
    /// UInt64
    UInt64,
    /// Float（f32）
    Float,
    /// Double（f64）
    Double,
    /// String
    String,
}

impl OpcType {
    /// 字符串 → 类型（OPC-7：点表词表与 `opc_type` 词表统一别名）。
    ///
    /// 点表 `data_type` 惯用 `f32/f64/i16/u16/bool/string`，而 `opc_type` 惯用
    /// `Int32/Float/Boolean…`；两套词表在本函数汇合（大小写不敏感）。
    pub(super) fn parse(s: &str) -> Result<Self, String> {
        let word = s.trim().to_ascii_lowercase();
        match word.as_str() {
            "boolean" | "bool" => Ok(Self::Boolean),
            "sbyte" | "i8" | "int8" => Ok(Self::SByte),
            "byte" | "u8" | "uint8" => Ok(Self::Byte),
            "int16" | "i16" => Ok(Self::Int16),
            "uint16" | "u16" => Ok(Self::UInt16),
            "int32" | "i32" | "int" => Ok(Self::Int32),
            "uint32" | "u32" | "uint" => Ok(Self::UInt32),
            "int64" | "i64" | "long" => Ok(Self::Int64),
            "uint64" | "u64" | "ulong" => Ok(Self::UInt64),
            "float" | "f32" | "float32" | "real" => Ok(Self::Float),
            "double" | "f64" | "float64" | "lreal" => Ok(Self::Double),
            "string" | "str" | "text" => Ok(Self::String),
            _ => Err(format!(
                "未知 opc_type {s:?}（支持 boolean/sbyte/byte/int16/uint16/int32/uint32/int64/uint64/float/double/string 及点表别名 bool/i16/u16/i32/u32/i64/u64/f32/f64）"
            )),
        }
    }
}

/// SampleValue + 显式类型 → Variant（写方向）。
///
/// OPC UA 服务端对 write 类型严格匹配（Int32 节点写 Int64 会 BadTypeMismatch），
/// 所以 `opc_type` 缺省时按 [`sample_value_to_variant`] 推断，显式给出时逐类型
/// **checked 转换**：
/// - 整数越界（byte 0-255、int16 -32768..32767…）→ `Err`，不做 `as` 回绕；
/// - 浮点写整型节点：带小数 / 非有限 → `Err`，不静默截断；
/// - 布尔串接受 true/false/1/0/on/off，其余 → `Err`（不静默写 false）；
/// - 值为 `Null` → `Err`（坏质量不得写回）。
pub(super) fn variant_with_type(
    v: &SampleValue,
    opc_type: Option<&str>,
) -> Result<Variant, String> {
    if matches!(v, SampleValue::Null) {
        return Err("value 为 Null（坏质量/无值），拒写".to_string());
    }
    let Some(t) = opc_type.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(sample_value_to_variant(v));
    };
    let ty = OpcType::parse(t)?;
    let out = match ty {
        OpcType::Boolean => Variant::Boolean(as_bool(v)?),
        OpcType::SByte => {
            Variant::SByte(int_in_range(v, "sbyte", i8::MIN as i128, i8::MAX as i128)? as i8)
        }
        OpcType::Byte => Variant::Byte(int_in_range(v, "byte", 0, u8::MAX as i128)? as u8),
        OpcType::Int16 => {
            Variant::Int16(int_in_range(v, "int16", i16::MIN as i128, i16::MAX as i128)? as i16)
        }
        OpcType::UInt16 => Variant::UInt16(int_in_range(v, "uint16", 0, u16::MAX as i128)? as u16),
        OpcType::Int32 => {
            Variant::Int32(int_in_range(v, "int32", i32::MIN as i128, i32::MAX as i128)? as i32)
        }
        OpcType::UInt32 => Variant::UInt32(int_in_range(v, "uint32", 0, u32::MAX as i128)? as u32),
        OpcType::Int64 => {
            Variant::Int64(int_in_range(v, "int64", i64::MIN as i128, i64::MAX as i128)? as i64)
        }
        OpcType::UInt64 => Variant::UInt64(int_in_range(v, "uint64", 0, u64::MAX as i128)? as u64),
        OpcType::Float => {
            let f = as_f64(v)?;
            let out = f as f32;
            if f.is_finite() && !out.is_finite() {
                return Err(format!("value {f} 超出 float(f32) 范围"));
            }
            Variant::Float(out)
        }
        OpcType::Double => Variant::Double(as_f64(v)?),
        OpcType::String => Variant::String(
            match v {
                SampleValue::Str(s) => s.clone(),
                SampleValue::Int(i) => i.to_string(),
                SampleValue::UInt(u) => u.to_string(),
                SampleValue::Float(f) => f.to_string(),
                SampleValue::Bool(b) => b.to_string(),
                SampleValue::Null => String::new(),
            }
            .as_str()
            .into(),
        ),
    };
    Ok(out)
}

/// 转 i128 并做目标类型范围检查（检查失败 `Err` 带类型名与区间）。
fn int_in_range(v: &SampleValue, ty: &str, min: i128, max: i128) -> Result<i128, String> {
    let n = as_i128(v)?;
    if n < min || n > max {
        return Err(format!("value {n} 超出 {ty} 范围（{min}..={max}）"));
    }
    Ok(n)
}

/// 任意数值/布尔/数字串 → i128（浮点必须整值且有限）。
fn as_i128(v: &SampleValue) -> Result<i128, String> {
    match v {
        SampleValue::Int(i) => Ok(*i as i128),
        SampleValue::UInt(u) => Ok(*u as i128),
        SampleValue::Float(f) => {
            if !f.is_finite() {
                return Err(format!("value {f} 非有限浮点，拒写整型节点"));
            }
            if f.fract() != 0.0 {
                return Err(format!("value {f} 带小数，拒写整型节点（不做静默截断）"));
            }
            Ok(*f as i128)
        }
        SampleValue::Bool(b) => Ok(u8::from(*b) as i128),
        SampleValue::Str(s) => s
            .trim()
            .parse::<i128>()
            .map_err(|_| format!("value 非整数串 {s:?}（拒写，不做 0 兜底）")),
        SampleValue::Null => Err("value 为 Null（坏质量/无值），拒写".to_string()),
    }
}

/// 任意数值/布尔/数字串 → f64。
fn as_f64(v: &SampleValue) -> Result<f64, String> {
    match v {
        SampleValue::Float(f) => Ok(*f),
        SampleValue::Int(i) => Ok(*i as f64),
        SampleValue::UInt(u) => Ok(*u as f64),
        SampleValue::Bool(b) => Ok(u8::from(*b) as f64),
        SampleValue::Str(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("value 非数字串 {s:?}（拒写，不做 0 兜底）")),
        SampleValue::Null => Err("value 为 Null（坏质量/无值），拒写".to_string()),
    }
}

/// 任意值 → bool（布尔串接受 true/false/1/0/on/off；其余拒写）。
fn as_bool(v: &SampleValue) -> Result<bool, String> {
    match v {
        SampleValue::Bool(b) => Ok(*b),
        SampleValue::Int(i) => Ok(*i != 0),
        SampleValue::UInt(u) => Ok(*u != 0),
        SampleValue::Float(f) => Ok(*f != 0.0),
        SampleValue::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Ok(true),
            "false" | "0" | "off" => Ok(false),
            other => Err(format!(
                "value 串 {other:?} 无法判为布尔（支持 true/false/1/0/on/off）"
            )),
        },
        SampleValue::Null => Err("value 为 Null（坏质量/无值），拒写".to_string()),
    }
}

/// JSON → `SampleValue`（写命令的 value 字段）。
///
/// OPC-17：`null` → `Null`（此前写成字符串 "null"）；整数先 `as_u64`
///（>i64::MAX 的计数不丢精度、不走 f64）再 `as_i64`（负数）再 `as_f64`。
pub(super) fn json_to_sample_value(v: &serde_json::Value) -> SampleValue {
    match v {
        serde_json::Value::Null => SampleValue::Null,
        serde_json::Value::Bool(b) => SampleValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                SampleValue::UInt(u)
            } else if let Some(i) = n.as_i64() {
                SampleValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                SampleValue::Float(f)
            } else {
                // 理论上不可达（serde_json 数字必是其中一种）；保留原文不猜 0。
                SampleValue::Str(n.to_string())
            }
        }
        serde_json::Value::String(s) => SampleValue::Str(s.clone()),
        other => SampleValue::Str(other.to_string()),
    }
}
