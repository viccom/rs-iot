//! 设备模型域数据类型（决策 D-M1/D-M2；lux 表 `devmodels` / `devreg`）。
//!
//! 结构定义见实施计划 §二；模型/设备级校验（id 字符集、tag 名唯一、
//! params 与 addr 一致性）见 T3（`validate` 模块复用本文件类型）。

use crate::types::TagProps;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------- 模型侧（lux Hash `devmodels`，field = model_id） ----------

/// 设备模型（物模型，"型号即模板"）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DeviceModel {
    /// 全局唯一模型 id（如 "vx_dtz341"；内置隐式模型 "_sys_rsiot"、迁移生成
    /// "_inline_&lt;inst&gt;_&lt;dev&gt;"）。
    pub model_id: String,
    /// 厂商。
    #[serde(default)]
    pub vendor: Option<String>,
    /// 型号。
    #[serde(default)]
    pub model_no: Option<String>,
    /// 类别（电表/PLC/变频器…）。
    #[serde(default)]
    pub category: Option<String>,
    /// 描述。
    #[serde(default)]
    pub desc: Option<String>,
    /// 声明的实例化参数名（如 `["base"]`；设备 params 按此齐套）。
    #[serde(default)]
    pub params: Vec<String>,
    /// 标签模板集合（name 模型内唯一）。
    #[serde(default)]
    pub tags: Vec<TagTemplate>,
    /// 开放扩展槽：捕获 wire 上未匹配已知字段的键，读-改-写原样保留
    /// （前向兼容——旧代码不丢新版本写入的扩展字段；同款先例
    /// `db::stat::InstanceStat.extra`）。空时不序列化，不用扩展槽的
    /// wire 逐字节不变。
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 标签模板（乐高最小对象）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TagTemplate {
    /// 标签名（设备内唯一，如 "ua"）。
    pub name: String,
    /// 数据类型（f32/f64/i16/u16/bool/string…，语义由消费插件解释）。
    pub data_type: String,
    /// 描述。
    #[serde(default)]
    pub desc: Option<String>,
    /// 工程单位。
    #[serde(default)]
    pub unit: Option<String>,
    /// 参数化地址（`addr::resolve` 语法；lua/纯软设备模型可 None）。
    #[serde(default)]
    pub addr: Option<String>,
    /// 读写语义。
    #[serde(default)]
    pub rw: TagRw,
    /// 型号级默认属性（Phase 1a `TagProps` 转任模板默认结构）。
    #[serde(default)]
    pub props: TagProps,
    /// 开放扩展槽（语义同 [`DeviceModel::extra`]，tag 级）。
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 标签读写语义。缺省**只读**（最小权限：观察到的数据点默认不可写；可写须
/// 显式声明——与前端 ModelForm 新建 tag 默认 'r' 对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TagRw {
    /// 只读（采集）。
    #[default]
    R,
    /// 可读可写（采集 + 写命令）。
    Rw,
}

// ---------- 设备侧（lux Hash `devreg`，field = devid） ----------

/// 全局设备实体（物理设备全局唯一；D-M1）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DeviceEntry {
    /// 物理设备全局唯一 id（= rtdata/tsink 的 devid，不变）。
    pub devid: String,
    /// 所属模型 model_id 引用。
    pub model: String,
    /// 实例化参数（如 `{"base":"100"}`；按模型 params 齐套）。
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// per-tag 属性覆盖（key = tag 名；三态：字段缺省 = 继承模板）。
    #[serde(default)]
    pub overrides: BTreeMap<String, TagOverride>,
    /// 描述。
    #[serde(default)]
    pub desc: Option<String>,
}

/// 实例级标签覆盖（三态：`None` = 继承模板，`Some` = 覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TagOverride {
    /// 历史归档白名单覆盖。
    #[serde(default)]
    pub history: Option<bool>,
    /// 死区三态覆盖：JSON 缺省 = 继承模板；显式 `null` = 显式关死区；
    /// 数值 = 覆盖（计划 T2：Option&lt;Option&lt;f64&gt;&gt; 区分"关"与"未设"）。
    #[serde(
        default,
        deserialize_with = "deserialize_some",
        skip_serializing_if = "Option::is_none"
    )]
    pub deadband: Option<Option<f64>>,
    /// 描述覆盖（空串 = 显式清除，None = 继承）。
    #[serde(default)]
    pub desc: Option<String>,
    /// 工程单位覆盖（空串 = 显式清除，None = 继承）。
    #[serde(default)]
    pub unit: Option<String>,
}

/// 双层 Option 反序列化 helper：serde 默认把 JSON `null` 折叠成外层 `None`，
/// 此 helper 让 `null` 到达内层（`Some(None)` = 显式关）。pub(crate)：REST
/// 三态请求体（routes_tag "字段缺省=不动 / null=恢复继承"）复用。
pub(crate) fn deserialize_some<'de, T, D>(de: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(de).map(Some)
}

// ---------- 实例绑定侧（InstConfig.devices 的新形态，M-2 接线） ----------

