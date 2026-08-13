//! instance CRUD（lux Hash `instances`）。

use crate::api::{hget, hgetall, AppState};
use axum::extract::{Query, State};
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct IdQuery {
    pub inst_id: String,
}

pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(hgetall(&st.lux, "instances").await)
}

pub async fn get(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Json<serde_json::Value> {
    match hget(&st.lux, "instances", &q.inst_id).await {
        Some(s) => {
            let val = serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s));
            Json(serde_json::json!({ q.inst_id: val }))
        }
        None => Json(serde_json::json!({})),
    }
}

#[derive(Deserialize)]
pub struct SetReq {
    pub inst_id: String,
    pub object: serde_json::Value,
}

pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Json<serde_json::Value> {
    let mut obj = req.object;
    if let Some(m) = obj.as_object_mut() {
        m.insert("inst_id".into(), req.inst_id.clone().into());
    }
    // 先反序列化校验整体合法性（host_kind/app_code/app_type 等），失败直接返回错误——
    // 避免"set 成功存入非法配置、startinst 反序列化才报错"的延迟失败让用户困惑。
    let inst: crate::types::InstConfig = match serde_json::from_value(obj.clone()) {
        Ok(i) => i,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "error",
                "msg": format!("invalid instance config: {e}")
            }));
        }
    };
    let s = serde_json::to_string(&obj).unwrap_or_default();
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;

    // 外部插件实例（host_kind=External）：签发 token，仅此次返回明文（管理员交付给外部进程）。
    // 注意：set 每次调 issue 会覆盖旧 token（旧 token 失效）；重复 set 即轮换。
    if inst.host_kind == crate::types::HostKind::External {
        match crate::db::ext_token::issue(&st.lux, &st.jwt_secret, &req.inst_id).await {
            Ok(token) => return Json(serde_json::json!({"status": "ok", "plugin_token": token})),
            Err(e) => tracing::warn!("[routes_inst] issue token for {} failed: {e}", req.inst_id),
        }
    }
    Json(serde_json::json!({"status": "ok"}))
}

#[derive(Deserialize)]
pub struct DelReq {
    pub inst_id: String,
}

