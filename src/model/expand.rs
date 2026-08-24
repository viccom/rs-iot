//! 绑定展开（M-2 装配期）：`DeviceBinding` → 设备模型点表 → 已解析绝对地址 →
//! 合成旧形态 [`DeviceSpec`] 供插件装配（计划 §一.2：resolve 一次性完成，
//! 插件后续逻辑看到的都是绝对地址串，地址解析器零改动）。
//!
//! 数据流：devreg 查设备（→ params/overrides）→ devmodels 查模板（→ tags）→
//! `addr::resolve(模板 addr, 设备 params)` → `DeviceSpec{value: devid,
//! children: [{label: tag 名, value: 绝对地址或 tag 名（纯软）, type: data_type}]}`。
//!
//! 绑定展开失败（设备不在 devreg / 模型悬空 / 参数缺）→ [`RsIotError::Model`]，
//! `startinst` 据此 4xx 拒启动（实例配置是绑定意图，展开是事实校验）。

use crate::error::RsIotError;
use crate::model::addr;
use crate::model::store;
use crate::model::types::{DeviceEntry, DeviceModel};
use crate::types::{ChildSpec, DeviceBinding, DeviceSpec, TagProps};
use lux::EmbeddedClient;
use std::collections::BTreeMap;

/// 展开产物（含设备实体与模型引用，供 props/覆盖消费方使用）。
#[derive(Debug, Clone)]
pub struct ExpandedDevice {
    /// 设备 id（= rtdata key）。
    pub devid: String,
    /// 协议绑定参数原样。
    pub bind: BTreeMap<String, String>,
    /// devreg 条目（params/overrides）。
    pub entry: DeviceEntry,
    /// 设备模型（tags 模板）。
    pub model: DeviceModel,
}

impl ExpandedDevice {
    /// tag 的 effective props（模板默认 ∪ 覆盖三态合并）。
    pub fn effective_props(&self, tag: &str) -> Option<TagProps> {
        let t = self.model.tags.iter().find(|t| t.name == tag)?;
        let props = match self.entry.overrides.get(tag) {
            Some(ov) => crate::model::effective::merge_tag_props(&t.props, ov),
            None => t.props.clone(),
        };
        Some(props)
    }

    /// 合成插件装配视图（旧 DeviceSpec 形态）：label = tag 名（= 采集 tag key），
    /// value = 已解析绝对地址（纯软模型 addr=None 时 = tag 名），type = data_type，
    /// slave_id ← bind。**不含 props**（props 权威在 effective，不再走内联声明）。
    pub fn to_device_spec(&self) -> DeviceSpec {
        let children = self
            .model
            .tags
            .iter()
            .map(|t| ChildSpec {
                label: t.name.clone(),
                value: match &t.addr {
                    Some(a) => addr::resolve(a, &self.entry.params).unwrap_or_else(|e| {
                        // 装配期已校验过（expand_bindings 拒绝坏地址），防御保留原样
                        tracing::warn!("[model] tag {} 地址解析失败（保留模板原样）: {e}", t.name);
                        a.clone()
                    }),
                    None => t.name.clone(),
                },
                r#type: t.data_type.clone(),
                props: None,
            })
            .collect();
        DeviceSpec {
            label: self.devid.clone(),
            value: self.devid.clone(),
            children,
            slave_id: self.bind.get("slave_id").and_then(|s| s.parse().ok()),
        }
    }
}

/// 展开单条绑定（devreg + devmodels 各查一次；装配期调用，非热路径）。
pub async fn expand_binding(
    c: &EmbeddedClient,
    b: &DeviceBinding,
) -> Result<ExpandedDevice, RsIotError> {
    let entry = store::get_device(c, &b.devid).await?.ok_or_else(|| {
        RsIotError::Model(crate::model::validate::ModelError::ModelNotFound {
            devid: b.devid.clone(),
            model: "(devreg)".into(),
        })
    })?;
    let model = store::get_model(c, &entry.model).await?.ok_or_else(|| {
        RsIotError::Model(crate::model::validate::ModelError::ModelNotFound {
            devid: b.devid.clone(),
            model: entry.model.clone(),
        })
    })?;
    // 装配期全量校验：params 齐套 + 地址可解析（坏配置拒启动，不带病运行）。
    crate::model::validate::validate_device_entry(&entry, &model)?;
    for t in &model.tags {
        if let Some(a) = &t.addr {
            addr::resolve(a, &entry.params).map_err(|e| {
                RsIotError::Config(format!(
                    "设备 {} 的 tag {} 地址解析失败: {e}",
                    b.devid, t.name
                ))
            })?;
        }
    }
    Ok(ExpandedDevice {
        devid: b.devid.clone(),
        bind: b.bind.clone(),
        entry,
        model,
    })
}

