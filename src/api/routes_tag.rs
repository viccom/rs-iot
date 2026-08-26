//! 标签元数据 API（M-4 T3 改造：tags 表退役，读写全部走模型注册表）。
//!
//! - `GET /api/v1/tags?dev_id=&history=`：**effective 视图**——`{tag_key:
//!   {…effective props, _source: "model"|"override"}}`（模型模板默认 ∪ 设备
//!   覆盖三态合并；有覆盖条目的 tag 标 `override`，纯模板派生标 `model`）；
//! - `POST /api/v1/tag/set`：写 devreg.overrides（三态：字段缺省 = 不动，
//!   显式 `null` = 恢复继承模板，值 = 覆盖。deadband 显式关死区传 `0.0`——
//!   下游过滤语义与无死区等价）；
//! - `POST /api/v1/tag/batch`：glob（`*` 通配，匹配 `<devid>.<tag>` 全键，
//!   [`crate::model::discovery::glob_match`]）批量写覆盖，返回 `{matched, updated}`；
//! - `POST /api/v1/tag/delete`：清该 tag **全部覆盖**（恢复模板默认——
//!   模型 tag 本身不删，删标签走 devmodel/update）；
//! - `GET /api/v1/tags/legacy-export`：旧 lux `tags` 表只读导出（Phase 1a
//!   数据丢弃前的升级过渡，保留一个版本——人工属性对照恢复，风险登记 §七）。

use crate::api::AppState;
use crate::model::discovery::glob_match;
use crate::model::store;
use crate::model::types::{deserialize_some, DeviceEntry, TagOverride};
use crate::types::{tag_key, TagProps};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

/// `GET /api/v1/tags` query。
#[derive(Deserialize)]
pub struct ListQuery {
    /// 设备 ID 精确过滤（缺省 = 全部）。
    pub dev_id: Option<String>,
    /// `history=true` 只列归档白名单内的标签（按 effective 值过滤）。
    pub history: Option<bool>,
}

/// effective 视图条目（props + 来源标注）。
#[derive(serde::Serialize)]
struct EffectiveView {
    #[serde(flatten)]
    props: TagProps,
    /// "model"=纯模板派生 / "override"=含设备覆盖。
    _source: &'static str,
}

/// `GET /api/v1/tags`：全注册表 effective 视图（模型∪覆盖现算——REST 冷路径，
/// 不走物化缓存以保证 _source 标注准确）。
pub async fn list(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let devs = match store::list_devices(&st.lux).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let models = match store::list_models(&st.lux).await {
        Ok(m) => m,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut out = serde_json::Map::new();
    for dev in &devs {
        if let Some(want) = &q.dev_id {
            if &dev.devid != want {
                continue;
            }
        }
        let Some(model) = models.get(&dev.model) else {
            continue; // 悬空引用（validate 拒于写入前，防御跳过）
        };
        for t in &model.tags {
            let base = base_props(t);
            let (props, source) = match dev.overrides.get(&t.name) {
                Some(ov) => (
                    crate::model::effective::merge_tag_props(&base, ov),
                    "override",
                ),
                None => (base, "model"),
            };
            if q.history == Some(true) && !props.history {
                continue;
            }
            let Ok(v) = serde_json::to_value(EffectiveView {
                props,
                _source: source,
            }) else {
                continue;
            };
            out.insert(tag_key(&dev.devid, &t.name), v);
        }
    }
    Json(serde_json::Value::Object(out)).into_response()
}

/// 模板 props 基准：顶层 `unit`/`desc` 垫底（口径同 `effective::effective_tags`）。
fn base_props(t: &crate::model::types::TagTemplate) -> TagProps {
    let mut base = t.props.clone();
    if base.unit.is_none() {
        base.unit = t.unit.clone();
    }
    if base.desc.is_none() {
        base.desc = t.desc.clone();
    }
    base
}

/// `POST /api/v1/tag/set` body：三态字段（缺省=不动，`null`=恢复继承，值=覆盖）。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct TagSetReq {
    /// 历史归档白名单覆盖。
    #[serde(default, deserialize_with = "deserialize_some")]
    pub history: Option<Option<bool>>,
    /// 死区覆盖（显式关死区传 `0.0`）。
    #[serde(default, deserialize_with = "deserialize_some")]
    pub deadband: Option<Option<f64>>,
    /// 描述覆盖。
    #[serde(default, deserialize_with = "deserialize_some")]
    pub desc: Option<Option<String>>,
    /// 工程单位覆盖。
    #[serde(default, deserialize_with = "deserialize_some")]
    pub unit: Option<Option<String>>,
}

