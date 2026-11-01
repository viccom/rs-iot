//! `opcuaClient` 单元测试（纯函数面）。
//!
//! 协议语义审查（`docs/designs/2026-11-01-protocol-semantics-audit.md` §4）修复的
//! 回归锁：OPC-1/2/3/4/6/7/8/10/12/14/15/16/17 + F-1/F-2/F-7。
//!
//! 网络路径（connect/read/write 的真实往返、超时、断连）无法在单测里覆盖，
//! 这里只锁可判定的纯函数与状态机判定；边界见各测试注释与报告。

use super::convert::{
    data_value_bad_status, data_value_to_sample, json_to_sample_value, sample_value_to_variant,
    variant_to_sample, variant_with_type, OpcType,
};
use super::node::{parse_node_spec, resolve_write_target, RawDevice, RawPoint};
use super::*;
use opcua::types::{
    AttributeId, DataValue, EndpointDescription, MessageSecurityMode, MonitoringMode, NamespaceMap,
    NodeId, NumericRange, StatusCode, Variant,
};
use std::time::Duration;

// ---------- 测试辅助 ----------

/// 构造命名空间表（`NamespaceMap::new_full` 吃 hashbrown 的 HashMap，插件侧不引该依赖，
/// 故按 `new()` + `add_namespace` 顺序注入；索引由库分配，断言用 get_index 回读）。
fn ns_map(uris: &[&str]) -> NamespaceMap {
    let mut map = NamespaceMap::new();
    for uri in uris {
        map.add_namespace(uri);
    }
    map
}

fn raw_device(devid: &str, points: &[(&str, &str, &str)]) -> RawDevice {
    RawDevice {
        devid: devid.to_string(),
        points: points
            .iter()
            .map(|(field, raw, ty)| RawPoint {
                field: (*field).to_string(),
                raw: (*raw).to_string(),
                declared_type: (*ty).to_string(),
            })
            .collect(),
    }
}

fn dv(value: Option<Variant>, status: Option<StatusCode>) -> DataValue {
    DataValue {
        value,
        status,
        ..Default::default()
    }
}

// ---------- OPC-1 坏质量 → Null ----------

#[test]
fn data_value_to_sample_marks_bad_quality_as_null() {
    // Good（无 status 字段按规范视为 Good）→ 正常值
    assert_eq!(
        data_value_to_sample(&dv(Some(Variant::Int32(7)), None)),
        SampleValue::Int(7)
    );
    // 显式 Good → 正常值
    assert_eq!(
        data_value_to_sample(&dv(Some(Variant::Double(1.5)), Some(StatusCode::Good))),
        SampleValue::Float(1.5)
    );
    // 坏状态即使带 value 也不得投递（此前静默丢弃 + warn，现统一 Null 标记）
    assert_eq!(
        data_value_to_sample(&dv(Some(Variant::Int32(7)), Some(StatusCode::BadTimeout))),
        SampleValue::Null
    );
    // 无 value（服务端未返回）→ Null
    assert_eq!(data_value_to_sample(&dv(None, None)), SampleValue::Null);
    // Uncertain 也非 Good → Null（不是有效测量值）
    assert_eq!(
        data_value_to_sample(&dv(Some(Variant::Int32(1)), Some(StatusCode::Uncertain))),
        SampleValue::Null
    );
}

#[test]
fn data_value_bad_status_reports_only_non_good() {
    assert!(data_value_bad_status(&dv(Some(Variant::Int32(1)), None)).is_none());
    assert!(data_value_bad_status(&dv(Some(Variant::Int32(1)), Some(StatusCode::Good))).is_none());
    let st = data_value_bad_status(&dv(
        Some(Variant::Int32(1)),
        Some(StatusCode::BadNodeIdUnknown),
    ))
    .expect("坏状态必须上报");
    assert_eq!(st, StatusCode::BadNodeIdUnknown);
}

#[test]
fn round_status_flags_all_bad_quality_without_empty_sample() {
    // 整轮全坏 → tick_fail（F-9：不再投递空 Sample，由调用方按该错误计失败）
    assert_eq!(
        round_status(3, 0, None).unwrap_err(),
        "all nodes bad quality"
    );
    // 部分好 → tick 成功（坏点已用 Null 显式标记）
    assert!(round_status(3, 1, None).is_ok());
    // 无点位（空点表）不算失败
    assert!(round_status(0, 0, None).is_ok());
    // 有读失败但仍有好点 → 失败可见化（错误文本透传）
    assert_eq!(
        round_status(3, 2, Some("read batch failed")).unwrap_err(),
        "read batch failed"
    );
}

// ---------- OPC-2 分批 ----------

#[test]
fn chunk_nodes_splits_by_server_limit() {
    assert_eq!(chunk_nodes(0, 10), Vec::<std::ops::Range<usize>>::new());
    assert_eq!(chunk_nodes(5, 10), vec![0..5]);
    assert_eq!(chunk_nodes(10, 10), vec![0..10]);
    assert_eq!(chunk_nodes(11, 10), vec![0..10, 10..11]);
    assert_eq!(chunk_nodes(25, 10), vec![0..10, 10..20, 20..25]);
    // 服务端声明 0/异常 → 按 1 兜底，绝不产生空批次死循环
    assert_eq!(chunk_nodes(3, 0), vec![0..1, 1..2, 2..3]);
}

