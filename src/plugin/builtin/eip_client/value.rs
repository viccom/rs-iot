//! 值转换：读 `PlcValue` → `SampleValue`；写 `SampleValue` → `PlcValue`（全量 checked）。
//!
//! 台账（`docs/designs/2026-11-01-protocol-semantics-audit.md` §6）对应关系：
//! - **E-7** 写值窄化回绕 + 非数字串写 0 + 负数静默写 0：
//!   全部 `try_from` / 范围比较，越界与非法串一律 `Err`（写入口据此回 422，F-2）；
//! - **E-2/E-3** 读值映射：库能解的 `PlcValue` 变体全覆盖，不猜类型；
//! - **E-9** 位写 / BOOL 数组元素写是库内的读-改-写（非原子），此处仅注释不改行为。

use crate::types::SampleValue;
use rust_ethernet_ip::PlcValue;

/// `PlcValue` → `SampleValue`（数值类归一 Float/Int，Bool→Bool，串类→Str）。
///
/// UDT（v1 不做成员级展开）序列化为 JSON 字符串入库可见——`UdtData`
/// 自带 serde Serialize，`{"symbol_id":..,"data":[..]}` 结构化可读。
///
/// `Ulint` 超 `i64::MAX` 选择**饱和**（`sample_value_uint` 见 F-5 是框架侧条目，本插件
/// 保持既有饱和语义不改：历史上 S7/OPC 的 u64 点位靠它避免符号翻转）。
pub(super) fn plc_to_sample(v: &PlcValue) -> SampleValue {
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
pub(super) fn json_to_sample_value(v: &serde_json::Value) -> SampleValue {
    match v {
        serde_json::Value::Bool(b) => SampleValue::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(SampleValue::Int)
            .or_else(|| n.as_u64().map(SampleValue::UInt))
            .or_else(|| n.as_f64().map(SampleValue::Float))
            .unwrap_or(SampleValue::Int(0)),
        serde_json::Value::String(s) => SampleValue::Str(s.clone()),
        serde_json::Value::Null => SampleValue::Null,
        other => SampleValue::Str(other.to_string()),
    }
}

/// SampleValue → 写 STRING tag 的可读串（A-1：`plc_type=string` + 非 Str 值时使用，
/// 此前 `unreachable!` 在数值+string 组合直接 panic 杀实例）。
pub(super) fn sample_to_string_repr(v: &SampleValue) -> String {
    match v {
        SampleValue::Str(s) => s.clone(),
        SampleValue::Int(i) => i.to_string(),
        SampleValue::UInt(u) => u.to_string(),
        SampleValue::Float(f) => f.to_string(),
        SampleValue::Bool(b) => b.to_string(),
        // Null 在写入口已被拒（E-7），此处防御性给空串。
        SampleValue::Null => String::new(),
    }
}

/// 写值 → i64（E-7 checked：不回绕、不静默）。
fn as_i64(v: &SampleValue) -> Result<i64, String> {
    match v {
        SampleValue::Int(i) => Ok(*i),
        SampleValue::UInt(u) => {
            i64::try_from(*u).map_err(|_| format!("值 {u} 超出 i64 范围（E-7）"))
        }
        SampleValue::Float(f) => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err(format!(
                    "浮点值 {f} 不是整数，不能写入整数类型（E-7，拒绝静默取整）"
                ));
            }
            if *f < i64::MIN as f64 || *f > i64::MAX as f64 {
                return Err(format!("浮点值 {f} 超出 i64 范围（E-7）"));
            }
            Ok(*f as i64)
        }
        SampleValue::Bool(b) => Ok(i64::from(u8::from(*b))),
        SampleValue::Str(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("字符串 '{s}' 不是整数（E-7：不得静默写 0）")),
        SampleValue::Null => Err("写值缺失（Null）".to_string()),
    }
}