/// 请求字段应用到覆盖条目（设备侧三态：`None`=继承模板）。
fn apply_override(ov: &mut TagOverride, req: &TagSetReq) {
    if let Some(h) = req.history {
        ov.history = h;
    }
    if let Some(d) = req.deadband {
        ov.deadband = d.map(Some);
    }
    if let Some(s) = req.desc.clone() {
        ov.desc = s;
    }
    if let Some(u) = req.unit.clone() {
        ov.unit = u;
    }
}

/// 单点写覆盖：读设备 → tag ∈ 模型校验 → 应用三态 → set_device（重验 + 缓存
/// 刷新）。返回合并后的 effective props。
async fn write_override(
    st: &AppState,
    dev_id: &str,
    tag: &str,
    req: &TagSetReq,
) -> Result<TagProps, StatusCode> {
    let mut dev: DeviceEntry = store::get_device(&st.lux, dev_id)
        .await
        .map_err(|e| {
            tracing::warn!("[tag] 读设备 {dev_id} 失败: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let model = store::get_model(&st.lux, &dev.model)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?; // 设备存在而模型悬空：写入前校验不可能
    let tmpl = model
        .tags
        .iter()
        .find(|t| t.name == tag)
        .ok_or(StatusCode::BAD_REQUEST)?; // tag 不在模型（未建模）
    let still_overridden = {
        let ov = dev.overrides.entry(tag.to_string()).or_default();
        apply_override(ov, req);
        *ov != TagOverride::default()
    };
    // 全字段 None 的空覆盖 = 已恢复继承——删除条目（devreg 不积空壳，
    // effective 视图 _source 回 "model"）。
    if !still_overridden {
        dev.overrides.remove(tag);
    }
    let merged = crate::model::effective::merge_tag_props(
        &base_props(tmpl),
        dev.overrides.get(tag).unwrap_or(&TagOverride::default()),
    );
    store::set_device(&st.lux, &dev).await.map_err(|e| {
        tracing::warn!("[tag] 写设备 {dev_id} 覆盖失败: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(merged)
}

/// `POST /api/v1/tag/set` body。
#[derive(Deserialize)]
pub struct SetReq {
    pub dev_id: String,
    pub tag: String,
    #[serde(flatten)]
    pub props: TagSetReq,
}

pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Response {
    if req.dev_id.trim().is_empty() || req.tag.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "dev_id/tag 不能为空");
    }
    match write_override(&st, &req.dev_id, &req.tag, &req.props).await {
        Ok(props) => Json(json!({
            "status": "ok",
            "key": tag_key(&req.dev_id, &req.tag),
            "props": props,
        }))
        .into_response(),
        // NOT_FOUND=设备不在 devreg；BAD_REQUEST=tag 不在模型——语义化文案。
        Err(StatusCode::NOT_FOUND) => err(StatusCode::NOT_FOUND, "device not found in devreg"),
        Err(StatusCode::BAD_REQUEST) => err(StatusCode::BAD_REQUEST, "tag not in device model"),
        Err(c) => err(c, "internal error"),
    }
}

/// `POST /api/v1/tag/batch` body：glob 批量写覆盖。
#[derive(Deserialize)]
pub struct BatchReq {
    /// glob 模式，匹配 `<devid>.<tag>` 全键（`*` 通配；如 `furnace*.temp*`）。
    pub pattern: String,
    #[serde(flatten)]
    pub props: TagSetReq,
}

pub async fn batch(State(st): State<AppState>, Json(req): Json<BatchReq>) -> Response {
    if req.pattern.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "pattern 不能为空");
    }
    // 匹配域 = effective 全量 key（模型 tag 的全集）。
    let devs = match store::list_devices(&st.lux).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let models = match store::list_models(&st.lux).await {
        Ok(m) => m,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut targets: Vec<(String, String)> = Vec::new();
    for dev in &devs {
        let Some(model) = models.get(&dev.model) else {
            continue;
        };
        for t in &model.tags {
            let key = tag_key(&dev.devid, &t.name);
            if glob_match(&req.pattern, &key) {
                targets.push((dev.devid.clone(), t.name.clone()));
            }
        }
    }
    let matched = targets.len();
    let mut updated = 0usize;
    for (devid, tag) in &targets {
        match write_override(&st, devid, tag, &req.props).await {
            Ok(_) => updated += 1,
            Err(c) => {
                // 单点失败不中断批量（最终一致口径同旧 batch_set）。
                tracing::warn!("[tag] batch 写 {devid}.{tag} 失败（{c}），继续");
            }
        }
    }
    Json(json!({"status": "ok", "matched": matched, "updated": updated})).into_response()
}

/// `POST /api/v1/tag/delete` body。
#[derive(Deserialize)]
pub struct DeleteReq {
    pub dev_id: String,
    pub tag: String,
}

pub async fn delete(State(st): State<AppState>, Json(req): Json<DeleteReq>) -> Response {
    let mut dev: DeviceEntry = match store::get_device(&st.lux, &req.dev_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, "device not found in devreg"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if dev.overrides.remove(&req.tag).is_none() {
        // 无覆盖 = 已是模板默认（幂等成功口径同旧 delete）。
        return Json(json!({"status": "ok", "cleared": false})).into_response();
    }
    if let Err(e) = store::set_device(&st.lux, &dev).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(json!({"status": "ok", "cleared": true})).into_response()
}

/// `GET /api/v1/tags/legacy-export`：旧 lux `tags` 表只读导出（升级过渡一个
/// 版本；Phase 1a 数据丢弃前人工对照恢复）。
pub async fn legacy_export(State(st): State<AppState>) -> Response {
    let v = st.lux.execute_value("HGETALL", &["tags"]).await;
    let mut out = serde_json::Map::new();
    if let Ok(lux::EmbeddedValue::Array(arr)) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let k = crate::db::str_from_value(&arr[i]);
            let raw = crate::db::str_from_value(&arr[i + 1]);
            let parsed = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
            out.insert(k, parsed);
            i += 2;
        }
    }
    Json(serde_json::Value::Object(out)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_req_three_state_wire() {
        // 缺省字段=外层 None（不动）；显式 null=内层 None（恢复继承）；值=覆盖。
        let r: SetReq =
            serde_json::from_str(r#"{"dev_id":"d","tag":"t","history":null,"deadband":1.5}"#)
                .unwrap();
        assert_eq!(r.props.history, Some(None));
        assert_eq!(r.props.deadband, Some(Some(1.5)));
        assert_eq!(r.props.desc, None, "缺省不动");
        assert_eq!(r.props.unit, None);

        let r: TagSetReq =
            serde_json::from_str(r#"{"history":true,"desc":"x","unit":null}"#).unwrap();
        assert_eq!(r.history, Some(Some(true)));
        assert_eq!(r.desc, Some(Some("x".into())));
        assert_eq!(r.unit, Some(None));
    }

    #[test]
    fn apply_override_maps_request_to_tri_state() {
        let mut ov = TagOverride {
            history: Some(true),
            deadband: Some(Some(9.9)),
            ..Default::default()
        };
        // deadband null → 清覆盖（None=继承）；history 缺省不动。
        apply_override(
            &mut ov,
            &TagSetReq {
                deadband: Some(None),
                ..Default::default()
            },
        );
        assert_eq!(ov.deadband, None, "null 恢复继承");
        assert_eq!(ov.history, Some(true), "缺省不动");
        // deadband 值 → Some(Some(v))（API 无法表达显式关死区——0.0 等价，见模块注释）。
        apply_override(
            &mut ov,
            &TagSetReq {
                deadband: Some(Some(0.0)),
                ..Default::default()
            },
        );
        assert_eq!(ov.deadband, Some(Some(0.0)));
    }
}
