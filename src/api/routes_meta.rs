//! agent-hmi 画面/主题文档 CRUD（lux Hash `screen:{id}` / `theme:{id}`）。
//!
//! 与 agent-hmi 的分工边界：本服务**只做存储**——id 词法约束 + 大小钳制 + JSON
//! 语法合法性，**不做 schema 内容校验**（画面/主题文档的合法性由 agent-hmi 侧
//! G1 门禁负责；服务端对 doc 内容原样存取透传）。契约见 rest-api.md §4.19。
//!
//! 存储形态：每 id 一个 lux Hash（key = `screen:{id}` / `theme:{id}`，单 field
//! `doc` → 完整 JSON 文本原样）。列表用 `KEYS <前缀>:*`（仓内先例
//! `routes_rtdata.rs` 全量列举同款——嵌入式 in-process 存储键空间有界，
//! 无需 SCAN 游标循环）。

use crate::api::{hget, AppState};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lux::EmbeddedValue;

/// id 约束（agent-hmi 契约）：`^[a-z0-9][a-z0-9-]{0,63}$`，即总长 1..=64，
/// 首字符小写字母/数字，其余可含连字符。
const ID_MAX_LEN: usize = 64;
/// 单文档大小上限（agent-hmi 契约）：128 KiB。
pub const MAX_DOC_BYTES: usize = 131_072;
/// Hash 内存放 JSON 文本的 field 名（存储细节，REST 契约不可见）。
const DOC_FIELD: &str = "doc";

/// 资源种类：lux key 前缀 + 列表响应字段名（screens/themes 同构）。
struct MetaKind {
    key_prefix: &'static str,
    list_field: &'static str,
}

const SCREENS: MetaKind = MetaKind {
    key_prefix: "screen",
    list_field: "screens",
};
const THEMES: MetaKind = MetaKind {
    key_prefix: "theme",
    list_field: "themes",
};

// ---------- handlers（screens） ----------

pub async fn screens_list(State(st): State<AppState>) -> Response {
    list(&st, &SCREENS).await
}

pub async fn screens_get(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    get(&st, &SCREENS, &id).await
}

pub async fn screens_put(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    put(&st, &SCREENS, &id, &body).await
}

pub async fn screens_delete(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    delete(&st, &SCREENS, &id).await
}

// ---------- handlers（themes） ----------

pub async fn themes_list(State(st): State<AppState>) -> Response {
    list(&st, &THEMES).await
}

pub async fn themes_get(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    get(&st, &THEMES, &id).await
}

pub async fn themes_put(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    put(&st, &THEMES, &id, &body).await
}

pub async fn themes_delete(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    delete(&st, &THEMES, &id).await
}

// ---------- 共享实现 ----------

/// 错误响应统一形态：`(status, {"error": msg})`（agent-hmi 契约）。
fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// `GET /api/v1/{screens|themes}`：全部 id 升序列表。
async fn list(st: &AppState, kind: &MetaKind) -> Response {
    let ids = ids_from_keys(&keys(st, kind.key_prefix).await, kind.key_prefix);
    Json(serde_json::json!({ kind.list_field: ids })).into_response()
}

/// `GET /api/v1/{screens|themes}/:id`：`{"id","doc"}`；不存在 404。
/// （非法 id 永不可被 PUT 写入，hget miss → 404，无需单独分支。）
async fn get(st: &AppState, kind: &MetaKind, id: &str) -> Response {
    match hget(&st.lux, &lux_key(kind.key_prefix, id), DOC_FIELD).await {
        Some(raw) => Json(serde_json::json!({ "id": id, "doc": parse_doc(&raw) })).into_response(),
        None => err(StatusCode::NOT_FOUND, "not found"),
    }
}

/// PUT 拒绝原因（纯校验，单测覆盖；handler 映射状态码）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PutReject {
    InvalidId,
    TooLarge,
    InvalidJson,
}