#[test]
fn transport_status_classification() {
    // 传输层/会话层错误 → 拆会话重连
    for st in [
        StatusCode::BadConnectionClosed,
        StatusCode::BadSecureChannelClosed,
        StatusCode::BadSessionClosed,
        StatusCode::BadSessionIdInvalid,
        StatusCode::BadTimeout,
        StatusCode::BadCommunicationError,
        StatusCode::BadServerNotConnected,
        StatusCode::BadRequestInterrupted,
    ] {
        assert!(is_transport_status(st), "{st} 应判为传输层错误");
    }
    // 单批业务错误（如节点不存在/批量过大）不拆会话
    for st in [
        StatusCode::BadNodeIdUnknown,
        StatusCode::BadTooManyOperations,
        StatusCode::BadNothingToDo,
    ] {
        assert!(!is_transport_status(st), "{st} 不应拆会话");
    }
}

#[test]
fn variant_as_u64_reads_operation_limit() {
    assert_eq!(variant_as_u64(&Variant::UInt32(500)), Some(500));
    assert_eq!(variant_as_u64(&Variant::UInt16(7)), Some(7));
    assert_eq!(variant_as_u64(&Variant::UInt64(9)), Some(9));
    assert_eq!(variant_as_u64(&Variant::Byte(3)), Some(3));
    assert_eq!(variant_as_u64(&Variant::Int32(12)), Some(12));
    assert_eq!(variant_as_u64(&Variant::Int32(-1)), None);
    assert_eq!(variant_as_u64(&Variant::String("x".into())), None);
}

// ---------- OPC-4 Display ----------

