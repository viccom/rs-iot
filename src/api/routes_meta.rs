//! agent-hmi 画面/主题文档 CRUD（lux Hash `screen:{id}` / `theme:{id}`）+ 版本管理。
//!
//! 与 agent-hmi 的分工边界：本服务**只做存储**——id 词法约束 + 大小钳制 + JSON
//! 语法合法性，**不做 schema 内容校验**（画面/主题文档的合法性由 agent-hmi 侧
//! G1 门禁负责；服务端对 doc 内容原样存取透传）。契约见 rest-api.md §4.19。
//!
//! 存储形态：每 id 一个 lux Hash（key = `screen:{id}` / `theme:{id}`，单 field
//! `doc` → 完整 JSON 文本原样）。列表用 `KEYS <前缀>:*`（仓内先例
//! `routes_rtdata.rs` 全量列举同款——嵌入式 in-process 存储键空间有界，
//! 无需 SCAN 游标循环）。
//!
//! 版本管理（P0，design `docs/designs/2026-10-30-screen-versioning-design.md`，
//! 权威源）：独立前缀键族 `sv:{kind}:{id}:*`（seq 计数器 / meta 元数据 hash /
//! v:{seq} 正文 SETEX）。**键族独立于 `screen:` 命名空间的根因**：`list()` 的
//! `KEYS screen:*` + `valid_id` 过滤只是隐式边界（§1.2 脆弱点），版本键放进
//! 去等于依赖"id 不能含冒号"这一条间接保护——独立前缀从根上不进扫描面
//! （tests/meta_versioning.rs 第 10 组固化）。
//!
//! 失败矩阵（embedded lux 无事务，§4.3）：**head 的 HSET 是整个写入序列的最后
//! 一步**——此前任何一步失败都中止并 500，宁可这次保存失败也不产生"内容变了
//! 但没历史"的不可追溯写入；head 之后的 publish 失败吞掉（SSE 是通知非事务
//! 参与方）。所有 lux 调用显式判 Err（不用吞错 helper——22ee8b0 原则）。

use crate::api::auth::AuthedUser;
use crate::api::AppState;
use crate::event_bus::EventPayload;
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// id 约束（agent-hmi 契约）：`^[a-z0-9][a-z0-9-]{0,63}$`，即总长 1..=64，
/// 首字符小写字母/数字，其余可含连字符。
const ID_MAX_LEN: usize = 64;
/// 单文档大小上限（agent-hmi 契约）：128 KiB。
pub const MAX_DOC_BYTES: usize = 131_072;
/// Hash 内存放 JSON 文本的 field 名（存储细节，REST 契约不可见）。
const DOC_FIELD: &str = "doc";
/// 资源种类：lux key 前缀 + 列表响应字段名 + 版本键族前缀（screens/themes 同构）。
///
/// `kind` 是资源名单词（SSE payload 的 `resource` 字段与指标 label 用），
/// `sv_prefix` = `sv:{key_prefix}`——版本键族的独立命名空间。
struct MetaKind {
    key_prefix: &'static str,
    list_field: &'static str,
    kind: &'static str,
    sv_prefix: &'static str,
    /// 指标数组索引（ProcessMetrics 的 meta_* 二元数组：0=screen 1=theme）。
    metric_idx: usize,
}

const SCREENS: MetaKind = MetaKind {
    key_prefix: "screen",
    list_field: "screens",
    kind: "screen",
    sv_prefix: "sv:screen",
    metric_idx: 0,
};
const THEMES: MetaKind = MetaKind {
    key_prefix: "theme",
    list_field: "themes",
    kind: "theme",
    sv_prefix: "sv:theme",
    metric_idx: 1,
};

/// `PUT`/`POST rollback` 的 query 参数：`?if_version=N` 乐观锁（缺省无条件写，
/// 完全向后兼容——既有编辑器/e2e 播种零改动）。
#[derive(Debug, Default, Deserialize)]
pub struct VersionQuery {
    /// 期望的当前版本号；与实际不符 → 409（不捕获、不留痕）。
    /// seq 键缺失视为 0——`?if_version=0` 是合法的"仍是原始版"断言（迁移期）。
    if_version: Option<u64>,
}

