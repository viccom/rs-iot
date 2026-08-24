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

/// `POST /api/v1/migrate/inline-devices`：一次性迁移（幂等，可重复执行）。
///
/// 扫描 lux `instances` 内联点表 → `migrate::convert` → 隐式模型 + devreg 条目
/// 入册（已存在跳过）；单条目失败记入 `failures` 继续（不中断整体）。实例配置
/// 改写为 DeviceBinding 属 M-2 T1（`InstConfig.devices` 类型升级时）。
pub async fn migrate_inline(State(st): State<AppState>) -> Response {
    let v = match st.lux.execute_value("HGETALL", &["instances"]).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut converted_models = 0u64;
    let mut converted_devices = 0u64;
    let mut converted_instances = 0u64;
    let mut failures: Vec<String> = Vec::new();
    if let lux::EmbeddedValue::Array(arr) = v {
        let mut i = 0;
        while i + 1 < arr.len() {
            let raw = crate::api::str_from_value(&arr[i + 1]);
            i += 2;
            let Ok(inst) = serde_json::from_str::<crate::types::InstConfig>(&raw) else {
                continue; // 坏值条目容错跳过（同 refresh_declared 口径）
            };
            let outcome = crate::model::migrate::convert(&inst);
            let mut any = false;
            for m in &outcome.models {
                match store::get_model(&st.lux, &m.model_id).await {
                    Ok(Some(_)) => continue, // 幂等：已迁移跳过
                    Ok(None) => {}
                    Err(e) => {
                        failures.push(format!("model {}: {e}", m.model_id));
                        continue;
                    }
                }
                match store::set_model(&st.lux, m).await {
                    Ok(()) => {
                        converted_models += 1;
                        any = true;
                    }
                    Err(e) => failures.push(format!("model {}: {e}", m.model_id)),
                }
            }
            for d in &outcome.devices {
                match store::get_device(&st.lux, &d.devid).await {
                    Ok(Some(_)) => continue,
                    Ok(None) => {}
                    Err(e) => {
                        failures.push(format!("device {}: {e}", d.devid));
                        continue;
                    }
                }
                match store::set_device(&st.lux, d).await {
                    Ok(()) => {
                        converted_devices += 1;
                        any = true;
                    }
                    Err(e) => failures.push(format!("device {}: {e}", d.devid)),
                }
            }
            if any {
                converted_instances += 1;
            }
        }
    }
    Json(json!({
        "status": "ok",
        "converted_models": converted_models,
        "converted_devices": converted_devices,
        "converted_instances": converted_instances,
        "failures": failures,
    }))
    .into_response()
}
