//! 一次性迁移（计划 §一.6 / M-2 T1）：旧内联点表（instances JSON 的
//! `devices[].children[]`）→ 隐式模型（`_inline_<inst>_<dev>`，地址原样、
//! props 初值带入模板默认）+ devreg 设备条目 + **实例配置改写为 DeviceBinding**。
//!
//! M-2 起 `InstConfig.devices` 类型已是 `Vec<DeviceBinding>`——旧 JSON 无法
//! 直接反序列化，本模块用 **legacy 宽松形态**（[`LegacyInst`]，serde 忽略未知
//! 字段）解析 lux instances 的原始 JSON，改写时 Value 级替换 devices 字段、
//! 其余字段原样保留。
//!
//! 幂等：模型/devreg 已存在跳过；实例 devices 已是绑定形态（无 `children`
//! 数组的 devices 元素）跳过改写。REST 可重复执行。

use crate::error::RsIotError;
use crate::model::store;
use crate::model::types::{DeviceEntry, DeviceModel, TagTemplate};
use crate::types::{ChildSpec, DeviceBinding};
use lux::EmbeddedClient;
use serde::Deserialize;

/// value=协议地址的插件（children[].value 原样进模板 addr；其余视为纯软设备：
/// value=tag 名、addr=None——dataSim/lua 等）。
const ADDR_VALUE_APPS: [&str; 4] = ["modbusClient", "opcuaClient", "s7Client", "eipClient"];

/// legacy 实例宽松形态（只解析迁移需要的字段；旧 lux instances JSON）。
#[derive(Debug, Deserialize)]
pub struct LegacyInst {
    /// 实例 id。
    pub inst_id: String,
    /// 插件名（决定 children.value 语义）。
    pub app_code: String,
    /// 旧内联设备列表。
    #[serde(default)]
    pub devices: Vec<LegacyDevice>,
}

/// legacy 设备形态（= 旧 `DeviceSpec`）。
#[derive(Debug, Deserialize)]
pub struct LegacyDevice {
    /// 设备 id。
    #[serde(default)]
    pub value: String,
    /// 旧点表。
    #[serde(default)]
    pub children: Vec<ChildSpec>,
    /// Modbus unit id（→ bind）。
    #[serde(default)]
    pub slave_id: Option<u8>,
}

/// 单实例转换产物。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvertOutcome {
    /// 隐式模型（每设备一个）。
    pub models: Vec<DeviceModel>,
    /// devreg 设备条目。
    pub devices: Vec<DeviceEntry>,
    /// 实例设备绑定（实例配置改写物）。
    pub bindings: Vec<DeviceBinding>,
}

/// 纯函数：legacy 实例 → 模型/设备/绑定三元组。
///
/// - tag 名：地址类插件取 label（空回退 value，重名加 `_2` 后缀）；纯软取 value；
/// - 地址：地址类插件 `addr=Some(value)` 原样；纯软 `addr=None`；
/// - `props` 初值带入模板默认（无则 TagProps::default）；
/// - `slave_id` → `DeviceBinding.bind`；空 devid 设备跳过。
pub fn convert(inst: &LegacyInst) -> ConvertOutcome {
    let mut out = ConvertOutcome::default();
    let is_addr_app = ADDR_VALUE_APPS.contains(&inst.app_code.as_str());
    for dev in &inst.devices {
        if dev.value.is_empty() {
            continue;
        }
        let model_id = format!(
            "_inline_{}_{}",
            sanitize(&inst.inst_id),
            sanitize(&dev.value)
        );
        let mut tags: Vec<TagTemplate> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        for ch in &dev.children {
            let raw_name = if is_addr_app && !ch.label.is_empty() {
                ch.label.clone()
            } else {
                ch.value.clone()
            };
            let name = sanitize(&raw_name);
            if name.is_empty() {
                continue;
            }
            let unique = if seen.contains(&name) {
                let mut i = 2;
                loop {
                    let cand = format!("{name}_{i}");
                    if !seen.contains(&cand) {
                        break cand;
                    }
                    i += 1;
                }
            } else {
                name
            };
            seen.insert(unique.clone());
            tags.push(TagTemplate {
                name: unique,
                data_type: ch.r#type.clone(),
                desc: None,
                unit: None,
                addr: if is_addr_app {
                    Some(ch.value.clone())
                } else {
                    None
                },
                rw: Default::default(),
                props: ch.props.clone().unwrap_or_default(),
                ..Default::default()
            });
        }
        out.models.push(DeviceModel {
            model_id: model_id.clone(),
            params: Vec::new(),
            tags,
            ..Default::default()
        });
        out.devices.push(DeviceEntry {
            devid: dev.value.clone(),
            model: model_id,
            ..Default::default()
        });
        let mut bind = std::collections::BTreeMap::new();
        if let Some(sid) = dev.slave_id {
            bind.insert("slave_id".to_string(), sid.to_string());
        }
        out.bindings.push(DeviceBinding {
            devid: dev.value.clone(),
            bind,
        });
    }
    out
}

