//! 一次性迁移（计划 §一.6）：旧内联点表（`InstConfig.devices[].children[]`）
//! → 隐式模型（`_inline_<inst>_<dev>`，地址原样、props 初值带入模板默认）
//! + devreg 设备条目 + DeviceBinding 产物。
//!
//! **M-1 范围**：模型与设备入册（`convert` 纯函数 + REST 端点）；实例配置改写
//! 为 DeviceBinding 在 M-2 T1（`InstConfig.devices` 类型升级时）落地——
//! `convert` 的 `bindings` 产物即 M-2 消费物。
//!
//! 幂等：按"目标模型/devreg 条目已存在"判定跳过（REST 可重复执行）。

use crate::model::types::{DeviceBinding, DeviceEntry, DeviceModel, TagTemplate};
use crate::types::{AppType, ChildSpec, DeviceSpec, HostKind, InstConfig, PluginName};

/// value=协议地址的插件（children[].value 原样进模板 addr；其余视为纯软设备：
/// value=tag 名、addr=None——dataSim/lua 等）。
const ADDR_VALUE_APPS: [&str; 4] = ["modbusClient", "opcuaClient", "s7Client", "eipClient"];

/// 单实例转换产物（M-2 消费 bindings）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvertOutcome {
    /// 隐式模型（每设备一个）。
    pub models: Vec<DeviceModel>,
    /// devreg 设备条目。
    pub devices: Vec<DeviceEntry>,
    /// 实例设备绑定（M-2 InstConfig 改写物）。
    pub bindings: Vec<DeviceBinding>,
}

/// 纯函数：旧式实例配置 → 模型/设备/绑定三元组。
///
/// - tag 名：地址类插件取 label（空回退 value，重名加 `_2` 后缀）；纯软取 value；
/// - 地址：地址类插件 `addr=Some(value)` 原样；纯软 `addr=None`；
/// - `props` 初值带入模板默认（无则 TagProps::default）；
/// - `slave_id` → `DeviceBinding.bind`；空 devid 设备跳过。
pub fn convert(inst: &InstConfig) -> ConvertOutcome {
    let mut out = ConvertOutcome::default();
    let is_addr_app = ADDR_VALUE_APPS.contains(&inst.app_code.as_ref());
    for dev in &inst.devices {
        if dev.value.is_empty() {
            continue;
        }
        let model_id = format!(
            "_inline_{}_{}",
            sanitize(inst.inst_id.as_ref()),
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

/// 构造测试用旧式实例配置。
#[cfg(test)]
pub(crate) fn legacy_inst(inst_id: &str, app_code: &str) -> InstConfig {
    InstConfig {
        inst_id: inst_id.into(),
        inst_name: String::new(),
        app_code: PluginName::new(app_code),
        app_type: AppType::South,
        auto_start: false,
        app_config: serde_json::json!({}),
        devices: vec![],
        host_kind: HostKind::Builtin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TagProps;

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
            DeviceSpec {
                label: "1号表".into(),
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
            DeviceSpec {
                label: "2号表".into(),
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

        let d1 = &out.devices[0];
        assert_eq!(d1.devid, "vx1");
        assert_eq!(d1.model, "_inline_mb1_vx1");
        assert!(d1.params.is_empty() && d1.overrides.is_empty());

        assert_eq!(
            out.bindings[0].bind.get("slave_id").map(String::as_str),
            Some("2"),
            "slave_id 迁移进 bind"
        );
        assert!(
            out.bindings[1].bind.is_empty(),
            "无 slave_id（回退 app_config）不产 bind 项"
        );
    }

    /// golden：dataSim 纯软设备 → addr=None、tag 名=value。
    #[test]
    fn golden_datasim_soft_device() {
        let mut inst = legacy_inst("sim1", "dataSim");
        inst.devices = vec![DeviceSpec {
            label: "演示".into(),
            value: "devA".into(),
            children: vec![child("温度标签", "temp", "float")],
            slave_id: None,
        }];
        let out = convert(&inst);
        let m = &out.models[0];
        assert_eq!(m.model_id, "_inline_sim1_devA");
        assert_eq!(m.tags[0].name, "temp", "纯软设备 tag 名=value");
        assert_eq!(m.tags[0].addr, None, "纯软设备无地址");
        assert_eq!(out.devices[0].devid, "devA");
    }

    /// 地址类插件 label 空回退 value；label 重名去重（`_2` 后缀）。
    #[test]
    fn tag_name_fallback_and_dedupe() {
        let mut inst = legacy_inst("s7x", "s7Client");
        inst.devices = vec![DeviceSpec {
            label: "d".into(),
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
            "label 空回退 value（sanitize 非法字符）；重名加 _2 后缀"
        );
    }

    /// 空 devid 设备跳过；无 devices 实例零产物。
    #[test]
    fn skips_empty_devid_and_empty_devices() {
        let mut inst = legacy_inst("x1", "modbusClient");
        inst.devices = vec![DeviceSpec {
            label: "空".into(),
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
        inst.devices = vec![DeviceSpec {
            label: "d".into(),
            value: "vx*1".into(),
            children: vec![child("t", "holding:0:f32", "f32")],
            slave_id: None,
        }];
        let out = convert(&inst);
        assert_eq!(out.models[0].model_id, "_inline_mb_1_vx_1");
        // devid 原样保留（数据面 key 不动；非法 devid 由 store 校验拦截）。
        assert_eq!(out.devices[0].devid, "vx*1");
    }
}
