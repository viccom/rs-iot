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
    pub instance: String,
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
pub enum AppType {
    /// 南向采集（Source）
    South,
    /// 北向发布（Sink）
    North,
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
    pub inst_id: String,
    /// 实例显示名。
    pub inst_name: String,
    /// 插件名（如 "dataSim"、"mqttPub"），用于在注册表查找。
    pub app_code: String,
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
}