/// model_id 非法字符（`valid_id` 口径）替换为 `_`（迁移生成的 id 必须合法）。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_whitespace() || ['.', '*', '{', '}', ','].contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// 迁移结果摘要（REST 载荷）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MigrationSummary {
    /// 新建隐式模型数。
    pub converted_models: u64,
    /// 新建 devreg 设备数。
    pub converted_devices: u64,
    /// 改写为绑定形态的实例数。
    pub converted_instances: u64,
    /// 单条失败（不中断整体）。
    pub failures: Vec<String>,
}

/// 实例 JSON 的 devices 元素是否已是绑定形态（无 `children`/`slave_id` 字段）。
fn devices_already_bound(v: &serde_json::Value) -> bool {
    match v.get("devices").and_then(|d| d.as_array()) {
        None => true, // 无 devices（lua/north 类）= 无需改写
        Some(arr) => arr
            .iter()
            .all(|d| d.get("children").is_none() && d.get("slave_id").is_none()),
    }
}

/// 执行整体迁移：扫 lux `instances` → 每条 legacy 实例：模型/设备入册（缺才建）
/// → 实例 JSON 改写 devices 为绑定 → HSET 回。
pub async fn run(c: &EmbeddedClient) -> Result<MigrationSummary, RsIotError> {
    let mut sum = MigrationSummary::default();
    let v = c.execute_value("HGETALL", &["instances"]).await?;
    let lux::EmbeddedValue::Array(arr) = v else {
        return Ok(sum);
    };
    let mut i = 0;
    while i + 1 < arr.len() {
        let inst_id = crate::db::str_from_value(&arr[i]);
        let raw = crate::db::str_from_value(&arr[i + 1]);
        i += 2;
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue; // 坏值条目容错跳过
        };
        if devices_already_bound(&json) {
            continue; // 已迁移（幂等）
        }
        let Ok(legacy) = serde_json::from_value::<LegacyInst>(json.clone()) else {
            sum.failures.push(format!("{inst_id}: 旧结构解析失败"));
            continue;
        };
        let outcome = convert(&legacy);
        for m in &outcome.models {
            if store::get_model(c, &m.model_id).await?.is_some() {
                continue;
            }
            match store::set_model(c, m).await {
                Ok(()) => sum.converted_models += 1,
                Err(e) => sum.failures.push(format!("model {}: {e}", m.model_id)),
            }
        }
        for d in &outcome.devices {
            if store::get_device(c, &d.devid).await?.is_some() {
                continue;
            }
            match store::set_device(c, d).await {
                Ok(()) => sum.converted_devices += 1,
                Err(e) => sum.failures.push(format!("device {}: {e}", d.devid)),
            }
        }
        // 实例改写：Value 级替换 devices 为绑定形态（其余字段原样保留）。
        let mut next = json.clone();
        next["devices"] = serde_json::to_value(&outcome.bindings)?;
        let rewritten = serde_json::to_string(&next)?;
        if c.execute("HSET", &["instances", &inst_id, &rewritten])
            .await
            .is_ok()
        {
            sum.converted_instances += 1;
        } else {
            sum.failures.push(format!("{inst_id}: 实例改写失败"));
        }
    }
    crate::model::cache::refresh_after_write(c).await;
    Ok(sum)
}

