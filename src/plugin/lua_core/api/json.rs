//! builtin json 模块（C0/R0，设计 §4.1/D5）：serde_json 桥 + 输入上限。
//!
//! `require("json")` 经 script.rs Builtin 分发 → [`super::builtin_module`] → 本模块
//! `make_module`。D5 分工判据：json 需要 Rust 依赖树（serde_json）→ 宿主注册（C0），
//! 纯算法（crc 等）才落 stdlib。
//!
//! - `encode(v) -> str`：Lua value → serde_json::Value → String。Lua 表键序不保证，
//!   消费方按 JSON 语义取值（不锁序列化字符串字面）。
//! - `decode(s) -> v`：≤[`MAX_BYTES`] 字节 → serde_json::Value → Lua value。超限
//!   先拒后解析（§4.5 输入验证规范：防超大输入内存放大）。
//!
//! 错误统一 RuntimeError 带上下文（"json encode: …" / "json: 输入超 1MB" /
//! "json 解析失败: …"），脚本可感知退避。

use mlua::{Lua, LuaSerdeExt, LuaString, Table};

/// decode 输入上限（字节，1MB；§4.1 json 卡 R0：纯函数仅输入大小上限）。
const MAX_BYTES: usize = 1024 * 1024;

/// 构造 json 模块表（encode/decode）。
pub(crate) fn make_module(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    let encode = lua.create_function(|lua, v: mlua::Value| {
        let j: serde_json::Value = lua
            .from_value(v)
            .map_err(|e| mlua::Error::RuntimeError(format!("json encode: {e}")))?;
        serde_json::to_string(&j)
            .map_err(|e| mlua::Error::RuntimeError(format!("json encode: {e}")))
    })?;
    t.set("encode", encode)?;

    let decode = lua.create_function(|lua, s: LuaString| {
        // mlua 0.12 `as_bytes` 返回 Vec<u8>（拥有所有权）；先查长度再解析（≤1MB）。
        let b = s.as_bytes();
        if b.len() > MAX_BYTES {
            return Err(mlua::Error::RuntimeError("json: 输入超 1MB".into()));
        }
        let j: serde_json::Value = serde_json::from_slice(&b)
            .map_err(|e| mlua::Error::RuntimeError(format!("json 解析失败: {e}")))?;
        lua.to_value(&j)
    })?;
    t.set("decode", decode)?;

    Ok(t)
}
