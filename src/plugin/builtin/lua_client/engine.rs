//! Lua VM 创建：库裁剪 + 指令 hook + 内存限额（spec §9 三层护栏）。

use crate::plugin::builtin::lua_client::config::LuaCfg;
use mlua::{Error, HookTriggers, Lua, LuaOptions, StdLib};

/// 创建一个沙箱化的 Lua 5.4 VM。
///
/// - 库裁剪：仅 TABLE|STRING|MATH|COROUTINE（剔除 io/os/debug/package；COROUTINE 必留，async 桥接依赖）。
/// - 指令 hook：每 `hook_instr_limit` 条指令触发，回调返回 Err 中断死循环。
/// - 内存限额：超 `mem_limit_mb` 触发 MemoryError。
/// - catch_rust_panics(true)：Rust callback panic 转 RuntimeError。
pub(super) fn new_vm(cfg: &LuaCfg) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::COROUTINE,
        LuaOptions::new().catch_rust_panics(true),
    )?;

    // 指令 hook：到点抛 RuntimeError 中断（VM 不死，下次 call_async 重新计数）。
    let limit = cfg.hook_instr_limit;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(limit),
        move |_lua, _dbg| {
            Err(Error::RuntimeError(format!(
                "instruction limit ({limit}) exceeded — 疑似死循环"
            )))
        },
    )?;

    // 内存限额（字节）。
    lua.set_memory_limit(cfg.mem_limit_mb * 1024 * 1024)?;

    Ok(lua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::builtin::lua_client::config::parse_cfg;
    use serde_json::json;

    fn cfg() -> LuaCfg {
        parse_cfg(&json!({"script":"t","endpoint":"x"})).unwrap()
    }

    #[test]
    fn sandbox_blocks_dangerous_libs() {
        let lua = new_vm(&cfg()).unwrap();
        let (ok, _): (bool, mlua::Value) = lua
            .load("pcall(function() return io.read end)")
            .eval()
            .unwrap();
        assert!(!ok, "io 库应被沙箱禁用");
        let (ok, _): (bool, mlua::Value) = lua
            .load("pcall(function() return os.execute end)")
            .eval()
            .unwrap();
        assert!(!ok, "os 库应被沙箱禁用");
    }

    #[test]
    fn hook_kills_infinite_loop() {
        let mut c = cfg();
        c.hook_instr_limit = 10_000;
        let lua = new_vm(&c).unwrap();
        let r: mlua::Result<()> = lua.load("while true do end").exec();
        assert!(r.is_err(), "死循环应被 hook 中断");
        let v: i64 = lua.load("return 1 + 1").eval().unwrap();
        assert_eq!(v, 2);
    }

    #[test]
    fn memory_limit_enforced() {
        let mut c = cfg();
        c.mem_limit_mb = 1;
        let lua = new_vm(&c).unwrap();
        let r: mlua::Result<String> = lua
            .load("local s='x'; for i=1,1000000 do s=s..string.rep('x',100) end; return s")
            .eval();
        assert!(r.is_err(), "爆内存应触发 MemoryError");
    }

    #[test]
    fn coroutine_available() {
        let lua = new_vm(&cfg()).unwrap();
        let v: i64 = lua
            .load("local function f() coroutine.yield() end; return 42")
            .eval()
            .unwrap();
        assert_eq!(v, 42, "COROUTINE 库必须可用（async 桥接依赖）");
    }
}
