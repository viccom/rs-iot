//! 脚本目录约定解析与 require 首段命名空间路由（设计 §3.1/D11）。
//!
//! 入口 `scripts/<name>/main.lua`；require 按首段路由四层：
//! builtin（Rust 注册单段名）/ stdlib（`_stdlib/` 官方镜像，永不回落）/
//! shared（`_lib/<作者>/`，第三方 ≥2 段）/ private（`scripts/<name>/lib/`，单段现状）。

use crate::plugin::lua_core::capability::BUILTIN_MODULES;
use std::path::{Path, PathBuf};

/// 脚本根目录（相对于进程工作目录）。生产可由 app_config 覆盖，Plan 1 固定。
///
/// `run` 用此常量构造默认 `LoadCtx::scripts_root`（可被 `app_config.scripts_root` 覆盖）。
pub(crate) const SCRIPTS_ROOT: &str = "scripts";

/// 官方保留段封闭集（§3.1/D11）：首段命中 → 只查 `_stdlib/`，miss 即错永不回落。
pub(crate) const RESERVED_TOP: &[&str] = &["algo", "codec", "proto", "cloud", "util", "oo"];

/// require 路由目标层（§3.1 四层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModuleSource {
    /// builtin 模块：首段 ∈ [`BUILTIN_MODULES`]（如 json），Rust 注册。
    Builtin(String),
    /// 官方库：`scripts/_stdlib/<路径>.lua`（保留段首段；只读镜像，永不回落其他层）。
    Stdlib(PathBuf),
    /// 站点共享库：`scripts/_lib/<首段>/<余段>.lua`（首段=作者/组织，≥2 段）。
    Shared(PathBuf),
    /// 脚本私有库：`scripts/<name>/lib/<路径>.lua`（单段名或旧 `lib.` 前缀）。
    Private(PathBuf),
}

/// 解析脚本入口路径：scripts/<name>/main.lua。
///
/// `name` 已在 `config::parse_cfg` 校验为 [A-Za-z0-9_-]，这里再做一次防御性断言。
pub(crate) fn main_lua_path(scripts_root: &Path, name: &str) -> PathBuf {
    debug_assert!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "script name 已校验"
    );
    scripts_root.join(name).join("main.lua")
}

