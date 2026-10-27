//! effective 属性合并纯函数（D-M2：`effective(tag) = 模板默认 ∪ 设备覆盖`）。
//!
//! 合并语义三态：`None` = 继承模板；`Some` = 覆盖；deadband 双层 Option 下
//! `Some(None)` = 显式关死区（与"未设"区分）。覆盖键引用了模型不存在的 tag
//! 时忽略（`validate` 拒于写入前，此处零成本防御）。
//!
//! 下游消费者：M-1 T8 物化缓存、M-4 REST effective 视图、Phase 1b 历史存储
//! 白名单/死区过滤（master plan D11）。

use crate::model::types::{DeviceEntry, DeviceModel, TagOverride};
use crate::types::{tag_key, TagProps};
use std::collections::{BTreeMap, HashMap};

/// 单 tag 三态合并：模板默认 ∪ 覆盖。
pub fn merge_tag_props(tmpl: &TagProps, ov: &TagOverride) -> TagProps {
    TagProps {
        history: ov.history.unwrap_or(tmpl.history),
        deadband: ov.deadband.unwrap_or(tmpl.deadband),
        desc: merge_opt_string(&tmpl.desc, &ov.desc),
        unit: merge_opt_string(&tmpl.unit, &ov.unit),
        // owner_inst 是运行时记账（出口登记填），模板默认通常为空、覆盖不可改。
        owner_inst: tmpl.owner_inst.clone(),
    }
}

/// 字符串覆盖语义：`None` 继承；空串显式清除（→ None）；非空覆盖。
fn merge_opt_string(tmpl: &Option<String>, ov: &Option<String>) -> Option<String> {
    match ov {
        None => tmpl.clone(),
        Some(s) if s.is_empty() => None,
        Some(s) => Some(s.clone()),
    }
}

/// 设备级合并：模型全部 tag → effective props（key = tag 名）。
///
/// 模板顶层 `unit`/`desc`（建模自然位、ConfigPush v2 载荷消费）为 props
/// 同名字段的**垫底默认**——props 显式值优先，缺省回落顶层（建模者两处
/// 任写一处，effective 视图均可见）。
///
/// 设备 overrides 中不在模型 tags 的键被忽略（validate 拒于写入前——防御）。
pub fn effective_tags(model: &DeviceModel, dev: &DeviceEntry) -> BTreeMap<String, TagProps> {
    let mut out = BTreeMap::new();
    for t in &model.tags {
        let mut base = t.props.clone();
        if base.unit.is_none() {
            base.unit = t.unit.clone();
        }
        if base.desc.is_none() {
            base.desc = t.desc.clone();
        }
        let props = match dev.overrides.get(&t.name) {
            Some(ov) => merge_tag_props(&base, ov),
            None => base,
        };
        out.insert(t.name.clone(), props);
    }
    out
}

