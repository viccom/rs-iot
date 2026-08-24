//! 模型/设备注册表 lux 存储（Hash `devmodels` field=model_id / `devreg`
//! field=devid；口径同 `db/dev_registry.rs`：EmbeddedClient 直连、RMW 非事务
//! 原子——管理面数据最终一致可接受）。
//!
//! T4：devmodels CRUD + 删除引用校验；T5：devreg CRUD。

use crate::error::RsIotError;
use crate::model::types::DeviceModel;
use crate::model::validate;
use lux::EmbeddedClient;
use std::collections::HashMap;

/// 模型表名。
const MODELS: &str = "devmodels";
/// 设备注册表名（T4 只读引用统计；T5 补 CRUD）。
const DEVICES: &str = "devreg";

/// 模型删除结果：被 devreg 引用时拒绝（REST 409 载荷为引用方 devid 列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// 已删除。
    Deleted,
    /// 被 devreg 设备引用，拒绝删除。
    Referenced(Vec<String>),
}

/// 写入模型：先 [`validate::validate_model`] 结构校验，再 HSET JSON。
///
/// upsert 语义（同 id 整体替换）——REST 层建模型走 create-only（重名 409），
/// adopt/迁移等内部更新路径直用本函数。
pub async fn set_model(c: &EmbeddedClient, m: &DeviceModel) -> Result<(), RsIotError> {
    validate::validate_model(m)?;
    let raw = serde_json::to_string(m)?;
    c.execute("HSET", &[MODELS, &m.model_id, &raw]).await?;
    Ok(())
}

/// 读单模型（缺 → None）。
pub async fn get_model(
    c: &EmbeddedClient,
    model_id: &str,
) -> Result<Option<DeviceModel>, RsIotError> {
    let v = c.execute_value("HGET", &[MODELS, model_id]).await?;
    match embedded_to_string(v) {
        Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        None => Ok(None),
    }
}