#[test]
fn variant_round_trips_to_sample() {
    // 读方向基础类型映射不变（签名从 Option 改为必返回值，坏质量判定上移到
    // data_value_to_sample：转换本身恒有输出）
    assert_eq!(
        variant_to_sample(&Variant::Double(1.5)),
        SampleValue::Float(1.5)
    );
    assert_eq!(variant_to_sample(&Variant::Int64(-7)), SampleValue::Int(-7));
    assert_eq!(
        variant_to_sample(&Variant::Boolean(true)),
        SampleValue::Bool(true)
    );
    assert_eq!(
        variant_to_sample(&Variant::String("hi".into())),
        SampleValue::Str("hi".into())
    );
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
fn variant_to_sample_uses_display_for_complex_types() {
    // 数组（非标量）此前输出 `Variant::Array(...)` 的 Debug 噪音
    let array = Variant::from(vec![1i32, 2, 3]);
    match variant_to_sample(&array) {
        SampleValue::Str(s) => {
            assert!(!s.contains("Variant::"), "不得带 Variant:: 前缀: {s}");
            assert!(s.contains("Array"), "应为可读数组形式: {s}");
        }
        other => panic!("非标量应转 Str，实际 {other:?}"),
    }
}

// ---------- OPC-6 写值类型安全 ----------

#[test]
fn variant_with_type_rejects_out_of_range_values() {
    // byte 0-255
    assert_eq!(
        variant_with_type(&SampleValue::Int(255), Some("Byte")).unwrap(),
        Variant::Byte(255)
    );
    assert!(variant_with_type(&SampleValue::Int(256), Some("Byte")).is_err());
    assert!(variant_with_type(&SampleValue::Int(-1), Some("Byte")).is_err());
    // sbyte -128..127
    assert_eq!(
        variant_with_type(&SampleValue::Int(-128), Some("SByte")).unwrap(),
        Variant::SByte(-128)
    );
    assert!(variant_with_type(&SampleValue::Int(128), Some("SByte")).is_err());
    // int16 / uint16
    assert_eq!(
        variant_with_type(&SampleValue::Int(-32768), Some("Int16")).unwrap(),
        Variant::Int16(-32768)
    );
    assert!(variant_with_type(&SampleValue::Int(32768), Some("Int16")).is_err());
    assert_eq!(
        variant_with_type(&SampleValue::Int(65535), Some("UInt16")).unwrap(),
        Variant::UInt16(65535)
    );
    assert!(variant_with_type(&SampleValue::Int(65536), Some("UInt16")).is_err());
    // int32 / uint32
    assert!(variant_with_type(&SampleValue::Int(2_147_483_648), Some("Int32")).is_err());
    assert!(variant_with_type(&SampleValue::Int(-1), Some("UInt32")).is_err());
    // uint64 负数拒写
    assert!(variant_with_type(&SampleValue::Int(-1), Some("UInt64")).is_err());
    // float 溢出 f32 拒写（1e300 → inf）
    assert!(variant_with_type(&SampleValue::Float(1e300), Some("Float")).is_err());
    assert!(variant_with_type(&SampleValue::Float(1e300), Some("Double")).is_ok());
    // 带小数的浮点写整型节点：拒写（不静默截断）
    assert!(variant_with_type(&SampleValue::Float(1.5), Some("Int32")).is_err());
    assert_eq!(
        variant_with_type(&SampleValue::Float(2.0), Some("Int32")).unwrap(),
        Variant::Int32(2)
    );
}

#[test]
fn variant_with_type_defaults_and_legacy_guards() {
    // 无 opc_type → 默认 Int64（既有行为，保留）
    assert_eq!(
        variant_with_type(&SampleValue::Int(9), None).unwrap(),
        Variant::Int64(9)
    );
    // 空 opc_type → 默认 Double（既有行为，保留）
    assert_eq!(
        variant_with_type(&SampleValue::Float(2.0), Some("")).unwrap(),
        Variant::Double(2.0)
    );
    // D-4：未知 opc_type 拒写（此前静默回退默认——"It32" 拼写错误按 Int64 写）
    assert!(
        variant_with_type(&SampleValue::Int(1), Some("unknown")).is_err(),
        "未知 opc_type 必须 Err"
    );
    // D-4：非数字串拒写（此前 unwrap_or(0) 静默写 0）
    assert!(
        variant_with_type(&SampleValue::Str("1O".into()), Some("Int32")).is_err(),
        "非数字字符串必须 Err"
    );
    // 数字串照常接受（兼容下行 JSON 值为字符串数字的既有用法）
    assert_eq!(
        variant_with_type(&SampleValue::Str("42".into()), Some("Int32")).unwrap(),
        Variant::Int32(42)
    );
    // string 类型 + 数值 → 可读串（对齐 eip A-1：不写 Debug 字面 "Int(5)"）
    assert_eq!(
        variant_with_type(&SampleValue::Int(5), Some("String")).unwrap(),
        Variant::String("5".into())
    );
}

#[test]
fn variant_with_type_bool_strings_and_aliases() {
    for s in ["true", "TRUE", "1", "on", "ON"] {
        assert_eq!(
            variant_with_type(&SampleValue::Str(s.into()), Some("Boolean")).unwrap(),
            Variant::Boolean(true),
            "{s} 应为 true"
        );
    }
    for s in ["false", "FALSE", "0", "off", "Off"] {
        assert_eq!(
            variant_with_type(&SampleValue::Str(s.into()), Some("Boolean")).unwrap(),
            Variant::Boolean(false),
            "{s} 应为 false"
        );
    }
    // 无法判定的布尔串拒写（不静默写 false）
    assert!(variant_with_type(&SampleValue::Str("yes".into()), Some("Boolean")).is_err());

    // 点表词表与 OPC 词表别名（OPC-7）
    assert_eq!(OpcType::parse("u32").unwrap(), OpcType::UInt32);
    assert_eq!(OpcType::parse("U16").unwrap(), OpcType::UInt16);
    assert_eq!(OpcType::parse("i32").unwrap(), OpcType::Int32);
    assert_eq!(OpcType::parse("i64").unwrap(), OpcType::Int64);
    assert_eq!(OpcType::parse("f32").unwrap(), OpcType::Float);
    assert_eq!(OpcType::parse("f64").unwrap(), OpcType::Double);
    assert_eq!(OpcType::parse("bool").unwrap(), OpcType::Boolean);
    assert_eq!(OpcType::parse("string").unwrap(), OpcType::String);
    assert_eq!(OpcType::parse("uint").unwrap(), OpcType::UInt32);
    assert_eq!(OpcType::parse("int").unwrap(), OpcType::Int32);
    assert!(OpcType::parse("u33").is_err());
}

#[test]
fn variant_with_type_rejects_null_value() {
    // OPC-17：写 Null 一律拒写（此前无 opc_type 时会写成 Variant::Empty）
    assert!(variant_with_type(&SampleValue::Null, None).is_err());
    assert!(variant_with_type(&SampleValue::Null, Some("Int32")).is_err());
}

// ---------- OPC-17 JSON → SampleValue ----------

#[test]
fn json_to_sample_value_null_and_large_integers() {
    // null 不再写成字符串 "null"
    assert_eq!(
        json_to_sample_value(&serde_json::Value::Null),
        SampleValue::Null
    );
    // 正整数走 u64（>i64::MAX 不丢精度）——既有断言 42→Int 随之调整为 UInt（有意变更）
    assert_eq!(
        json_to_sample_value(&serde_json::json!(42)),
        SampleValue::UInt(42)
    );
    assert_eq!(
        json_to_sample_value(&serde_json::json!(u64::MAX)),
        SampleValue::UInt(u64::MAX)
    );
    // 负数走 i64
    assert_eq!(
        json_to_sample_value(&serde_json::json!(-7)),
        SampleValue::Int(-7)
    );
    // 小数走 f64
    assert_eq!(
        json_to_sample_value(&serde_json::json!(1.5)),
        SampleValue::Float(1.5)
    );
    // 字符串/布尔
    assert_eq!(
        json_to_sample_value(&serde_json::json!("x")),
        SampleValue::Str("x".into())
    );
    assert_eq!(
        json_to_sample_value(&serde_json::json!(true)),
        SampleValue::Bool(true)
    );
}

// ---------- OPC-8 NodeId 解析 / OPC-15 下标范围 ----------

#[test]
fn parse_node_id_accepts_standard_forms() {
    assert_eq!(
        parse_node_spec("ns=2;s=v1", None).unwrap().node_id,
        NodeId::new(2, "v1")
    );
    assert_eq!(
        parse_node_spec("i=2258", None).unwrap().node_id,
        NodeId::new(0, 2258u32)
    );
    assert_eq!(
        parse_node_spec("ns=0;i=2258", None).unwrap().node_id,
        NodeId::new(0, 2258u32)
    );
    // 前缀大小写不敏感 + 前后空白（OPC-8）
    assert_eq!(
        parse_node_spec("  NS=2 ; S = v1  ", None).unwrap().node_id,
        NodeId::new(2, "v1")
    );
    assert!(parse_node_spec("not a node", None).is_err());
}

#[test]
fn parse_node_spec_invalid_keeps_raw_string_in_error() {
    let err = parse_node_spec("  ns=2;x=v1 ", None).unwrap_err();
    assert!(err.contains("ns=2;x=v1"), "错误信息须带原始串: {err}");
    let err = parse_node_spec("", None).unwrap_err();
    assert!(err.contains("空"), "空串错误可读: {err}");
}

#[test]
fn parse_node_spec_index_range_syntax() {
    // OPC-15：尾部 [n] / [a:b] 解析为 index_range
    let spec = parse_node_spec("ns=2;s=arr[3]", None).unwrap();
    assert_eq!(spec.node_id, NodeId::new(2, "arr"));
    assert_eq!(spec.index_range, NumericRange::Index(3));

    let spec = parse_node_spec("ns=2;s=arr[1:4]", None).unwrap();
    assert_eq!(spec.node_id, NodeId::new(2, "arr"));
    assert_eq!(spec.index_range, NumericRange::Range(1, 4));

    // 无范围 → None（其余保持空）
    let spec = parse_node_spec("ns=2;s=arr", None).unwrap();
    assert_eq!(spec.index_range, NumericRange::None);

    // 方括号内容不是合法 NumericRange → 属于 NodeId 字面本身，不截断
    let spec = parse_node_spec("ns=2;s=abc[]", None).unwrap();
    assert_eq!(spec.node_id, NodeId::new(2, "abc[]"));
    assert_eq!(spec.index_range, NumericRange::None);
}

#[test]
fn read_and_write_items_carry_index_range() {
    // OPC-15：下标范围读写都用（不只是解析出来放着）
    let spec = parse_node_spec("ns=2;s=arr[1:2]", None).unwrap();
    let rv = read_value_id(&spec);
    assert_eq!(rv.index_range, NumericRange::Range(1, 2));
    assert_eq!(rv.attribute_id, AttributeId::Value as u32);
    let wv = write_value(&spec, Variant::Int32(1));
    assert_eq!(wv.index_range, NumericRange::Range(1, 2));
    assert_eq!(wv.attribute_id, AttributeId::Value as u32);
    assert_eq!(wv.value.value, Some(Variant::Int32(1)));

    // 无范围 → 空（其余点位不受影响）
    let plain = parse_node_spec("ns=2;s=arr", None).unwrap();
    assert!(read_value_id(&plain).index_range.is_none());
    assert!(write_value(&plain, Variant::Int32(1)).index_range.is_none());
}

#[test]
fn build_prepared_resolves_nsu_and_skips_bad_nodes() {
    // 连接期装配：nsu= 用注入的命名空间表解析；坏地址跳过但同设备其余点保留
    let map = ns_map(&["urn:rs:test"]);
    let ns = map.get_index("urn:rs:test").unwrap();
    let raw = vec![raw_device(
        "d1",
        &[
            ("a", "nsu=urn:rs:test;s=A", "f64"),
            ("b", "bad node", "f64"),
            ("c", "ns=2;s=C", "bool"),
        ],
    )];
    let prep = build_prepared(&raw, Some(&map), 500, "inst-test");
    assert_eq!(prep.max_nodes_per_read, 500);
    assert_eq!(prep.devices.len(), 1);
    let points = &prep.devices[0].points;
    assert_eq!(points.len(), 2, "坏地址被跳过: {points:?}");
    assert_eq!(points[0].0, "a");
    assert_eq!(points[0].1.node_id, NodeId::new(ns, "A"));
    assert_eq!(points[1].0, "c");
}

#[test]
fn parse_node_spec_nsu_resolves_via_namespace_array() {
    let map = ns_map(&["urn:rs:test", "urn:other"]);
    let ns_test = map
        .get_index("urn:rs:test")
        .expect("测试表须含 urn:rs:test");
    let ns_other = map.get_index("urn:other").expect("测试表须含 urn:other");
    // nsu= URI 经命名空间表解析成索引
    let spec = parse_node_spec("nsu=urn:rs:test;s=Pump1", Some(&map)).unwrap();
    assert_eq!(spec.node_id, NodeId::new(ns_test, "Pump1"));
    // 与 ns= 混用大小写/空白
    let spec = parse_node_spec(" NSU = urn:other ; I = 42 ", Some(&map)).unwrap();
    assert_eq!(spec.node_id, NodeId::new(ns_other, 42u32));

    // 无命名空间表（未连接）→ 明确 Err 且带原始串
    let err = parse_node_spec("nsu=urn:rs:test;s=x", None).unwrap_err();
    assert!(err.contains("nsu=urn:rs:test;s=x"), "{err}");
    // URI 不在 NamespaceArray
    let err = parse_node_spec("nsu=urn:missing;s=x", Some(&map)).unwrap_err();
    assert!(err.contains("urn:missing"), "{err}");
}

// ---------- F-1 写目标契约（tag → 点位） ----------

#[test]
fn resolve_write_target_prefers_node_id() {
    let devices = vec![raw_device(
        "d1",
        &[
            ("temp", "ns=2;s=Temp", "f64"),
            ("pump", "ns=2;s=Pump", "bool"),
        ],
    )];
    // node_id 优先，即便同时带 tag；声明的 opc_type 从点表回查
    let t = resolve_write_target(&devices, "d1", Some("ns=2;s=Temp"), Some("pump"), None).unwrap();
    assert_eq!(t.spec.node_id, NodeId::new(2, "Temp"));
    assert_eq!(t.opc_type.as_deref(), Some("f64"));
    // node_id 不在点表 → 无默认类型（仍可写，由命令显式 opc_type 兜底）
    let t = resolve_write_target(&devices, "d1", Some("ns=2;s=Other"), None, None).unwrap();
    assert!(t.opc_type.is_none());
}

#[test]
fn resolve_write_target_resolves_tag_across_devids() {
    let devices = vec![
        raw_device("d1", &[("temp", "ns=2;s=Temp", "f64")]),
        raw_device("d2", &[("flow", "ns=2;s=Flow", "f32")]),
    ];
    // 目标 devid 命中
    let t = resolve_write_target(&devices, "d2", None, Some("flow"), None).unwrap();
    assert_eq!(t.spec.node_id, NodeId::new(2, "Flow"));
    assert_eq!(t.opc_type.as_deref(), Some("f32"));
    // 跨 devid 唯一 tag 兜底（HMI 只发 tag，devid 可能不匹配）
    let t = resolve_write_target(&devices, "dX", None, Some("temp"), None).unwrap();
    assert_eq!(t.spec.node_id, NodeId::new(2, "Temp"));
    // 找不到
    let err = resolve_write_target(&devices, "d1", None, Some("nope"), None).unwrap_err();
    assert!(err.contains("nope"), "{err}");
    // 缺 node_id/tag
    assert!(resolve_write_target(&devices, "d1", None, None, None).is_err());
}

#[test]
fn resolve_write_target_rejects_ambiguous_tag() {
    let devices = vec![
        raw_device("d1", &[("temp", "ns=2;s=Temp1", "f64")]),
        raw_device("d2", &[("temp", "ns=2;s=Temp2", "f64")]),
    ];
    // 两个设备都叫 temp 且目标 devid 不匹配 → 歧义必须 Err（不能随便挑一个）
    let err = resolve_write_target(&devices, "d9", None, Some("temp"), None).unwrap_err();
    assert!(err.contains("重名"), "{err}");
    // 目标 devid 明确时按该设备解析
    let t = resolve_write_target(&devices, "d2", None, Some("temp"), None).unwrap();
    assert_eq!(t.spec.node_id, NodeId::new(2, "Temp2"));
}

#[test]
fn build_write_request_composes_tag_type_and_index_range() {
    // 命令层组合契约（F-1 + OPC-6/7/15/17）：不触碰网络
    let devices = vec![raw_device(
        "d1",
        &[
            ("temp", "ns=2;s=Temp", "f64"),
            ("pump", "ns=2;s=Pump", "bool"),
            ("arr", "ns=2;s=Arr[1:3]", "f32"),
        ],
    )];
    // HMI 只发 {tag,value} → 点表声明类型作为默认（f64 → Double）
    let wv = build_write_request(
        &serde_json::json!({"tag":"temp","value":42}),
        "d1",
        &devices,
        None,
    )
    .unwrap();
    assert_eq!(wv.node_id, NodeId::new(2, "Temp"));
    assert_eq!(wv.value.value, Some(Variant::Double(42.0)));

    // 布尔串经点表 bool 类型（on/off 别名）
    let wv = build_write_request(
        &serde_json::json!({"tag":"pump","value":"off"}),
        "d1",
        &devices,
        None,
    )
    .unwrap();
    assert_eq!(wv.value.value, Some(Variant::Boolean(false)));

    // 命令 opc_type 覆盖点表类型
    let wv = build_write_request(
        &serde_json::json!({"tag":"temp","value":7,"opc_type":"int32"}),
        "d1",
        &devices,
        None,
    )
    .unwrap();
    assert_eq!(wv.value.value, Some(Variant::Int32(7)));

    // 数组下标范围透传到 WriteValue（OPC-15）
    let wv = build_write_request(
        &serde_json::json!({"tag":"arr","value":1.5}),
        "d1",
        &devices,
        None,
    )
    .unwrap();
    assert_eq!(wv.index_range, NumericRange::Range(1, 3));

    // null 值 / 缺目标 / 越界值：命令层即拒（不产生网络写）
    assert!(build_write_request(
        &serde_json::json!({"tag":"temp","value":null}),
        "d1",
        &devices,
        None
    )
    .is_err());
    assert!(build_write_request(&serde_json::json!({"value":1}), "d1", &devices, None).is_err());
    let devices_u8 = vec![raw_device("d1", &[("b", "ns=2;s=B", "byte")])];
    assert!(build_write_request(
        &serde_json::json!({"tag":"b","value":300}),
        "d1",
        &devices_u8,
        None
    )
    .is_err());
}

// ---------- OPC-10/11/12 配置 ----------

#[test]
fn reconnect_count_excludes_first_connect() {
    // OPC-13：首连（connects=1）不是重连；第 2 次起 +1
    assert_eq!(reconnects_after_connect(1), 0);
    assert_eq!(reconnects_after_connect(2), 1);
    assert_eq!(reconnects_after_connect(5), 4);
}

#[test]
fn over_period_detection() {
    // F-7：严格等于周期不算超时（interval 正常节奏）
    assert!(!is_over_period(
        Duration::from_secs(2),
        Duration::from_secs(3)
    ));
    assert!(!is_over_period(
        Duration::from_secs(3),
        Duration::from_secs(3)
    ));
    assert!(is_over_period(
        Duration::from_secs(4),
        Duration::from_secs(3)
    ));
}

#[test]
fn uses_nsu_detects_namespace_uri_form() {
    // 只有 nsu= 形式依赖服务端 NamespaceArray（决定读表失败是否算采集失败）
    assert!(uses_nsu("nsu=urn:x;s=A"));
    assert!(uses_nsu("  NSU = urn:x ; s = A "));
    assert!(!uses_nsu("ns=2;s=A"));
    assert!(!uses_nsu("i=2258"));
    assert!(!uses_nsu("nsu_not_really"));
}

#[test]
fn parse_cfg_defaults_and_timeout_range() {
    let cfg = parse_cfg(&serde_json::json!({"endpoint":"opc.tcp://x:1234"})).unwrap();
    assert_eq!(cfg.endpoint, "opc.tcp://x:1234");
    assert_eq!(cfg.security_policy, "none");
    assert_eq!(cfg.period, 3);
    assert!(cfg.username.is_none());
    assert!(!cfg.verify_server, "verify_server defaults to false");
    // OPC-10：读写请求超时默认 10s，范围 100..120000 夹紧
    assert_eq!(cfg.request_timeout_ms, 10_000);
    // OPC-11：trust_server_certs 默认 true（兼容旧行为）
    assert!(cfg.trust_server_certs);

    let cfg2 = parse_cfg(&serde_json::json!({"endpoint":"x","period":0})).unwrap();
    assert_eq!(cfg2.period, 1, "period 0 clamped to 1");
    assert!(
        parse_cfg(&serde_json::json!({})).is_err(),
        "missing endpoint errors"
    );

    let cfg3 = parse_cfg(&serde_json::json!({"endpoint":"x","request_timeout_ms":50})).unwrap();
    assert_eq!(cfg3.request_timeout_ms, 100, "低于下限夹到 100ms");
    let cfg4 = parse_cfg(&serde_json::json!({"endpoint":"x","request_timeout_ms":999999})).unwrap();
    assert_eq!(cfg4.request_timeout_ms, 120_000, "高于上限夹到 120s");
    let cfg5 = parse_cfg(&serde_json::json!({"endpoint":"x","request_timeout_ms":3000})).unwrap();
    assert_eq!(cfg5.request_timeout_ms, 3000);
}

#[test]
fn parse_security_supports_all_six_policies() {
    let cases = [
        ("none", SecurityPolicy::None, MessageSecurityMode::None),
        (
            "Basic128Rsa15",
            SecurityPolicy::Basic128Rsa15,
            MessageSecurityMode::SignAndEncrypt,
        ),
        (
            "basic256",
            SecurityPolicy::Basic256,
            MessageSecurityMode::SignAndEncrypt,
        ),
        (
            "Basic256Sha256",
            SecurityPolicy::Basic256Sha256,
            MessageSecurityMode::SignAndEncrypt,
        ),
        (
            "Aes128_Sha256_RsaOaep",
            SecurityPolicy::Aes128Sha256RsaOaep,
            MessageSecurityMode::SignAndEncrypt,
        ),
        (
            "aes256sha256rsapss",
            SecurityPolicy::Aes256Sha256RsaPss,
            MessageSecurityMode::SignAndEncrypt,
        ),
    ];
    for (s, policy, mode) in cases {
        let (p, m) = parse_security(s).unwrap();
        assert_eq!(p, policy, "{s} 策略映射错误");
        assert_eq!(m, mode, "{s} 安全模式固定 SignAndEncrypt");
    }
    // 空串等价 none；库的 URI 形式也接受
    assert_eq!(parse_security("").unwrap().0, SecurityPolicy::None);
    assert_eq!(
        parse_security("http://opcfoundation.org/UA/SecurityPolicy#Basic256Sha256")
            .unwrap()
            .0,
        SecurityPolicy::Basic256Sha256
    );
    assert!(parse_security("unknown").is_err());
}

// ---------- OPC-14 端点列表 ----------

#[test]
fn format_endpoint_list_truncates_and_dedups() {
    let mk = |policy: &str, mode: MessageSecurityMode| EndpointDescription {
        security_policy_uri: policy.into(),
        security_mode: mode,
        ..Default::default()
    };
    let eps = vec![
        mk("None", MessageSecurityMode::None),
        mk("None", MessageSecurityMode::None),
        mk("Basic256Sha256", MessageSecurityMode::SignAndEncrypt),
    ];
    let s = format_endpoint_list(&eps, 10);
    assert!(s.contains("None/None"), "{s}");
    assert!(s.contains("Basic256Sha256/SignAndEncrypt"), "{s}");
    // 去重：None/None 只出现一次
    assert_eq!(s.matches("None/None").count(), 1, "{s}");

    // 截断到上限并提示剩余数量
    let many: Vec<_> = (0..12)
        .map(|i| mk(&format!("P{i}"), MessageSecurityMode::None))
        .collect();
    let s = format_endpoint_list(&many, 10);
    assert_eq!(s.matches("P").count(), 10, "只列前 10 条: {s}");
    assert!(s.contains("…2"), "应提示被截断数量: {s}");
    // 空列表
    assert!(format_endpoint_list(&[], 10).is_empty());
}

// ---------- OPC-16 日志限流 ----------

#[test]
fn rate_limit_suppresses_repeats_within_window() {
    let mut rl = RateLimit::new(Duration::from_secs(60));
    assert!(rl.allow("d1.temp"));
    assert!(!rl.allow("d1.temp"), "窗口内同一点位第二拍不再打日志");
    assert!(rl.allow("d1.flow"), "不同点位各自独立");
    // 窗口外的键被裁剪后允许（用 0 窗口模拟过期）
    let mut rl0 = RateLimit::new(Duration::ZERO);
    assert!(rl0.allow("k"));
    assert!(rl0.allow("k"), "零窗口 = 不抑制");
}

// ---------- OPC-9 Subscription（订阅/monitored items） ----------

use super::subscription::{
    aggregate_notifications, build_monitored_items, fallback_to_polling, watchdog_expired,
    AcquisitionMode, HandleAllocator,
};
use opcua::types::Error;

#[test]
fn parse_cfg_subscription_defaults_and_clamps() {
    // 缺省：auto 模式 + 订阅参数缺省值
    let cfg = parse_cfg(&serde_json::json!({"endpoint":"opc.tcp://x:1234"})).unwrap();
    assert_eq!(cfg.acquisition_mode, AcquisitionMode::Auto);
    assert_eq!(cfg.subscription.publishing_interval_ms, 1000);
    assert_eq!(cfg.subscription.sampling_interval_ms, 0);
    assert_eq!(cfg.subscription.queue_size, 1);
    assert_eq!(cfg.subscription.watchdog_secs, 60);

    // 三种模式词表
    for (word, mode) in [
        ("auto", AcquisitionMode::Auto),
        ("subscription", AcquisitionMode::Subscription),
        ("polling", AcquisitionMode::Polling),
    ] {
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"x", "acquisition_mode": word
        }))
        .unwrap();
        assert_eq!(cfg.acquisition_mode, mode, "{word}");
    }
    // 未知模式词 → 配置错误（不静默回退，拼写错误必须显式暴露）
    assert!(
        parse_cfg(&serde_json::json!({"endpoint":"x","acquisition_mode":"sub"})).is_err(),
        "未知 acquisition_mode 必须 Err"
    );

    // 各参数夹紧（publishing 50..=60000 / sampling 0..=60000 / queue 1..=100 / watchdog 10..=3600）
    let cfg = parse_cfg(&serde_json::json!({
        "endpoint":"x",
        "publishing_interval_ms": 10,
        "sampling_interval_ms": 99999,
        "queue_size": 0,
        "subscription_watchdog_secs": 5,
    }))
    .unwrap();
    assert_eq!(
        cfg.subscription.publishing_interval_ms, 50,
        "publishing 下限 50"
    );
    assert_eq!(
        cfg.subscription.sampling_interval_ms, 60_000,
        "sampling 上限 60000"
    );
    assert_eq!(cfg.subscription.queue_size, 1, "queue 下限 1");
    assert_eq!(cfg.subscription.watchdog_secs, 10, "watchdog 下限 10");

    let cfg = parse_cfg(&serde_json::json!({
        "endpoint":"x",
        "publishing_interval_ms": 99999,
        "queue_size": 200,
        "subscription_watchdog_secs": 99999,
    }))
    .unwrap();
    assert_eq!(
        cfg.subscription.publishing_interval_ms, 60_000,
        "publishing 上限 60000"
    );
    assert_eq!(cfg.subscription.queue_size, 100, "queue 上限 100");
    assert_eq!(cfg.subscription.watchdog_secs, 3600, "watchdog 上限 3600");

    // 合法原值不动（含 sampling=0 语义"随发布间隔"）
    let cfg = parse_cfg(&serde_json::json!({
        "endpoint":"x",
        "publishing_interval_ms": 200,
        "sampling_interval_ms": 0,
        "queue_size": 5,
        "subscription_watchdog_secs": 30,
    }))
    .unwrap();
    assert_eq!(cfg.subscription.publishing_interval_ms, 200);
    assert_eq!(cfg.subscription.sampling_interval_ms, 0);
    assert_eq!(cfg.subscription.queue_size, 5);
    assert_eq!(cfg.subscription.watchdog_secs, 30);
}

