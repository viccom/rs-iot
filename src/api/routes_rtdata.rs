//! 实时数据查询路由：从 lux Hash `rtdata:<devId>` 读取。
//!
//! - `GET /api/v1/rtdatas`：返回所有设备 `{devId: {field: <parsed cell>}}`
//! - `GET /api/v1/rtdatas?dev_id=xxx`：返回单个设备的 `{field: <parsed cell>}`
//! - `GET /api/v1/rtdatas?tag=foo`：tag 模糊过滤（field 形如 `<devid>.<tag>`，取末段匹配）
//! - `GET /api/v1/rtdatas?limit=N&offset=M`：每个 dev 的点位内存裁剪（限点数过大场景）

use crate::api::{hgetall, AppState};
use axum::extract::{Query, State};
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;

/// 查询参数：dev_id/tag 模糊过滤 + 每设备点位裁剪（limit/offset）。
///
/// - `dev_id`：None 返回全部设备；Some 返回单设备。
/// - `tag`：模糊匹配 field 末段（`<devid>.<tag>` → 取 `<tag>` 做 `contains`），None 不过滤。
/// - `limit`/`offset`：每个 dev 的点位（field）内存裁剪，None 不裁剪。
///
/// **类型过滤不支持**：rtdata cell `[ts, localTime, value]` 无类型信息（lux 不存原始类型），
/// 类型在 `ChildSpec.type`（InstConfig.devices），devId↔inst_id 反查贵，故推前端处理。
#[derive(Deserialize)]
pub struct DevQuery {
    /// 设备 ID；为 None 时返回全部设备的实时数据。
    pub dev_id: Option<String>,
    /// tag 模糊匹配（field 末段 contains）。
    pub tag: Option<String>,
    /// 每设备点位裁剪上限（offset 起算）。
    pub limit: Option<usize>,
    /// 每设备点位裁剪偏移。
    pub offset: Option<usize>,
}

/// 取 field 末段（`<devid>.<tag>` → `<tag>`；devid 不含 `.`，故 split 末段即 tag）。
fn tag_of(field: &str) -> &str {
    field.split('.').next_back().unwrap_or(field)
}

/// 对单设备 hgetall 结果做 tag 过滤 + limit/offset 裁剪（内存）。
///
/// 返回新 Object（过滤/裁剪后的 field→cell）。filter/limit/offset 任一 None 则该步跳过。
fn filter_dev(
    data: serde_json::Value,
    tag_filter: Option<&str>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> serde_json::Value {
    // 快路径：无任何过滤/裁剪条件，原样返回。
    if tag_filter.is_none() && limit.is_none() && offset.is_none() {
        return data;
    }
    let mut entries = match data {
        serde_json::Value::Object(m) => m.into_iter().collect::<Vec<_>>(),
        other => return other,
    };
    // tag 过滤：field 末段 contains（大小写敏感，对标前端 devFilter 的 includes 语义）。
    if let Some(t) = tag_filter {
        entries.retain(|(k, _)| tag_of(k).contains(t));
    }
    // limit/offset 裁剪（offset 缺省 0）。
    let off = offset.unwrap_or(0);
    if limit.is_some() || off > 0 {
        let end = off
            .saturating_add(limit.unwrap_or(usize::MAX))
            .min(entries.len());
        entries = if off >= entries.len() {
            Vec::new()
        } else {
            entries[off..end].to_vec()
        };
    }
    serde_json::Value::Object(entries.into_iter().collect())
}

/// `GET /api/v1/rtdatas[?dev_id=xxx&tag=&limit=&offset=]`：读取 lux 实时数据。
pub async fn rtdatas(
    State(st): State<AppState>,
    Query(q): Query<DevQuery>,
) -> Json<serde_json::Value> {
    let tag = q.tag.as_deref();
    match q.dev_id.as_deref() {
        Some(dev_id) => {
            let data = hgetall(&st.lux, &format!("rtdata:{dev_id}")).await;
            Json(filter_dev(data, tag, q.limit, q.offset))
        }
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
                    let filtered = filter_dev(data, tag, q.limit, q.offset);
                    map.insert(dev, filtered);
                }
            }
            Json(serde_json::Value::Object(map))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造一个 fake dev Object：field `<dev>.<tag>` → cell。
    fn dev_obj(dev: &str, tags: &[(&str, &str)]) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for (tag, val) in tags {
            m.insert(format!("{dev}.{tag}"), json!([0, "t", val]));
        }
        serde_json::Value::Object(m)
    }

    #[test]
    fn tag_of_takes_last_segment() {
        assert_eq!(tag_of("devA.temp"), "temp");
        assert_eq!(tag_of("devB.humidity_out"), "humidity_out");
        // devid 不含 '.' 时 field 可能无分隔——取原串。
        assert_eq!(tag_of("plainfield"), "plainfield");
    }

    #[test]
    fn filter_no_args_returns_unchanged() {
        let data = dev_obj("devA", &[("temp", "1"), ("humidity", "2")]);
        let got = filter_dev(data.clone(), None, None, None);
        assert_eq!(got, data, "无过滤条件原样返回");
    }

    #[test]
    fn filter_by_tag_keeps_matching_only() {
        let data = dev_obj(
            "devA",
            &[("temp", "1"), ("humidity", "2"), ("pressure", "3")],
        );
        // tag_of 取 field 末段：<dev>.<tag> → <tag>。"hum" 匹配 humidity，不匹配 temp/pressure。
        let got = filter_dev(data, Some("hum"), None, None);
        let fields: Vec<String> = got.as_object().unwrap().keys().cloned().collect();
        assert_eq!(fields, vec!["devA.humidity".to_string()]);
    }

    #[test]
    fn filter_limit_offset_slices_fields() {
        let data = dev_obj("devA", &[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]);
        // offset=1 limit=2 → 跳过 a，取 b、c（依赖 field 顺序稳定——serde_json Map 保留插入序）。
        let got = filter_dev(data, None, Some(2), Some(1));
        let fields: Vec<&String> = got.as_object().unwrap().keys().collect();
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn filter_offset_beyond_empty() {
        let data = dev_obj("devA", &[("a", "1")]);
        let got = filter_dev(data, None, None, Some(10));
        assert!(got.as_object().unwrap().is_empty(), "offset 超界返回空");
    }

    #[test]
    fn filter_combines_tag_and_limit() {
        let data = dev_obj(
            "devA",
            &[
                ("temp1", "1"),
                ("temp2", "2"),
                ("humid1", "3"),
                ("temp3", "4"),
            ],
        );
        // tag="temp" 匹配 temp1/temp2/temp3（3 条），limit=1 offset=1 → temp2。
        let got = filter_dev(data, Some("temp"), Some(1), Some(1));
        assert_eq!(got.as_object().unwrap().len(), 1);
    }
}
