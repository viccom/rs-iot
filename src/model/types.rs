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
}

/// 标签读写语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TagRw {
    /// 只读（采集）。
    R,
    /// 可读可写（采集 + 写命令）。
    #[default]
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
/// 此 helper 让 `null` 到达内层（`Some(None)` = 显式关）。
fn deserialize_some<'de, T, D>(de: D) -> Result<Option<T>, D::Error>
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
