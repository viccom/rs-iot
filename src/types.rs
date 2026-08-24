//! 核心数据类型。对标 Lite_IOT `pluginM.QueueData` / `CtlMessage` / `sysutil.InstConfig`。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ---------- newtype：用类型隔离不同语义的字符串 ID ----------

/// 边缘端硬件 ID（topic 根，多边缘端隔离）。
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Hwid(String);

/// 插件实例 ID（状态/命令路由 key）。
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct InstId(String);

/// 用户 ID（lux Hash `users` 的 field）。预留 newtype，api DTO 暂沿用 `String`。
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct UserId(String);

/// 插件名（注册表 key，对应 `InstConfig::app_code`，如 "dataSim"/"luaNorth"）。
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PluginName(String);

impl Hwid {
    /// 构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取内含字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl InstId {
    /// 构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取内含字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl UserId {
    /// 构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取内含字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl PluginName {
    /// 构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取内含字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! impl_string_newtype {
    ($name:ident) => {
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.into())
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
impl_string_newtype!(Hwid);
impl_string_newtype!(InstId);
impl_string_newtype!(UserId);
impl_string_newtype!(PluginName);

// ---------- 数据模型 ----------

/// 一条采样数据（南向插件产出，经 pipeline 流向北向 sink）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// 边缘端硬件 ID。
    pub hwid: Hwid,
    /// 设备 ID（对标 Lite_IOT devid）。
    pub devid: String,
    /// 变量值表，key 形如 "tag" 或 "devId.tag"。
    pub values: BTreeMap<String, SampleValue>,
    /// Unix 时间戳（秒）。
    pub timestamp: i64,
}

/// 采样值。`untagged` 让 JSON 直接是数字/布尔/字符串。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum SampleValue {
    /// 浮点采样值。
    Float(f64),
    /// 整数采样值。
    Int(i64),
    /// 布尔采样值。
    Bool(bool),
    /// 字符串采样值。
    Str(String),
}

/// 标签全局属性（lux Hash `tags`；设计见 docs/designs/2026-08-24-tag-registry-plan.md）。
///
/// 叠加层——采集地址语义仍由各插件 `ChildSpec` 私有承载，本结构只存跨插件
/// 共享属性。是后续历史存储归档白名单（`history`）、死区过滤（`deadband`）、
/// 报警结构化绑定（P2 遗留）的唯一权威源。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TagProps {
    /// 历史归档白名单（默认 false = 不归档；历史存储 feature 消费）。
    #[serde(default)]
    pub history: bool,
    /// 绝对死区：|new − last_archived| ≥ deadband 才归档（PI exception 语义；
    /// 仅数值型生效，Bool/Str 值变即写）。None = 无死区。
    #[serde(default)]
    pub deadband: Option<f64>,
    /// 描述（UI 展示）。
    #[serde(default)]
    pub desc: Option<String>,
    /// 工程单位（对齐 OPC UA EngineeringUnits）。
    #[serde(default)]
    pub unit: Option<String>,
    /// 首个生产者实例 id（出口登记填；API set 不修改此字段）。
    #[serde(default)]
    pub owner_inst: String,
}

/// 标签身份单一规范化源：`(devid, tag)` → 全局唯一键 `<devid>.<tag>`。
///
/// rtdata field（`db/rtdata.rs`）、lux `tags` 表 field、后续 tsink series 与
/// 报警 key 全部经本函数——保证"标签变量 id 一致性"。语义与 rtdata 历史行为
/// 完全一致（tag 已含 devid 串则原样返回，不重复拼接）。
pub fn tag_key(devid: &str, tag: &str) -> String {
    if tag.contains(devid) {
        tag.to_string()
    } else {
        format!("{devid}.{tag}")
    }
}

/// 命令下行（对标 Lite_IOT `pluginM.CtlMessage`）。
///
/// 由北向（MQTT `<hwid>/command`）或 REST 注入，按 `instance` 路由到对应插件 task。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlMessage {
    /// 目标实例 ID（路由 key）。
    pub instance: InstId,
    /// 目标设备 ID。
    pub devid: String,
    /// 命令名。
    pub cmd: String,
    /// 命令负载（自由文本/JSON）。
    pub data: String,
}

/// 插件方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AppType {
    /// 南向采集（Source）
    South,
    /// 北向发布（Sink）
    North,
}

/// 插件实现形态（进程内内置 / 进程外外部经 bridge 接入）。
///
/// 标记 + bridge 准入约束：`External` 实例才签发/校验外部 token、才接受外部进程报到。
/// `InstanceManager` 的 spawn/stop/路由不感知此字段（对所有 Source 一视同仁）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    /// 进程内 Rust trait（inventory 静态注册）。
    #[default]
    Builtin,
    /// 进程外外部插件，经 bridge（sidecarClient）接入，任意语言。
    External,
}

impl HostKind {
    /// 受控词汇字符串(builtin/external),供 Prometheus label / REST 输出复用。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::External => "external",
        }
    }
}

