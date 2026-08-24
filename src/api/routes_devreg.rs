//! 设备注册表管理 API（M-1 T6；决策 D-M1，风格对齐 routes_tag）。
//!
//! **过渡路径**：`/api/v1/devices` 与 `/api/v1/device/set|delete` 现由
//! routes_device（实例内联点表聚合）占用——M-2 InstConfig 切 DeviceBinding
//! 时换路径并更新 MCP/前端消费方。M-1 期设备注册表 REST 走 `/api/v1/devreg*`。
//!
//! - `GET /api/v1/devreg?devid=&model=`：设备列表/过滤，含 effective 摘要
//!   （模型 tags ∪ 覆盖三态合并结果）；
//! - `POST /api/v1/devreg/set`：设备 CRUD（upsert——覆盖编辑是常规工作流；
//!   写前三级校验：devid 字符集/模型存在/params 齐/覆盖 tag ∈ 模型）；
//! - `POST /api/v1/devreg/delete`：删除 + 级联（dev2inst 归属 + rtdata；
//!   历史不级联——master plan D9）。

use crate::api::AppState;
use crate::model::effective;
use crate::model::store;
use crate::model::types::DeviceEntry;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

/// `GET /api/v1/devreg` query。
#[derive(Deserialize)]
pub struct ListQuery {
    /// 精确过滤设备 id。
    pub devid: Option<String>,
    /// 按模型 id 过滤。
    pub model: Option<String>,
}

/// `GET /api/v1/devreg`：`{devid: {…DeviceEntry, effective: {tag: TagProps}}}`。
///
/// 悬空模型引用（validate 拒于写入前，防御）设备照常列出，effective 为空对象。
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
    for dev in devs {
        if let Some(want) = &q.devid {
            if &dev.devid != want {
                continue;
            }
        }
        if let Some(want) = &q.model {
            if &dev.model != want {
                continue;
            }
        }
        let eff = models
            .get(&dev.model)
            .map(|m| effective::effective_tags(m, &dev))
            .unwrap_or_default();
        let mut v = match serde_json::to_value(&dev) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "effective".into(),
                serde_json::to_value(&eff).unwrap_or_default(),
            );
        }
        out.insert(dev.devid.clone(), v);
    }
    Json(serde_json::Value::Object(out)).into_response()
}

/// `POST /api/v1/devreg/set` body = DeviceEntry JSON。
pub async fn set(State(st): State<AppState>, Json(dev): Json<DeviceEntry>) -> Response {
    match store::set_device(&st.lux, &dev).await {
        Ok(()) => Json(json!({"status": "ok", "devid": dev.devid})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// `POST /api/v1/devreg/delete` body。
#[derive(Deserialize)]
pub struct DeleteReq {
    /// 目标设备 id。
    pub devid: String,
}

pub async fn delete(State(st): State<AppState>, Json(req): Json<DeleteReq>) -> Response {
    match store::delete_device(&st.lux, &req.devid).await {
        Ok(Some(())) => Json(json!({"status": "ok"})).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "device not found"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
