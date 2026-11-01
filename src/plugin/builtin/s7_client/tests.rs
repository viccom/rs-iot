use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_s7::cotp::{Cotp, CotpConnection};
use tokio_s7::header::S7Header;
use tokio_s7::packet::{S7Data, S7DataCodec};
use tokio_s7::paramter::{S7Parameter, SetupComParameter};
use tokio_s7::tpkt::TPKT;
use tokio_s7::types::{
    PduType, S7DataVariableType, S7ErrorClass, S7MessageType, S7ParamVariableType,
};

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
    assert_eq!(parse_kind("bool").unwrap().0, Kind::Bool);
    assert_eq!(parse_kind("bit").unwrap().0, Kind::Bool);
    assert_eq!(parse_kind("byte").unwrap().0, Kind::Byte);
    assert_eq!(parse_kind("i16").unwrap().0, Kind::I16);
    assert_eq!(parse_kind("uint16").unwrap().0, Kind::U16);
    assert_eq!(parse_kind("int32").unwrap().0, Kind::I32);
    assert_eq!(parse_kind("f32").unwrap().0, Kind::F32);
    assert_eq!(parse_kind("float").unwrap().0, Kind::F32);
    assert_eq!(parse_kind("f64").unwrap().0, Kind::F64);
    assert_eq!(parse_kind("double").unwrap().0, Kind::F64);
    assert!(parse_kind("").is_err());
}

