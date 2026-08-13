//! device CRUD——设备元数据**单一权威源 = 实例配置**（`InstConfig.devices`）。
//!
//! 历史设计：lux `devices` Hash 为独立设备注册表，但唯一写入方是 `device/set`
//! 手动 API，无 bootstrap 预置、建实例不同步写——是**孤立死表**（写方=手动 API，
//! 读方=同一 API，前端/pipeline/采集全不用）。REST 客户端读到空表而实例实际在
//! 采集（现场 bug）。已废弃该 Hash，统一到实例配置：
//!
//! - `GET /api/v1/devices`：从实例配置**聚合**派生（单一数据源，与前端采集器页/
//!   rtdata 类型映射同源）
//! - `GET /api/v1/device/get`：聚合后过滤单设备
//! - `POST /api/v1/device/set`：**单点实例配置操作**（body 含 `inst_id` 定位——
//!   设备挂在实例下，无实例不成设备），读-改-写整包实例配置，免去整包覆盖
//! - `POST /api/v1/device/delete`：同上，从实例点表移除该设备

use crate::api::{hget, AppState};
use crate::types::{DeviceSpec, InstConfig};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct IdQuery {
    pub dev_id: String,
}

/// `GET /api/v1/devices`：聚合所有实例配置的设备表 `{dev_id: DeviceSpec}`。
pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    let insts = crate::api::hgetall(&st.lux, "instances").await;
    Json(serde_json::Value::Object(aggregate_from_insts(&insts)))
}

/// `GET /api/v1/device/get?dev_id=`：聚合后过滤单设备；不存在返空对象。
pub async fn get(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Json<serde_json::Value> {
    let insts = crate::api::hgetall(&st.lux, "instances").await;
    let agg = aggregate_from_insts(&insts);
    match agg.get(&q.dev_id) {
        Some(v) => Json(serde_json::json!({ q.dev_id: v })),
        None => Json(serde_json::json!({})),
    }
}

/// `POST /api/v1/device/set` body：`inst_id` 定位实例 + `dev_id` + `object`（DeviceSpec）。
#[derive(Deserialize)]
pub struct SetReq {
    /// 设备所属实例 ID（单点操作定位）。
    pub inst_id: String,
    /// 设备 ID（作为 DeviceSpec.value 强制一致）。
    pub dev_id: String,
    /// DeviceSpec JSON（label/children/slave_id 等；value 以 dev_id 为准）。
    pub object: serde_json::Value,
}

/// `POST /api/v1/device/set`：单点 upsert 设备到指定实例点表（读-改-写整包实例）。
///
/// 实例不存在 → 404；object 非法 DeviceSpec → 400。upsert 语义：同 dev_id 存在则
/// 替换，否则追加。**不**签发 ext_token（不改 host_kind，与 routes_inst::set 的
/// External 分支无关）。
pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Response {
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
    let mut dev: DeviceSpec = match serde_json::from_value(req.object) {
        Ok(d) => d,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid device spec: {e}"),
            )
        }
    };
    dev.value = req.dev_id.clone(); // 强制 dev_id 一致（设备 ID 由路径字段决定）
    let mut found = false;
    for d in &mut cfg.devices {
        if d.value == req.dev_id {
            *d = dev.clone();
            found = true;
            break;
        }
    }
    if !found {
        cfg.devices.push(dev);
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
    /// 设备所属实例 ID。
    pub inst_id: String,
    /// 设备 ID（从实例点表移除）。
    pub dev_id: String,
}

/// `POST /api/v1/device/delete`：从指定实例点表移除设备（单点）。实例不存在 → 404。
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
    cfg.devices.retain(|d| d.value != req.dev_id);
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
    Json(serde_json::json!({"status": "deleted"})).into_response()
}

/// 从实例配置表聚合设备：输入 `{inst_id: InstConfig}`，输出 `{dev_id: DeviceSpec}`。
///
/// 同 dev 被多实例引用时**合并**：children 按 tag（ChildSpec.value）去重（首个实例
/// 优先），label/slave_id 取首个非空/Some。纯函数便于单测。
fn aggregate_from_insts(insts: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    let Some(cfgs) = insts.as_object() else {
        return map;
    };
    for cfg in cfgs.values() {
        let Some(devices) = cfg.get("devices").and_then(|d| d.as_array()) else {
            continue;
        };
        for dev in devices {
            let Some(dev_id) = dev.get("value").and_then(|v| v.as_str()) else {
                continue;
            };
            if dev_id.is_empty() {
                continue;
            }
            match map.get_mut(dev_id) {
                Some(existing) => merge_children(existing, dev),
                None => {
                    map.insert(dev_id.to_string(), dev.clone());
                }
            }
        }
    }
    map
}

