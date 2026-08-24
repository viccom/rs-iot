//! 模型/设备校验（计划 T3 结构自校验 + T5 引用完整性）。
//!
//! 结构自校验（`DeviceModel::validate` / [`valid_id`]）：id 与 tag 名字符集、
//! tag 名模型内唯一、模板 addr 占位符 ⊆ 模型 params。
//! 引用完整性（T5 补）：模型存在、参数齐套、覆盖 tag ∈ 模型、绑定合法。

use crate::model::addr;
use crate::model::types::{DeviceEntry, DeviceModel};
use std::collections::BTreeSet;
use std::fmt;

/// 校验错误（结构 + 引用两族；`#[non_exhaustive]` 允许后续期新增变体）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelError {
    /// id/tag 名为空或含非法字符（空白与 `. * { } ,`——分别与 tag_key 层级、
    /// glob 通配、地址占位符语法、wire 串接冲突）。
    InvalidId {
        /// 语义名（"model_id"/"devid"/"tag"…）。
        what: &'static str,
        /// 非法值。
        value: String,
    },
    /// tag 名模型内重复。
    DuplicateTag {
        /// 模型 id。
        model: String,
        /// 重复 tag 名。
        tag: String,
    },
    /// 模板 addr 占位符未在模型 params 声明。
    UndeclaredParam {
        /// 模型 id。
        model: String,
        /// tag 名。
        tag: String,
        /// 未声明的参数名。
        param: String,
    },
    /// 模板 addr 本身非法（未闭合括号等；转 [`addr::AddrError`] 文案）。
    BadAddr {
        /// 模型 id。
        model: String,
        /// tag 名。
        tag: String,
        /// 非法原因。
        reason: String,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { what, value } => {
                write!(
                    f,
                    "{what} {value:?} 非法：须非空且不含空白与 . * {{ }} , 字符"
                )
            }
            Self::DuplicateTag { model, tag } => write!(f, "模型 {model} 的 tag {tag:?} 重复"),
            Self::UndeclaredParam { model, tag, param } => write!(
                f,
                "模型 {model} 的 tag {tag:?} 地址引用了未声明参数 {{{param}}}"
            ),
            Self::BadAddr { model, tag, reason } => {
                write!(f, "模型 {model} 的 tag {tag:?} 地址模板非法：{reason}")
            }
        }
    }
}

impl std::error::Error for ModelError {}

/// id/tag 名合法性：非空、无空白、不含 `. * { } ,`（允许中文等非 ASCII——
/// 迁移场景既有 label 如 "炉温" 合法）。
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || ['.', '*', '{', '}', ','].contains(&c))
}