/// 构造测试用 legacy 实例（golden 测试脚手架）。
#[cfg(test)]
pub(crate) fn legacy_inst(inst_id: &str, app_code: &str) -> LegacyInst {
    LegacyInst {
        inst_id: inst_id.into(),
        app_code: app_code.into(),
        devices: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChildSpec, TagProps};

    fn child(label: &str, value: &str, typ: &str) -> ChildSpec {
        ChildSpec {
            label: label.into(),
            value: value.into(),
            r#type: typ.into(),
            props: None,
        }
    }

    /// golden：modbus 旧实例 → 模型（地址原样/tag=label/slave_id 进 bind）。
    #[test]
    fn golden_modbus_instance() {
        let mut inst = legacy_inst("mb1", "modbusClient");
        inst.devices = vec![
            LegacyDevice {
                value: "vx1".into(),
                children: vec![
                    ChildSpec {
                        label: "温度".into(),
                        value: "holding:0:f32".into(),
                        r#type: "f32".into(),
                        props: Some(TagProps {
                            history: true,
                            deadband: Some(0.5),
                            ..Default::default()
                        }),
                    },
                    child("电压", "holding:2:f32", "f32"),
                ],
                slave_id: Some(2),
            },
            LegacyDevice {
                value: "vx2".into(),
                children: vec![child("温度", "input:4:u16", "u16")],
                slave_id: None,
            },
        ];
        let out = convert(&inst);
        assert_eq!(out.models.len(), 2);
        assert_eq!(out.devices.len(), 2);
        assert_eq!(out.bindings.len(), 2);

        let m1 = &out.models[0];
        assert_eq!(m1.model_id, "_inline_mb1_vx1");
        assert!(m1.params.is_empty(), "地址原样无参数化");
        assert_eq!(m1.tags.len(), 2);
        assert_eq!(m1.tags[0].name, "温度");
        assert_eq!(m1.tags[0].addr.as_deref(), Some("holding:0:f32"));
        assert!(m1.tags[0].props.history, "props 初值带入模板默认");
        assert_eq!(m1.tags[0].props.deadband, Some(0.5));
        assert_eq!(m1.tags[1].name, "电压");

        assert_eq!(out.devices[0].devid, "vx1");
        assert_eq!(out.devices[0].model, "_inline_mb1_vx1");

        assert_eq!(
            out.bindings[0].bind.get("slave_id").map(String::as_str),
            Some("2"),
            "slave_id 迁移进 bind"
        );
        assert!(out.bindings[1].bind.is_empty());
    }

    /// golden：dataSim 纯软设备 → addr=None、tag 名=value。
    #[test]
    fn golden_datasim_soft_device() {
        let mut inst = legacy_inst("sim1", "dataSim");
        inst.devices = vec![LegacyDevice {
            value: "devA".into(),
            children: vec![child("温度标签", "temp", "float")],
            slave_id: None,
        }];
        let out = convert(&inst);
        assert_eq!(out.models[0].tags[0].name, "temp", "纯软设备 tag 名=value");
        assert_eq!(out.models[0].tags[0].addr, None, "纯软设备无地址");
        assert_eq!(out.devices[0].devid, "devA");
    }

    /// 地址类插件 label 空回退 value；label 重名去重（`_2` 后缀）。
    #[test]
    fn tag_name_fallback_and_dedupe() {
        let mut inst = legacy_inst("s7x", "s7Client");
        inst.devices = vec![LegacyDevice {
            value: "plc1".into(),
            children: vec![
                child("", "DB1.DBD0", "f32"), // label 空 → 回退 value
                child("x", "DB1.DBD4", "f32"),
                child("x", "DB1.DBD8", "f32"), // 重名 → x_2
            ],
            slave_id: None,
        }];
        let out = convert(&inst);
        let names: Vec<&str> = out.models[0].tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["DB1_DBD0", "x", "x_2"],
            "label 空回退 value（sanitize）；重名 _2 后缀"
        );
    }

    /// 空 devid 设备跳过；无 devices 实例零产物。
    #[test]
    fn skips_empty_devid_and_empty_devices() {
        let mut inst = legacy_inst("x1", "modbusClient");
        inst.devices = vec![LegacyDevice {
            value: String::new(),
            children: vec![child("t", "holding:0:f32", "f32")],
            slave_id: None,
        }];
        assert_eq!(convert(&inst), ConvertOutcome::default());
        assert_eq!(
            convert(&legacy_inst("x2", "dataSim")),
            ConvertOutcome::default()
        );
    }

    /// model_id 含非法字符时 sanitize。
    #[test]
    fn model_id_sanitized() {
        let mut inst = legacy_inst("mb.1", "modbusClient");
        inst.devices = vec![LegacyDevice {
            value: "vx*1".into(),
            children: vec![child("t", "holding:0:f32", "f32")],
            slave_id: None,
        }];
        let out = convert(&inst);
        assert_eq!(out.models[0].model_id, "_inline_mb_1_vx_1");
        assert_eq!(out.devices[0].devid, "vx*1");
    }

    /// devices 已是绑定形态的判定（幂等改写守卫）。
    #[test]
    fn devices_already_bound_detection() {
        let legacy = serde_json::json!({"devices": [{"label":"d","value":"v","children":[]}]});
        assert!(!devices_already_bound(&legacy), "含 children = 未迁移");
        let bound = serde_json::json!({"devices": [{"devid":"v","bind":{}}]});
        assert!(devices_already_bound(&bound), "绑定形态跳过");
        let none = serde_json::json!({"inst_id":"x"});
        assert!(devices_already_bound(&none), "无 devices 无需改写");
    }
}