/// 设备规格（对标 Lite_IOT devices：label/value/children）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceSpec {
    /// 显示标签。
    #[serde(default)]
    pub label: String,
    /// 设备 ID
    #[serde(default)]
    pub value: String,
    /// 设备下的变量列表。
    #[serde(default)]
    pub children: Vec<ChildSpec>,
    /// Modbus unit id（仅 `modbusClient` 用；其它插件忽略）。
    /// 缺省 `None` → modbus 回退到 `app_config.slave_id`（向后兼容旧单 slave 配置）。
    #[serde(default)]
    pub slave_id: Option<u8>,
}

/// 变量规格（对标 Lite_IOT devices.children）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChildSpec {
    /// 显示标签。
    #[serde(default)]
    pub label: String,
    /// 变量名/tag
    #[serde(default)]
    pub value: String,
    /// 值类型（"float"/"int"/"bool" 等）。
    #[serde(default)]
    pub r#type: String,
    /// 声明式标签属性初值（Tag Registry）：出口登记首见建卡时落表——
    /// tags 表已有记录则忽略（只补缺不覆盖；人工 API 修改优先）。缺省 None 向后兼容。
    #[serde(default)]
    pub props: Option<TagProps>,
}

/// 插件实例配置（对标 `sysutil.InstConfig`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstConfig {
    /// 实例 ID（唯一，作为状态/命令路由 key）。
    pub inst_id: InstId,
    /// 实例显示名（缺省留空，配置容错）。
    #[serde(default)]
    pub inst_name: String,
    /// 插件名（如 "dataSim"、"luaNorth"），用于在注册表查找。
    pub app_code: PluginName,
    /// 插件方向（南向 Source / 北向 Sink）。
    pub app_type: AppType,
    /// 是否在启动时自动拉起（South→Source、North→Sink 按方向分发）。
    #[serde(default)]
    pub auto_start: bool,
    /// 任意 JSON 配置，由各插件自行解析。
    #[serde(default)]
    pub app_config: serde_json::Value,
    /// 设备规格列表（南向采集用）。
    #[serde(default)]
    pub devices: Vec<DeviceSpec>,
    /// 插件形态（内置/外部）。缺省 `Builtin`，向后兼容老配置。
    #[serde(default)]
    pub host_kind: HostKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_spec_slave_id_deserializes_when_present() {
        let json = serde_json::json!({"value":"m1","slave_id":2});
        let d: DeviceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(d.slave_id, Some(2));
    }

    #[test]
    fn child_spec_props_absent_is_none_for_backward_compat() {
        // 旧配置无 props → None（全部既有实例配置零迁移）。
        let json = serde_json::json!({"label":"t","value":"holding:0:f32","type":"float"});
        let c: ChildSpec = serde_json::from_value(json).unwrap();
        assert_eq!(c.props, None);
    }

    #[test]
    fn child_spec_props_deserializes_and_roundtrips() {
        let json = serde_json::json!({
            "label":"炉温","value":"DB1.DBD0","type":"f32",
            "props":{"history":true,"deadband":0.5,"unit":"°C"}
        });
        let c: ChildSpec = serde_json::from_value(json).unwrap();
        let props = c.props.clone().unwrap();
        assert!(props.history);
        assert_eq!(props.deadband, Some(0.5));
        assert_eq!(props.unit.as_deref(), Some("°C"));
        assert_eq!(props.owner_inst, "", "声明式初值不填 owner（出口登记填）");
        let back = serde_json::to_value(&c).unwrap();
        let c2: ChildSpec = serde_json::from_value(back).unwrap();
        assert_eq!(c2.props, c.props, "roundtrip");
    }

    #[test]
    fn tag_key_joins_devid_and_tag() {
        assert_eq!(tag_key("dev1", "temp"), "dev1.temp");
        assert_eq!(
            tag_key("furnace_02", "zone1.pressure"),
            "furnace_02.zone1.pressure"
        );
    }

    #[test]
    fn tag_key_passthrough_when_tag_already_contains_devid() {
        // 与 rtdata 历史行为一致：tag 已含 devid 串则原样（不重复拼接）。
        assert_eq!(tag_key("dev1", "dev1.temp"), "dev1.temp");
    }

    #[test]
    fn tag_key_empty_devid_passthrough() {
        // devid 为空串 → contains 恒真 → 原样（兼容现状语义，防御性记录）。
        assert_eq!(tag_key("", "temp"), "temp");
    }

    #[test]
    fn tag_props_defaults_all_off() {
        // 空 JSON → 全默认（history=false、无死区、无 owner）——归档白名单模型的安全默认。
        let p: TagProps = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p, TagProps::default());
        assert!(!p.history);
        assert_eq!(p.deadband, None);
        assert_eq!(p.owner_inst, "");
    }

    #[test]
    fn tag_props_json_roundtrip() {
        let p = TagProps {
            history: true,
            deadband: Some(0.5),
            desc: Some("炉温".into()),
            unit: Some("°C".into()),
            owner_inst: "sim1".into(),
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: TagProps = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn device_spec_slave_id_absent_is_none_for_backward_compat() {
        // 老配置无 slave_id → None（opcua/s7 等其它插件也走这条，字段透明）
        let json = serde_json::json!({"value":"m1"});
        let d: DeviceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(d.slave_id, None);
    }

    #[test]
    fn device_spec_slave_id_serializes_roundtrip() {
        let d = DeviceSpec {
            label: "m".into(),
            value: "m1".into(),
            children: vec![],
            slave_id: Some(5),
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: DeviceSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(back.slave_id, Some(5));
    }
}