/// 写值 → u64（E-7 checked：负数 Err，不回绕）。
fn as_u64(v: &SampleValue) -> Result<u64, String> {
    match v {
        SampleValue::Int(i) => u64::try_from(*i)
            .map_err(|_| format!("负值 {i} 不能写入无符号类型（E-7：此前静默写 0）")),
        SampleValue::UInt(u) => Ok(*u),
        SampleValue::Float(f) => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err(format!(
                    "浮点值 {f} 不是整数，不能写入无符号整数类型（E-7）"
                ));
            }
            if *f < 0.0 || *f > u64::MAX as f64 {
                return Err(format!("浮点值 {f} 超出 u64 范围（E-7）"));
            }
            Ok(*f as u64)
        }
        SampleValue::Bool(b) => Ok(u64::from(u8::from(*b))),
        SampleValue::Str(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("字符串 '{s}' 不是无符号整数（E-7：不得静默写 0）")),
        SampleValue::Null => Err("写值缺失（Null）".to_string()),
    }
}

/// 写值 → f64（E-7：非数字串 Err）。
fn as_f64(v: &SampleValue) -> Result<f64, String> {
    let f = match v {
        SampleValue::Float(f) => *f,
        SampleValue::Int(i) => *i as f64,
        SampleValue::UInt(u) => *u as f64,
        SampleValue::Bool(b) => f64::from(u8::from(*b)),
        SampleValue::Str(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("字符串 '{s}' 不是数值（E-7：不得静默写 0）"))?,
        SampleValue::Null => return Err("写值缺失（Null）".to_string()),
    };
    if !f.is_finite() {
        return Err(format!("值 {f} 非有限数，拒写（E-7）"));
    }
    Ok(f)
}

/// f64 → f32（E-7：超 f32 上限 Err，不产生 inf）。
fn as_f32(f: f64) -> Result<f32, String> {
    let g = f as f32;
    if !g.is_finite() {
        return Err(format!("值 {f} 超出 REAL(f32) 范围（E-7）"));
    }
    Ok(g)
}

/// 写值 → bool（E-7：只认 true/false/1/0/on/off，其余 Err 而不是静默 false）。
///
/// 位路径（`Tag.5`）在库内只接受 `PlcValue::Bool`（<REG>client.rs:1295-1322），
/// 写入口对位路径统一走本函数（HMI 推按钮发 `1`/`0` 也可用）。
pub(super) fn to_bool(v: &SampleValue) -> Result<bool, String> {
    match v {
        SampleValue::Bool(b) => Ok(*b),
        SampleValue::Int(i) => Ok(*i != 0),
        SampleValue::UInt(u) => Ok(*u != 0),
        SampleValue::Float(f) => Ok(*f != 0.0),
        SampleValue::Str(s) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("on") || t == "1" {
                Ok(true)
            } else if t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off") || t == "0" {
                Ok(false)
            } else {
                Err(format!(
                    "字符串 '{s}' 不是布尔值（只认 true/false/1/0/on/off）"
                ))
            }
        }
        SampleValue::Null => Err("写值缺失（Null）".to_string()),
    }
}

/// SampleValue + 显式 plc_type → PlcValue（**全量 checked**，E-7）。
///
/// CIP 类型化写入（写 DINT tag 传 REAL 会类型不匹配），`plc_type=None`（或空白）退回
/// 默认推断：bool→Bool、整数→Dint（AB 最常用）、浮点→Real、串→String。
///
/// 越界/非整数/非数字串一律 `Err`：此前 `as i8/i16/i32` 会回绕（`300 as i8 = 44`）、
/// `parse().unwrap_or(0)` 把非数字串写成 0、`as_u()` 把负数 `max(0)` 写成 0。
///
/// **未知 plc_type 返回 Err**（A-6，2026-11-01 审查）：此前静默回退默认推断，
/// "dnit" 类拼写错误会按 Dint 写入 PLC——与 s7 `parse_kind` 的报错口径对齐。
pub(super) fn plc_with_type(v: &SampleValue, plc_type: Option<&str>) -> Result<PlcValue, String> {
    let Some(t) = plc_type.map(str::trim).filter(|s| !s.is_empty()) else {
        return default_plc_value(v);
    };
    let out = match t.to_lowercase().as_str() {
        "bool" => PlcValue::Bool(to_bool(v)?),
        "sint" => PlcValue::Sint(
            i8::try_from(as_i64(v)?).map_err(|_| out_of_range(v, "SINT(-128..=127)"))?,
        ),
        "int" => PlcValue::Int(
            i16::try_from(as_i64(v)?).map_err(|_| out_of_range(v, "INT(-32768..=32767)"))?,
        ),
        "dint" => PlcValue::Dint(
            i32::try_from(as_i64(v)?).map_err(|_| out_of_range(v, "DINT(-2^31..=2^31-1)"))?,
        ),
        "lint" => PlcValue::Lint(as_i64(v)?),
        "usint" => PlcValue::Usint(
            u8::try_from(as_u64(v)?).map_err(|_| out_of_range(v, "USINT(0..=255)"))?,
        ),
        "uint" => PlcValue::Uint(
            u16::try_from(as_u64(v)?).map_err(|_| out_of_range(v, "UINT(0..=65535)"))?,
        ),
        "udint" => PlcValue::Udint(
            u32::try_from(as_u64(v)?).map_err(|_| out_of_range(v, "UDINT(0..=2^32-1)"))?,
        ),
        "ulint" => PlcValue::Ulint(as_u64(v)?),
        "real" => PlcValue::Real(as_f32(as_f64(v)?)?),
        "lreal" => PlcValue::Lreal(as_f64(v)?),
        "string" => PlcValue::String(sample_to_string_repr(v)),
        other => {
            return Err(format!(
                "未知 plc_type {other:?}（支持 bool/sint/int/dint/lint/usint/uint/udint/ulint/real/lreal/string）"
            ));
        }
    };
    Ok(out)
}

