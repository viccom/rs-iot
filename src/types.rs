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

/// 插件名（注册表 key，对应 `InstConfig::app_code`，如 "dataSim"/"mqttPub"）。
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
}

/// 插件实例配置（对标 `sysutil.InstConfig`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstConfig {
    /// 实例 ID（唯一，作为状态/命令路由 key）。
    pub inst_id: InstId,
    /// 实例显示名（缺省留空，配置容错）。
    #[serde(default)]
    pub inst_name: String,
    /// 插件名（如 "dataSim"、"mqttPub"），用于在注册表查找。
    pub app_code: PluginName,
    /// 插件方向（南向 Source / 北向 Sink）。
    pub app_type: AppType,
    /// 是否在启动时自动拉起（仅南向生效）。
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
