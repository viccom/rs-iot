//! stdlib 种子（rust-embed 内嵌，设计 §3.6 D5：内网冷启动 `_stdlib/` 未同步时的兜底）。
//!
//! 权威源在官方仓 rsiot-lualib（T13 建制）；本目录种子只收录带 `M.selftest`
//! 且冒烟通过的库（FreeIOE vendored，§3.3 质量闸门）。debug 构建下 rust-embed
//! 按编译期记录的绝对路径读文件（源码树在即有效）；release 构建真嵌入。

/// 种子内嵌目录（rust-embed 8：`folder` 相对 crate 根 Cargo.toml 解析，同
/// `src/api/frontend.rs` 惯例）。依赖已是无条件主依赖（mime-guess feature 不影响 derive）。
#[derive(rust_embed::RustEmbed)]
#[folder = "src/plugin/lua_core/stdlib_seed/"]
struct Seed;

/// 种子模块清单（selftest 冒烟遍历集，须与 `stdlib_seed/` 目录一一对应）。
/// 1.5.0 扩容：1.3.0 三库（middleclass/stream_buffer/bcd）+ 1.4.0 两库
/// （cov/threshold）进种子——冷启动内网现场（无 upstream 可达）require 兜底；
/// 全部带 M.selftest 过闸。
pub(crate) const SEED_MODULES: &[&str] = &[
    "algo/crc16",
    "algo/cov",
    "codec/basexx",
    "codec/bcd",
    "oo/middleclass",
    "proto/stream_buffer",
    "util/retry",
    "util/tablex",
    "util/threshold",
];

/// 取种子源码：`path` 为点分模块名的 `/` 形式（`algo/crc16`），返回 `<path>.lua` 内容。
pub(crate) fn seed_get(path: &str) -> Option<Vec<u8>> {
    let name = format!("{path}.lua");
    Seed::get(&name).map(|f| f.data.to_vec())
}

/// 启动/CI 冒烟（设计 §3.3 质量闸门）：逐库 load→eval 取模块表→调 `selftest`。
///
/// `load` 即语法闸门（FreeIOE 上游有语法错误先例，vendored 前必须过闸）；模块文件
/// 自身 `return M`，eval 返回值即模块表，**不入 globals**（与 Lua require 语义一致）。
/// 返回通过的模块数。
///
/// T7 仅测试接线；实例启动路径接入由 T9（sync 后校验）/manifest dry-run 落地。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn seed_selftest_all(lua: &mlua::Lua) -> mlua::Result<usize> {
    let mut ok = 0;
    for path in SEED_MODULES {
        let bytes = seed_get(path).unwrap_or_else(|| {
            panic!("stdlib 种子缺失：{path}.lua（embed 目录与 SEED_MODULES 不一致）")
        });
        let src = String::from_utf8(bytes)
            .map_err(|_| mlua::Error::RuntimeError(format!("stdlib 种子 {path}.lua 非 UTF-8")))?;
        let m: mlua::Table = lua
            .load(src)
            .set_name(format!("={path}"))
            .eval()
            .map_err(|e| {
                mlua::Error::RuntimeError(format!("stdlib 种子 {path} 加载失败（语法闸门）: {e}"))
            })?;
        let f: mlua::Function = m.get("selftest").map_err(|_| {
            mlua::Error::RuntimeError(format!(
                "stdlib 种子 {path} 返回表缺 M.selftest（§3.3 约定）"
            ))
        })?;
        if !f.call::<bool>(())? {
            return Err(mlua::Error::RuntimeError(format!(
                "stdlib 种子 {path} selftest=false"
            )));
        }
        ok += 1;
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::lua_core::config::parse_cfg;
    use crate::plugin::lua_core::engine::new_vm;
    use serde_json::json;

    /// T7 种子集完整：SEED_MODULES 与 embed 目录一一对应；未知路径 miss。
    #[test]
    fn seed_modules_all_present() {
        for p in SEED_MODULES {
            assert!(seed_get(p).is_some(), "种子缺 {p}.lua");
        }
        assert!(seed_get("algo/nonexistent").is_none(), "未知路径应 miss");
    }

    /// T7 冒烟闸门（§3.3）：全部种子库在**沙箱 VM**（lua_core 生产形态——仅
    /// TABLE|STRING|MATH|COROUTINE，非满配 Lua）上 load→eval→selftest 通过。
    /// VM 须先装 require 路由（load_main）：1.5.0 起种子含跨库依赖
    /// （cov → oo.middleclass / util.tablex），裸 VM 无 require 会炸——这正是
    /// 生产语义（种子互依赖经保留段路由→_stdlib miss→种子兜底）。
    #[test]
    fn seed_selftest_all_passes_on_sandbox_vm() {
        // load_main 需要脚本目录存在（装 require 路由时会读 main.lua）——临时目录
        // 提供最小 main.lua（与 stdlib_sync 测试 fresh_scripts 同法）。
        let root = std::env::temp_dir().join(format!("rsiot-seedtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("t")).unwrap();
        std::fs::write(root.join("t/main.lua"), "-- seed selftest host\n").unwrap();
        let cfg = parse_cfg(&json!({"script":"t","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = crate::plugin::lua_core::script::LoadCtx {
            scripts_root: std::sync::Arc::from(root.as_path()),
            script_name: std::sync::Arc::<str>::from("t"),
        };
        crate::plugin::lua_core::script::load_main(&lua, &lctx)
            .expect("load_main 应装好 require 路由");
        assert_eq!(
            seed_selftest_all(&lua).expect("全部种子 selftest 应通过"),
            SEED_MODULES.len(),
            "通过数应等于种子数"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