/// 模型结构自校验：model_id/tag 名/addr 占位符一致性。
pub fn validate_model(m: &DeviceModel) -> Result<(), ModelError> {
    if !valid_id(&m.model_id) {
        return Err(ModelError::InvalidId {
            what: "model_id",
            value: m.model_id.clone(),
        });
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let declared: BTreeSet<&str> = m.params.iter().map(String::as_str).collect();
    for t in &m.tags {
        if !valid_id(&t.name) {
            return Err(ModelError::InvalidId {
                what: "tag",
                value: t.name.clone(),
            });
        }
        if !seen.insert(t.name.as_str()) {
            return Err(ModelError::DuplicateTag {
                model: m.model_id.clone(),
                tag: t.name.clone(),
            });
        }
        if let Some(a) = &t.addr {
            for name in addr::placeholders(a).map_err(|e| ModelError::BadAddr {
                model: m.model_id.clone(),
                tag: t.name.clone(),
                reason: e.to_string(),
            })? {
                if !declared.contains(name.as_str()) {
                    return Err(ModelError::UndeclaredParam {
                        model: m.model_id.clone(),
                        tag: t.name.clone(),
                        param: name,
                    });
                }
            }
        }
    }
    Ok(())
}

/// 设备条目结构自校验（devid 非空合法；引用完整性见 T5 的 devreg 写入校验）。
pub fn validate_device_id(dev: &DeviceEntry) -> Result<(), ModelError> {
    if !valid_id(&dev.devid) {
        return Err(ModelError::InvalidId {
            what: "devid",
            value: dev.devid.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{TagOverride, TagTemplate};

    fn ok_model() -> DeviceModel {
        DeviceModel {
            model_id: "vx_dtz341".into(),
            params: vec!["base".into(), "db".into()],
            tags: vec![
                TagTemplate {
                    name: "ua".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:{base}:f32".into()),
                    ..Default::default()
                },
                TagTemplate {
                    name: "总加".into(),
                    data_type: "f64".into(),
                    addr: Some("DB{db}.DBD{base}+4".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn valid_id_rules() {
        assert!(valid_id("vx_dtz341"));
        assert!(valid_id("_sys_rsiot"));
        assert!(valid_id("总加"), "允许中文（迁移场景既有 label）");
        assert!(!valid_id(""), "空非法");
        assert!(!valid_id("a b"), "空白非法");
        assert!(!valid_id("a.b"), "点号与 tag_key 层级冲突");
        assert!(!valid_id("a*b"), "星号与 glob 冲突");
        assert!(!valid_id("{a}"), "花括号与地址占位符冲突");
        assert!(!valid_id("a,b"), "逗号与 wire 串接冲突");
    }

    #[test]
    fn well_formed_model_passes() {
        assert_eq!(validate_model(&ok_model()), Ok(()));
    }

    #[test]
    fn empty_model_id_rejected() {
        let mut m = ok_model();
        m.model_id = String::new();
        assert_eq!(
            validate_model(&m),
            Err(ModelError::InvalidId {
                what: "model_id",
                value: String::new()
            })
        );
    }

    #[test]
    fn illegal_model_id_rejected() {
        let mut m = ok_model();
        m.model_id = "bad id".into();
        assert!(matches!(
            validate_model(&m),
            Err(ModelError::InvalidId {
                what: "model_id",
                ..
            })
        ));
    }

    #[test]
    fn duplicate_tag_name_rejected() {
        let mut m = ok_model();
        m.tags[1].name = "ua".into();
        assert_eq!(
            validate_model(&m),
            Err(ModelError::DuplicateTag {
                model: "vx_dtz341".into(),
                tag: "ua".into()
            })
        );
    }

    #[test]
    fn empty_or_illegal_tag_name_rejected() {
        let mut m = ok_model();
        m.tags[0].name = String::new();
        assert!(matches!(
            validate_model(&m),
            Err(ModelError::InvalidId { what: "tag", .. })
        ));
        let mut m = ok_model();
        m.tags[0].name = "a.b".into();
        assert!(matches!(
            validate_model(&m),
            Err(ModelError::InvalidId { what: "tag", .. })
        ));
    }

    #[test]
    fn addr_placeholder_undeclared_rejected() {
        let mut m = ok_model();
        m.params = vec!["base".into()]; // 少声明 db
        assert_eq!(
            validate_model(&m),
            Err(ModelError::UndeclaredParam {
                model: "vx_dtz341".into(),
                tag: "总加".into(),
                param: "db".into()
            })
        );
    }

    #[test]
    fn malformed_addr_template_rejected() {
        let mut m = ok_model();
        m.tags[0].addr = Some("holding:{base".into()); // 未闭合
        assert!(matches!(
            validate_model(&m),
            Err(ModelError::BadAddr { .. })
        ));
    }

    #[test]
    fn none_addr_skips_consistency() {
        // lua/纯软设备模型 addr=None（M-2 dataSim 路径）。
        let mut m = ok_model();
        m.tags[0].addr = None;
        m.tags[1].addr = None;
        assert_eq!(validate_model(&m), Ok(()));
    }

    #[test]
    fn declared_but_unused_param_allowed() {
        // params ⊇ addr 占位符即可（同模型参数被不同 tag 部分使用）。
        let mut m = ok_model();
        m.params.push("spare".into());
        assert_eq!(validate_model(&m), Ok(()));
    }

    #[test]
    fn device_id_validated() {
        let ok = DeviceEntry {
            devid: "dev1".into(),
            model: "m".into(),
            ..Default::default()
        };
        assert_eq!(validate_device_id(&ok), Ok(()));
        let bad = DeviceEntry {
            devid: "dev 1".into(),
            model: "m".into(),
            ..Default::default()
        };
        assert!(matches!(
            validate_device_id(&bad),
            Err(ModelError::InvalidId { what: "devid", .. })
        ));
    }

    #[test]
    fn override_keys_use_same_charset_via_valid_id() {
        // 覆盖键 = tag 名：同一字符集规则（BTreeMap key，写入侧校验复用 valid_id）。
        assert!(valid_id("ua"));
        assert!(!valid_id("u a"));
        let _ = TagOverride::default();
    }
}