/// `POST /{kind}/{id}/rollback` body。
#[derive(Debug, Deserialize)]
pub struct RollbackBody {
    /// 回滚到的目标版本号（该版正文将成为新 head）。
    to_version: u64,
}

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
    Query(q): Query<VersionQuery>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    put(
        &st,
        &SCREENS,
        &id,
        &body,
        q.if_version,
        author_of(user.as_ref()),
    )
    .await
}

pub async fn screens_delete(
    State(st): State<AppState>,
    Path(id): Path<String>,
    user: Option<Extension<AuthedUser>>,
) -> Response {
    delete(&st, &SCREENS, &id, author_of(user.as_ref())).await
}

/// `GET /api/v1/screens/{id}/versions`：版本元数据列表（seq 降序，至多 cap 条）。
pub async fn screens_versions(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    versions_list(&st, &SCREENS, &id).await
}

/// `GET /api/v1/screens/{id}/versions/{seq}`：单版正文取回（404/410 矩阵）。
pub async fn screens_version_get(
    State(st): State<AppState>,
    Path((id, seq)): Path<(String, u64)>,
) -> Response {
    version_get(&st, &SCREENS, &id, seq).await
}

/// `POST /api/v1/screens/{id}/rollback`：回滚到目标版（回滚本身可再回滚）。
pub async fn screens_rollback(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<VersionQuery>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    rollback(
        &st,
        &SCREENS,
        &id,
        &body,
        q.if_version,
        &author_of(user.as_ref()),
    )
    .await
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
    Query(q): Query<VersionQuery>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    put(
        &st,
        &THEMES,
        &id,
        &body,
        q.if_version,
        author_of(user.as_ref()),
    )
    .await
}

pub async fn themes_delete(
    State(st): State<AppState>,
    Path(id): Path<String>,
    user: Option<Extension<AuthedUser>>,
) -> Response {
    delete(&st, &THEMES, &id, author_of(user.as_ref())).await
}

/// `GET /api/v1/themes/{id}/versions`（MetaKind 泛化，机制免费覆盖 themes）。
pub async fn themes_versions(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    versions_list(&st, &THEMES, &id).await
}

/// `GET /api/v1/themes/{id}/versions/{seq}`。
pub async fn themes_version_get(
    State(st): State<AppState>,
    Path((id, seq)): Path<(String, u64)>,
) -> Response {
    version_get(&st, &THEMES, &id, seq).await
}

/// `POST /api/v1/themes/{id}/rollback`。
pub async fn themes_rollback(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<VersionQuery>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    rollback(
        &st,
        &THEMES,
        &id,
        &body,
        q.if_version,
        &author_of(user.as_ref()),
    )
    .await
}

// ---------- 共享实现 ----------

/// 错误响应统一形态：`(status, {"error": msg})`（agent-hmi 契约）。
fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// author 提取（design §3.6）：JWT 路径 auth 中间件已注入 `AuthedUser`；
/// accesskey 直通路径无注入 → `"unknown"`（不因审计缺失而失败写）。
fn author_of(user: Option<&Extension<AuthedUser>>) -> String {
    user.map(|e| e.user_id.clone())
        .unwrap_or_else(|| "unknown".into())
}

/// `GET /api/v1/{screens|themes}`：全部 id 升序列表。
async fn list(st: &AppState, kind: &MetaKind) -> Response {
    let ids = ids_from_keys(&keys(st, kind.key_prefix).await, kind.key_prefix);
    Json(serde_json::json!({ kind.list_field: ids })).into_response()
}

