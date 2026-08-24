//! 设备模型管理 API（M-1 T6；决策 D-M2，风格对齐 routes_tag）。
//!
//! - `GET /api/v1/devmodels?model_id=`：模型列表 / 单查；
//! - `POST /api/v1/devmodel/set`：建模型（**create-only**：重名 409——模型是
//!   多设备共享模板，误覆盖代价高；adopt 等更新路径走 store 层）；
//! - `POST /api/v1/devmodel/delete`：删模型（被 devreg 引用 → 409 + 引用列表）。

use crate::api::AppState;
use crate::model::store::{self, DeleteOutcome};
use crate::model::types::DeviceModel;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

/// `GET /api/v1/devmodels` query。
#[derive(Deserialize)]
pub struct ListQuery {
    /// 单查模型 id（缺省 = 全部）。
    pub model_id: Option<String>,
}

/// `GET /api/v1/devmodels`：`{model_id: DeviceModel}` 对象（单查命中即单键）。
pub async fn list(State(st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let all = match store::list_models(&st.lux).await {
        Ok(m) => m,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut out = serde_json::Map::new();
    for (id, m) in all {
        if let Some(want) = &q.model_id {
            if &id != want {
                continue;
            }
        }
        let Ok(v) = serde_json::to_value(&m) else {
            continue;
        };
        out.insert(id, v);
    }
    Json(serde_json::Value::Object(out)).into_response()
}

/// `POST /api/v1/devmodel/set` body = DeviceModel JSON（缺省字段容错）。
pub async fn set(State(st): State<AppState>, Json(m): Json<DeviceModel>) -> Response {
    match store::get_model(&st.lux, &m.model_id).await {
        Ok(Some(_)) => {
            return err(
                StatusCode::CONFLICT,
                &format!(
                    "model {} already exists（create-only；删除后重建或走迁移）",
                    m.model_id
                ),
            );
        }
        Ok(None) => {}
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    match store::set_model(&st.lux, &m).await {
        Ok(()) => Json(json!({"status": "ok", "model_id": m.model_id})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// `POST /api/v1/devmodel/delete` body。
#[derive(Deserialize)]
pub struct DeleteReq {
    /// 目标模型 id。
    pub model_id: String,
}

pub async fn delete(State(st): State<AppState>, Json(req): Json<DeleteReq>) -> Response {
    match store::delete_model(&st.lux, &req.model_id).await {
        Ok(None) => err(StatusCode::NOT_FOUND, "model not found"),
        Ok(Some(DeleteOutcome::Deleted)) => Json(json!({"status": "ok"})).into_response(),
        Ok(Some(DeleteOutcome::Referenced(refs))) => {
            let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "conflict",
                    "referenced_by": refs,
                })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