/// 全局 effective 表：多设备 → `tag_key(devid, tag)` → effective props。
///
/// 模型引用悬空（devreg 指向不存在的 model_id）的设备跳过——validate 拒于
/// 写入前，本函数不报错（装载路径容错）。同 key 冲突 last-wins。
pub fn effective_all(
    models: &HashMap<String, DeviceModel>,
    devs: &[DeviceEntry],
) -> HashMap<String, TagProps> {
    // tag_key 碰撞 fail-closed（审计 C3）：点号歧义使不同 (devid,tag) 对可撞出
    // 同 key（("a","b.c") ≡ ("a.b","c")；contains 直通分支同族）——后写胜会
    // 让未授权方蹭到授权方的白名单/死区（越权归档）。碰撞 key 整体剔除
    // （宁可不授权，不误授权——与 L3 默认零归档的安全姿态一致）+ warn 列出
    // 冲突源，现场按日志改命名即可恢复。rest-api.md §4.14 声明 tag/devid
    // 不含点的约束不变，本防线只兜病态命名的漏网。
    let mut out: HashMap<String, TagProps> = HashMap::new();
    let mut sources: HashMap<String, String> = HashMap::new();
    let mut collisions: Vec<(String, String, String)> = Vec::new();
    for dev in devs {
        if let Some(model) = models.get(&dev.model) {
            for (tag, props) in effective_tags(model, dev) {
                let key = tag_key(&dev.devid, &tag);
                let src = format!("{}/{}", dev.devid, tag);
                match sources.get(&key) {
                    Some(prev) if prev != &src => {
                        collisions.push((key.clone(), prev.clone(), src.clone()));
                        out.remove(&key);
                    }
                    _ => {
                        sources.insert(key.clone(), src);
                        out.insert(key, props);
                    }
                }
            }
        }
    }
    for (key, a, b) in &collisions {
        tracing::warn!(
            "[model] tag_key 碰撞剔除（fail-closed）：key '{key}' 同时来自 {a} 与 {b}——含点命名歧义，请改用不含点的 tag/devid 命名"
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::TagTemplate;

    fn tmpl_props() -> TagProps {
        TagProps {
            history: true,
            deadband: Some(0.5),
            desc: Some("模板描述".into()),
            unit: Some("kPa".into()),
            owner_inst: String::new(),
        }
    }

    #[test]
    fn empty_override_inherits_all_from_template() {
        let merged = merge_tag_props(&tmpl_props(), &TagOverride::default());
        assert_eq!(merged, tmpl_props());
    }

    #[test]
    fn override_history_false_beats_template_true() {
        let ov = TagOverride {
            history: Some(false),
            ..Default::default()
        };
        let merged = merge_tag_props(&tmpl_props(), &ov);
        assert!(!merged.history, "Some(false) 显式关归档");
        assert_eq!(merged.deadband, Some(0.5), "未覆盖字段继承");
    }

    #[test]
    fn deadband_tri_state_inherit_off_and_override() {
        let t = tmpl_props();
        // None（缺省）= 继承模板 Some(0.5)。
        assert_eq!(
            merge_tag_props(&t, &TagOverride::default()).deadband,
            Some(0.5)
        );
        // Some(None) = 显式关死区（模板有死区也被关掉）。
        assert_eq!(
            merge_tag_props(
                &t,
                &TagOverride {
                    deadband: Some(None),
                    ..Default::default()
                }
            )
            .deadband,
            None
        );
        // Some(Some(2.0)) = 覆盖。
        assert_eq!(
            merge_tag_props(
                &t,
                &TagOverride {
                    deadband: Some(Some(2.0)),
                    ..Default::default()
                }
            )
            .deadband,
            Some(2.0)
        );
    }

    #[test]
    fn desc_empty_string_clears_and_value_overrides() {
        let t = tmpl_props();
        // 空串 = 显式清除。
        assert_eq!(
            merge_tag_props(
                &t,
                &TagOverride {
                    desc: Some(String::new()),
                    ..Default::default()
                }
            )
            .desc,
            None
        );
        // 非空 = 覆盖。
        assert_eq!(
            merge_tag_props(
                &t,
                &TagOverride {
                    desc: Some("现场描述".into()),
                    ..Default::default()
                }
            )
            .desc,
            Some("现场描述".to_string())
        );
    }

    #[test]
    fn unit_override_and_clear() {
        let t = tmpl_props();
        assert_eq!(
            merge_tag_props(
                &t,
                &TagOverride {
                    unit: Some("MPa".into()),
                    ..Default::default()
                }
            )
            .unit,
            Some("MPa".to_string())
        );
        assert_eq!(
            merge_tag_props(
                &t,
                &TagOverride {
                    unit: Some(String::new()),
                    ..Default::default()
                }
            )
            .unit,
            None
        );
    }

    #[test]
    fn owner_inst_passes_through_from_template() {
        let t = TagProps {
            owner_inst: "sim1".into(),
            ..tmpl_props()
        };
        let merged = merge_tag_props(
            &t,
            &TagOverride {
                history: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            merged.owner_inst, "sim1",
            "owner_inst 运行时记账，覆盖不可改"
        );
    }

    fn model_two_tags() -> DeviceModel {
        DeviceModel {
            model_id: "m1".into(),
            params: vec!["base".into()],
            tags: vec![
                TagTemplate {
                    name: "ua".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:{base}:f32".into()),
                    props: TagProps {
                        history: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                TagTemplate {
                    name: "ub".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:{base}+2:f32".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn effective_tags_covers_all_model_tags_and_merges() {
        let m = model_two_tags();
        let dev = DeviceEntry {
            devid: "d1".into(),
            model: "m1".into(),
            params: [("base".to_string(), "100".to_string())].into(),
            overrides: [(
                "ub".to_string(),
                TagOverride {
                    history: Some(true),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let eff = effective_tags(&m, &dev);
        assert_eq!(eff.len(), 2);
        assert!(eff["ua"].history, "无覆盖走模板默认");
        assert!(eff["ub"].history, "覆盖生效");
    }

    #[test]
    fn template_toplevel_unit_desc_fallback_into_props() {
        // 顶层 unit/desc 是 props 同名字段的垫底默认（props 优先）。
        let mut m = model_two_tags();
        m.tags[0].unit = Some("V".into());
        m.tags[0].desc = Some("顶层描述".into());
        m.tags[1].unit = Some("A".into());
        m.tags[1].props.unit = Some("props 优先".into());
        let dev = DeviceEntry {
            devid: "d1".into(),
            model: "m1".into(),
            ..Default::default()
        };
        let eff = effective_tags(&m, &dev);
        assert_eq!(eff["ua"].unit.as_deref(), Some("V"), "顶层垫底");
        assert_eq!(eff["ua"].desc.as_deref(), Some("顶层描述"));
        assert_eq!(
            eff["ub"].unit.as_deref(),
            Some("props 优先"),
            "props 显式值优先于顶层"
        );
    }

    #[test]
    fn effective_tags_ignores_override_of_unknown_tag() {
        // 覆盖键引用模型不存在的 tag：忽略不 panic（validate 拒于写入前）。
        let m = model_two_tags();
        let dev = DeviceEntry {
            devid: "d1".into(),
            model: "m1".into(),
            overrides: [(
                "no_such_tag".to_string(),
                TagOverride {
                    history: Some(true),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let eff = effective_tags(&m, &dev);
        assert_eq!(eff.len(), 2, "未知覆盖键不产出条目");
    }

    #[test]
    fn effective_all_keys_by_tag_key_and_skips_dangling_model() {
        let mut models = HashMap::new();
        models.insert("m1".to_string(), model_two_tags());
        let d1 = DeviceEntry {
            devid: "devA".into(),
            model: "m1".into(),
            ..Default::default()
        };
        let d2 = DeviceEntry {
            devid: "devB".into(),
            model: "m1".into(),
            ..Default::default()
        };
        // 悬空模型引用：validate 拒于写入前，装载容错跳过。
        let dangling = DeviceEntry {
            devid: "devC".into(),
            model: "nope".into(),
            ..Default::default()
        };
        let all = effective_all(&models, &[d1, d2, dangling]);
        assert_eq!(all.len(), 4, "两设备 × 两 tag；悬空设备零条目");
        assert!(all.contains_key("devA.ua"));
        assert!(all.contains_key("devB.ub"));
    }
}
#[cfg(test)]
mod collision_tests {
    use super::*;
    use crate::model::types::{DeviceEntry, DeviceModel, TagTemplate};

    fn model(tag_name: &str) -> DeviceModel {
        DeviceModel {
            model_id: "m".into(),
            tags: vec![TagTemplate {
                name: tag_name.into(),
                data_type: "f32".into(),
                props: TagProps {
                    history: true,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn dev(devid: &str, model_id: &str) -> DeviceEntry {
        DeviceEntry {
            devid: devid.into(),
            model: model_id.into(),
            ..Default::default()
        }
    }

    /// C3 红测①（join 碰撞）：("a","b.c") 与 ("a.b","c") 撞出同 key
    /// "a.b.c"——碰撞 key 必须整体剔除（fail-closed，不授权任何一方）。
    #[test]
    fn collision_join_fail_closed() {
        let models = HashMap::from([
            ("m2".to_string(), model("b.c")),
            ("m1".to_string(), model("c")),
        ]);
        let devs = [dev("a", "m2"), dev("a.b", "m1")];
        let out = effective_all(&models, &devs);
        assert!(
            !out.contains_key("a.b.c"),
            "碰撞 key 必须剔除（fail-closed）: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    /// C3 红测②（无碰撞零影响）。
    #[test]
    fn no_collision_passthrough() {
        let models = HashMap::from([("m1".to_string(), model("c"))]);
        let devs = [dev("devA", "m1")];
        let out = effective_all(&models, &devs);
        assert!(
            out.contains_key("devA.c"),
            "正常命名不受影响: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }
}