/// require 首段命名空间路由（§3.1/D11，计划 T6 修正版）。返回 Err = 违规名。
///
/// 判别顺序（顺序即语义）：
/// 1. 违规拒绝：穿越（`..`）、绝对路径、反斜杠、内嵌斜杠、空段——沿用原
///    `local_module_path` 防穿越规则；
/// 2. **builtin 名单优先**：json 等是单段 builtin 名，必须在"单段=私有"判断之前；
/// 3. 单段名或旧 `lib.` 前缀 → 私有层 `scripts/<name>/lib/`（现状不变；这同时保证
///    到达共享层的一定 ≥2 段——D11「单段进不了共享层」的结构性落实）；
/// 4. 保留段首段 → Stdlib `scripts/_stdlib/<路径>.lua`（miss 即错，**永不回落**
///    共享/私有——防投毒核心规则）；
/// 5. 其余 → Shared `scripts/_lib/<首段>/<余段>.lua`（首段=作者/组织）。
pub(crate) fn route(
    scripts_root: &Path,
    script_name: &str,
    req: &str,
) -> Result<ModuleSource, String> {
    if req.contains("..") || req.starts_with('/') || req.contains('\\') {
        return Err("路径违规（禁止 ..、绝对路径、反斜杠）".into());
    }
    let parts: Vec<&str> = req.split('.').collect();
    if parts.iter().any(|p| p.is_empty() || p.contains('/')) {
        return Err("空段或内嵌斜杠".into());
    }
    let first = parts[0];
    // ② builtin 名单优先（json 是单段 builtin 名——先于单段=私有判断）
    if BUILTIN_MODULES.contains(&first) {
        return Ok(ModuleSource::Builtin(first.to_string()));
    }
    // ③ 单段或旧 "lib." 前缀 → 私有；裸 "lib" 剥前缀后为空 → 拒绝（同旧规则）
    let priv_seg: &[&str] = if first == "lib" { &parts[1..] } else { &parts };
    if priv_seg.is_empty() {
        return Err("裸 'lib' 前缀无模块名".into());
    }
    if parts.len() == 1 || first == "lib" {
        return Ok(ModuleSource::Private(
            scripts_root
                .join(script_name)
                .join("lib")
                .join(format!("{}.lua", priv_seg.join("/"))),
        ));
    }
    // ④ 官方保留段 → _stdlib/（只查官方镜像，miss 由 script.rs 报"未同步"——永不回落）
    if RESERVED_TOP.contains(&first) {
        return Ok(ModuleSource::Stdlib(
            scripts_root.join("_stdlib").join(format!("{}.lua", parts.join("/"))),
        ));
    }
    // ⑤ 第三方共享：_lib/<作者>/<库路径>.lua（≥2 段已由 ③ 保证）
    Ok(ModuleSource::Shared(
        scripts_root
            .join("_lib")
            .join(first)
            .join(format!("{}.lua", parts[1..].join("/"))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_path_basic() {
        let p = main_lua_path(Path::new("scripts"), "devA");
        assert_eq!(p, PathBuf::from("scripts/devA/main.lua"));
    }

    /// T6 首段路由（§3.1/D11）——分支顺序为计划修正版：builtin 名单优先于"单段=私有"
    /// 判断（json 等是单段 builtin 名），共享层 ≥2 段由单段走私有分支保证。
    #[test]
    fn route_by_first_segment() {
        let root = Path::new("scripts");
        // builtin 层：首段 ∈ BUILTIN_MODULES（json 是单段名——须先于单段=私有判断）
        assert_eq!(
            route(root, "devA", "json").unwrap(),
            ModuleSource::Builtin("json".into())
        );
        // stdlib 层：首段 ∈ 保留段 → scripts/_stdlib/<路径>.lua
        assert_eq!(
            route(root, "devA", "algo.crc16").unwrap(),
            ModuleSource::Stdlib(PathBuf::from("scripts/_stdlib/algo/crc16.lua"))
        );
        assert_eq!(
            route(root, "devA", "cloud.onenet.token").unwrap(),
            ModuleSource::Stdlib(PathBuf::from("scripts/_stdlib/cloud/onenet/token.lua"))
        );
        // shared 层：首段=作者/组织 → scripts/_lib/<首段>/<余段>.lua（第三方 ≥2 段，
        // 第三段起任意深度；viccom.algo 合法——algo 在此是 viccom 的库名，非保留段开头）
        assert_eq!(
            route(root, "devA", "viccom.calcex").unwrap(),
            ModuleSource::Shared(PathBuf::from("scripts/_lib/viccom/calcex.lua"))
        );
        assert_eq!(
            route(root, "devA", "viccom.calcex.poller").unwrap(),
            ModuleSource::Shared(PathBuf::from("scripts/_lib/viccom/calcex/poller.lua"))
        );
        assert_eq!(
            route(root, "devA", "viccom.algo").unwrap(),
            ModuleSource::Shared(PathBuf::from("scripts/_lib/viccom/algo.lua"))
        );
        // private 层：单段 → scripts/<name>/lib/<段>.lua（现状不变，§3.1 第 4 行；
        // D11「单段进不了共享层」由此分支结构性保证——viccom 单段也走私有）
        assert_eq!(
            route(root, "devA", "myutil").unwrap(),
            ModuleSource::Private(PathBuf::from("scripts/devA/lib/myutil.lua"))
        );
        assert_eq!(
            route(root, "devA", "viccom").unwrap(),
            ModuleSource::Private(PathBuf::from("scripts/devA/lib/viccom.lua"))
        );
        // 兼容旧 "lib." 前缀 → 私有（段映射同现状 local_module_path）
        assert_eq!(
            route(root, "devA", "lib.util").unwrap(),
            ModuleSource::Private(PathBuf::from("scripts/devA/lib/util.lua"))
        );
        assert_eq!(
            route(root, "devA", "lib.a.b").unwrap(),
            ModuleSource::Private(PathBuf::from("scripts/devA/lib/a/b.lua"))
        );
    }

    /// T6 违规名强制拒绝（§3.1/D11）：穿越/绝对路径/反斜杠/空段沿用现有防穿越规则。
    #[test]
    fn route_rejects_violations() {
        let root = Path::new("scripts");
        assert!(route(root, "a", "").is_err());
        assert!(route(root, "a", "a.").is_err(), "尾段空");
        assert!(route(root, "a", ".a").is_err(), "首段空");
        assert!(route(root, "a", "a..b").is_err(), "中段空");
        assert!(route(root, "a", "../evil").is_err(), "防穿越（沿用现有规则）");
        assert!(route(root, "a", "/etc/passwd").is_err(), "绝对路径");
        assert!(route(root, "a", "..\\x").is_err(), "反斜杠");
        assert!(route(root, "a", "a/b").is_err(), "内嵌斜杠（沿用现有规则）");
        assert!(route(root, "a", "lib").is_err(), "裸 lib 前缀无模块名");
    }
}
