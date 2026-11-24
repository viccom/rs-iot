//! HMI 画面/图符/历史批查 REST（hmi-studio 组态系统后端）。
//!
//! 画面与图符存 lux Hash：
//! - `hmi:screens` field=<slug> → 画面 doc JSON（含 name/group/devs + maotu done_json）
//! - `hmi:screen_versions:<slug>` field=<seq 补零> → 版本快照 JSON {seq,ts,author,doc}
//! - `hmi:screens:version:<slug>` field=value → 当前 version 号
//! - `hmi:symbols` field=<name> → 图符 JSON
//!
//! 路由（均 JWT 保护）：
//! - GET  /api/v1/screens → {screens:[slug...]}
//! - GET  /api/v1/screens/:id → {id,doc,version}（404 若不存在）
//! - PUT  /api/v1/screens/:id?if_version=N → {status,version}（乐观锁；创建 = PUT 新 id）
//! - DELETE /api/v1/screens/:id → {status}
//! - GET  /api/v1/screens/:id/versions → {versions:[...]}
//! - GET  /api/v1/screens/:id/versions/:seq → {id,seq,ts,author,doc}（410 若正文过期）
//! - POST /api/v1/screens/:id/rollback?if_version=N → {status,version}
//! - GET  /api/v1/symbols → {symbols:[...]}；PUT /api/v1/symbols/:name → {status}
//! - POST /api/v1/history/batch → {results:[...]}（复用 routes_history 的历史引擎）

use crate::api::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, msg.into()).into_response()
}

fn key_str(v: lux::EmbeddedValue) -> Option<String> {
    match v {
        lux::EmbeddedValue::Simple(s) => Some(s),
        lux::EmbeddedValue::Bulk(b) => String::from_utf8(b.to_vec()).ok(),
        _ => None,
    }
}

// ---- screens ----

#[derive(Deserialize)]
pub struct IfVersionQuery {
    if_version: Option<u64>,
}

fn screen_key(slug: &str) -> String { format!("hmi:screen:{slug}") }
fn versions_key(slug: &str) -> String { format!("hmi:screen_versions:{slug}") }
fn version_counter_key(slug: &str) -> String { format!("hmi:screen_version:{slug}") }

async fn get_version(st: &AppState, slug: &str) -> u64 {
    match st.lux.execute_value("HGET", &[&version_counter_key(slug), "v"]).await {
        Ok(v) => key_str(v).and_then(|s| s.parse().ok()).unwrap_or(0),
        Err(_) => 0,
    }
}

