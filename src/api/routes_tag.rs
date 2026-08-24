//! 标签属性管理 API（Tag Registry；设计见 docs/designs/2026-08-24-tag-registry-plan.md）。
//!
//! - `GET /api/v1/tags?dev_id=&history=`：全表/按设备/按 history 过滤；
//! - `POST /api/v1/tag/set`：单点改属性（owner_inst 系统管理不接受修改；
//!   全量覆盖语义：字段缺省 = 清除该属性）；
//! - `POST /api/v1/tag/batch`：glob（`*` 通配，匹配 `<devid>.<tag>` 全键）批量改
//!   **已存在**记录，返回 `{matched, updated}`——批量初始化工具，非运行时过滤规则；
//! - `POST /api/v1/tag/delete`：删属性记录（rtdata 值/后续历史不动；再出数重新占位）。

use crate::api::AppState;
use crate::db::tag_registry::{self, TagSetReq};
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
    /// `history=true` 只列归档白名单内的标签。
    pub history: Option<bool>,
}

pub async fn list(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let all = match tag_registry::list(&st.lux).await {
        Ok(m) => m,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    // dev_id 过滤：key 前缀 `<devid>.`（tag_key 规范化保证）。
    let prefix = q.dev_id.as_deref().map(|d| format!("{d}."));
    let mut out = serde_json::Map::new();
    for (key, props) in all {
        if let Some(p) = &prefix {
            if !key.starts_with(p.as_str()) {
                continue;
            }
        }
        if q.history == Some(true) && !props.history {
            continue;
        }
        out.insert(key, serde_json::to_value(&props).unwrap_or_default());
    }
    Json(serde_json::Value::Object(out)).into_response()
}

/// `POST /api/v1/tag/set` body（全量覆盖：缺省字段清除该属性）。
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
    match tag_registry::set(&st.lux, &req.dev_id, &req.tag, &req.props).await {
        Ok(props) => Json(json!({"status": "ok", "key": crate::types::tag_key(&req.dev_id, &req.tag), "props": props})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST /api/v1/tag/batch` body：glob 批量改已存在记录。
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
    match tag_registry::batch_set(&st.lux, &req.pattern, &req.props).await {
        Ok((matched, updated)) => {
            Json(json!({"status": "ok", "matched": matched, "updated": updated})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST /api/v1/tag/delete` body。
#[derive(Deserialize)]
pub struct DeleteReq {
    pub dev_id: String,
    pub tag: String,
}

pub async fn delete(State(st): State<AppState>, Json(req): Json<DeleteReq>) -> Response {
    match tag_registry::delete(&st.lux, &req.dev_id, &req.tag).await {
        Ok(()) => Json(json!({"status": "ok"})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_req_flattens_props_fields() {
        let v = serde_json::json!({"dev_id":"dev1","tag":"temp","history":true,"deadband":0.5});
        let r: SetReq = serde_json::from_value(v).unwrap();
        assert_eq!((r.dev_id.as_str(), r.tag.as_str()), ("dev1", "temp"));
        assert_eq!(r.props.history, Some(true));
        assert_eq!(r.props.deadband, Some(0.5));
        assert_eq!(r.props.desc, None);
    }

    #[test]
    fn batch_req_flattens_props() {
        let v = serde_json::json!({"pattern":"furnace*.temp*","props":null,"history":true});
        // props 字段显式 null 时 flatten 兼容（history 顶层即可）——TagSetReq 全 default 字段。
        let r: BatchReq = serde_json::from_value(v).unwrap();
        assert_eq!(r.pattern, "furnace*.temp*");
        assert_eq!(r.props.history, Some(true));
    }
}
