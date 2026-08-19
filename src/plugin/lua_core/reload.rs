//! luaClient 实例 reload 核心（Q11/Q15，T10 自 `api/routes_lua.rs` 移入）。
//!
//! 落位依据：`_lib/` 全局 watcher（hotload，Q15）与 REST 端点（api）共用本入口；
//! 若留在 api 层，lua_core → api 反向依赖破坏分层（api → lua_core 已存在）。
//! 移入 lua_core 后两个调用方都单向依赖本模块。lux 直连（HGET/HGETALL
//! `execute_value`），不借 `api::hget*` helper。

use crate::plugin::instance::InstanceManager;
use crate::types::InstConfig;

/// 单实例 reload 结果（REST `reload` 端点映射 HTTP 码、批量版映射 JSON，
/// 两出口共用 [`reload_lua_inst`] 的分类语义）。
pub(crate) enum ReloadOutcome {
    /// 成功。
    Ok,
    /// 实例不在 lux `instances` 表。
    NotFound,
    /// 配置不可解析 / 非 luaClient 实例（消息随原因）。
    BadRequest(String),
    /// stop+start 启动失败（旧实例已停）。
    StartFail(String),
}

/// lux 返回值 → String（HGET/HGETALL 的 Bulk/Simple/Int 形态；与
/// `api::str_from_value` 同义的最小本地副本，避免反向依赖）。
fn str_from_value(v: &lux::EmbeddedValue) -> String {
    match v {
        lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
        lux::EmbeddedValue::Simple(s) => s.clone(),
        lux::EmbeddedValue::Int(i) => i.to_string(),
        _ => String::new(),
    }
}

/// reload 核心：读 lux `instances` 配置 → 校验 luaClient → stop + start（复用
/// `InstanceManager`，与单实例端点/热加载同机制，spec §5.3）。
pub(crate) async fn reload_lua_inst(
    lux: &lux::EmbeddedClient,
    mgr: &InstanceManager,
    inst_id: &str,
) -> ReloadOutcome {
    let raw = match lux.execute_value("HGET", &["instances", inst_id]).await {
        // Nil（字段缺席）→ NotFound，与 api::hget 语义一致。
        Ok(v) if !matches!(v, lux::EmbeddedValue::Nil) => str_from_value(&v),
        _ => return ReloadOutcome::NotFound,
    };
    let cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return ReloadOutcome::BadRequest(format!("invalid config: {e}")),
    };
    if cfg.app_code.as_ref() != "luaClient" {
        return ReloadOutcome::BadRequest("not a luaClient instance".into());
    }
    let _ = mgr.stop(inst_id).await;
    match mgr.start_source(cfg).await {
        Ok(()) => ReloadOutcome::Ok,
        Err(e) => ReloadOutcome::StartFail(e.to_string()),
    }
}

/// 全实例 reload（Q11，设计 §3.6 D1「同步后引用实例自动 reload」；Q15 `_lib/`
/// watcher 同走此入口）：遍历 lux `instances`，`app_code == "luaClient"` 的逐一
/// stop + start。
///
/// stdlib 同步换新 `_stdlib/`、或 `_lib/` 共享库变更后，运行中 VM 已加载的模块
/// 不会自动失效，须重启实例重走 require（Q15 全实例策略：未直接引用的实例也一并
/// 重启——引用关系无登记，简单起见全刷）。配置缺席/解析失败/非 lua 实例自然跳过
/// （批量语义只管 lua）。返回 `[{inst_id, result: "ok"|<错误>}]` 供 REST 响应与审计。
pub(crate) async fn reload_all_lua_instances(
    lux: &lux::EmbeddedClient,
    mgr: &InstanceManager,
) -> Vec<serde_json::Value> {
    let pairs = match lux.execute_value("HGETALL", &["instances"]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    // HGETALL 返回 [k1, v1, k2, v2, ...] 扁平数组（同 api::hgetall / try_start_from_lux）。
    let mut i = 0;
    while i + 1 < pairs.len() {
        let inst_id = str_from_value(&pairs[i]);
        match reload_lua_inst(lux, mgr, &inst_id).await {
            ReloadOutcome::Ok => out.push(serde_json::json!({"inst_id": inst_id, "result": "ok"})),
            ReloadOutcome::StartFail(m) => {
                out.push(serde_json::json!({"inst_id": inst_id, "result": m}))
            }
            // NotFound 不可达（键来自同一次 HGETALL，HGET 必命中）；
            // BadRequest（非 lua/坏配置）跳过——批量语义只管 lua。
            _ => {}
        }
        i += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lux 返回值字符串化（Simple/Int/Nil 形态；Bulk 分支同构，构造依赖 lux 内部
    /// Bytes 类型形态，省略构造仅覆盖可比形态）。
    #[test]
    fn str_from_value_covers_lux_shapes() {
        assert_eq!(str_from_value(&lux::EmbeddedValue::Simple("x".into())), "x");
        assert_eq!(str_from_value(&lux::EmbeddedValue::Int(7)), "7");
        assert_eq!(str_from_value(&lux::EmbeddedValue::Nil), "");
    }
}