/// 展开实例全部绑定 → 合成旧形态设备规格列表（插件装配入口）。
///
/// 绑定为空 → 空列表（luaClient/dataSim 允许空——M-2 dataSim 也要求预建设备，
/// 空绑定返回空 = 插件自然退出，语义同旧空 devices）。
pub async fn expand_to_specs(
    c: &EmbeddedClient,
    bindings: &[DeviceBinding],
) -> Result<Vec<DeviceSpec>, RsIotError> {
    let mut out = Vec::with_capacity(bindings.len());
    for b in bindings {
        out.push(expand_binding(c, b).await?.to_device_spec());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::store;
    use crate::model::types::{DeviceEntry, TagOverride, TagTemplate};

    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("m2expand-{tag}-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    /// 模型：参数化 modbus 地址 + 纯软 tag（addr=None）。
    fn model() -> crate::model::types::DeviceModel {
        crate::model::types::DeviceModel {
            model_id: "mx".into(),
            params: vec!["base".into()],
            tags: vec![
                TagTemplate {
                    name: "va".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:{base}:f32".into()),
                    props: TagProps {
                        history: true,
                        deadband: Some(0.5),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                TagTemplate {
                    name: "vb".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:{base}+24:f32".into()),
                    ..Default::default()
                },
                TagTemplate {
                    name: "soft".into(),
                    data_type: "float".into(),
                    addr: None, // 纯软（dataSim 类）
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    async fn seed(c: &lux::EmbeddedClient) {
        store::set_model(c, &model()).await.unwrap();
        store::set_device(
            c,
            &DeviceEntry {
                devid: "ex1".into(),
                model: "mx".into(),
                params: [("base".to_string(), "100".to_string())].into(),
                overrides: [(
                    "vb".to_string(),
                    TagOverride {
                        deadband: Some(Some(9.0)),
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn expand_resolves_parametrized_addresses() {
        let c = test_client("resolve").await;
        seed(&c).await;
        let specs = expand_to_specs(
            &c,
            &[DeviceBinding {
                devid: "ex1".into(),
                bind: [("slave_id".to_string(), "7".to_string())].into(),
            }],
        )
        .await
        .unwrap();
        assert_eq!(specs.len(), 1);
        let dev = &specs[0];
        assert_eq!(dev.value, "ex1");
        assert_eq!(dev.slave_id, Some(7), "slave_id 从 bind 迁入");
        // label=tag 名，value=已解析地址；纯软 tag value=tag 名。
        let va = &dev.children[0];
        assert_eq!(
            (va.label.as_str(), va.value.as_str()),
            ("va", "holding:100:f32")
        );
        let vb = &dev.children[1];
        assert_eq!(vb.value, "holding:124:f32", "+24 偏移在装配期完成");
        let soft = &dev.children[2];
        assert_eq!((soft.label.as_str(), soft.value.as_str()), ("soft", "soft"));
        assert!(
            dev.children.iter().all(|ch| ch.props.is_none()),
            "props 不走内联"
        );
    }

    #[tokio::test]
    async fn expand_missing_device_rejected() {
        let c = test_client("missing").await;
        let err = expand_to_specs(
            &c,
            &[DeviceBinding {
                devid: "ghost".into(),
                bind: BTreeMap::new(),
            }],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RsIotError::Model(_)),
            "设备不在 devreg 拒绝: {err:?}"
        );
    }

    #[tokio::test]
    async fn expand_missing_param_rejected() {
        let c = test_client("noaddr").await;
        store::set_model(&c, &model()).await.unwrap();
        // 直接 HSET 种缺参设备（写入校验会拦 set_device——此处模拟脏数据防御路径）。
        c.execute(
            "HSET",
            &[
                "devreg",
                "ex2",
                r#"{"devid":"ex2","model":"mx","params":{}}"#,
            ],
        )
        .await
        .unwrap();
        assert!(
            expand_to_specs(
                &c,
                &[DeviceBinding {
                    devid: "ex2".into(),
                    bind: BTreeMap::new()
                }]
            )
            .await
            .is_err(),
            "params 不齐拒绝展开"
        );
    }

    #[tokio::test]
    async fn effective_props_merge_via_expanded() {
        let c = test_client("eff").await;
        seed(&c).await;
        let ex = expand_binding(
            &c,
            &DeviceBinding {
                devid: "ex1".into(),
                bind: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            ex.effective_props("va").unwrap().deadband,
            Some(0.5),
            "模板默认"
        );
        assert_eq!(
            ex.effective_props("vb").unwrap().deadband,
            Some(9.0),
            "覆盖生效"
        );
        assert!(ex.effective_props("nope").is_none());
    }

    #[tokio::test]
    async fn empty_bindings_expand_to_empty() {
        let c = test_client("empty").await;
        assert!(expand_to_specs(&c, &[]).await.unwrap().is_empty());
    }
}
