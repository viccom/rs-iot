//! 脚本目录约定解析：`scripts/<name>/main.lua` + 本地 lib/*.lua（require 限定）。

use std::path::{Path, PathBuf};

/// 脚本根目录（相对于进程工作目录）。生产可由 app_config 覆盖，Plan 1 固定。
///
/// `run` 用此常量构造默认 `LoadCtx::scripts_root`（可被 `app_config.scripts_root` 覆盖）。
pub(super) const SCRIPTS_ROOT: &str = "scripts";

/// 解析脚本入口路径：scripts/<name>/main.lua。
///
/// `name` 已在 `config::parse_cfg` 校验为 [A-Za-z0-9_-]，这里再做一次防御性断言。
pub(super) fn main_lua_path(scripts_root: &Path, name: &str) -> PathBuf {
    debug_assert!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "script name 已校验"
    );
    scripts_root.join(name).join("main.lua")
}

/// 解析本地模块路径：scripts/<name>/lib/<mod_path>.lua（require 限定在目录内）。
///
/// `req` 是 require 参数（如 "lib.foo" 或 "foo"）；返回 None 表示拒绝（越界或含 `..`）。
pub(super) fn local_module_path(scripts_root: &Path, name: &str, req: &str) -> Option<PathBuf> {
    if req.contains("..") || req.starts_with('/') || req.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = req.split('.').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || p.contains('/') || p.contains('\\'))
    {
        return None;
    }
    // 接受 "lib.foo" 或裸 "foo"：统一映射到 scripts/<name>/lib/<...>.lua
    let rel = if parts.first() == Some(&"lib") {
        parts[1..].join("/")
    } else {
        parts.join("/")
    };
    if rel.is_empty() {
        return None;
    }
    Some(
        scripts_root
            .join(name)
            .join("lib")
            .join(format!("{rel}.lua")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_path_basic() {
        let p = main_lua_path(Path::new("scripts"), "devA");
        assert_eq!(p, PathBuf::from("scripts/devA/main.lua"));
    }

    #[test]
    fn local_module_resolves() {
        let p = local_module_path(Path::new("scripts"), "devA", "lib.util").unwrap();
        assert_eq!(p, PathBuf::from("scripts/devA/lib/util.lua"));
        let p = local_module_path(Path::new("scripts"), "devA", "util").unwrap();
        assert_eq!(p, PathBuf::from("scripts/devA/lib/util.lua"));
    }

    #[test]
    fn local_module_rejects_traversal() {
        assert!(local_module_path(Path::new("scripts"), "a", "../../etc/passwd").is_none());
        assert!(local_module_path(Path::new("scripts"), "a", "/etc/passwd").is_none());
        assert!(local_module_path(Path::new("scripts"), "a", "..\\x").is_none());
        assert!(local_module_path(Path::new("scripts"), "a", "").is_none());
    }
}