#[test]
fn parse_kind_string_variants() {
    // 缺省 max_len 254（S7 常见 String[254]）
    assert_eq!(
        parse_kind("string").unwrap(),
        (Kind::String, DEFAULT_STRING_MAX_LEN)
    );
    // string[64] 显式声明长度
    assert_eq!(parse_kind("STRING[64]").unwrap(), (Kind::String, 64));
    assert_eq!(parse_kind("wstring").unwrap(), (Kind::WString, 254));
    assert_eq!(parse_kind("WString[32]").unwrap(), (Kind::WString, 32));
    // 坏长度报错
    assert!(parse_kind("string[abc]").is_err());
    // 0 clamp 到 1（不报错）
    assert_eq!(parse_kind("string[0]").unwrap(), (Kind::String, 1));
    // 无闭合括号
    assert!(parse_kind("string[64").is_err());
    // B-4：超 S7 规范上限 254 的声明长度钳到 254——库 read/write_string 以
    // `max_len + 2`（u16）算总长，65535 回绕成 1 会让写路径 buf[1] 越界 panic。
    assert_eq!(
        parse_kind("string[65535]").unwrap(),
        (Kind::String, DEFAULT_STRING_MAX_LEN),
        "65535 必须钳到 254（防库内 u16 回绕 panic）"
    );
    assert_eq!(parse_kind("wstring[9999]").unwrap(), (Kind::WString, 254));
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
    assert_eq!(cfg.connection_type, 0x01, "connection_type defaults to PG");
    assert_eq!(
        cfg.max_items_per_request, 20,
        "batch size defaults to protocol max"
    );

    let cfg2 = parse_cfg(
        &serde_json::json!({"endpoint":"x","port":1025,"plc_type":"s300","rack":0,"slot":2,"period":0,"timeout_ms":1000,"auto_reconnect":false,"connection_type":"op","max_items_per_request":10}),
    )
    .unwrap();
    assert_eq!(cfg2.plc_type, PlcType::S300);
    assert_eq!(cfg2.port, 1025, "port overridable (非标端口 PLC / 测试)");
    assert_eq!(cfg2.rack_slot, Some((0, 2)));
    assert_eq!(cfg2.period, 1, "period 0 clamped to 1");
    assert_eq!(cfg2.timeout_ms, 1000);
    assert!(!cfg2.auto_reconnect);
    assert_eq!(cfg2.connection_type, 0x03);
    assert_eq!(cfg2.max_items_per_request, 10);

    // 协议上限 clamp：>20 收敛到 20，0 收敛到 1
    let over = parse_cfg(&serde_json::json!({"endpoint":"x","max_items_per_request":100})).unwrap();
    assert_eq!(over.max_items_per_request, 20);
    let under = parse_cfg(&serde_json::json!({"endpoint":"x","max_items_per_request":0})).unwrap();
    assert_eq!(under.max_items_per_request, 1);

    assert!(
        parse_cfg(&serde_json::json!({})).is_err(),
        "missing endpoint errors"
    );
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","plc_type":"bad"})).is_err());
    assert!(
        parse_cfg(&serde_json::json!({"endpoint":"x","connection_type":"xxx"})).is_err(),
        "bad connection_type errors"
    );
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
    // S-6（2026-11-01 协议语义审查）有意变更：`infer_kind` 已**不再用于写路径**
    // （`Int → I64` 会让 `DB1.DBD0` 写 4 字节点位时覆盖 8 字节）。断言保留为
    // "旧行为基线"，说明被替换掉的映射是什么；写路径的 kind 决策见
    // [`PreparedWrite::build`] 与 `write_prepare_*` 系列测试。
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

// ---------- S-3：PDU 分片 ----------

#[test]
fn max_items_per_request_follows_negotiated_pdu() {
    // 公式 (pdu - 20) / 12（cap 放到不干扰的位置看纯 PDU 口径）：
    // 240 → 18（S7-300 常见老 PDU）、480 → 38、1140 → 93
    assert_eq!(max_items_per_request(240, 200), 18);
    assert_eq!(max_items_per_request(480, 200), 38);
    assert_eq!(max_items_per_request(1140, 200), 93);
    // 960（库缺省 PDU）→ 78
    assert_eq!(max_items_per_request(960, 200), 78);
    // 配置上限（缺省 20，协议单请求项数的保守上界）仍然收紧：
    // 480 PDU 能装 38 项，但缺省配置只发 20 项；240 PDU 才是真瓶颈（18 < 20）
    assert_eq!(max_items_per_request(240, 20), 18);
    assert_eq!(max_items_per_request(480, 20), 20);
    assert_eq!(max_items_per_request(1140, 20), 20);
    assert_eq!(max_items_per_request(480, 10), 10);
    assert_eq!(max_items_per_request(960, 1), 1);
    // 退化输入：PDU 再小也至少 1 项；cap 0 按 1
    assert_eq!(max_items_per_request(0, 20), 1);
    assert_eq!(max_items_per_request(20, 20), 1);
    assert_eq!(max_items_per_request(480, 0), 1);
}

#[test]
fn effective_items_per_request_honours_max_pdu_size() {
    let cfg = parse_cfg(&serde_json::json!({"endpoint":"x","max_pdu_size":240})).unwrap();
    // 对端回 480（大于我们提议的 240）→ 配置上界生效，仍按 240 算 18 项
    assert_eq!(effective_items_per_request(480, &cfg), 18);
    assert_eq!(effective_items_per_request(240, &cfg), 18);
    // 对端回更小的 120 → (120-20)/12 = 8
    assert_eq!(effective_items_per_request(120, &cfg), 8);
    // 缺省配置（960 提议 + cap 20）：480 → 20（cap 收紧）
    let def = parse_cfg(&serde_json::json!({"endpoint":"x"})).unwrap();
    assert_eq!(effective_items_per_request(480, &def), 20);
}

// ---------- S-10 / S-14 / S-19：kind 与解码 ----------

#[test]
fn parse_kind_ubyte_and_raw_variants() {
    // S-10：`ubyte` 是 u8（0-255），`byte` 保持 i8（既有断言见
    // decode_item_big_endian_roundtrip 的 0xFF → -1）
    assert_eq!(parse_kind("ubyte").unwrap().0, Kind::UByte);
    assert_eq!(parse_kind("u8").unwrap().0, Kind::UByte);
    assert_eq!(parse_kind("uint8").unwrap().0, Kind::UByte);
    assert_eq!(parse_kind("byte").unwrap().0, Kind::Byte);
    // S-14：raw:N / bytes:N
    assert_eq!(parse_kind("raw:4").unwrap(), (Kind::Raw(4), 4));
    assert_eq!(parse_kind("bytes:16").unwrap(), (Kind::Raw(16), 16));
    assert_eq!(parse_kind_full("RAW:2").unwrap().kind, Kind::Raw(2));
    assert!(parse_kind("raw:0").is_err(), "raw 长度 0 拒绝");
    assert!(parse_kind("raw:1025").is_err(), "raw 超上限拒绝");
    assert!(parse_kind("raw:").is_err());
    assert!(parse_kind("raw").is_err(), "缺 N 的 raw 拒绝");
    // 长度声明只允许出现在 string/wstring 上
    assert!(parse_kind("i16[4]").is_err());
    assert!(parse_kind("raw:4[2]").is_err());
}

#[test]
fn parse_kind_full_tracks_declared_length() {
    // S-8：缺 `[N]` 与显式 [N] 必须可区分（缺省 254 的读法在 240B PDU 上必失败）
    let p = parse_kind_full("string").unwrap();
    assert_eq!(p.kind, Kind::String);
    assert_eq!(p.max_len, DEFAULT_STRING_MAX_LEN);
    assert!(!p.len_declared);
    assert!(parse_kind_full("string[10]").unwrap().len_declared);
    assert_eq!(parse_kind_full("string[10]").unwrap().max_len, 10);
    assert!(!parse_kind_full("wstring").unwrap().len_declared);
    assert!(parse_kind_full("fibre").is_err());
}

#[test]
fn decode_item_ubyte_raw_and_u64() {
    // S-10：ubyte 0xFF → 255（byte 同字节 → -1，见 roundtrip 测试）
    assert_eq!(
        decode_item(Kind::UByte, &data_item(S7ReturnCode::Success, vec![0xFF])),
        Some(SampleValue::Int(255))
    );
    // S-19：u64 走 UInt 变体，超 2^63 不符号翻转
    assert_eq!(
        decode_item(Kind::U64, &data_item(S7ReturnCode::Success, vec![0xFF; 8])),
        Some(SampleValue::UInt(u64::MAX))
    );
    assert_eq!(
        decode_item(
            Kind::U64,
            &data_item(S7ReturnCode::Success, vec![0x80, 0, 0, 0, 0, 0, 0, 0])
        ),
        Some(SampleValue::UInt(1 << 63))
    );
    // S-14：raw:N → 大写十六进制串
    assert_eq!(
        decode_item(
            Kind::Raw(4),
            &data_item(S7ReturnCode::Success, vec![0xDE, 0xAD, 0xBE, 0xEF])
        ),
        Some(SampleValue::Str("DEADBEEF".into()))
    );
    assert_eq!(
        decode_item(
            Kind::Raw(4),
            &data_item(S7ReturnCode::Success, vec![0xDE, 0xAD])
        ),
        None,
        "raw 数据短于 N 必须失败"
    );
}

#[test]
fn build_request_rejects_single_point_kinds() {
    for kind in [Kind::String, Kind::WString, Kind::Raw(4)] {
        assert!(
            build_request("DB1.DBB0", kind).is_err(),
            "{kind:?} 不应进批量路径"
        );
    }
    assert_eq!(build_request("DB1.DBB0", Kind::UByte).unwrap().count, 1);
}

// ---------- S-11：写值 checked 转换 ----------

#[test]
fn convert_write_checks_integer_ranges() {
    // 旧实现 `70000 as u16 = 4464`、`-1 as u16 = 65535`（静默写错值）
    assert!(convert_write(Kind::U16, &SampleValue::Int(70_000)).is_err());
    assert!(convert_write(Kind::U16, &SampleValue::Int(-1)).is_err());
    assert_eq!(
        convert_write(Kind::U16, &SampleValue::Int(65_535)).unwrap(),
        WVal::U16(65_535)
    );
    // i16 / i32 / i64
    assert!(convert_write(Kind::I16, &SampleValue::Int(40_000)).is_err());
    assert_eq!(
        convert_write(Kind::I16, &SampleValue::Int(-32_768)).unwrap(),
        WVal::I16(-32_768)
    );
    assert!(convert_write(Kind::I32, &SampleValue::Int(i64::MAX)).is_err());
    assert_eq!(
        convert_write(Kind::I64, &SampleValue::Int(i64::MIN)).unwrap(),
        WVal::I64(i64::MIN)
    );
    // u32 / u64
    assert!(convert_write(Kind::U32, &SampleValue::Int(-5)).is_err());
    assert!(convert_write(Kind::U32, &SampleValue::Int(4_294_967_296)).is_err());
    assert_eq!(
        convert_write(Kind::U64, &SampleValue::UInt(u64::MAX)).unwrap(),
        WVal::U64(u64::MAX)
    );
    // 无符号数写有符号点位：超 i64 拒绝
    assert!(convert_write(Kind::I64, &SampleValue::UInt(u64::MAX)).is_err());
    // 小数写整型点位：拒绝（不再 `as` 截断）
    assert!(convert_write(Kind::I16, &SampleValue::Float(1.5)).is_err());
    assert_eq!(
        convert_write(Kind::I16, &SampleValue::Float(42.0)).unwrap(),
        WVal::I16(42)
    );
    // 非数字串 / 非有限浮点：Err（旧实现 unwrap_or(0) 写 0）
    assert!(convert_write(Kind::I16, &SampleValue::Str("abc".into())).is_err());
    assert!(convert_write(Kind::I16, &SampleValue::Str("".into())).is_err());
    assert_eq!(
        convert_write(Kind::I16, &SampleValue::Str(" 42 ".into())).unwrap(),
        WVal::I16(42)
    );
    assert!(convert_write(Kind::F64, &SampleValue::Float(f64::NAN)).is_err());
    assert!(convert_write(Kind::F64, &SampleValue::Float(f64::INFINITY)).is_err());
    // f32 溢出（1e300 会变 inf）
    assert!(convert_write(Kind::F32, &SampleValue::Float(1e300)).is_err());
    assert_eq!(
        convert_write(Kind::F32, &SampleValue::Float(0.5)).unwrap(),
        WVal::F32(0.5)
    );
    // Null 不可写
    assert!(convert_write(Kind::I16, &SampleValue::Null).is_err());
}

#[test]
fn convert_write_byte_kinds_differ_in_sign() {
    // S-10：byte = i8（-128..127），ubyte = u8（0..255）
    assert_eq!(
        convert_write(Kind::Byte, &SampleValue::Int(-1)).unwrap(),
        WVal::I8(-1)
    );
    assert!(convert_write(Kind::Byte, &SampleValue::Int(200)).is_err());
    assert_eq!(
        convert_write(Kind::UByte, &SampleValue::Int(255)).unwrap(),
        WVal::U8(255)
    );
    assert!(convert_write(Kind::UByte, &SampleValue::Int(256)).is_err());
    assert!(convert_write(Kind::UByte, &SampleValue::Int(-1)).is_err());
}

#[test]
fn convert_write_bool_accepts_word_forms() {
    for (v, want) in [
        (SampleValue::Bool(true), true),
        (SampleValue::Int(0), false),
        (SampleValue::Int(1), true),
        (SampleValue::UInt(1), true),
        (SampleValue::Float(0.0), false),
        (SampleValue::Str("true".into()), true),
        (SampleValue::Str("FALSE".into()), false),
        (SampleValue::Str("1".into()), true),
        (SampleValue::Str("0".into()), false),
        (SampleValue::Str("on".into()), true),
        (SampleValue::Str("off".into()), false),
    ] {
        assert_eq!(
            convert_write(Kind::Bool, &v).unwrap(),
            WVal::Bool(want),
            "{v:?}"
        );
    }
    // 非 0/1 数字与非约定字符串：拒绝（旧实现 `!= 0` 一律 true）
    assert!(convert_write(Kind::Bool, &SampleValue::Int(2)).is_err());
    assert!(convert_write(Kind::Bool, &SampleValue::Str("yes".into())).is_err());
    assert!(convert_write(Kind::Bool, &SampleValue::Str("".into())).is_err());
}

#[test]
fn convert_write_string_and_raw() {
    assert_eq!(
        convert_write(Kind::String, &SampleValue::Str("abc".into())).unwrap(),
        WVal::Str("abc".into())
    );
    assert!(
        convert_write(Kind::String, &SampleValue::Int(1)).is_err(),
        "数值写 string 点位拒绝"
    );
    assert!(
        convert_write(Kind::Raw(4), &SampleValue::Str("DEADBEEF".into())).is_err(),
        "raw 只读"
    );
}

// ---------- S-18 / S-20：配置边界 ----------

#[test]
fn parse_endpoint_splits_host_and_port() {
    assert_eq!(
        parse_endpoint("192.168.0.1", 102).unwrap(),
        ("192.168.0.1".into(), 102)
    );
    assert_eq!(
        parse_endpoint("192.168.0.1:1102", 102).unwrap(),
        ("192.168.0.1".into(), 1102),
        "S-18：endpoint 自带端口必须拆出来（否则库拼成 ip:1102:102）"
    );
    assert_eq!(
        parse_endpoint(" plc.local:102 ", 5000).unwrap(),
        ("plc.local".into(), 102)
    );
    // IPv6 方括号
    assert_eq!(parse_endpoint("[::1]", 102).unwrap(), ("[::1]".into(), 102));
    assert_eq!(
        parse_endpoint("[::1]:1102", 102).unwrap(),
        ("[::1]".into(), 1102)
    );
    // 裸 IPv6 / 空端口 / 越界端口 / 空主机
    assert!(parse_endpoint("::1", 102).is_err(), "裸 IPv6 需方括号");
    assert!(parse_endpoint("host:", 102).is_err());
    assert!(parse_endpoint(":102", 102).is_err());
    assert!(parse_endpoint("host:0", 102).is_err());
    assert!(parse_endpoint("host:70000", 102).is_err());
    assert!(parse_endpoint("", 102).is_err());
    assert!(parse_endpoint("[::1", 102).is_err());
    assert!(parse_endpoint("[::1]x", 102).is_err());
}

#[test]
fn parse_cfg_endpoint_port_and_checked_bounds() {
    // endpoint 带端口优先于 port 键
    let cfg = parse_cfg(&serde_json::json!({"endpoint":"1.2.3.4:1102","port":5000})).unwrap();
    assert_eq!(cfg.endpoint, "1.2.3.4");
    assert_eq!(cfg.port, 1102);
    // 端口越界/0 拒绝（旧实现 `as u16` 把 70000 截成 4464）
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","port":70000})).is_err());
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","port":0})).is_err());
    // rack/slot checked + 单独给时另一个取 0（S-20）
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","rack":300})).is_err());
    let only_rack = parse_cfg(&serde_json::json!({"endpoint":"x","rack":1})).unwrap();
    assert_eq!(only_rack.rack_slot, Some((1, 0)));
    let only_slot = parse_cfg(&serde_json::json!({"endpoint":"x","slot":2})).unwrap();
    assert_eq!(only_slot.rack_slot, Some((0, 2)));
    // max_pdu_size 边界（S-3）
    assert_eq!(
        parse_cfg(&serde_json::json!({"endpoint":"x"}))
            .unwrap()
            .max_pdu_size,
        960
    );
    assert_eq!(
        parse_cfg(&serde_json::json!({"endpoint":"x","max_pdu_size":240}))
            .unwrap()
            .max_pdu_size,
        240
    );
    assert_eq!(
        parse_cfg(&serde_json::json!({"endpoint":"x","max_pdu_size":4096}))
            .unwrap()
            .max_pdu_size,
        4096
    );
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","max_pdu_size":239})).is_err());
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","max_pdu_size":4097})).is_err());
    assert!(parse_cfg(&serde_json::json!({"endpoint":"x","max_pdu_size":"960"})).is_err());
    // verify_write 缺省 true（S-1）
    assert!(
        parse_cfg(&serde_json::json!({"endpoint":"x"}))
            .unwrap()
            .verify_write
    );
    assert!(
        !parse_cfg(&serde_json::json!({"endpoint":"x","verify_write":false}))
            .unwrap()
            .verify_write
    );
}

// ---------- F-1 / S-6 / S-7：写目标解析 ----------

/// 构造写索引：devA{temp=f32 DB1.DBD0, flag=bool M10.0, name=string[10] DB1.DBB20}；
/// devB{temp=i16 DB2.DBW0}（跨设备同名 tag，用于歧义用例）。
fn test_index() -> WriteIndex {
    let mut idx = WriteIndex::default();
    for (field, addr, kind, arg, declared) in [
        ("temp", "DB1.DBD0", Kind::F32, 0u16, false),
        ("flag", "M10.0", Kind::Bool, 0, false),
        ("name", "DB1.DBB20", Kind::String, 10, true),
        ("blob", "DB1.DBB40", Kind::Raw(4), 4, true),
    ] {
        idx.insert(
            "devA",
            PointSpec {
                field: field.into(),
                addr: addr.into(),
                kind,
                arg,
                len_declared: declared,
            },
        );
    }
    idx.insert(
        "devB",
        PointSpec {
            field: "temp".into(),
            addr: "DB2.DBW0".into(),
            kind: Kind::I16,
            arg: 0,
            len_declared: false,
        },
    );
    idx
}

#[test]
fn write_prepare_tag_contract_resolves_kind_from_point_table() {
    let idx = test_index();
    // F-1：HMI 只发 {tag,value}，kind 从点表声明取（S-6）
    let p = PreparedWrite::build(r#"{"tag":"temp","value":42.5}"#, "devA", &idx).unwrap();
    assert_eq!(p.addr, "DB1.DBD0");
    assert_eq!(p.kind, Kind::F32);
    assert_eq!(p.value, WVal::F32(42.5));
    // bool 点位：kind 来自点表，位地址形态校验通过
    let p = PreparedWrite::build(r#"{"tag":"flag","value":true}"#, "devA", &idx).unwrap();
    assert_eq!((p.addr.as_str(), p.kind), ("M10.0", Kind::Bool));
    // address 契约：旧行为不变（显式 kind 与点表一致即可）
    let p = PreparedWrite::build(
        r#"{"address":"DB1.DBD0","value":1.5,"kind":"f32"}"#,
        "devA",
        &idx,
    )
    .unwrap();
    assert_eq!(p.kind, Kind::F32);
    // 跨设备唯一 tag 兜底（devB 的 temp 与 devA 重名，故这里用 devB 自己的）
    let p = PreparedWrite::build(r#"{"tag":"temp","value":1}"#, "devB", &idx).unwrap();
    assert_eq!((p.addr.as_str(), p.kind), ("DB2.DBW0", Kind::I16));
}

#[test]
fn write_prepare_rejects_missing_or_mismatched_kind() {
    let idx = test_index();
    // S-6：不在点表 + 未给 kind → Err（旧实现默认 I64 写 8 字节）
    let e = PreparedWrite::build(r#"{"address":"DB9.DBD0","value":42}"#, "devA", &idx).unwrap_err();
    assert!(e.contains("无法确定写入宽度"), "得到: {e}");
    // 不在点表但给了 kind：允许（原生客户端旧契约）
    let p = PreparedWrite::build(
        r#"{"address":"DB9.DBD0","value":42,"kind":"i16"}"#,
        "devA",
        &idx,
    )
    .unwrap();
    assert_eq!(p.kind, Kind::I16);
    // 显式 kind 与点表声明不符 → Err（i16 写 f32 点位会覆盖错宽度）
    let e = PreparedWrite::build(
        r#"{"address":"DB1.DBD0","value":1,"kind":"i16"}"#,
        "devA",
        &idx,
    )
    .unwrap_err();
    assert!(e.contains("不符"), "得到: {e}");
    // 缺 target / 缺 value
    assert!(PreparedWrite::build(r#"{"value":1}"#, "devA", &idx).is_err());
    assert!(PreparedWrite::build(r#"{"tag":"temp"}"#, "devA", &idx).is_err());
    // 坏 JSON
    assert!(PreparedWrite::build("{", "devA", &idx).is_err());
    // 位地址 + 数值 kind（S-12）
    assert!(PreparedWrite::build(
        r#"{"address":"DB1.DBX0.5","value":1,"kind":"u16"}"#,
        "devA",
        &idx,
    )
    .is_err());
}

#[test]
fn write_prepare_reports_ambiguous_tag() {
    let idx = test_index();
    // devC 不存在该 tag，且全表有两处 "temp" → 歧义拒绝
    let e = PreparedWrite::build(r#"{"tag":"temp","value":1}"#, "devC", &idx).unwrap_err();
    assert!(e.contains("无法确定目标"), "得到: {e}");
    // 全表都没有
    let e = PreparedWrite::build(r#"{"tag":"nope","value":1}"#, "devA", &idx).unwrap_err();
    assert!(e.contains("均无此点位"), "得到: {e}");
}

#[test]
fn write_prepare_string_length_rules() {
    let idx = test_index();
    // S-7：max_len 取点表声明的 [10]
    let p = PreparedWrite::build(r#"{"tag":"name","value":"abc"}"#, "devA", &idx).unwrap();
    assert_eq!(p.arg, 10);
    // 命令给更小的 max_len → 允许
    let p =
        PreparedWrite::build(r#"{"tag":"name","value":"abc","max_len":5}"#, "devA", &idx).unwrap();
    assert_eq!(p.arg, 5);
    // 命令给更大 → Err（旧实现按命令全长写，覆盖后 244 字节）
    let e = PreparedWrite::build(r#"{"tag":"name","value":"abc","max_len":20}"#, "devA", &idx)
        .unwrap_err();
    assert!(e.contains("超过点位声明的"), "得到: {e}");
    // 点表未声明长度 + 命令给 max_len → Err（提示点表补 [N]）
    let e = PreparedWrite::build(
        r#"{"address":"DB1.DBB60","value":"abc","kind":"string","max_len":10}"#,
        "devA",
        &idx,
    )
    .unwrap_err();
    assert!(e.contains("点表 kind 写 string[N]"), "得到: {e}");
    // 完全没有长度信息 → Err
    assert!(PreparedWrite::build(
        r#"{"address":"DB1.DBB60","value":"abc","kind":"string"}"#,
        "devA",
        &idx,
    )
    .is_err());
    // 命令显式 kind 自带 [N] 且点位未登记 → 允许
    let p = PreparedWrite::build(
        r#"{"address":"DB1.DBB60","value":"abc","kind":"string[6]"}"#,
        "devA",
        &idx,
    )
    .unwrap();
    assert_eq!(p.arg, 6);
    // 命令显式 [N] 超过点表声明 → Err
    assert!(PreparedWrite::build(
        r#"{"address":"DB1.DBB20","value":"abc","kind":"string[20]"}"#,
        "devA",
        &idx,
    )
    .is_err());
}

#[test]
fn write_index_addr_lookup_prefers_devid_and_checks_ambiguity() {
    let mut idx = WriteIndex::default();
    idx.insert(
        "devA",
        PointSpec {
            field: "t".into(),
            addr: "DB1.DBD0".into(),
            kind: Kind::F32,
            arg: 0,
            len_declared: false,
        },
    );
    idx.insert(
        "devB",
        PointSpec {
            field: "t".into(),
            addr: "DB1.DBD0".into(),
            kind: Kind::I16,
            arg: 0,
            len_declared: false,
        },
    );
    // 本设备命中优先
    assert_eq!(idx.by_addr("devA", "DB1.DBD0").unwrap().kind, Kind::F32);
    assert_eq!(idx.by_addr("devB", "DB1.DBD0").unwrap().kind, Kind::I16);
    // 跨设备同址且 kind 不一致 → 不推断（None），避免按错宽度写
    assert_eq!(idx.by_addr("devC", "DB1.DBD0"), None);
    // 未登记地址 → None
    assert_eq!(idx.by_addr("devA", "DB9.DBD0"), None);
}

// ---------- S-1：写后校验比较 ----------

#[test]
fn value_matches_rules() {
    // 整数精确（含 u64 全量，不经 f64 以免精度丢失）
    assert!(value_matches(
        Kind::U64,
        &SampleValue::UInt(u64::MAX),
        &SampleValue::UInt(u64::MAX)
    ));
    assert!(!value_matches(
        Kind::U64,
        &SampleValue::UInt(u64::MAX),
        &SampleValue::UInt(u64::MAX - 1)
    ));
    // 浮点相对误差 1e-6
    assert!(value_matches(
        Kind::F32,
        &SampleValue::Float(42.5),
        &SampleValue::Float(42.500_000_1)
    ));
    assert!(!value_matches(
        Kind::F64,
        &SampleValue::Float(1.0),
        &SampleValue::Float(1.01)
    ));
    // 0 附近不做除零式误判
    assert!(value_matches(
        Kind::F64,
        &SampleValue::Float(0.0),
        &SampleValue::Float(1e-9)
    ));
    // bool 按位比较
    assert!(value_matches(
        Kind::Bool,
        &SampleValue::Bool(true),
        &SampleValue::Bool(true)
    ));
    assert!(!value_matches(
        Kind::Bool,
        &SampleValue::Bool(true),
        &SampleValue::Bool(false)
    ));
    // 字符串 / raw 精确
    assert!(value_matches(
        Kind::String,
        &SampleValue::Str("abc".into()),
        &SampleValue::Str("abc".into())
    ));
    assert!(!value_matches(
        Kind::Raw(2),
        &SampleValue::Str("DEAD".into()),
        &SampleValue::Str("BEEF".into())
    ));
    // 类型不符
    assert!(!value_matches(
        Kind::I16,
        &SampleValue::Int(1),
        &SampleValue::Str("1".into())
    ));
}

// ---------- S-15 / F-7：限流 ----------

#[test]
fn warn_limiter_suppresses_within_window() {
    let mut w = WarnLimiter::new(Duration::from_secs(60));
    assert!(w.allow("dev.p1"), "首拍放行");
    assert!(!w.allow("dev.p1"), "同 key 窗口内抑制");
    assert!(w.allow("dev.p2"), "不同 key 独立放行");
    // 窗口极短时下一个窗口重新放行
    let mut w2 = WarnLimiter::new(Duration::from_millis(0));
    assert!(w2.allow("k"));
    assert!(w2.allow("k"));
}

#[test]
fn overrun_warn_is_rate_limited() {
    let mut o = OverrunWarn::default();
    let t0 = std::time::Instant::now();
    assert!(o.record(t0), "首拍告警");
    assert!(!o.record(t0 + Duration::from_secs(1)), "窗口内抑制");
    assert_eq!(o.count, 2, "计数不受限流影响");
    assert!(!o.record(t0 + Duration::from_secs(59)));
    assert!(o.record(t0 + Duration::from_secs(60)), "窗口到期重新告警");
    assert_eq!(o.count, 4);
}

// ---------- S-2 / S-3：真实 socket 闭环（协商 PDU + 拆连接不触发热重连） ----------

/// 构造 S7 响应字节：回填 header 长度字段 + TPKT 总长。
///
/// `S7Data::self_check` 是私有的，手搓响应的 header 长度字段缺省为 0，而 client 端
/// `from_be_bytes` 靠这两个字段定位 datum——必须显式设置（同 tests/s7_e2e.rs）。
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

/// AckData 响应头（库对 AckData 恒按 12 字节解析，必须真 12 字节且 NoError）。
fn ack_header() -> S7Header {
    let mut h = S7Header::new(S7MessageType::AckData);
    h.error_class = Some(S7ErrorClass::NoError);
    h.error_code = Some(0x00);
    h
}

/// COTP CR 的应答：ConnectConfirm（client 只校验 pdu_type）。
fn build_cotp_cc() -> Vec<u8> {
    finalize(S7Data {
        tpkt: TPKT::default(),
        cotp: Some(Cotp::CotpConnection(CotpConnection {
            pdu_type: PduType::ConnectConfirm,
            ..CotpConnection::default()
        })),
        header: None,
        parameter: None,
        datum: None,
    })
}

/// Setup Communication 应答：协商 PDU 由调用方指定。
fn build_setup_response(pdu: u16) -> Vec<u8> {
    finalize(S7Data {
        tpkt: TPKT::default(),
        cotp: Some(Cotp::CotpData(tokio_s7::cotp::CotpData::default())),
        header: Some(ack_header()),
        parameter: Some(S7Parameter::SetupParameter(SetupComParameter::new(pdu))),
        datum: None,
    })
}

/// 最小 S7 mock：只应答 COTP CR（→CC）与 Setup Communication（协商 PDU 可控），
/// 收到 COTP DR/DC（0x80/0xC0）即关连接。返回 `(host, port, accept 计数, 取消令牌)`。
async fn spawn_min_mock(pdu: u16) -> (String, u16, Arc<AtomicUsize>, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let counter = accepts.clone();
    let token = CancellationToken::new();
    let tk = token.clone();
    tokio::spawn(async move {
        loop {
            let conn = tokio::select! {
                _ = tk.cancelled() => break,
                r = listener.accept() => r,
            };
            let Ok((stream, _)) = conn else { break };
            counter.fetch_add(1, Ordering::SeqCst);
            let tkc = tk.clone();
            tokio::spawn(async move {
                let mut framed = tokio_util::codec::Framed::new(stream, S7DataCodec);
                loop {
                    let frame = tokio::select! {
                        _ = tkc.cancelled() => break,
                        f = framed.next() => f,
                    };
                    let Some(Ok(raw)) = frame else { break };
                    if raw.len() < 6 {
                        break;
                    }
                    match raw[5] {
                        0xE0 => {
                            let _ = framed.send(build_cotp_cc()).await;
                        }
                        // COTP DR/DC → 关连接（disconnect 的收尾）。
                        0x80 | 0xC0 => break,
                        // DtData：Setup Communication 的 param 首字节是 0xF0。
                        0xF0 if raw.len() >= 18 && raw[17] == 0xF0 => {
                            let _ = framed.send(build_setup_response(pdu)).await;
                        }
                        _ => break,
                    }
                }
            });
        }
    });
    (addr.ip().to_string(), addr.port(), accepts, token)
}

/// S-3 + S-2 闭环：协商 PDU 被读出并换算成每请求项数；disconnect 之后 drop
/// **不得**触发库 actor 的无间隔热重连（未修时 0.6s 建连 2000+ 次）。
#[tokio::test]
async fn connect_reads_negotiated_pdu_and_shutdown_stops_actor() {
    let (host, port, accepts, token) = spawn_min_mock(240).await;
    let cfg = parse_cfg(&serde_json::json!({"endpoint": host, "port": port})).unwrap();
    let (mut c, pdu) = connect(&cfg)
        .await
        .expect("connect 应成功（mock 应答 CR+Setup）");
    assert_eq!(pdu, 240, "S-3：协商 PDU 必须从 S7Client.pdu_length 读出");
    assert_eq!(
        max_items_per_request(pdu, cfg.max_items_per_request),
        18,
        "S-3：240B PDU → 18 项/请求（旧实现固定 20 项必超限）"
    );
    assert!(
        shutdown_conn(&mut c, "ut").await,
        "disconnect 应拿到 actor 正常退出的回执"
    );
    drop(c);
    // S-2 核心断言：disconnect 之后 drop 不再建连（库热重连的直接观测）。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        1,
        "drop 已 disconnect 的 client 后不得出现新连接（库 actor 热重连）"
    );
    // 生产重连路径：shutdown → drop → 新建 client。
    let (mut c2, pdu2) = connect(&cfg).await.expect("重连应成功");
    assert_eq!(pdu2, 240);
    assert!(shutdown_conn(&mut c2, "ut").await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(accepts.load(Ordering::SeqCst), 2, "重连只应新建 1 条连接");
    token.cancel();
}
