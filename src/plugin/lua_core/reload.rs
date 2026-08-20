//! luaClient 实例 reload 核心（Q11/Q15，T10 自 `api/routes_lua.rs` 移入）。
//!
//! 落位依据：`_lib/` 全局 watcher（hotload，Q15）与 REST 端点（api）共用本入口；
//! 若留在 api 层，lua_core → api 反向依赖破坏分层（api → lua_core 已存在）。
//! 移入 lua_core 后两个调用方都单向依赖本模块。lux 直连（HGET/HGETALL
//! `execute_value`），不借 `api::hget*` helper。
//!
//! 两条 reload 路径（阶段 A 审查修复后语义对齐 Q11）：
//! - **批量**（[`reload_all_lua_instances`]，stdlib sync / `_lib` watcher 触发）：
//!   经控制通道发 `"reload"` → 实例 task 内走既有 `Evt::Reload`（`reload_vm`
//!   VM 级替换，**失败回退旧 VM**）——不再 stop+start（旧实现会复活管理员已停
//!   实例，且坏库语法错误会让实例停机无回退，两条均违背 Q11）。
//! - **单实例**（[`reload_lua_inst`]，REST `POST /lua/reload` 显式操作）：保持
//!   stop + start 全量重启（管理员显式指定，语义 = 重启该实例）。

use crate::plugin::instance::{InstState, InstanceManager};
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
/// watcher 同走此入口）：遍历 lux `instances`，**仅当前 Running 的 luaClient 实例**
/// 经控制通道发 `"reload"` → 实例 task 内 `reload_vm` VM 级替换（require 重执行
/// 天然生效；manifest dry-run 失败或 on_init dry-run 超限 → **回退旧 VM**，Q11）。
///
/// 阶段 A 审查修复（原实现 stop+start 的两个缺陷）：
/// 1. **不复活已停实例**——`auto_start=false` 从未启动或运行中手动停止的实例
///    被隐式全量操作拉起，破坏 InstanceManager 按实例启停语义；
/// 2. **失败有回退**——坏 `_lib`/`_stdlib` 文件让 `start_source` 失败后实例停机
///    （旧实现仅在响应 JSON 报告），现在 reload 失败旧 VM 继续。
///
/// 非运行实例记 `skipped`（响应可见，管理员可判断哪些实例下次启动时自然取新库）。
/// 返回 `[{inst_id, result}]` 供 REST 响应与审计。
pub(crate) async fn reload_all_lua_instances(
    lux: &lux::EmbeddedClient,
    mgr: &InstanceManager,
) -> Vec<serde_json::Value> {
    let pairs = match lux.execute_value("HGETALL", &["instances"]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => return Vec::new(),
    };
    let states = mgr.status();
    let mut out = Vec::new();
    // HGETALL 返回 [k1, v1, k2, v2, ...] 扁平数组（同 api::hgetall / try_start_from_lux）。
    let mut i = 0;
    while i + 1 < pairs.len() {
        let inst_id = str_from_value(&pairs[i]);
        let raw = str_from_value(&pairs[i + 1]);
        let app_code: String = serde_json::from_str::<InstConfig>(&raw)
            .ok()
            .map(|c| c.app_code.as_ref().to_string())
            .unwrap_or_default();
        if !batch_reload_target(&app_code, states.get(&inst_id).copied()) {
            // 非 lua 实例静默跳过（批量语义只管 lua）；lua 但非 Running 记 skipped。
            if app_code == "luaClient" {
                out.push(serde_json::json!({
                    "inst_id": inst_id,
                    "result": format!("skipped（非运行态 {:?}，不复活；下次启动自然取新库）",
                        states.get(&inst_id).copied()),
                }));
            }
            i += 2;
            continue;
        }
        // 控制通道直达（fire-and-forget）：reload 结果由实例侧日志/stat 呈现
        // （reload_vm 失败 → "热加载失败，旧 VM 继续" warn + 旧 VM 不受影响）。
        let result = if mgr.ctl_send(&inst_id, "reload", "") {
            "ok（VM 级 reload 触发，失败自动回退旧 VM）"
        } else {
            "控制通道不可达（实例可能刚停止，跳过）"
        };
        out.push(serde_json::json!({"inst_id": inst_id, "result": result}));
        i += 2;
    }
    out
}

/// 批量 reload 目标判定（纯函数便于单测）：luaClient 且当前 Running 才触发。
fn batch_reload_target(app_code: &str, state: Option<InstState>) -> bool {
    app_code == "luaClient" && matches!(state, Some(InstState::Running))
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

    /// 批量 reload 目标判定（阶段 A 审查修复：不复活非运行实例）：
    /// 仅「luaClient 且 Running」触发；未启动/已停/失败态/非 lua 一律跳过。
    #[test]
    fn batch_reload_target_only_running_lua() {
        use crate::plugin::instance::InstState;
        assert!(batch_reload_target("luaClient", Some(InstState::Running)));
        // 管理员已停（含从未启动的 None）→ 不复活
        assert!(!batch_reload_target("luaClient", Some(InstState::Stopped)));
        assert!(!batch_reload_target("luaClient", Some(InstState::Failed)));
        assert!(!batch_reload_target("luaClient", None));
        // 非 lua 实例不进批量语义
        assert!(!batch_reload_target(
            "modbusClient",
            Some(InstState::Running)
        ));
    }
}
