//! Lua 子系统对外管理面（REST 唯一入口）。
//!
//! `api::routes_lua` 只经本模块触达 Lua 引擎，不再直接引用 `loader`/`stdlib`/
//! `stdlib_sync`/`reload` 内部模块（分层门禁 `scripts/check_layers.py` 的 FORBIDDEN
//! 规则）。本模块只做**策略收口**（保留段判定、已装版本读取、种子计数）与类型/函数
//! 转发，不承载新逻辑——真正的实现在各内部模块，此处是它们的稳定缝。

use std::path::Path;

/// 全实例 reload：仅 stdlib sync 端点（`lua-http`）使用。
#[cfg(feature = "lua-http")]
pub(crate) use crate::plugin::lua_core::reload::reload_all_lua_instances;
pub(crate) use crate::plugin::lua_core::reload::{reload_lua_inst, ReloadOutcome};
pub(crate) use crate::plugin::lua_core::stdlib_sync::InventoryEntry;

/// 保留段判定（D11 封闭集，单一事实源 `loader::RESERVED_TOP`）。
///
/// REST 的 `_lib/` 写入校验与 loader 的 require 路由共用这一判定——此前 api 层
/// 直接遍历 `loader::RESERVED_TOP`，两处各自持有同一规则。
pub(crate) fn is_reserved_top(seg: &str) -> bool {
    crate::plugin::lua_core::loader::RESERVED_TOP.contains(&seg)
}

/// 保留段全集（仅测试使用：REST 的保留段拒绝测试遍历此表；如需对外回显，
/// 去掉 `cfg(test)` 即可——事实源仍是 `loader::RESERVED_TOP`）。
#[cfg(test)]
pub(crate) fn reserved_top() -> &'static [&'static str] {
    crate::plugin::lua_core::loader::RESERVED_TOP
}

/// 本地 stdlib 清单（版本 + 条目）。
pub(crate) fn stdlib_inventory(scripts_root: &Path) -> (Option<String>, Vec<InventoryEntry>) {
    crate::plugin::lua_core::stdlib_sync::inventory(scripts_root)
}

/// 本地已装 stdlib 版本（`_stdlib/.index.json`；缺失 → None，首装跟随 latest）。
/// 仅 stdlib sync 的 pin 解析（`lua-http`）使用。
#[cfg(feature = "lua-http")]
pub(crate) fn installed_stdlib_version(scripts_root: &Path) -> Option<String> {
    crate::plugin::lua_core::stdlib_sync::read_index(scripts_root).map(|i| i.version)
}

/// 种子模块数（仅测试使用：REST 清单回显测试的期望值分母）。
#[cfg(test)]
pub(crate) fn seed_module_count() -> usize {
    crate::plugin::lua_core::stdlib::SEED_MODULES.len()
}

/// 验签同步官方源/镜像（仅 `lua-http` feature；无此 feature 时 sync 端点一并缺席）。
#[cfg(feature = "lua-http")]
pub(crate) async fn stdlib_sync(
    upstream: &str,
    scripts_root: &Path,
    pin: Option<&str>,
) -> Result<crate::plugin::lua_core::stdlib_sync::SyncReport, String> {
    crate::plugin::lua_core::stdlib_sync::sync(upstream, scripts_root, pin).await
}
