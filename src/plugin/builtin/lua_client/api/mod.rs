//! ctx 对象 + host API 注册（spec §7）。L1/L3/L4/L6 在此；L5 http 在 [`http`]；测试在 [`tests`]。
//!
//! Plan 8 Task 1 拆分自原 `api.rs`（守 <500 行，原 616 行）：
//!
//! - `mod.rs`（本文件）：register 编排 + L1 request / L3 send_command / L4 kv / L6 log·now·time + register_log helper。
//! - [`http`]：L5 http_get/post + headers_to_vec + http_request。
//! - [`tests`]：4 个 API 单测。
//!
//! L2 数据 API（get_value/set_value/get_device/list_devices）由 Plan 8 Task 2 加到本文件。

#[cfg(test)]
mod tests;

#[cfg(feature = "lua-http")]
mod http;

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::{CtlMessage, InstConfig, InstId};
use mlua::{Lua, LuaSerdeExt, LuaString, Value};

/// 在 `lua` 上注册 ctx 全局对象 + host 函数（`run` 调用）。
///
/// - L1 `ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err`：async，经
///   `create_async_function` 桥接，脚本 `ctx:request(...)` 调用时 mlua 自动驱动协程，
///   内部 `io.request` → IO task 独占处理 → oneshot 回执（spec §8 模式 Z）。
/// - L4 `ctx:kv_set(key, v)` / `ctx:kv_get(key) -> v|nil`：async，落 lux Hash
///   `luakv:<inst>`（`db::luakv`），脚本本地持久状态——跨重启保留、实例间隔离。
///   value 经 `serde_json::Value` 中转（Lua value <-> JSON 文本）。
/// - L6 `log_info/warn/err/debug(msg)`：tracing，前缀 `[lua:<inst>]`。
/// - L6 `now()`/`time()`：当前本地毫秒/秒时间戳（f64）。
///
/// `ctx_host` 自 L4 kv 起用于 lux 读写；L2 get_value 等后续 plan 亦经此
/// 读设备最新值/实例配置。
pub(super) fn register(
    lua: &Lua,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: IoEngine,
) -> mlua::Result<()> {
    let ctx = lua.create_table()?;

    // --- L1 ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err ---
    // create_async_function：ctx:request 内部 coroutine.yield 桥接 IO task（spec §8）。
    // 首参 `_self` 接收 Lua 方法调用语法 `ctx:request(...)` 隐式传入的 self（ctx 表本身），
    // 不使用——若漏掉会导致 mlua 把 ctx 表当 frame 解析（bad argument #1: Lua table to string）。
    let io_req = io.clone();
    let request_fn = lua.create_async_function(
        move |lua, (_self, frame, timeout_ms): (Value, LuaString, u64)| {
            let io = io_req.clone();
            async move {
                let bytes = frame.as_bytes().to_vec();
                match io.request(&bytes, timeout_ms).await {
                    Ok(resp) => {
                        Ok::<_, mlua::Error>((Value::String(lua.create_string(&resp)?), Value::Nil))
                    }
                    Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?))),
                }
            }
        },
    )?;
    ctx.set("request", request_fn)?;

    // --- L6 log_info / log_warn / log_err / log_debug ---
    let name = inst.inst_id.as_str().to_string();
    register_log(lua, &ctx, &name)?;

    // --- L6 now() / time() ---
    ctx.set(
        "now",
        lua.create_function(|_lua, _self: Value| {
            Ok(chrono::Local::now().timestamp_millis() as f64)
        })?,
    )?;
    ctx.set(
        "time",
        lua.create_function(|_lua, _self: Value| Ok(chrono::Local::now().timestamp() as f64))?,
    )?;

    // --- L4 ctx:kv_set(key, v) / ctx:kv_get(key) -> v|nil ---
    // value 经 serde_json::Value 中转（Lua value <-> JSON 文本），落 lux luakv:<inst>。
    // async（lux 操作 await）→ create_async_function；_self 吸收冒号隐式 self。
    let lux_set = ctx_host.lux.clone();
    let inst_set = inst.inst_id.as_str().to_string();
    let kv_set_fn =
        lua.create_async_function(move |lua, (_self, key, v): (Value, LuaString, Value)| {
            let lux = lux_set.clone();
            let inst = inst_set.clone();
            async move {
                let key = key.to_string_lossy().to_string();
                let json: serde_json::Value = lua.from_value(v)?;
                let s = serde_json::to_string(&json)
                    .map_err(|e| mlua::Error::RuntimeError(format!("kv_set 序列化: {e}")))?;
                crate::db::luakv::set(&lux, &inst, &key, &s)
                    .await
                    .map_err(|e| mlua::Error::RuntimeError(format!("kv_set: {e}")))?;
                Ok(())
            }
        })?;
    ctx.set("kv_set", kv_set_fn)?;

    let lux_get = ctx_host.lux.clone();
    let inst_get = inst.inst_id.as_str().to_string();
    let kv_get_fn = lua.create_async_function(move |lua, (_self, key): (Value, LuaString)| {
        let lux = lux_get.clone();
        let inst = inst_get.clone();
        async move {
            let key = key.to_string_lossy().to_string();
            match crate::db::luakv::get(&lux, &inst, &key).await {
                Ok(Some(s)) => {
                    let json: serde_json::Value = serde_json::from_str(&s)
                        .map_err(|e| mlua::Error::RuntimeError(format!("kv_get 反序列化: {e}")))?;
                    let v: Value = lua.to_value(&json)?;
                    Ok(v)
                }
                Ok(None) => Ok(Value::Nil),
                Err(e) => Err(mlua::Error::RuntimeError(format!("kv_get: {e}"))),
            }
        }
    })?;
    ctx.set("kv_get", kv_get_fn)?;

    // --- L3 ctx:send_command(inst_id, devid, cmd, data) -> bool ---
    // 经 CtlRegistry::send 投到目标实例 cmd_rx（与 MQTT/REST 写入口同总线）。
    // send 是同步 try_send → create_function（非 async）；_self 吸收冒号隐式 self。
    let ctl = ctx_host.ctl_registry.clone();
    let send_fn = lua.create_function(
        move |_lua,
              (_self, inst_id, devid, cmd, data): (
            Value,
            LuaString,
            LuaString,
            LuaString,
            LuaString,
        )| {
            let msg = CtlMessage {
                instance: InstId::from(inst_id.to_string_lossy().to_string()),
                devid: devid.to_string_lossy().to_string(),
                cmd: cmd.to_string_lossy().to_string(),
                data: data.to_string_lossy().to_string(),
            };
            let inst_str = msg.instance.to_string();
            Ok(ctl.send(&inst_str, msg))
        },
    )?;
    ctx.set("send_command", send_fn)?;

    // --- L5 ctx:http_get/http_post（feature lua-http）---
    // 实现移 http.rs（守 <500）；此处委托 http::register 注册到同一 ctx 表。
    #[cfg(feature = "lua-http")]
    http::register(lua, &ctx, inst)?;

    lua.globals().set("ctx", ctx)?;
    Ok(())
}