#[test]
fn fallback_to_polling_classifies_service_errors() {
    let mk = |st: StatusCode| Error::new(st, "test");
    // auto + 服务不支持类 → 降级轮询
    for st in [
        StatusCode::BadServiceUnsupported,
        StatusCode::BadNotSupported,
        StatusCode::BadNotImplemented,
        StatusCode::BadOutOfService,
    ] {
        assert!(
            fallback_to_polling(AcquisitionMode::Auto, &mk(st)),
            "{st} 在 auto 模式应降级轮询"
        );
    }
    // auto + 非服务不支持（传输错误/业务错误）→ 不降级（走拆会话重试）
    for st in [
        StatusCode::BadTimeout,
        StatusCode::BadConnectionClosed,
        StatusCode::BadNodeIdUnknown,
        StatusCode::BadTooManyOperations,
    ] {
        assert!(
            !fallback_to_polling(AcquisitionMode::Auto, &mk(st)),
            "{st} 不应触发降级"
        );
    }
    // subscription 模式永不降级（显式要求订阅，失败拆会话重试）
    assert!(!fallback_to_polling(
        AcquisitionMode::Subscription,
        &mk(StatusCode::BadServiceUnsupported)
    ));
    // polling 模式不进订阅路径
    assert!(!fallback_to_polling(
        AcquisitionMode::Polling,
        &mk(StatusCode::BadServiceUnsupported)
    ));
}

