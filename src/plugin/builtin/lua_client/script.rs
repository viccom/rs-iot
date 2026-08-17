//! 脚本加载与编译 + 注入受控 require（限定 `scripts/<name>/lib/`，spec §18 红线）。
//!
//! 沙箱 `new_with` 未装 package 库（require 本就不可用），这里手动注入一个受控
//! require 全局函数，路径限定在脚本目录的 lib/ 下，防穿越逃逸。

use crate::plugin::builtin::lua_client::loader;
use mlua::Lua;
use std::path::Path;
use std::sync::Arc;

/// 加载上下文：脚本根 + 脚本名（供 require 闭包解析模块路径）。
#[derive(Clone)]
pub(super) struct LoadCtx {
    /// 脚本根目录（`scripts`）。
    pub(super) scripts_root: Arc<Path>,
    /// 脚本目录名（如 `devA`）。
    pub(super) script_name: Arc<str>,
}

/// 在 `lua` 上注入受控 require（限定 `scripts/<name>/lib/`）并加载 main.lua 入口。
pub(super) fn load_main(lua: &Lua, lctx: &LoadCtx) -> mlua::Result<()> {
    let lctx2 = lctx.clone();
    let require_fn = lua.create_function(move |lua, req: String| -> mlua::Result<()> {
        let lctx = lctx2.clone();
        match loader::local_module_path(&lctx.scripts_root, &lctx.script_name, &req) {
            Some(p) => {
                let src = std::fs::read_to_string(&p)
                    .map_err(|e| mlua::Error::RuntimeError(format!("require {req}: {e}")))?;
                lua.load(&src).set_name(format!("={req}")).exec()?;
                Ok(())
            }
            None => Err(mlua::Error::RuntimeError(format!(
                "require '{req}' 越界或不存在（仅限 lib/）"
            ))),
        }
    })?;
    lua.globals().set("require", require_fn)?;

    let main_path = loader::main_lua_path(&lctx.scripts_root, &lctx.script_name);
    let src = std::fs::read_to_string(&main_path)
        .map_err(|e| mlua::Error::RuntimeError(format!("加载脚本 {}: {e}", main_path.display())))?;
    lua.load(&src)
        .set_name(format!("={}_main", lctx.script_name))
        .exec()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::builtin::lua_client::config::parse_cfg;
    use crate::plugin::builtin::lua_client::engine::new_vm;
    use serde_json::json;
    use std::fs;

    #[test]
    fn load_main_defines_functions() {
        let tmp = std::env::temp_dir().join(format!("lua-load-{}", std::process::id()));
        let dir = tmp.join("devA");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("main.lua"),
            "function collect(ctx) return {v=1} end",
        )
        .unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        let has: bool = lua
            .load("return type(collect) == 'function'")
            .eval()
            .unwrap();
        assert!(has, "main.lua 应定义 collect");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn require_rejects_traversal() {
        let tmp = std::env::temp_dir().join(format!("lua-req-{}", std::process::id()));
        fs::create_dir_all(tmp.join("devA/lib")).unwrap();
        fs::write(tmp.join("devA/main.lua"), "").unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        let r: mlua::Result<()> = lua.load("require('../../etc/passwd')").exec();
        assert!(r.is_err(), "越界 require 应被拒");

        let _ = fs::remove_dir_all(&tmp);
    }
}
