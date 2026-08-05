//! 实时数据查询路由：从 lux Hash `rtdata:<devId>` 读取。
//!
//! - `GET /api/v1/rtdatas`：返回所有设备 `{devId: {field: <parsed cell>}}`
//! - `GET /api/v1/rtdatas?dev_id=xxx`：返回单个设备的 `{field: <parsed cell>}`

use crate::api::{hgetall, AppState};
use axum::extract::{Query, State};
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;

/// 可选 dev_id 查询参数。
#[derive(Deserialize)]
pub struct DevQuery {
    /// 设备 ID；为 None 时返回全部设备的实时数据。
    pub dev_id: Option<String>,
}

/// `GET /api/v1/rtdatas[?dev_id=xxx]`：读取 lux 实时数据。
pub async fn rtdatas(
    State(st): State<AppState>,
    Query(q): Query<DevQuery>,
) -> Json<serde_json::Value> {
    match q.dev_id.as_deref() {
        Some(dev_id) => Json(hgetall(&st.lux, &format!("rtdata:{dev_id}")).await),
        None => {
            let mut map = serde_json::Map::new();
            if let Ok(EmbeddedValue::Array(keys)) =
                st.lux.execute_value("KEYS", &["rtdata:*"]).await
            {
                for k in keys {
                    let key = match &k {
                        EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
                        EmbeddedValue::Simple(s) => s.clone(),
                        _ => continue,
                    };
                    let dev = key.strip_prefix("rtdata:").unwrap_or(&key).to_string();
                    let data = hgetall(&st.lux, &key).await;
                    map.insert(dev, data);
                }
            }
            Json(serde_json::Value::Object(map))
        }
    }
}
