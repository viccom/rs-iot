//! 模型/设备注册表 lux 存储（Hash `devmodels` field=model_id / `devreg`
//! field=devid；口径同 `db/dev_registry.rs`：EmbeddedClient 直连、RMW 非事务
//! 原子——管理面数据最终一致可接受）。
//!
//! T4：devmodels CRUD + 删除引用校验；T5：devreg CRUD。

use crate::error::RsIotError;
use crate::model::types::{DeviceEntry, DeviceModel};
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

/// 模型更新结果（[`update_model`]）：引用完整性预检不通过时拒绝落盘。
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOutcome {
    /// 已更新（同型号设备 effective 随模板生效）。
    Updated,
    /// 模型不存在（REST 映射 404）。
    NotFound,
    /// 新模板与既有引用设备冲突（REST 409 载荷）；`(devid, 原因)` 列表。
    Conflict(Vec<(String, validate::ModelError)>),
}

/// 更新模型：结构校验 + **引用设备预检**（每台引用设备在新模板下重验
/// params 齐套/覆盖 tag ∈ tags），全部通过才落盘——预检先行则无需回滚。
pub async fn update_model(
    c: &EmbeddedClient,
    m: &DeviceModel,
) -> Result<UpdateOutcome, RsIotError> {
    if get_model(c, &m.model_id).await?.is_none() {
        return Ok(UpdateOutcome::NotFound);
    }
    validate::validate_model(m)?;
    let refs = referencing_devices(c, &m.model_id).await?;
    let mut conflicts = Vec::new();
    for devid in &refs {
        if let Some(dev) = get_device(c, devid).await? {
            if let Err(e) = validate::validate_device_entry(&dev, m) {
                conflicts.push((devid.clone(), e));
            }
        }
    }
    if !conflicts.is_empty() {
        return Ok(UpdateOutcome::Conflict(conflicts));
    }
    set_model(c, m).await?;
    Ok(UpdateOutcome::Updated)
}

/// 写入模型：先 [`validate::validate_model`] 结构校验，再 HSET JSON。
///
/// upsert 语义（同 id 整体替换）——REST 层建模型走 create-only（重名 409），
/// adopt/迁移等内部更新路径直用本函数。
pub async fn set_model(c: &EmbeddedClient, m: &DeviceModel) -> Result<(), RsIotError> {
    validate::validate_model(m)?;
    let raw = serde_json::to_string(m)?;
    c.execute("HSET", &[MODELS, &m.model_id, &raw]).await?;
    crate::model::cache::refresh_after_write(c).await;
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
    crate::model::cache::refresh_after_write(c).await;
    Ok(Some(DeleteOutcome::Deleted))
}

// ---------- devreg（T5） ----------

/// 写入设备条目：结构校验 + 模型存在（→ `ModelNotFound`）+ 引用完整性后 HSET。
///
/// upsert 语义（同 devid 整体替换）——设备覆盖编辑（三态 overrides）是常规
/// 工作流，与模型的 create-only（REST 重名 409）不同。
pub async fn set_device(c: &EmbeddedClient, dev: &DeviceEntry) -> Result<(), RsIotError> {
    validate::validate_device_id(dev)?;
    let model = get_model(c, &dev.model).await?.ok_or_else(|| {
        RsIotError::Model(validate::ModelError::ModelNotFound {
            devid: dev.devid.clone(),
            model: dev.model.clone(),
        })
    })?;
    validate::validate_device_entry(dev, &model)?;
    let raw = serde_json::to_string(dev)?;
    c.execute("HSET", &[DEVICES, &dev.devid, &raw]).await?;
    crate::model::cache::refresh_after_write(c).await;
    Ok(())
}

/// 读单设备（缺 → None）。
pub async fn get_device(
    c: &EmbeddedClient,
    devid: &str,
) -> Result<Option<DeviceEntry>, RsIotError> {
    let v = c.execute_value("HGET", &[DEVICES, devid]).await?;
    match embedded_to_string(v) {
        Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        None => Ok(None),
    }
}

/// 全设备表（HGETALL；坏值条目跳过）。
pub async fn list_devices(c: &EmbeddedClient) -> Result<Vec<DeviceEntry>, RsIotError> {
    let v = c.execute_value("HGETALL", &[DEVICES]).await?;
    let mut out = Vec::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            if let Ok(dev) = serde_json::from_str::<DeviceEntry>(&raw) {
                out.push(dev);
            }
            i += 2;
        }
    }
    Ok(out)
}