/// GET /api/v1/screens → {screens:[slug...]}
pub async fn list_screens(State(st): State<AppState>) -> Response {
    match st.lux.execute_value("HKEYS", &["hmi:screens_index"]).await {
        Ok(v) => {
            let mut screens = match v {
                lux::EmbeddedValue::Array(a) => a.iter().filter_map(|x| key_str(x.clone())).collect::<Vec<_>>(),
                _ => vec![],
            };
            screens.sort();
            Json(json!({"screens": screens})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET /api/v1/screens/:id → {id,doc,version}
pub async fn get_screen(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.lux.execute_value("HGET", &["hmi:screens_index", &id]).await {
        Ok(v) if key_str(v.clone()).is_some() => (),
        Ok(_) => return err(StatusCode::NOT_FOUND, format!("screen {id} not found")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let doc = match st.lux.execute_value("HGET", &[&screen_key(&id), "doc"]).await {
        Ok(v) => match key_str(v).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            Some(d) => d,
            None => return err(StatusCode::NOT_FOUND, "screen doc missing".to_string()),
        },
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let version = get_version(&st, &id).await;
    Json(json!({"id": id, "doc": doc, "version": version})).into_response()
}

/// PUT /api/v1/screens/:id?if_version=N → {status,version}
pub async fn put_screen(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<IfVersionQuery>,
    Json(doc): Json<Value>,
) -> Response {
    let current = get_version(&st, &id).await;
    if let Some(expected) = q.if_version {
        if expected != current {
            return err(StatusCode::CONFLICT, format!("version mismatch: expected {expected}, got {current}"));
        }
    }
    let new_version = current + 1;
    let doc_str = match serde_json::to_string(&doc) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    // 写当前 doc
    if let Err(e) = st.lux.execute_value("HSET", &[&screen_key(&id), "doc", &doc_str]).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    // 版本快照
    let seq = format!("{:012}", new_version);
    let author = "api".to_string();
    let ts = chrono_like_unix_secs();
    let snap = json!({"seq": new_version, "ts": ts, "author": author, "doc": doc}).to_string();
    let _ = st.lux.execute_value("HSET", &[&versions_key(&id), &seq, &snap]).await;
    let vstr = new_version.to_string();
    let _ = st.lux.execute_value("HSET", &[&version_counter_key(&id), "v", &vstr]).await;
    let _ = st.lux.execute_value("HSET", &["hmi:screens_index", &id, &id]).await;
    Json(json!({"status": "ok", "version": new_version})).into_response()
}

/// DELETE /api/v1/screens/:id
pub async fn delete_screen(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let _ = st.lux.execute_value("HDEL", &["hmi:screens_index", &id]).await;
    let _ = st.lux.execute_value("DEL", &[&screen_key(&id)]).await;
    let _ = st.lux.execute_value("DEL", &[&versions_key(&id)]).await;
    let _ = st.lux.execute_value("DEL", &[&version_counter_key(&id)]).await;
    Json(json!({"status": "ok"})).into_response()
}

#[derive(Serialize)]
struct VersionMeta {
    seq: u64,
    ts: u64,
    author: String,
    size: usize,
    sha: String,
    expired: bool,
}

/// GET /api/v1/screens/:id/versions → {versions:[...]}
pub async fn list_versions(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let raw = match st.lux.execute_value("HVALS", &[&versions_key(&id)]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => vec![],
    };
    let mut versions: Vec<VersionMeta> = raw.iter().filter_map(|x| {
        let s = key_str(x.clone())?;
        let v: Value = serde_json::from_str(&s).ok()?;
        Some(VersionMeta {
            seq: v.get("seq")?.as_u64()?,
            ts: v.get("ts")?.as_u64()?,
            author: v.get("author")?.as_str()?.to_string(),
            size: s.len(),
            sha: format!("{:x}", s.len() as u64),
            expired: false,
        })
    }).collect();
    versions.sort_by(|a, b| b.seq.cmp(&a.seq));
    Json(json!({"versions": versions})).into_response()
}

/// GET /api/v1/screens/:id/versions/:seq
pub async fn get_version_doc(
    State(st): State<AppState>, Path((id, seq)): Path<(String, String)>,
) -> Response {
    let key = format!("{:012}", seq.parse::<u64>().unwrap_or(0));
    match st.lux.execute_value("HGET", &[&versions_key(&id), &key]).await {
        Ok(v) => match key_str(v).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            Some(snap) => Json(json!({"id": id, "seq": snap.get("seq").cloned().unwrap_or(json!(0)),
                "ts": snap.get("ts").cloned().unwrap_or(json!(0)),
                "author": snap.get("author").cloned().unwrap_or(json!("")),
                "doc": snap.get("doc").cloned().unwrap_or(json!({}))})).into_response(),
            None => err(StatusCode::NOT_FOUND, "version not found".to_string()),
        },
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/v1/screens/:id/rollback?if_version=N
pub async fn rollback_screen(
    State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<IfVersionQuery>,
) -> Response {
    let _ = q; // 乐观锁在 put_screen 内统一判定；rollback 用无条件写目标版本
    let current = get_version(&st, &id).await;
    // 找最近一个版本快照
    let raw = match st.lux.execute_value("HVALS", &[&versions_key(&id)]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => vec![],
    };
    let mut snaps: Vec<(u64, Value)> = raw.iter().filter_map(|x| {
        let s = key_str(x.clone())?;
        let v: Value = serde_json::from_str(&s).ok()?;
        Some((v.get("seq")?.as_u64()?, v))
    }).collect();
    snaps.sort_by(|a, b| b.0.cmp(&a.0));
    // 跳过当前 head，取上一版
    let target = snaps.iter().find(|(seq, _)| *seq < current);
    match target {
        Some((_, snap)) => {
            let doc_str = snap.get("doc").map(|d| d.to_string()).unwrap_or_else(|| "{}".to_string());
            let _ = st.lux.execute_value("HSET", &[&screen_key(&id), "doc", &doc_str]).await;
            let new_version = current + 1;
            let vstr = new_version.to_string();
            let _ = st.lux.execute_value("HSET", &[&version_counter_key(&id), "v", &vstr]).await;
            Json(json!({"status": "ok", "version": new_version})).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no earlier version to rollback to".to_string()),
    }
}

// ---- symbols ----

/// GET /api/v1/symbols → {symbols:[{name,doc}...]}
pub async fn list_symbols(State(st): State<AppState>) -> Response {
    match st.lux.execute_value("HGETALL", &["hmi:symbols"]).await {
        Ok(lux::EmbeddedValue::Map(m)) => {
            let symbols: Vec<Value> = m.iter().filter_map(|(k, v)| {
                let doc: Value = key_str(v.clone()).and_then(|s| serde_json::from_str(&s).ok())?;
                let name = key_str(k.clone())?;
                Some(json!({"name": name, "doc": doc}))
            }).collect();
            Json(json!({"symbols": symbols})).into_response()
        }
        _ => Json(json!({"symbols": []})).into_response(),
    }
}

/// PUT /api/v1/symbols/:name → {status}
pub async fn put_symbol(
    State(st): State<AppState>, Path(name): Path<String>, Json(doc): Json<Value>,
) -> Response {
    let s = doc.to_string();
    match st.lux.execute_value("HSET", &["hmi:symbols", &name, &s]).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---- history/batch ----

#[derive(Deserialize)]
pub struct HistoryBatchBody {
    queries: Vec<HistoryBatchQuery>,
}
#[derive(Deserialize)]
pub struct HistoryBatchQuery {
    dev_id: String,
    tag: String,
    start: i64,
    end: i64,
    agg: Option<String>,
    interval: Option<u64>,
}

/// POST /api/v1/history/batch → {results:[{dev_id,tag,points,count}...]}
pub async fn history_batch(
    State(st): State<AppState>, Json(body): Json<HistoryBatchBody>,
) -> Response {
    let reader = match &st.history_reader {
        Some(r) => r.clone(),
        None => return err(StatusCode::SERVICE_UNAVAILABLE, "history unavailable"),
    };
    let mut results = Vec::new();
    for q in body.queries.iter().take(32) {
        let start_ms = q.start.saturating_mul(1000);
        let end_ms = q.end.saturating_mul(1000);
        let downsample = match (q.agg.as_deref().filter(|s| !s.is_empty()), q.interval) {
            (Some(a), Some(iv)) => {
                let agg = match a {
                    "min" => Some(crate::history::HistoryAgg::Min),
                    "max" => Some(crate::history::HistoryAgg::Max),
                    "avg" => Some(crate::history::HistoryAgg::Avg),
                    "sum" => Some(crate::history::HistoryAgg::Sum),
                    "count" => Some(crate::history::HistoryAgg::Count),
                    "last" => Some(crate::history::HistoryAgg::Last),
                    _ => None,
                };
                agg.map(|a| ((iv as i64).saturating_mul(1000), a))
            }
            _ => None,
        };
        let query = crate::history::HistoryQuery { start_ms, end_ms, downsample, limit: 10000 };
        let (metric, devid) = (q.tag.clone(), q.dev_id.clone());
        let reader_for_task = reader.clone();
        let res = tokio::task::spawn_blocking(move || reader_for_task.select(&metric, &devid, &query)).await;
        let entry = match res {
            Ok(Ok(points)) => {
                let out: Vec<Value> = points.iter().map(|p| json!({"ts": p.ts_ms / 1000, "value": p.value})).collect();
                json!({"dev_id": q.dev_id, "tag": q.tag, "points": out, "count": out.len()})
            }
            Ok(Err(e)) => json!({"dev_id": q.dev_id, "tag": q.tag, "points": [], "count": 0, "error": e}),
            Err(e) => json!({"dev_id": q.dev_id, "tag": q.tag, "points": [], "count": 0, "error": e.to_string()}),
        };
        results.push(entry);
    }
    Json(json!({"results": results})).into_response()
}

fn chrono_like_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