/// 级联删除实例的全部痕迹：停运行 task → 清 rtdata（逐设备）→ 清 stat →
/// 清 trace → 删配置 → 清 token/luakv。
///
/// **顺序关键**：先 `HGET` 读配置拿 devid 列表，再 `HDEL` 删配置——否则配置删了
/// 就读不到关联设备，rtdata 会残留成孤儿（mqttPub 仍发布）。各步骤容错（warn 不
/// 中断），尽力清完避免半残留，与原 `delete` 的语义一致。`pub(crate)` 供
/// `purge` 批量复用。
pub(crate) async fn cascade_delete(st: &AppState, inst_id: &str) {
    // 1. 先读配置，收集该实例的所有 devid（rtdata key 依据）。
    let devids: Vec<String> = match hget(&st.lux, "instances", inst_id).await {
        Some(raw) => serde_json::from_str::<crate::types::InstConfig>(&raw)
            .map(|cfg| cfg.devices.into_iter().map(|d| d.value).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    // 2. 停运行中的实例 task（若在跑，否则 no-op）。
    if let Err(e) = st.mgr.stop(inst_id).await {
        tracing::warn!("[routes_inst] stop {inst_id} on delete failed: {e}");
    }
    // 3. 清该实例所有设备的实时数据（防 mqttPub 发孤儿）。
    for dev_id in &devids {
        if let Err(e) = crate::db::rtdata::delete_device(&st.lux, dev_id).await {
            tracing::warn!("[routes_inst] delete rtdata:{dev_id} failed: {e}");
        }
    }
    // 4. 清实例指标快照。
    if let Err(e) = crate::db::stat::delete(&st.lux, inst_id).await {
        tracing::warn!("[routes_inst] delete stat:{inst_id} failed: {e}");
    }
    // 5. 清报文 trace 注册（内存态，deactivate 即从 DashMap 移除）。
    st.trace_registry.deactivate(inst_id);
    // 6. 删实例配置（HDEL）。
    let _ = st.lux.execute("HDEL", &["instances", inst_id]).await;
    // 7. 清外部插件 token（实例没了 token 必失效——安全底线）。
    if let Err(e) = crate::db::ext_token::remove(&st.lux, inst_id).await {
        tracing::warn!("[routes_inst] remove token for {inst_id} failed: {e}");
    }
    // 8. 清 Lua kv（脚本持久状态随实例失效）。
    if let Err(e) = crate::db::luakv::remove(&st.lux, inst_id).await {
        tracing::warn!("[routes_inst] remove luakv for {inst_id} failed: {e}");
    }
}

pub async fn delete(
    State(st): State<AppState>,
    Json(req): Json<DelReq>,
) -> Json<serde_json::Value> {
    cascade_delete(&st, &req.inst_id).await;
    Json(serde_json::json!({"status": "deleted"}))
}

#[derive(Deserialize)]
pub struct PurgeReq {
    /// 清空范围：`all`（删所有实例 + 级联，重置业务数据）/ `rtdata`（只清实时数据缓存）。
    pub scope: String,
}

/// `POST /api/v1/purge`：清空业务数据（重置）。
///
/// - `scope=all`：遍历所有实例逐个 `cascade_delete`（停 task + 清 rtdata/stat/trace/
///   配置/token/luakv），再兜底 `KEYS` 清残留孤儿。**绝不碰** `users`/`passwords`/
///   `groups`/`accesskey`/`ext_tokens` Hash 整体——只删已删实例的精确 field。
/// - `scope=rtdata`：只 `KEYS rtdata:*` + DEL（清实时数据缓存，保留实例配置）。
///
/// 无 FLUSHDB（lux 未验证支持），全用 `KEYS` + `DEL`（对标 `routes_rtdata.rs:88` 遍历）。
pub async fn purge(
    State(st): State<AppState>,
    Json(req): Json<PurgeReq>,
) -> Json<serde_json::Value> {
    match req.scope.as_str() {
        "all" => {
            let insts = hgetall(&st.lux, "instances").await;
            let n_inst = insts.as_object().map(|o| o.len()).unwrap_or(0);
            if let Some(obj) = insts.as_object() {
                for inst_id in obj.keys() {
                    cascade_delete(&st, inst_id).await;
                }
            }
            // 兜底：清 cascade 可能遗漏的孤儿（配置已不在但数据残留的历史痕迹）。
            let n_rtdata = del_by_pattern(&st.lux, "rtdata:*").await;
            let _ = del_by_pattern(&st.lux, "stat:*").await;
            let _ = del_by_pattern(&st.lux, "luakv:*").await;
            Json(serde_json::json!({
                "status": "purged",
                "instances": n_inst,
                "rtdata_keys": n_rtdata,
            }))
        }
        "rtdata" => {
            let n = del_by_pattern(&st.lux, "rtdata:*").await;
            Json(serde_json::json!({"status": "purged", "rtdata_keys": n}))
        }
        other => Json(serde_json::json!({
            "status": "error",
            "msg": format!("unknown scope: '{other}', expected 'all' or 'rtdata'")
        })),
    }
}

/// `KEYS <pattern>` + 逐个 `DEL`，返回删除数量。
///
/// 仿 `routes_rtdata.rs:88` 的 KEYS 遍历模式。仅用于 purge 的兜底清理，
/// **不**对 `users`/`passwords`/`groups`/`accesskey`/`ext_tokens` 等系统 key 调用。
async fn del_by_pattern(c: &lux::EmbeddedClient, pattern: &str) -> usize {
    let keys = match c.execute_value("KEYS", &[pattern]).await {
        Ok(EmbeddedValue::Array(ks)) => ks,
        _ => return 0,
    };
    let mut n = 0;
    for k in keys {
        let key = match &k {
            EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            EmbeddedValue::Simple(s) => s.clone(),
            _ => continue,
        };
        if c.execute("DEL", &[&key]).await.is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 启动测试 lux（仿 `db/rtdata.rs` batch_tests harness，端口 0 + 唯一 data_dir）。
    async fn start_lux(name: &str) -> (lux::ServerHandle, lux::EmbeddedClient) {
        let dir = std::env::temp_dir().join(format!("purge-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sc = lux::ServerConfig::default();
        sc.port = 0;
        sc.data_dir = dir.to_str().unwrap().to_string();
        sc.storage = lux::StorageConfig {
            mode: lux::StorageMode::Tiered,
            dir: sc.data_dir.clone(),
        };
        sc.save_interval = std::time::Duration::from_secs(60);
        let h = lux::run_with_config(sc).await.unwrap();
        let c = h.client();
        (h, c)
    }

    /// purge 兜底清理：只删匹配 pattern 的 key，其他前缀保留（绝不误伤 stat/users 等）。
    #[tokio::test]
    async fn del_by_pattern_clears_matching_keys_only() {
        let (_h, c) = start_lux("pattern").await;
        c.execute("SET", &["rtdata:a", "1"]).await.unwrap();
        c.execute("SET", &["rtdata:b", "1"]).await.unwrap();
        c.execute("SET", &["stat:x", "1"]).await.unwrap();
        c.execute("SET", &["users:keep", "1"]).await.unwrap();
        let n = del_by_pattern(&c, "rtdata:*").await;
        assert_eq!(n, 2, "两个 rtdata key 被删");
        let stat_exists = c.execute_value("EXISTS", &["stat:x"]).await.unwrap();
        assert_ne!(stat_exists, lux::EmbeddedValue::Int(0), "stat 保留");
        let users_exists = c.execute_value("EXISTS", &["users:keep"]).await.unwrap();
        assert_ne!(users_exists, lux::EmbeddedValue::Int(0), "users 保留");
    }
}