/// 删除设备 + 级联（计划 §四完整语义）：**解绑实例引用**（instances JSON 的
/// devices 数组移除该 devid 条目，其余字段原样——同 migrate 改写口径）+ HDEL
/// devreg + 清 dev2inst 归属 + 删 `rtdata:<devid>`（best-effort，对齐
/// routes_device 删除口径；历史不级联——master plan D9）。返回解绑实例数；
/// 不存在 → `None`。
pub async fn delete_device(c: &EmbeddedClient, devid: &str) -> Result<Option<usize>, RsIotError> {
    if get_device(c, devid).await?.is_none() {
        return Ok(None);
    }
    let unbound = unbind_from_instances(c, devid).await?;
    c.execute("HDEL", &[DEVICES, devid]).await?;
    c.execute("HDEL", &["dev2inst", devid]).await?;
    if let Err(e) = crate::db::rtdata::delete_device(c, devid).await {
        tracing::warn!("[model] delete rtdata:{devid} failed: {e}");
    }
    crate::model::cache::refresh_after_write(c).await;
    Ok(Some(unbound))
}

/// 从 lux `instances` 全表移除对 `devid` 的绑定（Value 级改写 devices 数组，
/// 其余字段保留）。无 devices 字段的实例（luaNorth 等）与坏值条目不动。
/// 返回改写的实例数。
async fn unbind_from_instances(c: &EmbeddedClient, devid: &str) -> Result<usize, RsIotError> {
    let v = c.execute_value("HGETALL", &["instances"]).await?;
    let mut n = 0usize;
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let inst_id = embedded_to_string(arr[i].clone()).unwrap_or_default();
            let raw = embedded_to_string(arr[i + 1].clone()).unwrap_or_default();
            i += 2;
            if inst_id.is_empty() {
                continue;
            }
            let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue; // 坏值容错（同 migrate 口径）
            };
            let Some(devs) = json.get_mut("devices").and_then(|d| d.as_array_mut()) else {
                continue;
            };
            let before = devs.len();
            devs.retain(|d| d.get("devid").and_then(|x| x.as_str()) != Some(devid));
            if devs.len() != before {
                let rewritten = serde_json::to_string(&json)?;
                c.execute("HSET", &["instances", &inst_id, &rewritten])
                    .await?;
                n += 1;
                tracing::info!("[model] 实例 {inst_id} 已解绑设备 {devid}（设备删除级联）");
            }
        }
    }
    Ok(n)
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

    // ===== T5 devreg CRUD =====

    fn device_entry(devid: &str) -> crate::model::types::DeviceEntry {
        crate::model::types::DeviceEntry {
            devid: devid.into(),
            model: "vx_dtz341".into(),
            params: [("base".to_string(), "100".to_string())].into(),
            overrides: [(
                "ua".to_string(),
                TagOverride {
                    deadband: Some(Some(0.5)),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn set_get_device_roundtrip() {
        let c = test_client("dev-rt").await;
        set_model(&c, &sample_model()).await.unwrap();
        set_device(&c, &device_entry("d1")).await.unwrap();
        let got = get_device(&c, "d1").await.unwrap().expect("存在");
        assert_eq!(got, device_entry("d1"));
    }

    #[tokio::test]
    async fn set_device_unknown_model_rejected() {
        let c = test_client("dev-nomodel").await;
        let err = set_device(&c, &device_entry("d1")).await.unwrap_err();
        assert!(
            matches!(
                &err,
                RsIotError::Model(ModelError::ModelNotFound { model, .. }) if model == "vx_dtz341"
            ),
            "应为 ModelNotFound，实际 {err:?}"
        );
        assert!(get_device(&c, "d1").await.unwrap().is_none(), "不入表");
    }

    #[tokio::test]
    async fn set_device_incomplete_params_rejected() {
        let c = test_client("dev-param").await;
        set_model(&c, &sample_model()).await.unwrap();
        let mut dev = device_entry("d1");
        dev.params.clear();
        let err = set_device(&c, &dev).await.unwrap_err();
        assert!(
            matches!(
                &err,
                RsIotError::Model(ModelError::MissingDeviceParam { param, .. }) if param == "base"
            ),
            "应为 MissingDeviceParam，实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn set_device_unknown_override_rejected() {
        let c = test_client("dev-ov").await;
        set_model(&c, &sample_model()).await.unwrap();
        let mut dev = device_entry("d1");
        dev.overrides
            .insert("nope".to_string(), TagOverride::default());
        assert!(matches!(
            set_device(&c, &dev).await,
            Err(RsIotError::Model(ModelError::UnknownTagOverride { .. }))
        ));
    }

    #[tokio::test]
    async fn set_device_upserts_overrides() {
        // 设备 upsert：覆盖编辑是常规工作流（与模型 create-only 不同）。
        let c = test_client("dev-upsert").await;
        set_model(&c, &sample_model()).await.unwrap();
        set_device(&c, &device_entry("d1")).await.unwrap();
        let mut edited = device_entry("d1");
        edited.overrides.insert(
            "ua".to_string(),
            TagOverride {
                history: Some(true),
                ..Default::default()
            },
        );
        set_device(&c, &edited).await.unwrap();
        let got = get_device(&c, "d1").await.unwrap().unwrap();
        assert_eq!(got, edited, "同 devid 整体替换");
    }

    #[tokio::test]
    async fn list_devices_maps_entries() {
        let c = test_client("dev-list").await;
        set_model(&c, &sample_model()).await.unwrap();
        set_device(&c, &device_entry("d1")).await.unwrap();
        set_device(&c, &device_entry("d2")).await.unwrap();
        c.execute("HSET", &["devreg", "bad", "{not json"])
            .await
            .unwrap();
        let all = list_devices(&c).await.unwrap();
        assert_eq!(all.len(), 2, "坏值跳过：{all:?}");
    }

    #[tokio::test]
    async fn delete_device_cascades_dev2inst_and_rtdata() {
        let c = test_client("dev-del").await;
        set_model(&c, &sample_model()).await.unwrap();
        set_device(&c, &device_entry("d1")).await.unwrap();
        // 种归属与实时数据。
        crate::db::dev_registry::note(&c, "d1", "inst-a")
            .await
            .unwrap();
        c.execute("HSET", &["rtdata:d1", "d1.temp", "[1,1,42]"])
            .await
            .unwrap();
        assert_eq!(delete_device(&c, "d1").await.unwrap(), Some(0));
        assert!(get_device(&c, "d1").await.unwrap().is_none());
        let v = c.execute_value("HGET", &["dev2inst", "d1"]).await.unwrap();
        assert!(matches!(v, lux::EmbeddedValue::Nil), "归属清理");
        let exists = c.execute_value("EXISTS", &["rtdata:d1"]).await.unwrap();
        assert!(
            !matches!(exists, lux::EmbeddedValue::Int(n) if n != 0),
            "rtdata 清理"
        );
    }

    #[tokio::test]
    async fn delete_device_missing_returns_none() {
        let c = test_client("dev-del-miss").await;
        assert!(delete_device(&c, "nope").await.unwrap().is_none());
    }

    /// 计划 §四删除级联完整语义：解绑实例引用（instances JSON 的 devices
    /// 数组移除该 devid 条目，其余字段/实例原样保留）。
    #[tokio::test]
    async fn delete_device_unbinds_instance_references() {
        let c = test_client("dev-unbind").await;
        set_model(&c, &sample_model()).await.unwrap();
        set_device(&c, &device_entry("d9")).await.unwrap();
        // i1 绑定 d9 + other（部分解绑）；i2 仅绑 d9（清空数组）；i3 无 devices（不动）。
        c.execute(
            "HSET",
            &[
                "instances",
                "i1",
                r#"{"inst_id":"i1","app_code":"modbusClient","app_config":{"endpoint":"x"},"devices":[{"devid":"d9","bind":{"slave_id":"3"}},{"devid":"other"}]}"#,
            ],
        )
        .await
        .unwrap();
        c.execute(
            "HSET",
            &[
                "instances",
                "i2",
                r#"{"inst_id":"i2","app_code":"dataSim","devices":[{"devid":"d9"}]}"#,
            ],
        )
        .await
        .unwrap();
        let i3_raw = r#"{"inst_id":"i3","app_code":"luaNorth"}"#;
        c.execute("HSET", &["instances", "i3", i3_raw])
            .await
            .unwrap();

        assert!(delete_device(&c, "d9").await.unwrap().is_some());
        // i1：d9 条目移除、other 保留、其余字段不动。
        let v = c.execute_value("HGET", &["instances", "i1"]).await.unwrap();
        let j: serde_json::Value = serde_json::from_str(&embedded_to_string(v).unwrap()).unwrap();
        let devs: Vec<String> = j["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["devid"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(devs, vec!["other"], "d9 解绑、other 保留");
        assert_eq!(j["app_code"], "modbusClient", "其余字段不动");
        // i2：devices 清空（实例保留）。
        let v = c.execute_value("HGET", &["instances", "i2"]).await.unwrap();
        let j: serde_json::Value = serde_json::from_str(&embedded_to_string(v).unwrap()).unwrap();
        assert!(j["devices"].as_array().unwrap().is_empty(), "清空不删实例");
        // i3：无 devices 字段原样。
        let v = c.execute_value("HGET", &["instances", "i3"]).await.unwrap();
        assert_eq!(embedded_to_string(v).unwrap(), i3_raw, "无绑定实例不动");
    }
}
