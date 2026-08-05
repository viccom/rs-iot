//! 核心数据类型。对标 Lite_IOT `pluginM.QueueData` / `CtlMessage` / `sysutil.InstConfig`。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 一条采样数据（南向插件产出，经 pipeline 流向北向 sink）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// 边缘端硬件 ID（topic 根，多边缘端隔离）。
    pub hwid: String,
    /// 设备 ID（对标 Lite_IOT devid）。
    pub devid: String,
    /// 变量值表，key 形如 "tag" 或 "devId.tag"。
    pub values: BTreeMap<String, SampleValue>,
    /// Unix 时间戳（秒）。
    pub timestamp: i64,
}

/// 采样值。`untagged` 让 JSON 直接是数字/布尔/字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SampleValue {
    Float(f64),
    Int(i64),
    Bool(bool),
    Str(String),
}

/// 命令下行（对标 Lite_IOT `pluginM.CtlMessage`）。
///
/// 由北向（MQTT `<hwid>/command`）或 REST 注入，按 `instance` 路由到对应插件 task。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtlMessage {
    pub instance: String,
    pub devid: String,
    pub cmd: String,
    pub data: String,
}

/// 插件方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppType {
    /// 南向采集（Source）
    South,
    /// 北向发布（Sink）
    North,
}

/// 设备规格（对标 Lite_IOT devices：label/value/children）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceSpec {
    #[serde(default)]
    pub label: String,
    /// 设备 ID
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub children: Vec<ChildSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChildSpec {
    #[serde(default)]
    pub label: String,
    /// 变量名/tag
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub r#type: String,
}

/// 插件实例配置（对标 `sysutil.InstConfig`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstConfig {
    pub inst_id: String,
    pub inst_name: String,
    /// 插件名（如 "dataSim"、"mqttPub"），用于在注册表查找。
    pub app_code: String,
    pub app_type: AppType,
    #[serde(default)]
    pub auto_start: bool,
    /// 任意 JSON 配置，由各插件自行解析。
    #[serde(default)]
    pub app_config: serde_json::Value,
    #[serde(default)]
    pub devices: Vec<DeviceSpec>,
}
