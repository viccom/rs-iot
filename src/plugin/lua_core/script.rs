//! 脚本加载与编译 + 注入受控 require（首段命名空间路由，设计 §3.1/D11）。
//!
//! 沙箱 `new_with` 未装 package 库（require 本就不可用），这里手动注入一个受控
//! require 全局函数：经 [`loader::route`] 按首段分发四层——builtin（Rust 注册）/
//! stdlib（`_stdlib/` 官方镜像文件优先，miss 回落 embed 种子，§3.6 D5）/
//! shared（`_lib/<作者>/`）/ private（`scripts/<name>/lib/`，现状语义）；
//! 违规名（穿越/空段）强制拒绝。

use crate::plugin::lua_core::{api, loader, stdlib};
use mlua::{Lua, Value};
use std::path::Path;
use std::sync::Arc;

/// 加载上下文：脚本根 + 脚本名（供 require 闭包解析模块路径）。
#[derive(Clone)]
pub(crate) struct LoadCtx {
    /// 脚本根目录（`scripts`）。
    pub(crate) scripts_root: Arc<Path>,
    /// 脚本目录名（如 `devA`）。
    pub(crate) script_name: Arc<str>,
}

/// 在 `lua` 上注入受控 require（`loader::route` 四层分发）并加载 main.lua 入口。
pub(crate) fn load_main(lua: &Lua, lctx: &LoadCtx) -> mlua::Result<()> {
    let lctx2 = lctx.clone();
    let require_fn = lua.create_function(move |lua, req: String| -> mlua::Result<Value> {
        let lctx = lctx2.clone();
        match loader::route(&lctx.scripts_root, &lctx.script_name, &req) {
            // 私有层：现状语义——读文件 + exec（库文件定义全局），返回 nil
            Ok(loader::ModuleSource::Private(p)) => {
                let src = std::fs::read_to_string(&p).map_err(|e| {
                    mlua::Error::RuntimeError(format!(
                        "require '{req}': 私有库 {} 读取失败（单段名走脚本私有层；\
                         共享库须 ≥2 段，如 viccom.calcex）: {e}",
                        p.display()
                    ))
                })?;
                lua.load(&src).set_name(format!("={req}")).exec()?;
                Ok(Value::Nil)
            }
            // builtin 层：Rust 注册模块（json 等，名单 BUILTIN_MODULES）
            Ok(loader::ModuleSource::Builtin(name)) => api::builtin_module(lua, &name),
            // 官方层：`_stdlib/<路径>.lua` 同步文件优先；miss 回落 embed 种子兜底
            //（§3.6 D5 离线冷启动）；两者皆无 → 可行动错误——永不回落共享/私有
            //（防投毒核心规则）
            Ok(loader::ModuleSource::Stdlib(p)) => {
                // route 已拒 `..`/`/`/`\`，点转斜杠后即种子内相对路径
                let rel = req.replace('.', "/");
                let src = match std::fs::read_to_string(&p) {
                    Ok(s) => s,
                    Err(_) => {
                        let bytes = stdlib::seed_get(&rel).ok_or_else(|| {
                            mlua::Error::RuntimeError(format!(
                                "官方库 {req} 未同步且不在种子集：请 sync 到 _stdlib/ 或检查模块名"
                            ))
                        })?;
                        String::from_utf8(bytes)
                            .map_err(|_| mlua::Error::RuntimeError(format!("种子 {rel}.lua 非 UTF-8")))?
                    }
                };
                lua.load(&src).set_name(format!("={req}")).eval::<Value>()
            }
            // 共享层：_lib/<作者>/<库路径>.lua，miss 报不存在（REST 可安装，§3.6）
            Ok(loader::ModuleSource::Shared(p)) => {
                let src = std::fs::read_to_string(&p).map_err(|e| {
                    mlua::Error::RuntimeError(format!(
                        "共享库 {req} 不存在（scripts/_lib/；{e}）"
                    ))
                })?;
                lua.load(&src)
                    .set_name(format!("={req}"))
                    .eval::<Value>()
            }
            // 违规名（穿越/空段等）强制拒绝
            Err(v) => Err(mlua::Error::RuntimeError(format!("require '{req}': {v}"))),
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
    use crate::plugin::lua_core::config::parse_cfg;
    use crate::plugin::lua_core::engine::new_vm;
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

    /// T6 集成（§3.1）：require 三层路由各取回值——stdlib（`_stdlib/`，eval 返回模块）、
    /// shared（`_lib/`，eval 返回模块）、private（现状语义：exec 定义全局）。
    #[test]
    fn require_routes_stdlib_shared_private() {
        let tmp = std::env::temp_dir().join(format!("lua-route6-pos-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("devA/lib")).unwrap();
        fs::create_dir_all(tmp.join("_stdlib/algo")).unwrap();
        fs::create_dir_all(tmp.join("_lib/org")).unwrap();
        fs::write(
            tmp.join("devA/main.lua"),
            r#"
local sx = require("algo.x")  -- Stdlib 层
local ol = require("org.lib") -- Shared 层
require("priv")               -- Private 层（现状语义：exec 定义全局）
route_res = { sx = sx.v, ol = ol.v, pv = priv_val }
"#,
        )
        .unwrap();
        fs::write(tmp.join("_stdlib/algo/x.lua"), "return { v = 41 }").unwrap();
        fs::write(tmp.join("_lib/org/lib.lua"), "return { v = 42 }").unwrap();
        fs::write(tmp.join("devA/lib/priv.lua"), "priv_val = 43").unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        assert_eq!(
            lua.load("return route_res.sx").eval::<i64>().unwrap(),
            41,
            "stdlib 层应取回模块返回值"
        );
        assert_eq!(
            lua.load("return route_res.ol").eval::<i64>().unwrap(),
            42,
            "shared 层应取回模块返回值"
        );
        assert_eq!(
            lua.load("return route_res.pv").eval::<i64>().unwrap(),
            43,
            "private 层应按现状语义加载（exec 定义全局）"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// T6 防投毒（§3.1/D11）：保留段 `algo.evil` 无 `_stdlib/` 文件 → "未同步"可行动
    /// 错误，**绝不回落**共享/私有诱饵（旧路由会命中 `devA/lib/algo/evil.lua`）；
    /// 共享单段 `viccom` 进不了共享层（走私有、无私有文件即报错）。
    #[test]
    fn require_reserved_never_falls_back_and_shared_single_segment_rejected() {
        let tmp = std::env::temp_dir().join(format!("lua-route6-neg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("devA/lib/algo")).unwrap();
        fs::create_dir_all(tmp.join("_lib/viccom")).unwrap();
        fs::create_dir_all(tmp.join("_lib/algo")).unwrap();
        fs::write(tmp.join("devA/main.lua"), "").unwrap();
        // 投毒诱饵：任一层回落到这些文件，全局 poison 即被置位
        fs::write(
            tmp.join("_lib/algo/evil.lua"),
            "poison = 'shared' return { poison = 'shared' }",
        )
        .unwrap();
        fs::write(tmp.join("devA/lib/algo/evil.lua"), "poison = 'private'").unwrap();
        fs::write(tmp.join("_lib/viccom/evil.lua"), "poison = 'viccom'").unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();

        // 防投毒核心：保留段缺官方文件 → "未同步"，不回落任何诱饵层
        let r1: mlua::Result<mlua::Value> = lua.load(r#"return require("algo.evil")"#).eval();
        let e1 = r1.err().expect("algo.evil 无 _stdlib 文件应报错");
        assert!(
            e1.to_string().contains("未同步"),
            "应给未同步可行动错误（不回落），实得：{e1}"
        );
        // D11：单段名进不了共享层——viccom 走私有层，无私有文件即报错
        let r2: mlua::Result<mlua::Value> = lua.load(r#"return require("viccom")"#).eval();
        let e2 = r2.err().expect("共享单段应报错（D11：第三方须 ≥2 段）");
        assert!(
            e2.to_string().contains("私有"),
            "错误应指向私有层（单段名走 scripts/<name>/lib/），实得：{e2}"
        );
        // 诱饵未被加载（防投毒证据）
        let clean: bool = lua.load("return poison == nil").eval().unwrap();
        assert!(clean, "任何诱饵文件都不应被执行");

        let _ = fs::remove_dir_all(&tmp);
    }

    /// T6 Builtin 分支接线：require("json") 分发到 api::builtin_module（T8 实现真身，
    /// 当前为占位错误——T8 落地后本测试随 json roundtrip 测试更新）。
    #[test]
    fn require_builtin_json_dispatches_to_api() {
        let tmp = std::env::temp_dir().join(format!("lua-route6-builtin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("devA/lib")).unwrap();
        fs::write(tmp.join("devA/main.lua"), "").unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        let r: mlua::Result<mlua::Value> = lua.load(r#"return require("json")"#).eval();
        let e = r.err().expect("json 未实现（T8）应报错");
        assert!(
            e.to_string().contains("json 未实现"),
            "Builtin 分支应分发到 api::builtin_module 占位错误，实得：{e}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// T7 种子兜底（§3.6 D5 离线冷启动）：无 `_stdlib/` 目录时 `require("algo.crc16")`
    /// 从 embed 种子取回模块且 `selftest()` 通过；种子也无的模块 → "未同步且不在种子集"。
    #[test]
    fn require_stdlib_falls_back_to_seed() {
        let tmp = std::env::temp_dir().join(format!("lua-seed-fb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("devA")).unwrap();
        fs::write(
            tmp.join("devA/main.lua"),
            r#"
local crc = require("algo.crc16")           -- 无 _stdlib/ → embed 种子兜底
seed_ok = crc.selftest()
local _, err = pcall(require, "algo.nope")   -- 种子也无 → 可行动错误
seed_miss_msg = tostring(err)
"#,
        )
        .unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        assert!(
            lua.load("return seed_ok").eval::<bool>().unwrap(),
            "种子版 algo.crc16 的 selftest 应通过（沙箱 VM 上 string.pack 可用）"
        );
        let msg: String = lua.load("return seed_miss_msg").eval().unwrap();
        assert!(
            msg.contains("未同步且不在种子集"),
            "种子也无应报可行动错误（永不回落其他层），实得：{msg}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// T7 优先级（§3.6）：`_stdlib/algo/crc16.lua` 存在时**文件优先**于种子——放置
    /// 不同返回值的假文件断言取到文件版（sync 镜像可覆盖种子旧版本）。
    #[test]
    fn require_stdlib_file_takes_priority_over_seed() {
        let tmp = std::env::temp_dir().join(format!("lua-seed-prio-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("devA")).unwrap();
        fs::create_dir_all(tmp.join("_stdlib/algo")).unwrap();
        fs::write(
            tmp.join("_stdlib/algo/crc16.lua"),
            "return { marker = 'file-version' }",
        )
        .unwrap();
        fs::write(
            tmp.join("devA/main.lua"),
            "file_won = require('algo.crc16').marker",
        )
        .unwrap();

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(tmp.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        assert_eq!(
            lua.load("return file_won").eval::<String>().unwrap(),
            "file-version",
            "_stdlib/ 文件应优先于 embed 种子"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