/// `PUT /api/v1/{screens|themes}/:id`：全量覆盖存储，body = doc 完整 JSON。
///
/// 校验边界（仅此三项，schema 内容校验归 agent-hmi G1 门禁）：
/// id 词法 → 400；大小 ≤ 128 KiB → 413；JSON 语法合法 → 400（存储完整性——
/// GET 契约要求 parse 后原样返回，非法 JSON 会让 doc 退化成字符串）。
/// 通过后按提交原文落库（不做规范化重序列化）。
pub(crate) fn validate_put(id: &str, body: &[u8]) -> Result<(), PutReject> {
    if !valid_id(id) {
        return Err(PutReject::InvalidId);
    }
    if body.len() > MAX_DOC_BYTES {
        return Err(PutReject::TooLarge);
    }
    // 语法校验即丢弃（存储用原文）。
    serde_json::from_slice::<serde_json::Value>(body)
        .map(|_| ())
        .map_err(|_| PutReject::InvalidJson)
}

async fn put(st: &AppState, kind: &MetaKind, id: &str, body: &[u8]) -> Response {
    if let Err(r) = validate_put(id, body) {
        return match r {
            PutReject::InvalidId => err(StatusCode::BAD_REQUEST, "invalid id"),
            PutReject::TooLarge => err(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
            PutReject::InvalidJson => err(StatusCode::BAD_REQUEST, "invalid json"),
        };
    }
    // validate_put 已保证 UTF-8 合法（serde_json 拒绝非法 UTF-8），lossy 仅兜底。
    let raw = String::from_utf8_lossy(body);
    // lux 写失败（磁盘满/WAL 等）必须 500——吞错报 ok 是静默数据丢失（审查
    // 2026-10-27）；同 routes_model/routes_alarm 的 Err→500 先例。
    match st
        .lux
        .execute("HSET", &[&lux_key(kind.key_prefix, id), DOC_FIELD, &raw])
        .await
    {
        Ok(_) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `DELETE /api/v1/{screens|themes}/:id`：幂等（不存在也 ok）；id 词法校验与
/// PUT 对称（非法 id 400，不放行构造越界 lux key）。
async fn delete(st: &AppState, kind: &MetaKind, id: &str) -> Response {
    if !valid_id(id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    match st
        .lux
        .execute("DEL", &[&lux_key(kind.key_prefix, id)])
        .await
    {
        Ok(_) => Json(serde_json::json!({ "status": "ok" })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------- 纯函数（单测覆盖） ----------

/// id 词法校验：首字符小写字母/数字，其余小写字母/数字/连字符，总长 1..=64。
pub(crate) fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.len() <= ID_MAX_LEN
}

/// lux key：`<前缀>:<id>`。
fn lux_key(prefix: &str, id: &str) -> String {
    format!("{prefix}:{id}")
}

/// 存储文本 → doc 值：parse 成功原样返回；失败退化为字符串（防御手工改库的数据）。
fn parse_doc(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or(serde_json::Value::String(raw.to_string()))
}

/// `KEYS <前缀>:*` 原始返回。
async fn keys(st: &AppState, prefix: &str) -> Vec<EmbeddedValue> {
    match st
        .lux
        .execute_value("KEYS", &[&format!("{prefix}:*")])
        .await
    {
        Ok(EmbeddedValue::Array(arr)) => arr,
        _ => Vec::new(),
    }
}

/// KEYS 返回值 → id 列表：去前缀 → 过滤合法 id（防御手工写入的越界 key 混入
/// 响应）→ 升序排序（列表顺序稳定，前端/agent-hmi 可 diff）。
fn ids_from_keys(keys: &[EmbeddedValue], prefix: &str) -> Vec<String> {
    let mut ids: Vec<String> = keys
        .iter()
        .filter_map(|k| match k {
            EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
            EmbeddedValue::Simple(s) => Some(s.clone()),
            _ => None,
        })
        .filter_map(|k| k.strip_prefix(&format!("{prefix}:")).map(str::to_string))
        .filter(|id| valid_id(id))
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use lux::EmbeddedValue;

    fn bulk(s: &str) -> EmbeddedValue {
        EmbeddedValue::Bulk(s.as_bytes().to_vec().into())
    }

    /// 验证意图：id 约束是 agent-hmi 与存储的共享契约——首字符词法与长度上限
    /// 一旦放行越界 id，列表去前缀与路由 `:id` 段就会见到不可预期的键名。
    #[test]
    fn valid_id_accepts_contract_shape() {
        for ok in ["a", "0", "a1", "a-b", "abc-123", &"a".repeat(64)] {
            assert!(valid_id(ok), "应放行: {ok}");
        }
        for bad in [
            "",              // 空
            "-a",            // 首字符连字符
            "A",             // 大写
            "aBc",           // 中段大写
            "a_b",           // 下划线
            "a b",           // 空格
            "a:b",           // 冒号（会伪造 key 前缀结构）
            "画面",          // 非 ASCII
            &"a".repeat(65), // 超长
        ] {
            assert!(!valid_id(bad), "应拒绝: {bad:?}");
        }
    }

    /// 验证意图：大小钳制是 413 契约的边界——131072 放行、131073 拒绝，
    /// 差一字节即换状态码。
    #[test]
    fn validate_put_size_clamp_boundary() {
        let ok = format!("{{\"v\":\"{}\"}}", "x".repeat(MAX_DOC_BYTES - 8));
        assert_eq!(ok.len(), MAX_DOC_BYTES);
        assert_eq!(validate_put("a", ok.as_bytes()), Ok(()));

        let big = " ".repeat(MAX_DOC_BYTES + 1);
        // 用合法 JSON 包裹避免撞 InvalidJson 掩盖 TooLarge（校验顺序：id → size → json）。
        let big_json = format!(" {{}}{}", big);
        assert!(big_json.len() > MAX_DOC_BYTES);
        assert_eq!(
            validate_put("a", big_json.as_bytes()),
            Err(PutReject::TooLarge)
        );
    }

    /// 验证意图：PUT 三类拒绝各归各的状态码路径——id 先于大小、大小先于 JSON，
    /// 顺序错了会把无效请求误报成别的错误。
    #[test]
    fn validate_put_reject_order_and_json_syntax() {
        assert_eq!(validate_put("-x", b"{}"), Err(PutReject::InvalidId));
        assert_eq!(validate_put("a", b"not json"), Err(PutReject::InvalidJson));
        assert_eq!(validate_put("a", b"[1,2,3]"), Ok(()));
        assert_eq!(validate_put("a", b"null"), Ok(()));
        // 空 body 不是合法 JSON 文档。
        assert_eq!(validate_put("a", b""), Err(PutReject::InvalidJson));
    }

    /// 验证意图：列表契约是"去前缀 + 过滤 + 升序"——直接决定 GET /screens 的
    /// 响应体，乱序或混入越界 key 会破坏 agent-hmi 的列表 diff。
    #[test]
    fn ids_from_keys_strips_filters_sorts() {
        let keys = vec![
            bulk("screen:c"),
            bulk("screen:a"),
            bulk("screen:b"),
            EmbeddedValue::Nil,     // RESP Nil 混入 → 跳过
            EmbeddedValue::Int(42), // 非 key 形态 → 跳过
            bulk("screens"),        // 无冒号（不匹配前缀结构）→ 去掉
            bulk("screen:-bad"),    // 去前缀后非法 id → 过滤
            bulk("screen:ok-2"),
        ];
        assert_eq!(
            ids_from_keys(&keys, "screen"),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "ok-2".to_string()
            ]
        );
        // 前缀隔离：theme 前缀看不见 screen 的键。
        assert!(ids_from_keys(&keys, "theme").is_empty());
    }

    /// 验证意图：GET 契约要求 doc 原样返回——parse 成功保结构；手工改库写坏
    /// 文本时退化字符串而非 500，读路径永不因存储脏数据崩。
    #[test]
    fn parse_doc_roundtrip_and_fallback() {
        let raw = r#"{"name":"line","x":1}"#;
        assert_eq!(parse_doc(raw), serde_json::json!({"name":"line","x":1}));
        assert_eq!(
            parse_doc("not json"),
            serde_json::Value::String("not json".to_string())
        );
    }
}