/// 越界错误文案（带目标类型区间，现场可直接改 plc_type）。
fn out_of_range(v: &SampleValue, target: &str) -> String {
    format!("写值 {v:?} 超出 {target}（E-7：此前 as 转换会静默回绕）")
}

/// 缺省推断（无 plc_type）：Bool→Bool、Int→Dint（checked）、Float→Real（checked）、
/// Str→String。越界 Err 而不是回绕（E-7）。
fn default_plc_value(v: &SampleValue) -> Result<PlcValue, String> {
    match v {
        SampleValue::Bool(b) => Ok(PlcValue::Bool(*b)),
        SampleValue::Int(i) => {
            Ok(PlcValue::Dint(i32::try_from(*i).map_err(|_| {
                out_of_range(v, "DINT(缺省推断；请显式 plc_type=lint/udint)")
            })?))
        }
        SampleValue::UInt(u) => {
            Ok(PlcValue::Udint(u32::try_from(*u).map_err(|_| {
                out_of_range(v, "UDINT(缺省推断；请显式 plc_type=ulint/lreal)")
            })?))
        }
        SampleValue::Float(f) => Ok(PlcValue::Real(as_f32(*f)?)),
        SampleValue::Str(s) => Ok(PlcValue::String(s.clone())),
        SampleValue::Null => Err("写值缺失（Null）".to_string()),
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
            plc_with_type(&SampleValue::Int(42), None).unwrap(),
            PlcValue::Dint(42)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Float(1.5), None).unwrap(),
            PlcValue::Real(1.5)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Bool(false), None).unwrap(),
            PlcValue::Bool(false)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Str("x".into()), None).unwrap(),
            PlcValue::String("x".into())
        );
        // 显式 plc_type（大小写不敏感）
        assert_eq!(
            plc_with_type(&SampleValue::Int(7), Some("LINT")).unwrap(),
            PlcValue::Lint(7)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Int(7), Some("uint")).unwrap(),
            PlcValue::Uint(7)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Int(1), Some("bool")).unwrap(),
            PlcValue::Bool(true)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Float(2.5), Some("lreal")).unwrap(),
            PlcValue::Lreal(2.5)
        );
        // A-1：数值→string 转可读串（此前 Debug 格式会写入字面 "Int(5)" 进 PLC）
        assert_eq!(
            plc_with_type(&SampleValue::Int(5), Some("string")).unwrap(),
            PlcValue::String("5".into())
        );
        // A-6：未知类型拒写（此前静默回退默认推断——"dnit" 拼写错误按 Dint 写 PLC）
        assert!(
            plc_with_type(&SampleValue::Int(9), Some("nope")).is_err(),
            "未知 plc_type 必须 Err"
        );
        // 空/空白 plc_type → 默认推断（显式空串视同缺省，不报错）
        assert_eq!(
            plc_with_type(&SampleValue::Int(9), Some("")).unwrap(),
            PlcValue::Dint(9)
        );
        assert_eq!(
            plc_with_type(&SampleValue::Int(9), Some("  ")).unwrap(),
            PlcValue::Dint(9)
        );
    }

    /// A-1：string 写的值串化——Str 原样、数值/布尔转可读串（写入 PLC STRING tag）。
    #[test]
    fn sample_to_string_repr_all_variants() {
        assert_eq!(
            sample_to_string_repr(&SampleValue::Str("abc".into())),
            "abc"
        );
        assert_eq!(sample_to_string_repr(&SampleValue::Int(5)), "5");
        assert_eq!(sample_to_string_repr(&SampleValue::Int(-5)), "-5");
        assert_eq!(sample_to_string_repr(&SampleValue::Float(1.5)), "1.5");
        assert_eq!(sample_to_string_repr(&SampleValue::Bool(true)), "true");
        assert_eq!(sample_to_string_repr(&SampleValue::Bool(false)), "false");
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
        // u64 超 i64 的 JSON 数字保留为 UInt（不再退化成 f64）
        assert_eq!(
            json_to_sample_value(&serde_json::json!(u64::MAX)),
            SampleValue::UInt(u64::MAX)
        );
        assert_eq!(
            json_to_sample_value(&serde_json::Value::Null),
            SampleValue::Null
        );
    }

    /// E-7：窄化必须 checked——历史上 `as` 回绕、`unwrap_or(0)`、`max(0)` 静默错写。
    #[test]
    fn plc_with_type_rejects_narrowing_and_bad_strings() {
        // 整数越界：300 写 SINT（此前 300 as i8 = 44）
        assert!(plc_with_type(&SampleValue::Int(300), Some("sint")).is_err());
        // 70000 写 UINT（此前 70000 as u16 = 4464）
        assert!(plc_with_type(&SampleValue::Int(70_000), Some("uint")).is_err());
        // -5 写 UDINT（此前 max(0) 静默写 0）
        let err =
            plc_with_type(&SampleValue::Int(-5), Some("udint")).expect_err("负数写无符号必须 Err");
        assert!(err.contains("E-7"), "{err}");
        // 负数写 USINT
        assert!(plc_with_type(&SampleValue::Int(-1), Some("usint")).is_err());
        // 缺省推断也只接受 i32 区间（此前 as i32 回绕）
        assert!(plc_with_type(&SampleValue::Int(i64::MAX), None).is_err());
        assert!(plc_with_type(&SampleValue::Int(1 << 40), None).is_err());
        // 非数字串（此前 parse().unwrap_or(0) 写 0）
        let err = plc_with_type(&SampleValue::Str("abc".into()), Some("dint"))
            .expect_err("非数字串必须 Err");
        assert!(err.contains("E-7"), "{err}");
        assert!(plc_with_type(&SampleValue::Str("abc".into()), Some("real")).is_err());
        // 浮点写整数：非整数值 Err（拒绝静默取整）
        assert!(plc_with_type(&SampleValue::Float(1.5), Some("dint")).is_err());
        assert_eq!(
            plc_with_type(&SampleValue::Float(2.0), Some("dint")).unwrap(),
            PlcValue::Dint(2)
        );
        // REAL 超 f32 上限 Err（此前 as f32 得 inf）
        assert!(plc_with_type(&SampleValue::Float(1e300), Some("real")).is_err());
        // Null 写值 Err
        assert!(plc_with_type(&SampleValue::Null, Some("dint")).is_err());
        // 布尔串：非布尔值 Err（此前一律 false）
        assert!(plc_with_type(&SampleValue::Str("yes-please".into()), Some("bool")).is_err());
        for (s, want) in [("on", true), ("OFF", false), ("1", true), ("0", false)] {
            assert_eq!(
                plc_with_type(&SampleValue::Str(s.into()), Some("bool")).unwrap(),
                PlcValue::Bool(want),
                "字符串 '{s}'"
            );
        }
        // 无符号边界合法值
        assert_eq!(
            plc_with_type(&SampleValue::Int(255), Some("usint")).unwrap(),
            PlcValue::Usint(255)
        );
        assert_eq!(
            plc_with_type(&SampleValue::UInt(u64::MAX), Some("ulint")).unwrap(),
            PlcValue::Ulint(u64::MAX)
        );
        // 字符串数字合法
        assert_eq!(
            plc_with_type(&SampleValue::Str("123".into()), Some("dint")).unwrap(),
            PlcValue::Dint(123)
        );
    }
}
