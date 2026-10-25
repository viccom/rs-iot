//! device CRUD——M-2 起设备**权威源 = 设备模型注册表（devmodels + devreg）**，
//! 实例配置只持绑定（`DeviceBinding{devid, bind}`）。
//!
//! - `GET /api/v1/devices`：devreg 全设备聚合（点表从设备模型展开，形态兼容
//!   旧 `{dev_id: {label, value, children, inst_id}}`——前端/MCP 消费方零改动）
//!   ∪ dev2inst 运行时补集（lua `set_value` 等未建模设备带 `runtime: true`，
//!   M-3 校验闸上线后消失）；devreg 派生条目也并入 dev2inst 归属
//!   （`owner_inst`，不带 `runtime`——devreg 仍是权威来源）；
//! - `GET /api/v1/device/get?dev_id=`：聚合后过滤单设备；
//! - `POST /api/v1/device/set`：**绑定设备到实例**（dev_id 须已存在于 devreg——
//!   元数据唯一出生地；object 为 bind 参数如 `{"slave_id": 2}`）；
//! - `POST /api/v1/device/delete`：从实例解绑（devreg 设备保留——全局实体，
//!   归属移除后无宿主才清 rtdata，同 cascade 口径）。

use crate::api::{hget, AppState};
use crate::model::store;
use crate::types::InstConfig;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

type Resp = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Resp {
    (code, msg.to_string()).into_response()
}

#[derive(Deserialize)]
pub struct IdQuery {
    pub dev_id: String,
}

/// `GET /api/v1/devices`：devreg 全设备聚合 ∪ dev2inst 运行时补集。
pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::Value::Object(aggregate(&st).await))
}

/// `GET /api/v1/device/get?dev_id=`：聚合后过滤单设备；不存在返空对象。
pub async fn get(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Json<serde_json::Value> {
    let agg = aggregate(&st).await;
    match agg.get(&q.dev_id) {
        Some(v) => Json(serde_json::json!({ q.dev_id: v })),
        None => Json(serde_json::json!({})),
    }
}

/// 聚合入口（list/get 共用）：devreg 派生 ∪ dev2inst 运行时补集；devreg 派生
/// 条目也并入 dev2inst 归属（owner_inst——前端/北向按宿主过滤设备用）。
async fn aggregate(st: &AppState) -> serde_json::Map<String, serde_json::Value> {
    let mut agg = serde_json::Map::new();
    // devreg 派生（权威）：每设备 children 从模型模板合成（label=tag 名，
    // value=参数化地址模板原样——绝对地址在实例装配期按设备 params 解析）。
    let models = store::list_models(&st.lux).await.unwrap_or_default();
    for dev in store::list_devices(&st.lux).await.unwrap_or_default() {
        let children: Vec<serde_json::Value> = models
            .get(&dev.model)
            .map(|m| {
                m.tags
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "label": t.name,
                            "value": t.addr.clone().unwrap_or_else(|| t.name.clone()),
                            "type": t.data_type,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        agg.insert(
            dev.devid.clone(),
            serde_json::json!({
                "label": dev.desc.clone().unwrap_or_else(|| dev.devid.clone()),
                "value": dev.devid,
                "model": dev.model,
                "params": dev.params,
                "children": children,
            }),
        );
    }
    // dev2inst 归属并入（D-OWN-3）：devreg 内设备给派生条目补 owner_inst
    // （不加 runtime——devreg 仍是权威来源；多宿主逗号串与补集口径一致）；
    // devreg 无此设备才走运行时补集（动态产生设备，带 runtime: true）。
    let reg = crate::db::dev_registry::all(&st.lux)
        .await
        .unwrap_or_default();
    for (devid, owners) in reg {
        if owners.is_empty() {
            continue;
        }
        let owner_inst = owners.join(",");
        if let Some(entry) = agg.get_mut(&devid) {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("owner_inst".into(), serde_json::json!(owner_inst));
            }
            continue;
        }
        agg.insert(
            devid.clone(),
            serde_json::json!({
                "label": devid, "value": devid, "children": [],
                "runtime": true, "owner_inst": owner_inst,
            }),
        );
    }
    agg
}

/// `POST /api/v1/device/set` body：`inst_id` + `dev_id` + `object`（bind 参数）。
#[derive(Deserialize)]
pub struct SetReq {
    /// 目标实例 ID。
    pub inst_id: String,
    /// devreg 设备 ID（须已存在——设备元数据唯一出生地在模型注册表）。
    pub dev_id: String,
    /// bind 参数 JSON（如 `{"slave_id": 2}`；整数自动转字符串）。
    pub object: serde_json::Value,
}

/// `POST /api/v1/device/set`：绑定设备到实例（upsert 绑定条目）。
///
/// 实例不存在 → 404；dev_id 不在 devreg → 400（指引先建设备/跑迁移）。
pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Response {
    let raw = match hget(&st.lux, "instances", &req.inst_id).await {
        Some(r) => r,
        None => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    if store::get_device(&st.lux, &req.dev_id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return err(
            StatusCode::BAD_REQUEST,
            &format!(
                "设备 {dev} 不在注册表（devreg）——先 POST /api/v1/devreg/set 建设 \
                 或执行 POST /api/v1/migrate/inline-devices 迁移",
                dev = req.dev_id
            ),
        );
    }
    let mut cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!(
                "invalid instance config: {e}（旧内联形态？先执行 /api/v1/migrate/inline-devices）"
            ),
            )
        }
    };
    let mut bind = serde_json::Map::new();
    if let Some(obj) = req.object.as_object() {
        for (k, v) in obj {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            bind.insert(k.clone(), serde_json::Value::String(s));
        }
    }
    let binding = crate::types::DeviceBinding {
        devid: req.dev_id.clone(),
        bind: bind
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
    };
    let mut found = false;
    for d in &mut cfg.devices {
        if d.devid == req.dev_id {
            *d = binding.clone();
            found = true;
            break;
        }
    }
    if !found {
        cfg.devices.push(binding);
    }
    let s = match serde_json::to_string(&cfg) {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("serialize: {e}"),
            )
        }
    };
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// `POST /api/v1/device/delete` body：`inst_id` + `dev_id`。
#[derive(Deserialize)]
pub struct DelReq {
    /// 实例 ID。
    pub inst_id: String,
    /// 解绑设备 ID（devreg 条目保留）。
    pub dev_id: String,
}

