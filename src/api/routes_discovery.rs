//! 发现队列管理 API（M-4 T2；决策 D-M3——发现是建模工作流的入口）。
//!
//! - `GET /api/v1/discoveries?dev_id=`：队列列表（count 降序；`?dev_id=` 按设备过滤）；
//! - `POST /api/v1/discovery/clear`：清除（body `{dev_id?}`——缺省全清）；
//! - `POST /api/v1/discovery/adopt`：一键建模——body 指定目标 model_id + tag
//!   建议（名/类型/地址/单位）并入模型 tags（同名跳过，模型是共享模板不误改），
//!   成功后对账清除队列中已建模条目（[`model::discovery::sweep_modeled`]）。

use crate::api::AppState;
use crate::model::discovery;
use crate::model::store::{self, UpdateOutcome};
use crate::model::types::TagTemplate;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

/// `GET /api/v1/discoveries` query。
#[derive(Deserialize)]
pub struct ListQuery {
    /// 按设备 id 精确过滤（缺省 = 全部）。
    pub dev_id: Option<String>,
}

/// `GET /api/v1/discoveries`：count 降序数组（发现热点前置，建模参考）。
pub async fn list(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let all = match discovery::list(&st.lux).await {
        Ok(m) => m,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut items: Vec<discovery::Discovery> = all
        .into_iter()
        .filter(|(k, d)| match &q.dev_id {
            Some(want) => k.starts_with(&format!("{want}.")) || d.devid == *want,
            None => true,
        })
        .map(|(_, d)| d)
        .collect();
    items.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_ts.cmp(&a.last_ts)));
    Json(json!(items)).into_response()
}

/// `POST /api/v1/discovery/clear` body。
#[derive(Deserialize, Default)]
pub struct ClearReq {
    /// 限定设备（缺省 = 全清）。
    #[serde(default)]
    pub dev_id: Option<String>,
}

pub async fn clear(State(st): State<AppState>, Json(req): Json<ClearReq>) -> Response {
    match discovery::clear(&st.lux, req.dev_id.as_deref()).await {
        Ok(n) => Json(json!({"status": "ok", "cleared": n})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST /api/v1/discovery/adopt` body 的单 tag 建议。
#[derive(Deserialize)]
pub struct AdoptTag {
    /// 标签名（模型内唯一；与既有同名 → 跳过不覆盖）。
    pub name: String,
    /// 数据类型（f32/f64/i16/bool/string…，语义由消费插件解释）。
    pub data_type: String,
    /// 参数化地址建议（可选；须只引用模型已声明参数）。
    #[serde(default)]
    pub addr: Option<String>,
    /// 工程单位建议。
    #[serde(default)]
    pub unit: Option<String>,
    /// 描述建议。
    #[serde(default)]
    pub desc: Option<String>,
}

/// `POST /api/v1/discovery/adopt` body。
#[derive(Deserialize)]
pub struct AdoptReq {
    /// 目标模型 id（须已存在——404）。
    pub model_id: String,
    /// 并入模型的 tag 建议集合。
    pub tags: Vec<AdoptTag>,
}

/// 一键建模：tag 并入模型（引用预检 409/结构校验 400）→ 队列对账清除
/// （effective 缓存刷新后已建模条目出队）。返回 `{merged, swept}`。
pub async fn adopt(State(st): State<AppState>, Json(req): Json<AdoptReq>) -> Response {
    let mut model = match store::get_model(&st.lux, &req.model_id).await {
        Ok(Some(m)) => m,
        Ok(None) => return err(StatusCode::NOT_FOUND, "model not found"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let known: std::collections::BTreeSet<String> =
        model.tags.iter().map(|t| t.name.clone()).collect();
    let mut merged = 0usize;
    for t in &req.tags {
        if known.contains(&t.name) {
            continue; // 已建模（或前次 adopt）——同名不覆盖共享模板
        }
        model.tags.push(TagTemplate {
            name: t.name.clone(),
            data_type: t.data_type.clone(),
            desc: t.desc.clone(),
            unit: t.unit.clone(),
            addr: t.addr.clone(),
            // 发现的数据点是观察所得——缺省只读（最小权限），可写须人工显式改模板。
            rw: crate::model::types::TagRw::R,
            ..Default::default()
        });
        merged += 1;
    }
    if merged == 0 {
        // 无新增（全部同名）：仅对账清队（等价幂等路径）。
        return match crate::model::cache::reload(&st.lux).await {
            Ok(eff) => match discovery::sweep_modeled(&st.lux, &eff).await {
                Ok(swept) => {
                    Json(json!({"status": "ok", "merged": 0, "swept": swept})).into_response()
                }
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            },
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }
    match store::update_model(&st.lux, &model).await {
        Ok(UpdateOutcome::Updated) => {
            // set_model 写路径已刷新 effective 缓存；sweep 清除已建模队列条目。
            let eff = crate::model::cache::effective();
            match discovery::sweep_modeled(&st.lux, &eff).await {
                Ok(swept) => {
                    Json(json!({"status": "ok", "merged": merged, "swept": swept})).into_response()
                }
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        // update_model 语义上模型必存在（上文已查），NotFound 是并发删除的罕见竞态。
        Ok(UpdateOutcome::NotFound) => err(StatusCode::NOT_FOUND, "model not found"),
        Ok(UpdateOutcome::Conflict(list)) => {
            let conflicts: Vec<serde_json::Value> = list
                .iter()
                .map(|(devid, e)| json!({"devid": devid, "reason": e.to_string()}))
                .collect();
            (
                StatusCode::CONFLICT,
                Json(json!({"status": "conflict", "conflicts": conflicts})),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}