/// 把 `new` 的 children 并入 `existing`（按 tag 去重，首个实例优先）。
///
/// existing 无 children 时先建空数组；new 无 children 时 no-op。
fn merge_children(existing: &mut serde_json::Value, new: &serde_json::Value) {
    let Some(new_children) = new.get("children").and_then(|c| c.as_array()) else {
        return;
    };
    if existing.get("children").is_none() {
        existing["children"] = serde_json::json!([]);
    }
    let mut seen: std::collections::HashSet<String> = existing
        .get("children")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ch| ch.get("value").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for ch in new_children {
        let Some(tag) = ch.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        if seen.insert(tag.to_string()) {
            if let Some(arr) = existing["children"].as_array_mut() {
                arr.push(ch.clone());
            }
        }
    }
}

// ---- 局部类型/工具 ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn insts_fixture() -> serde_json::Value {
        json!({
            "mb-1": {
                "inst_id": "mb-1", "app_code": "modbusClient", "app_type": "south",
                "app_config": {}, "auto_start": true, "host_kind": "builtin",
                "devices": [
                    {"label": "设备A", "value": "devA", "children": [
                        {"label": "温度", "value": "temp", "type": "float"},
                        {"label": "湿度", "value": "humidity", "type": "float"}
                    ]},
                    {"label": "设备B", "value": "devB", "children": [
                        {"label": "转速", "value": "speed", "type": "i16"}
                    ]}
                ]
            },
            "mb-2": {
                "inst_id": "mb-2", "app_code": "modbusClient", "app_type": "south",
                "app_config": {}, "auto_start": true, "host_kind": "builtin",
                "devices": [
                    {"label": "", "value": "devA", "children": [
                        {"label": "温度2", "value": "temp", "type": "float"},
                        {"label": "压力", "value": "pressure", "type": "u16"}
                    ]}
                ]
            }
        })
    }

    /// 验证意图（规则7）：聚合必须从实例配置派生——devices Hash 是孤立死表，
    /// 从它读 = 查不到实例实际采集的设备（现场 bug 根因）。
    #[test]
    fn aggregate_collects_all_instances_devices() {
        let agg = aggregate_from_insts(&insts_fixture());
        assert!(agg.contains_key("devA"));
        assert!(agg.contains_key("devB"));
        // devA 被两实例引用 → children 合并去重（temp 只一次，pressure 并入）。
        let dev_a = &agg["devA"];
        let tags: Vec<&str> = dev_a["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["value"].as_str())
            .collect();
        assert_eq!(
            tags,
            vec!["temp", "humidity", "pressure"],
            "合并去重+保留首个实例顺序"
        );
        assert_eq!(dev_a["label"], "设备A", "label 取首个非空");
    }

    #[test]
    fn aggregate_skips_empty_dev_id_and_non_array() {
        let insts = json!({
            "x": {"devices": [{"value": "", "children": []}, {"label": "no-value"}]},
            "y": {"devices": "not-array"}
        });
        let agg = aggregate_from_insts(&insts);
        assert!(agg.is_empty(), "空 dev_id 与非数组均跳过");
    }

    #[test]
    fn aggregate_non_object_insts_returns_empty() {
        assert!(aggregate_from_insts(&json!([1, 2])).is_empty());
    }

    #[test]
    fn merge_children_dedups_by_tag() {
        let mut existing = json!({"value": "d", "children": [{"value": "a", "type": "float"}]});
        let new = json!({"value": "d", "children": [
            {"value": "a", "type": "float"},
            {"value": "b", "type": "i16"}
        ]});
        merge_children(&mut existing, &new);
        let tags: Vec<&str> = existing["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["value"].as_str())
            .collect();
        assert_eq!(tags, vec!["a", "b"], "重复 tag 跳过，新 tag 追加");
    }

    #[test]
    fn merge_children_creates_array_when_missing() {
        let mut existing = json!({"value": "d"});
        let new = json!({"children": [{"value": "a"}]});
        merge_children(&mut existing, &new);
        assert_eq!(existing["children"].as_array().unwrap().len(), 1);
    }
}