#[test]
fn watchdog_expired_strictly_after_timeout() {
    let now = Instant::now();
    let timeout = Duration::from_secs(60);
    // 恰好等于窗口不算过期（下一拍仍可等 keep-alive）
    assert!(!watchdog_expired(now, now - timeout, timeout));
    // 超过窗口 → 过期
    assert!(watchdog_expired(
        now,
        now - timeout - Duration::from_millis(1),
        timeout
    ));
    // 有活动 → 不过期
    assert!(!watchdog_expired(now, now, timeout));
}

#[test]
fn handle_allocator_starts_at_one_and_is_unique() {
    // client_handle=0 被库保留为"自动分配"标记，插件必须自己分配非 0 且唯一
    let mut alloc = HandleAllocator::new();
    let mut seen = std::collections::HashSet::new();
    for expected in 1..=100u32 {
        let h = alloc.next_handle();
        assert_eq!(h, expected, "单调递增从 1 开始");
        assert!(seen.insert(h), "handle 必须唯一: {h}");
    }
}

#[test]
fn build_monitored_items_maps_handles_to_devices_and_fields() {
    let devices = vec![
        PreparedDevice {
            devid: "d1".to_string(),
            points: vec![
                (
                    "temp".to_string(),
                    parse_node_spec("ns=2;s=Temp", None).unwrap(),
                ),
                (
                    "arr".to_string(),
                    parse_node_spec("ns=2;s=Arr[1:3]", None).unwrap(),
                ),
            ],
        },
        PreparedDevice {
            devid: "d2".to_string(),
            points: vec![("pump".to_string(), parse_node_spec("i=2258", None).unwrap())],
        },
    ];
    let mut alloc = HandleAllocator::new();
    let (reqs, handles) = build_monitored_items(&devices, 0.0, 2, &mut alloc);

    // 全部点位各一个请求；跨设备进同一订阅（扁平列表）
    assert_eq!(reqs.len(), 3);
    assert_eq!(handles.len(), 3);
    // handle → (devid, field) 映射完整且与请求一一对应
    let mut by_handle: std::collections::BTreeMap<u32, (String, String)> =
        handles.iter().map(|(h, v)| (*h, v.clone())).collect();
    for req in &reqs {
        let h = req.requested_parameters.client_handle;
        assert!(h != 0, "client_handle 不得为 0（库保留字）");
        let (devid, field) = by_handle.remove(&h).expect("请求 handle 必须在映射中");
        // 每个请求的 item_to_monitor 与点表 NodeId/下标范围一致
        assert_eq!(req.item_to_monitor.attribute_id, AttributeId::Value as u32);
        match (devid.as_str(), field.as_str()) {
            ("d1", "temp") => assert_eq!(req.item_to_monitor.node_id, NodeId::new(2, "Temp")),
            ("d1", "arr") => {
                assert_eq!(req.item_to_monitor.node_id, NodeId::new(2, "Arr"));
                assert_eq!(req.item_to_monitor.index_range, NumericRange::Range(1, 3));
            }
            ("d2", "pump") => assert_eq!(req.item_to_monitor.node_id, NodeId::new(0, 2258u32)),
            other => panic!("未知映射 {other:?}"),
        }
        // 监控参数：Reporting + 采样间隔透传 + 队列 + 丢旧
        assert_eq!(req.monitoring_mode, MonitoringMode::Reporting);
        assert_eq!(req.requested_parameters.sampling_interval, 0.0);
        assert_eq!(req.requested_parameters.queue_size, 2);
        assert!(req.requested_parameters.discard_oldest);
        assert!(req.requested_parameters.filter.is_null());
    }
    assert!(by_handle.is_empty(), "映射无孤儿: {by_handle:?}");

    // 按服务端 MaxMonitoredItemsPerCall 分批（探测缺省 1000 → 一批）
    assert_eq!(chunk_nodes(reqs.len(), 1000).len(), 1);
    // 声明 2 → 两批（3 点 → 2+1），绝不空批
    assert_eq!(
        chunk_nodes(3, 2),
        vec![0..2, 2..3],
        "monitored items 按服务端上限分批"
    );
}