/// 注册 log_info/warn/err/debug 到 ctx 表（tracing，前缀 [lua:<name>]）。
///
/// `tracing::Level` 是 struct + 关联常量（非 enum），不可 `as u8`/`match` 分发；
/// 闭包捕获 `Level`（`Copy`）后用 `==` 比较显式调对应宏（info!/warn!/error!/debug!
/// 是不同宏，无法按 level 一阶化）。
fn register_log(lua: &Lua, ctx: &mlua::Table, name: &str) -> mlua::Result<()> {
    for (key, level) in [
        ("log_info", tracing::Level::INFO),
        ("log_warn", tracing::Level::WARN),
        ("log_err", tracing::Level::ERROR),
        ("log_debug", tracing::Level::DEBUG),
    ] {
        let n = name.to_string();
        let f = lua.create_function(move |_lua, (_self, msg): (Value, LuaString)| {
            let msg = msg.to_string_lossy().to_string();
            if level == tracing::Level::INFO {
                tracing::info!("[lua:{n}] {msg}");
            } else if level == tracing::Level::WARN {
                tracing::warn!("[lua:{n}] {msg}");
            } else if level == tracing::Level::ERROR {
                tracing::error!("[lua:{n}] {msg}");
            } else {
                tracing::debug!("[lua:{n}] {msg}");
            }
            Ok(())
        })?;
        ctx.set(key, f)?;
    }
    Ok(())
}