/// `POST /api/v1/device/delete`：从实例解绑（单点）。实例不存在 → 404。
///
/// 设备是全局实体：只移绑定 + dev2inst 归属；归属清空才清 rtdata
/// （多宿主共写设备删一个绑定不清数据——dev_registry::remove_owner 口径）。
pub async fn delete(State(st): State<AppState>, Json(req): Json<DelReq>) -> Response {
    let raw = match hget(&st.lux, "instances", &req.inst_id).await {
        Some(r) => r,
        None => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    let mut cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid instance config: {e}"),
            )
        }
    };
    cfg.devices.retain(|d| d.devid != req.dev_id);
    let s = match serde_json::to_string(&cfg) {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("serialize: {e}"),
            )
        }
    };
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;
    // 归属移除；列表空（无其他宿主）→ 清 rtdata（避免孤儿被北向发布）。
    match crate::db::dev_registry::remove_owner(&st.lux, &req.dev_id, &req.inst_id).await {
        Ok(true) => {
            if let Err(e) = crate::db::rtdata::delete_device(&st.lux, &req.dev_id).await {
                tracing::warn!("[routes_device] delete rtdata:{} failed: {e}", req.dev_id);
            }
        }
        Ok(false) => {}
        Err(e) => tracing::warn!("[routes_device] remove_owner {}: {e}", req.dev_id),
    }
    Json(serde_json::json!({"status": "deleted"})).into_response()
}

#[cfg(test)]
mod tests {
    /// 绑定参数 object → bind map（整数/字符串值统一字符串化）。
    #[test]
    fn bind_object_normalizes_values() {
        let obj = serde_json::json!({"slave_id": 2, "note": "x"});
        let bind: std::collections::BTreeMap<String, String> = obj
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => (k.clone(), s.clone()),
                other => (k.clone(), other.to_string()),
            })
            .collect();
        assert_eq!(bind.get("slave_id").map(String::as_str), Some("2"));
        assert_eq!(bind.get("note").map(String::as_str), Some("x"));
    }
}