#[test]
fn aggregate_notifications_groups_by_devid_with_bad_quality_null() {
    let handles: std::collections::HashMap<u32, (String, String)> = [
        (1u32, ("d1".to_string(), "temp".to_string())),
        (2u32, ("d1".to_string(), "big".to_string())),
        (3u32, ("d2".to_string(), "msg".to_string())),
        (4u32, ("d1".to_string(), "temp".to_string())),
    ]
    .into_iter()
    .collect();
    let batch = vec![
        // 好值（Double → Float）
        (1, dv(Some(Variant::Double(1.5)), None)),
        // 坏质量 → Null + 计入坏点清单（复用 OPC-1 语义）
        (
            2,
            dv(Some(Variant::UInt64(9)), Some(StatusCode::BadTimeout)),
        ),
        // 字符串
        (3, dv(Some(Variant::String("hi".into())), None)),
        // 同点后到覆盖先到（queue>1 或跨 publish 时"最新值胜出"）
        (4, dv(Some(Variant::Double(9.9)), None)),
        // 未知 handle（上一连接残留）→ 丢弃不出现在结果
        (99, dv(Some(Variant::Int32(1)), None)),
    ];
    let (by_dev, bad_keys) = aggregate_notifications(batch, &handles);
    let d1 = by_dev.get("d1").expect("d1 聚合");
    assert_eq!(d1.get("temp"), Some(&SampleValue::Float(9.9)), "后到值覆盖");
    assert_eq!(
        d1.get("big"),
        Some(&SampleValue::Null),
        "坏质量 → Null（OPC-1 语义在订阅路径同样成立）"
    );
    let d2 = by_dev.get("d2").expect("d2 聚合");
    assert_eq!(d2.get("msg"), Some(&SampleValue::Str("hi".into())));
    assert_eq!(by_dev.len(), 2, "未知 handle 不产生分组");
    assert_eq!(bad_keys, vec!["d1.big".to_string()], "坏点键 = devid.field");

    // UInt64 → UInt（F-5：不 as i64 符号翻转）
    let batch = vec![(2, dv(Some(Variant::UInt64(u64::MAX)), None))];
    let (by_dev, bad) = aggregate_notifications(batch, &handles);
    assert_eq!(
        by_dev.get("d1").and_then(|m| m.get("big")),
        Some(&SampleValue::UInt(u64::MAX))
    );
    assert!(bad.is_empty(), "Good 状态的 UInt64 不是坏点");
}