/// 采集器实例的设备绑定（实例只绑定设备，不拥有设备）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DeviceBinding {
    /// 引用的 devreg 设备 id。
    pub devid: String,
    /// 协议绑定参数（如 `{"slave_id":"2"}`；`DeviceSpec.slave_id` 迁移目标）。
    #[serde(default)]
    pub bind: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_override_deadband_wire_tri_state() {
        // 缺省 = 继承（字段整体省略）。
        let absent: TagOverride = serde_json::from_str(r#"{"history":true}"#).unwrap();
        assert_eq!(absent.deadband, None);
        assert!(serde_json::to_value(&absent)
            .unwrap()
            .get("deadband")
            .is_none());
        // 显式 null = Some(None)（显式关死区）。
        let off: TagOverride = serde_json::from_str(r#"{"deadband":null}"#).unwrap();
        assert_eq!(off.deadband, Some(None));
        assert_eq!(
            serde_json::to_value(&off).unwrap()["deadband"],
            serde_json::Value::Null,
            "Some(None) 序列化为 null（roundtrip）"
        );
        // 数值 = Some(Some(v))。
        let v: TagOverride = serde_json::from_str(r#"{"deadband":0.5}"#).unwrap();
        assert_eq!(v.deadband, Some(Some(0.5)));
        let back: TagOverride = serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
        assert_eq!(back, v, "roundtrip");
    }

    #[test]
    fn device_model_minimal_json_and_full_roundtrip() {
        // 最小 JSON（旧/手工配置容错：全部增强字段缺省）。
        let m: DeviceModel = serde_json::from_str(r#"{"model_id":"m1"}"#).unwrap();
        assert_eq!(m.model_id, "m1");
        assert!(m.tags.is_empty() && m.params.is_empty());
        // 不用扩展槽时 wire 不引入新键（skip_serializing_if 锁定）。
        assert!(!serde_json::to_value(&m)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("extra"));
        // 全字段往返。
        let full = DeviceModel {
            model_id: "vx_dtz341".into(),
            vendor: Some("威胜".into()),
            model_no: Some("DTZ34Z".into()),
            category: Some("电表".into()),
            desc: Some("三相电能表".into()),
            params: vec!["base".into()],
            tags: vec![TagTemplate {
                name: "ua".into(),
                data_type: "f32".into(),
                desc: Some("A相电压".into()),
                unit: Some("V".into()),
                addr: Some("holding:{base}:f32".into()),
                rw: TagRw::R,
                props: TagProps {
                    history: true,
                    deadband: Some(0.5),
                    ..Default::default()
                },
                extra: [("x_scale".into(), serde_json::json!(0.1))]
                    .into_iter()
                    .collect(),
            }],
            extra: [("x_code".into(), serde_json::json!(7))]
                .into_iter()
                .collect(),
        };
        let back: DeviceModel =
            serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn device_entry_defaults_and_roundtrip() {
        let e: DeviceEntry = serde_json::from_str(r#"{"devid":"d1","model":"m1"}"#).unwrap();
        assert!(e.params.is_empty() && e.overrides.is_empty() && e.desc.is_none());
        let full = DeviceEntry {
            devid: "d1".into(),
            model: "m1".into(),
            params: [("base".to_string(), "100".to_string())].into(),
            overrides: [(
                "ua".to_string(),
                TagOverride {
                    deadband: Some(None),
                    ..Default::default()
                },
            )]
            .into(),
            desc: Some("1号电表".into()),
        };
        let back: DeviceEntry =
            serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn device_binding_wire() {
        let b: DeviceBinding = serde_json::from_str(r#"{"devid":"d1"}"#).unwrap();
        assert!(b.bind.is_empty());
        let full = DeviceBinding {
            devid: "d1".into(),
            bind: [("slave_id".to_string(), "2".to_string())].into(),
        };
        let back: DeviceBinding =
            serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn device_model_unknown_fields_roundtrip_preserved() {
        // 前向兼容：wire 上未匹配已知字段的键（新版本写入的扩展键）读入后
        // 原样保留，不静默丢弃——旧代码读-改-写不丢新字段。
        let src = r#"{"model_id":"m1","vendor":"v","x_ext":{"code":7},"tags":[]}"#;
        let m: DeviceModel = serde_json::from_str(src).unwrap();
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(back["x_ext"], serde_json::json!({"code": 7}));
    }

    #[test]
    fn tag_template_unknown_fields_roundtrip_preserved() {
        let src = r#"{"model_id":"m1","tags":[{"name":"ua","data_type":"f32","x_scale":0.1}]}"#;
        let m: DeviceModel = serde_json::from_str(src).unwrap();
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(back["tags"][0]["x_scale"], serde_json::json!(0.1));
    }

    #[test]
    fn tag_rw_wire_lowercase() {
        assert_eq!(
            serde_json::to_value(TagRw::R).unwrap(),
            serde_json::json!("r")
        );
        assert_eq!(
            serde_json::to_value(TagRw::Rw).unwrap(),
            serde_json::json!("rw")
        );
        let r: TagRw = serde_json::from_str("\"r\"").unwrap();
        assert_eq!(r, TagRw::R);
    }

    #[test]
    fn tag_rw_defaults_to_read_only() {
        // 最小权限缺省（审查修复）：JSON 缺省 rw 字段 → 只读。
        assert_eq!(TagRw::default(), TagRw::R);
        let t: TagTemplate = serde_json::from_str(r#"{"name":"a","data_type":"f32"}"#).unwrap();
        assert_eq!(t.rw, TagRw::R, "wire 缺省只读");
    }
}