/// `GET /api/v1/{screens|themes}/:id`：`{"id","doc","version"}`；不存在 404。
/// （非法 id 永不可被 PUT 写入，HGET miss → 404，无需单独分支。）
///
/// 读失败一律 500（不用 `db::hget` 的 None 吞错——那会把存储故障伪装成 404/版本 0；
/// 复审 L5：seq 读失败曾 `unwrap_or(0)`，与「所有 lux 调用显式判 Err」自相矛盾）。
async fn get(st: &AppState, kind: &MetaKind, id: &str) -> Response {
    let raw = match st
        .lux
        .execute_value("HGET", &[&lux_key(kind.key_prefix, id), DOC_FIELD])
        .await
    {
        Ok(EmbeddedValue::Nil) => return err(StatusCode::NOT_FOUND, "not found"),
        Ok(v) => crate::db::str_from_value(&v),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let version = match read_seq(st, kind, id).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    Json(serde_json::json!({
        "id": id, "doc": parse_doc(&raw), "version": version
    }))
    .into_response()
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

/// 版本写入序列的结果——区分"判重命中"（ok + 旧版本号）与"新建版本"。
enum PutOutcome {
    /// 内容未变：返回当前版本号，不建版、不 INCR、不发事件。
    Deduplicated(u64),
    /// 内容变更（或首建）：新版本号。
    Versioned(u64),
}

/// `PUT` 主流程（design §3.1/§3.2 捕获与乐观锁）。
///
/// 写入顺序即失败矩阵（§4.3）：判重 → if_version 校验 → 捕获旧版（失败 500
/// 不写 head）→ INCR（失败 500）→ HSET head（最后一步，失败 500）→ publish。
async fn put(
    st: &AppState,
    kind: &MetaKind,
    id: &str,
    body: &[u8],
    if_version: Option<u64>,
    author: String,
) -> Response {
    if let Err(r) = validate_put(id, body) {
        return match r {
            PutReject::InvalidId => err(StatusCode::BAD_REQUEST, "invalid id"),
            PutReject::TooLarge => err(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
            PutReject::InvalidJson => err(StatusCode::BAD_REQUEST, "invalid json"),
        };
    }
    match put_versioned(st, kind, id, body, if_version, &author).await {
        Ok(PutOutcome::Deduplicated(v)) => {
            Json(serde_json::json!({ "status": "ok", "version": v })).into_response()
        }
        Ok(PutOutcome::Versioned(v)) => {
            Json(serde_json::json!({ "status": "ok", "version": v })).into_response()
        }
        Err(PutErr::Conflict(cur)) => {
            st.metrics.meta_version_conflict(kind.metric_idx);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "version conflict", "current_version": cur
                })),
            )
                .into_response()
        }
        Err(PutErr::Lux(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// 版本化写入的内部错误（handler 映射状态码；纯校验已在外层完成）。
enum PutErr {
    /// if_version 不匹配（当前实际版本号随行）。
    Conflict(u64),
    /// lux 命令失败——如实 500，不吞错报 ok（22ee8b0 原则）。
    Lux(String),
}

impl PutErr {
    fn lux(e: impl std::fmt::Display) -> Self {
        Self::Lux(e.to_string())
    }
}

/// 判重 → 乐观锁 → 捕获 → INCR → 写 head → 事件 → 紧缩（design §3.1 四步序）。
async fn put_versioned(
    st: &AppState,
    kind: &MetaKind,
    id: &str,
    body: &[u8],
    if_version: Option<u64>,
    author: &str,
) -> Result<PutOutcome, PutErr> {
    // 读旧 doc + 当前 seq（首建 old=None；遗留画面 seq=0——"机制上线前的原貌"）。
    let old_doc: Option<String> = match st
        .lux
        .execute_value("HGET", &[&lux_key(kind.key_prefix, id), DOC_FIELD])
        .await
    {
        Ok(EmbeddedValue::Nil) => None,
        Ok(v) => Some(crate::db::str_from_value(&v)),
        Err(e) => return Err(PutErr::lux(e)),
    };
    let cur_seq = read_seq(st, kind, id).await.map_err(PutErr::lux)?;

    // 乐观锁（§3.2）**先于判重**：提供且不匹配即 409——即使提交内容与请求读到的
    // 旧 head 逐字节相同（并发窗口里旧 head 可能已过期），也不把过期版本号的写
    // 当成功（复审 L4：判重先执行的窗口可绕过 if_version 校验）。
    if let Some(expect) = if_version {
        if expect != cur_seq {
            return Err(PutErr::Conflict(cur_seq));
        }
    }

    // 内容判重（§4.1 收敛线 1）：字节级同内容 → 不建版、不 INCR、直接返回当前
    // version（改一个像素保存十次只产生一条版本；e2e 全量 upsert 播种幂等）。
    if old_doc.as_deref().map(|o| o.as_bytes()) == Some(body) {
        return Ok(PutOutcome::Deduplicated(cur_seq));
    }

    // 捕获旧内容为版本（§3.1 步 3）：old 存在才捕获；seq=0 的遗留画面捕获为
    // 版本 0——任何画面任何时刻都满足"versions 最后一条 = 可回退的最老形态"。
    if let Some(old) = &old_doc {
        capture_version(st, kind, id, cur_seq, old, author)
            .await
            .map_err(PutErr::lux)?;
        st.metrics.meta_versions_created(kind.metric_idx);
    }

    // INCR（失败 500：孤儿正文/元数据由 TTL 与紧缩清理，正确性无损——§4.3）。
    let new_seq = incr_seq(st, kind, id).await.map_err(PutErr::lux)?;

    // head 的 HSET 是最后一步（§4.2 不变量）：成功即新版生效。
    if let Err(e) = st
        .lux
        .execute(
            "HSET",
            &[
                &lux_key(kind.key_prefix, id),
                DOC_FIELD,
                &String::from_utf8_lossy(body),
            ],
        )
        .await
    {
        return Err(PutErr::lux(e));
    }

    // publish（head 已落库；SSE 非事务参与方，发送失败本就静默）。
    st.event_bus.publish(EventPayload::Screen {
        id: id.to_string(),
        resource: kind.kind,
        action: "put",
        version: new_seq,
        actor: author.to_string(),
        ts: crate::event_bus::now_iso8601(),
    });

    // 紧缩检查（§4.4）：HLEN 超阈值时收敛元数据容量。失败只影响历史完整性，
    // 不影响已生效的 head——吞掉记日志（与 §4.3 "head 之后失败只影响通知面"
    // 同级的尾部步骤）。
    if let Err(e) = compact_if_needed(st, kind, id).await {
        tracing::warn!(
            "[meta] {} {id} 紧缩失败（下次写入再试）: {e}",
            kind.key_prefix
        );
    }

    Ok(PutOutcome::Versioned(new_seq))
}

/// `DELETE /api/v1/{screens|themes}/:id`：幂等（不存在也 ok）+ 版本键族级联删
/// （§2.3——正文虽带 TTL，但 30 天内同 slug 重建会读到上一世代孤儿版本，
/// seq 撞车 = "自己没写过的历史"）。
async fn delete(st: &AppState, kind: &MetaKind, id: &str, author: String) -> Response {
    if !valid_id(id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    // 删除前读 seq（publish 载荷用；读失败只影响审计载荷，不阻断删除——幂等语义优先）。
    let seq_before = match read_seq(st, kind, id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "[meta] {} {id} 删除前读 seq 失败（SSE version 记 0）: {e}",
                kind.key_prefix
            );
            0
        }
    };

    let key = lux_key(kind.key_prefix, id);
    if let Err(e) = st.lux.execute("DEL", &[&key]).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    // 级联：seq 计数器 + 元数据 hash。
    for k in [sv_seq_key(kind, id), sv_meta_key(kind, id)] {
        if let Err(e) = st.lux.execute("DEL", &[&k]).await {
            return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    }
    // 级联：正文键（键空间有界 ≤cap 量级，KEYS 即可，无需 SCAN）。
    let pattern = format!("{}:{id}:v:*", kind.sv_prefix);
    match st.lux.execute_value("KEYS", &[&pattern]).await {
        Ok(EmbeddedValue::Array(keys)) => {
            for k in keys {
                let key = crate::db::str_from_value(&k);
                if let Err(e) = st.lux.execute("DEL", &[&key]).await {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
                }
            }
        }
        Ok(_) => {}
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }

    st.event_bus.publish(EventPayload::Screen {
        id: id.to_string(),
        resource: kind.kind,
        action: "delete",
        version: seq_before,
        actor: author,
        ts: crate::event_bus::now_iso8601(),
    });
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

/// `GET .../versions`：`{"versions":[{seq,ts,author,size,sha,expired}]}`，
/// seq 降序至多 cap 条；`expired` 由 MGET 正文键批量判定（一次往返，§3.3）。
async fn versions_list(st: &AppState, kind: &MetaKind, id: &str) -> Response {
    if !valid_id(id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let cap = st.config.meta.versions_cap;
    // HGETALL 直接判错（db::hgetall 吞错成空对象——lux 故障必须如实 500）。
    let raw = match st
        .lux
        .execute_value("HGETALL", &[&sv_meta_key(kind, id)])
        .await
    {
        Ok(EmbeddedValue::Array(arr)) => arr,
        Ok(_) => Vec::new(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    // field/value 对 → (seq, VersionMeta)，解析失败的脏条目跳过（防御手工改库）。
    let mut entries: Vec<(u64, serde_json::Value)> = Vec::new();
    let mut i = 0;
    while i + 1 < raw.len() {
        let seq = crate::db::str_from_value(&raw[i]).parse::<u64>().ok();
        let meta: Option<serde_json::Value> =
            serde_json::from_str(&crate::db::str_from_value(&raw[i + 1])).ok();
        if let (Some(seq), Some(meta)) = (seq, meta) {
            entries.push((seq, meta));
        }
        i += 2;
    }
    entries.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq)); // seq 降序
    entries.truncate(cap);

    // MGET 正文键批量判存活（一次往返）。
    let body_keys: Vec<String> = entries
        .iter()
        .map(|(seq, _)| sv_body_key(kind, id, *seq))
        .collect();
    let alive: Vec<bool> = if body_keys.is_empty() {
        Vec::new()
    } else {
        let refs: Vec<&str> = body_keys.iter().map(String::as_str).collect();
        match st.lux.execute_value("MGET", &refs).await {
            Ok(EmbeddedValue::Array(vals)) => {
                // MGET 契约：每个 key 一个 value（缺 = Nil）。长度不符视为协议异常——
                // 否则尾部 key 会被 zip 补成"已过期"，把存储异常伪装成 TTL 淘汰。
                if vals.len() != body_keys.len() {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "MGET 响应长度不符");
                }
                vals.iter()
                    .map(|v| !matches!(v, EmbeddedValue::Nil))
                    .collect()
            }
            Ok(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "MGET 响应形状异常"),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };

    let versions: Vec<serde_json::Value> = entries
        .iter()
        .zip(alive.iter().chain(std::iter::repeat(&false)))
        .map(|((seq, meta), alive)| {
            // 元数据行原样透传（seq/ts/author/size/sha），expired 由本次批量判定注入。
            let mut row = meta.clone();
            row["seq"] = serde_json::json!(seq);
            row["expired"] = serde_json::json!(!alive);
            row
        })
        .collect();
    Json(serde_json::json!({ "versions": versions })).into_response()
}

/// `GET .../versions/{seq}`：`{id,seq,ts,author,doc}`；元数据无该 seq → 404；
/// 正文缺失（TTL 过期）→ 410（§3.3）。
async fn version_get(st: &AppState, kind: &MetaKind, id: &str, seq: u64) -> Response {
    if !valid_id(id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let meta = match st
        .lux
        .execute_value("HGET", &[&sv_meta_key(kind, id), &seq.to_string()])
        .await
    {
        Ok(EmbeddedValue::Nil) => return err(StatusCode::NOT_FOUND, "not found"),
        Ok(v) => crate::db::str_from_value(&v),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
    match st
        .lux
        .execute_value("GET", &[&sv_body_key(kind, id, seq)])
        .await
    {
        Ok(EmbeddedValue::Nil) => err(StatusCode::GONE, "version body expired"),
        Ok(v) => {
            let raw = crate::db::str_from_value(&v);
            Json(serde_json::json!({
                "id": id, "seq": seq,
                "ts": meta["ts"], "author": meta["author"],
                "doc": parse_doc(&raw),
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST .../rollback`：读目标版 → 快照当前 head 为新版本（回滚可再回滚）→
/// 写 head = 目标正文 → INCR → 事件（§3.4）。可选 `?if_version` 同 PUT。
async fn rollback(
    st: &AppState,
    kind: &MetaKind,
    id: &str,
    body: &[u8],
    if_version: Option<u64>,
    author: &str,
) -> Response {
    if !valid_id(id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let to: RollbackBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid json"),
    };
    match rollback_versioned(st, kind, id, to.to_version, if_version, author).await {
        // 目标正文与当前 head 同内容：no-op（不建版、不 INCR、不发帧、不计数——
        // 复审 L6：重复点同一版回滚不再产生重复版本条目）。
        Ok(RollbackOutcome::Unchanged(v)) => {
            Json(serde_json::json!({ "status": "ok", "version": v })).into_response()
        }
        Ok(RollbackOutcome::Rolled(v)) => {
            st.metrics.meta_rollback(kind.metric_idx);
            Json(serde_json::json!({ "status": "ok", "version": v })).into_response()
        }
        Err(RbErr::NotFound) => err(StatusCode::NOT_FOUND, "not found"),
        Err(RbErr::Gone) => err(StatusCode::GONE, "version body expired"),
        Err(RbErr::Conflict(cur)) => {
            st.metrics.meta_version_conflict(kind.metric_idx);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "version conflict", "current_version": cur
                })),
            )
                .into_response()
        }
        Err(RbErr::Lux(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// rollback 内部错误（handler 映射状态码）。
enum RbErr {
    NotFound,
    Gone,
    Conflict(u64),
    Lux(String),
}

/// 回滚结果：`Unchanged` = 目标正文与当前 head 逐字节相同（no-op，不建版/不 INCR/
/// 不发帧/不计数）；`Rolled` = 产生新版本号（回滚前的 head 已快照为新版本）。
enum RollbackOutcome {
    Unchanged(u64),
    Rolled(u64),
}

async fn rollback_versioned(
    st: &AppState,
    kind: &MetaKind,
    id: &str,
    to_version: u64,
    if_version: Option<u64>,
    author: &str,
) -> Result<RollbackOutcome, RbErr> {
    // 目标版元数据（无 → 404）与正文（TTL 过期 → 410）。
    let meta_raw = match st
        .lux
        .execute_value("HGET", &[&sv_meta_key(kind, id), &to_version.to_string()])
        .await
    {
        Ok(EmbeddedValue::Nil) => return Err(RbErr::NotFound),
        Ok(v) => crate::db::str_from_value(&v),
        Err(e) => return Err(RbErr::Lux(e.to_string())),
    };
    let target: String = match st
        .lux
        .execute_value("GET", &[&sv_body_key(kind, id, to_version)])
        .await
    {
        Ok(EmbeddedValue::Nil) => return Err(RbErr::Gone),
        Ok(v) => crate::db::str_from_value(&v),
        Err(e) => return Err(RbErr::Lux(e.to_string())),
    };
    let _ = meta_raw; // 元数据仅用于存在性判定（目标正文的 ts/author 归属原版本）

    // 快照当前 head 为新版本（§3.4 步 2——回滚本身可再回滚）。
    let head: Option<String> = match st
        .lux
        .execute_value("HGET", &[&lux_key(kind.key_prefix, id), DOC_FIELD])
        .await
    {
        Ok(EmbeddedValue::Nil) => None,
        Ok(v) => Some(crate::db::str_from_value(&v)),
        Err(e) => return Err(RbErr::Lux(e.to_string())),
    };
    let cur_seq = read_seq(st, kind, id).await.map_err(RbErr::Lux)?;

    if let Some(expect) = if_version {
        if expect != cur_seq {
            return Err(RbErr::Conflict(cur_seq));
        }
    }

    // 目标内容 == 当前 head：回滚是 no-op（不建版、不 INCR、不发帧——复审 L6）。
    if let Some(head) = &head {
        if head.as_bytes() == target.as_bytes() {
            return Ok(RollbackOutcome::Unchanged(cur_seq));
        }
        capture_version(st, kind, id, cur_seq, head, author)
            .await
            .map_err(RbErr::Lux)?;
        st.metrics.meta_versions_created(kind.metric_idx);
    }

    let new_seq = incr_seq(st, kind, id).await.map_err(RbErr::Lux)?;
    if let Err(e) = st
        .lux
        .execute("HSET", &[&lux_key(kind.key_prefix, id), DOC_FIELD, &target])
        .await
    {
        return Err(RbErr::Lux(e.to_string()));
    }

    st.event_bus.publish(EventPayload::Screen {
        id: id.to_string(),
        resource: kind.kind,
        action: "rollback",
        version: new_seq,
        actor: author.to_string(),
        ts: crate::event_bus::now_iso8601(),
    });
    if let Err(e) = compact_if_needed(st, kind, id).await {
        tracing::warn!(
            "[meta] {} {id} 紧缩失败（下次写入再试）: {e}",
            kind.key_prefix
        );
    }
    Ok(RollbackOutcome::Rolled(new_seq))
}

// ---------- 版本存储原语（sv: 键族） ----------

fn sv_seq_key(kind: &MetaKind, id: &str) -> String {
    format!("{}:{id}:seq", kind.sv_prefix)
}
fn sv_meta_key(kind: &MetaKind, id: &str) -> String {
    format!("{}:{id}:meta", kind.sv_prefix)
}
fn sv_body_key(kind: &MetaKind, id: &str, seq: u64) -> String {
    format!("{}:{id}:v:{seq}", kind.sv_prefix)
}

/// 当前版本号（`GET sv:...:seq`）。键缺失（遗留画面/首建）= 0。
/// 读命令失败如实上抛（不吞错成 0——那是把故障伪装成"原始版"）。
async fn read_seq(st: &AppState, kind: &MetaKind, id: &str) -> Result<u64, String> {
    match st.lux.execute_value("GET", &[&sv_seq_key(kind, id)]).await {
        Ok(EmbeddedValue::Nil) => Ok(0),
        Ok(v) => crate::db::str_from_value(&v)
            .parse::<u64>()
            .map_err(|e| format!("corrupt seq key: {e}")),
        Err(e) => Err(e.to_string()),
    }
}

/// `INCR sv:...:seq` → 新版本号。
async fn incr_seq(st: &AppState, kind: &MetaKind, id: &str) -> Result<u64, String> {
    match st.lux.execute_value("INCR", &[&sv_seq_key(kind, id)]).await {
        Ok(v) => crate::db::str_from_value(&v)
            .parse::<u64>()
            .map_err(|e| format!("corrupt incr result: {e}")),
        Err(e) => Err(e.to_string()),
    }
}

/// 捕获旧内容为版本 `seq`：SETEX 正文（TTL 自动淘汰，§4.1 收敛线 2）+
/// HSET 元数据（seq/ts/author/size/sha，~140B，§2.2）。
/// 两步任一失败都上抛——head 尚未写，中止保住"内容变了必有历史"（§4.3）。
async fn capture_version(
    st: &AppState,
    kind: &MetaKind,
    id: &str,
    seq: u64,
    old_doc: &str,
    author: &str,
) -> Result<(), String> {
    let ttl = st.config.meta.versions_ttl_secs.max(1);
    let ttl_str = ttl.to_string();
    st.lux
        .execute("SETEX", &[&sv_body_key(kind, id, seq), &ttl_str, old_doc])
        .await
        .map_err(|e| e.to_string())?;
    let meta = serde_json::json!({
        "seq": seq,
        "ts": chrono::Utc::now().timestamp(),
        "author": author,
        "size": old_doc.len(),
        "sha": sha16(old_doc.as_bytes()),
    });
    st.lux
        .execute(
            "HSET",
            &[&sv_meta_key(kind, id), &seq.to_string(), &meta.to_string()],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 元数据容量紧缩（§4.4）：HLEN > 2×cap 时按 seq 降序保留最新 cap 条且正文
/// 仍在的 → DEL 整键 → 逐条重写。非原子（读改写窗口内并发的另一次捕获可能
/// 丢元数据，正文仍在但短期不出现在列表——概率极低、后果可自愈，§7 登记）。
/// 每次写入至多触发一次。
async fn compact_if_needed(st: &AppState, kind: &MetaKind, id: &str) -> Result<(), String> {
    let cap = st.config.meta.versions_cap;
    let hlen = match st
        .lux
        .execute_value("HLEN", &[&sv_meta_key(kind, id)])
        .await
    {
        Ok(v) => crate::db::str_from_value(&v).parse::<u64>().unwrap_or(0),
        Err(e) => return Err(e.to_string()),
    };
    if hlen as usize <= 2 * cap {
        return Ok(());
    }
    st.metrics.meta_compaction(kind.metric_idx);

    // 全量读 → 排序降序 → 保留前 cap 条且正文存活。
    let raw = match st
        .lux
        .execute_value("HGETALL", &[&sv_meta_key(kind, id)])
        .await
    {
        Ok(EmbeddedValue::Array(arr)) => arr,
        Ok(_) => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let mut entries: Vec<(String, String)> = Vec::new(); // (seq_str, meta_json)
    let mut i = 0;
    while i + 1 < raw.len() {
        entries.push((
            crate::db::str_from_value(&raw[i]),
            crate::db::str_from_value(&raw[i + 1]),
        ));
        i += 2;
    }
    // 防御：并发紧缩窗口里 HGETALL 可能读到空表（另一请求刚 DEL 未重写）。此时
    // **不做破坏性 DEL**（复审 M1：原实现在存活判定不可信时仍 DEL，会清空别家刚
    // 写回的元数据）。
    if entries.is_empty() {
        return Ok(());
    }
    entries.sort_by(|a, b| {
        let sa: u64 = a.0.parse().unwrap_or(0);
        let sb: u64 = b.0.parse().unwrap_or(0);
        sb.cmp(&sa)
    });
    entries.truncate(cap);

    let keys: Vec<String> = entries
        .iter()
        .map(|(seq, _)| sv_body_key(kind, id, seq.parse().unwrap_or(0)))
        .collect();
    let alive: Vec<bool> = if keys.is_empty() {
        Vec::new()
    } else {
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        match st.lux.execute_value("MGET", &refs).await {
            Ok(EmbeddedValue::Array(vals)) if vals.len() == keys.len() => vals
                .iter()
                .map(|v| !matches!(v, EmbeddedValue::Nil))
                .collect(),
            // **存活判定不可信时拒绝紧缩**（复审 M1）：原实现把错误吞成
            // `vec![false; …]`，随后仍执行 `DEL meta` + 重写空集——一次瞬态
            // MGET 失败就清空整屏版本历史（与 §4.3「无静默数据丢失路径」冲突）。
            Ok(_) => return Err("MGET 响应形状/长度异常（已跳过紧缩，元数据保持原样）".into()),
            Err(e) => return Err(e.to_string()),
        }
    };
    let keep: Vec<&(String, String)> = entries
        .iter()
        .zip(alive.iter().chain(std::iter::repeat(&false)))
        .filter(|(_, alive)| **alive)
        .map(|(e, _)| e)
        .collect();

    // DEL 整键 + 重写保留集（无 HDEL——分类表有、解析器无，§1.1）。
    st.lux
        .execute("DEL", &[&sv_meta_key(kind, id)])
        .await
        .map_err(|e| e.to_string())?;
    for (seq, meta) in keep {
        st.lux
            .execute("HSET", &[&sv_meta_key(kind, id), seq, meta])
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
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

/// 内容 sha256 前 16 hex（判重展示与审计指纹；`sha2` 已在依赖树，无新依赖）。
fn sha16(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
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

    /// 验证意图：sv: 键族的键名形状是列表隔离（§1.2）与级联删（§2.3）的共享
    /// 契约——前缀错一格就会撞进 `KEYS screen:*` 扫描面或漏删世代正文。
    #[test]
    fn sv_key_shapes() {
        assert_eq!(sv_seq_key(&SCREENS, "a"), "sv:screen:a:seq");
        assert_eq!(sv_meta_key(&SCREENS, "a"), "sv:screen:a:meta");
        assert_eq!(sv_body_key(&SCREENS, "a", 7), "sv:screen:a:v:7");
        assert_eq!(sv_seq_key(&THEMES, "a"), "sv:theme:a:seq");
        // 独立前缀断言：任何 sv 键都不以 screen:/theme: 开头。
        assert!(!sv_body_key(&SCREENS, "a", 1).starts_with("screen:"));
    }

    /// 验证意图：sha16 是元数据审计指纹（§2.2）——长度恒 16、同内容稳定、
    /// 异内容可区分；前 8 字节截取与十六进制小写是展示契约。
    #[test]
    fn sha16_stable_and_discriminating() {
        let a = sha16(b"hello");
        assert_eq!(a.len(), 16);
        assert_eq!(a, sha16(b"hello"));
        assert_ne!(a, sha16(b"hellp"));
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