/// 全模型表（HGETALL 扁平序列解析；坏值条目跳过）。
pub async fn list_models(c: &EmbeddedClient) -> Result<HashMap<String, DeviceModel>, RsIotError> {
    let v = c.execute_value("HGETALL", &[MODELS]).await?;
    let mut out = HashMap::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let id = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !id.is_empty() {
                if let Ok(m) = serde_json::from_str::<DeviceModel>(&raw) {
                    out.insert(id, m);
                }
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 引用某模型的设备 devid 列表（删除守卫；devreg 只读，T5 补写侧）。
pub async fn referencing_devices(
    c: &EmbeddedClient,
    model_id: &str,
) -> Result<Vec<String>, RsIotError> {
    let v = c.execute_value("HGETALL", &[DEVICES]).await?;
    let mut out = Vec::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let devid = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if !devid.is_empty() {
                if let Ok(dev) = serde_json::from_str::<crate::model::types::DeviceEntry>(&raw) {
                    if dev.model == model_id {
                        out.push(devid);
                    }
                }
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 删除模型：被 devreg 引用 → [`DeleteOutcome::Referenced`]（不删）；
/// 无引用 → HDEL。模型不存在 → `None`（REST 映射 404）。
pub async fn delete_model(
    c: &EmbeddedClient,
    model_id: &str,
) -> Result<Option<DeleteOutcome>, RsIotError> {
    if get_model(c, model_id).await?.is_none() {
        return Ok(None);
    }
    let refs = referencing_devices(c, model_id).await?;
    if !refs.is_empty() {
        return Ok(Some(DeleteOutcome::Referenced(refs)));
    }
    c.execute("HDEL", &[MODELS, model_id]).await?;
    Ok(Some(DeleteOutcome::Deleted))
}

/// lux EmbeddedValue → String（Bulk/Simple；同 dev_registry 口径）。
fn embedded_to_string(v: lux::EmbeddedValue) -> Option<String> {
    match v {
        lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(&b).to_string()),
        lux::EmbeddedValue::Simple(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{TagOverride, TagTemplate};
    use crate::model::validate::ModelError;
    use crate::types::TagProps;

    /// 唯一临时 data_dir（tag 区分用例防并行互扰；dev_registry::tests 模式）。
    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("devmodels-{tag}-{}", std::process::id()))
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

    fn sample_model() -> DeviceModel {
        DeviceModel {
            model_id: "vx_dtz341".into(),
            vendor: Some("威胜".into()),
            params: vec!["base".into()],
            tags: vec![TagTemplate {
                name: "ua".into(),
                data_type: "f32".into(),
                addr: Some("holding:{base}:f32".into()),
                props: TagProps {
                    history: true,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn set_get_roundtrip() {
        let c = test_client("roundtrip").await;
        set_model(&c, &sample_model()).await.unwrap();
        let got = get_model(&c, "vx_dtz341").await.unwrap().expect("存在");
        assert_eq!(got, sample_model());
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let c = test_client("missing").await;
        assert!(get_model(&c, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_rejects_invalid_model() {
        // 结构校验在写入前：重复 tag 名拒于门外。
        let c = test_client("invalid").await;
        let mut m = sample_model();
        m.tags.push(TagTemplate {
            name: "ua".into(),
            data_type: "f64".into(),
            ..Default::default()
        });
        let err = set_model(&c, &m).await.unwrap_err();
        assert!(
            matches!(&err, RsIotError::Model(ModelError::DuplicateTag { .. })),
            "应为 Model 校验错误，实际 {err:?}"
        );
        assert!(
            get_model(&c, "vx_dtz341").await.unwrap().is_none(),
            "非法模型不入表"
        );
    }

    #[tokio::test]
    async fn list_maps_all_and_skips_bad_entries() {
        let c = test_client("list").await;
        let mut m2 = sample_model();
        m2.model_id = "m2".into();
        set_model(&c, &sample_model()).await.unwrap();
        set_model(&c, &m2).await.unwrap();
        // 坏值条目：跳过不炸（HGETALL 解析容错，同 tag_registry 口径）。
        c.execute("HSET", &[MODELS, "broken", "{not json"])
            .await
            .unwrap();
        let all = list_models(&c).await.unwrap();
        assert_eq!(all.len(), 2, "坏值跳过：{all:?}");
        assert!(all.contains_key("vx_dtz341") && all.contains_key("m2"));
    }

    /// 手工种一条 devreg 引用（T5 前的测试脚手架）。
    async fn seed_device_ref(c: &EmbeddedClient, devid: &str, model_id: &str) {
        let raw = format!(r#"{{"devid":"{devid}","model":"{model_id}"}}"#);
        c.execute("HSET", &["devreg", devid, &raw]).await.unwrap();
    }

    #[tokio::test]
    async fn referencing_devices_finds_refs() {
        let c = test_client("refs").await;
        set_model(&c, &sample_model()).await.unwrap();
        seed_device_ref(&c, "d1", "vx_dtz341").await;
        seed_device_ref(&c, "d2", "vx_dtz341").await;
        seed_device_ref(&c, "d3", "other_model").await;
        let mut refs = referencing_devices(&c, "vx_dtz341").await.unwrap();
        refs.sort();
        assert_eq!(refs, vec!["d1", "d2"]);
    }

    #[tokio::test]
    async fn delete_blocked_by_reference() {
        let c = test_client("del-ref").await;
        set_model(&c, &sample_model()).await.unwrap();
        seed_device_ref(&c, "d1", "vx_dtz341").await;
        assert_eq!(
            delete_model(&c, "vx_dtz341").await.unwrap(),
            Some(DeleteOutcome::Referenced(vec!["d1".into()])),
            "被引用拒绝删除"
        );
        assert!(
            get_model(&c, "vx_dtz341").await.unwrap().is_some(),
            "模型保留"
        );
        // 解除引用后可删。
        c.execute("HDEL", &["devreg", "d1"]).await.unwrap();
        assert_eq!(
            delete_model(&c, "vx_dtz341").await.unwrap(),
            Some(DeleteOutcome::Deleted)
        );
        assert!(get_model(&c, "vx_dtz341").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_missing_returns_none() {
        let c = test_client("del-miss").await;
        assert!(delete_model(&c, "nope").await.unwrap().is_none());
    }

    /// T5 前瞻锁定：坏 JSON 的 devreg 条目不参与引用统计（解析容错）。
    #[tokio::test]
    async fn referencing_devices_tolerates_bad_entries() {
        let c = test_client("refs-bad").await;
        c.execute("HSET", &["devreg", "bad", "{not json"])
            .await
            .unwrap();
        assert!(referencing_devices(&c, "m").await.unwrap().is_empty());
        let _ = TagOverride::default();
    }
}
