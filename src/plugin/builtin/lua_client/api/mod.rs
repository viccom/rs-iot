//! ctx 对象 + host API 注册（spec §7）。L1/L2/L3/L4/L6 在此；L5 http 在 [`http`]；测试在 [`tests`]。
//!
//! Plan 8 Task 1 拆分自原 `api.rs`（守 <500 行，原 616 行）；Task 2 加 L2 数据 API：
//!
//! - `mod.rs`（本文件）：register 编排 + L1 request / L2 数据 / L3 send_command / L4 kv /
//!   L6 log·now·time + register_log + parse_rtdata_value helper。
//! - [`http`]：L5 http_get/post + headers_to_vec + http_request。
//! - [`tests`]：API 单测（含 L2 数据闭环）。

#[cfg(test)]
mod tests;

#[cfg(feature = "lua-http")]
mod http;

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::{CtlMessage, InstConfig, InstId, Sample, SampleValue};
use mlua::{Lua, LuaSerdeExt, LuaString, Value};
use tokio::sync::mpsc;

/// 组 `Sample` 推 hub（`set_value`/`set_values` 共用，DRY）。
///
/// `values` 空 → 直接返回（无点不上报）。`tx.send` 失败（hub 关）→ warn（对标
/// `run()` collect 推 hub 容错语义）。timestamp 取当前本地秒。
async fn push_sample(
    tx: &mpsc::Sender<Sample>,
    hwid: &crate::types::Hwid,
    devid: String,
    values: std::collections::BTreeMap<String, SampleValue>,
) {
    if values.is_empty() {
        return;
    }
    let sample = Sample {
        hwid: hwid.clone(),
        devid,
        values,
        timestamp: chrono::Local::now().timestamp(),
    };
    if tx.send(sample).await.is_err() {
        tracing::warn!("[luaClient] set_value(s): hub 关闭，值未上报");
    }
}

/// 在 `lua` 上注册 ctx 全局对象 + host 函数（`run` 调用）。
///
/// - L1 `ctx:request(frame, timeout_ms) -> resp|nil,err`：async，经 `create_async_function`
///   桥接，脚本 `ctx:request(...)` 调用时 mlua 自动驱动协程，内部 `io.request` → IO task
///   独占处理 → oneshot 回执（spec §8 模式 Z）。
/// - L2 `ctx:set_value(devid, tag, v)`：async，推 [`Sample`] 到 hub（数据上行总线，与
///   collect 同路径，pipeline 入库）；devid 由脚本显式传入（多设备支持）。
/// - L2 `ctx:set_values(devid, {tag=v,...})`：async，批量推一个设备多 tag（一个 [`Sample`]，
///   多点同时刻 timestamp，贴合 `on_data` 被动帧多点一次上报）。
/// - L2 `ctx:get_value(devid, tag) -> str|nil`：async，读 lux `rtdata:<devid>` 最新值
///   （`[ts, localTime, value]` cell → value 字符串）。
/// - L2 `ctx:get_device(devid) -> table|nil`：async，读 lux `HGETALL rtdata:<devid>`
///   → `{tag: value_str}`（设备当前全部数据点）。
/// - L2 `ctx:list_devices() -> table`：返 `inst.devices`（`{label, value}` 数组，同步）。
/// - L3 `ctx:send_command(inst, devid, cmd, data) -> bool`：经 [`crate::host::CtlRegistry`]
///   投递（`try_send` 非阻塞）。
/// - L4 `ctx:kv_set(key, v)` / `ctx:kv_get(key) -> v|nil`：async，落 lux `luakv:<inst>`
///   （[`crate::db::luakv`]），脚本本地持久状态——跨重启保留、实例间隔离。value 经
///   `serde_json::Value` 中转（Lua value <-> JSON 文本）。
/// - L6 `log_info/warn/err/debug(msg)`：tracing，前缀 `[lua:<inst>]`。
/// - L6 `now()`/`time()`：当前本地毫秒/秒时间戳（f64）。
///
/// `tx` 为 hub [`mpsc::Sender`]（L2 `set_value` 推 [`Sample`]）；`ctx_host` 提供 lux
/// （L2/L4 读写）+ `ctl_registry`（L3 路由）。`transport` 为底层链路类型字符串
///（`cfg.transport.as_str()`，"tcp"/"serial"/"udp"），供 `ctx:transport()` 暴露给脚本
///（脚本据此选帧格式：serial 加 CRC16、tcp 加 MBAP 头）。
pub(super) fn register(
    lua: &Lua,
    ctx_host: HostContext,
    inst: &InstConfig,
    io: IoEngine,
    tx: mpsc::Sender<Sample>,
    transport: &'static str,
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

    // --- L2 ctx:set_value(devid, tag, v) → 推 Sample 到 hub（数据上行总线）---
    // devid 由脚本显式传入（多设备支持：脚本可对 ctx:list_devices() 的任一 devid 上报）。
    // value 经 lua_value_to_sample 转 SampleValue；单点便捷，等价 set_values(devid,{[tag]=v})。
    let tx_set = tx.clone();
    let hwid_set = ctx_host.hwid.clone();
    let set_value_fn = lua.create_async_function(
        move |_lua, (_self, devid, tag, v): (Value, LuaString, LuaString, Value)| {
            let tx = tx_set.clone();
            let hwid = hwid_set.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                let sv = super::lua_value_to_sample(&v)
                    .ok_or_else(|| mlua::Error::RuntimeError("set_value: 不支持的值类型".into()))?;
                let mut values = std::collections::BTreeMap::new();
                values.insert(tag, sv);
                push_sample(&tx, &hwid, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_value", set_value_fn)?;

    // --- L2 ctx:set_values(devid, {tag=v,...}) → 批量推一个设备多 tag（一个 Sample）---
    // 贴合 Sample 多变量语义：on_data 解析被动帧多点一次上报（点间同时刻 timestamp）。
    // table 经 pairs 遍历 → BTreeMap；空 table 不上报。
    let tx_sv = tx.clone();
    let hwid_sv = ctx_host.hwid.clone();
    let set_values_fn = lua.create_async_function(
        move |_lua, (_self, devid, table): (Value, LuaString, mlua::Table)| {
            let tx = tx_sv.clone();
            let hwid = hwid_sv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let mut values = std::collections::BTreeMap::new();
                for pair in table.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    if let Some(sv) = super::lua_value_to_sample(&v) {
                        values.insert(k, sv);
                    }
                }
                push_sample(&tx, &hwid, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_values", set_values_fn)?;

    // --- L2 ctx:get_value(devid, tag) -> str|nil ---
    // 读 lux HGET rtdata:<devid> field=<devid>.<tag> → [ts, localTime, value] → value_str。
    // field 已含 devid 前缀（pipeline 写入格式）则原样用，否则补 <devid>.<tag>。
    let lux_gv = ctx_host.lux.clone();
    let get_value_fn = lua.create_async_function(
        move |lua, (_self, devid, tag): (Value, LuaString, LuaString)| {
            let lux = lux_gv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                let field = if tag.contains(&devid) {
                    tag
                } else {
                    format!("{devid}.{tag}")
                };
                match lux
                    .execute_value("HGET", &[&format!("rtdata:{devid}"), &field])
                    .await
                {
                    Ok(v) => {
                        let s = match v {
                            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(&b).to_string(),
                            lux::EmbeddedValue::Simple(s) => s,
                            _ => return Ok(Value::Nil),
                        };
                        match parse_rtdata_value(&s) {
                            Some(val) => Ok(Value::String(lua.create_string(&val)?)),
                            None => Ok(Value::Nil),
                        }
                    }
                    Err(_) => Ok(Value::Nil),
                }
            }
        },
    )?;
    ctx.set("get_value", get_value_fn)?;

    // --- L2 ctx:get_device(devid) -> table|nil ---
    // 读 lux HGETALL rtdata:<devid> → {tag: value_str}（设备当前全部数据点）。
    // field=<devid>.<tag> → 剥前缀得 tag；cell=[ts,localTime,value] → value_str。
    let lux_gd = ctx_host.lux.clone();
    let get_device_fn =
        lua.create_async_function(move |lua, (_self, devid): (Value, LuaString)| {
            let lux = lux_gd.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let key = format!("rtdata:{devid}");
                let prefix = format!("{devid}.");
                match lux.execute_value("HGETALL", &[&key]).await {
                    Ok(lux::EmbeddedValue::Array(arr)) => {
                        if arr.is_empty() {
                            // 缺省设备（lux 无 rtdata:<devid>）→ HGETALL 返空数组，返回 nil
                            // （与非 Array 分支一致语义：无数据点即设备不存在）。
                            return Ok(Value::Nil);
                        }
                        let t = lua.create_table()?;
                        let mut i = 0;
                        while i + 1 < arr.len() {
                            let field = match &arr[i] {
                                lux::EmbeddedValue::Bulk(b) => {
                                    String::from_utf8_lossy(b).into_owned()
                                }
                                lux::EmbeddedValue::Simple(s) => s.clone(),
                                _ => {
                                    i += 2;
                                    continue;
                                }
                            };
                            let raw = match &arr[i + 1] {
                                lux::EmbeddedValue::Bulk(b) => {
                                    String::from_utf8_lossy(b).into_owned()
                                }
                                lux::EmbeddedValue::Simple(s) => s.clone(),
                                _ => {
                                    i += 2;
                                    continue;
                                }
                            };
                            let tag = field.strip_prefix(&prefix).unwrap_or(&field);
                            if let Some(v) = parse_rtdata_value(&raw) {
                                t.set(tag, v)?;
                            }
                            i += 2;
                        }
                        Ok(Value::Table(t))
                    }
                    _ => Ok(Value::Nil),
                }
            }
        })?;
    ctx.set("get_device", get_device_fn)?;

    // --- L2 ctx:list_devices() -> table ---
    // 返 inst.devices（当前实例设备树，{label, value} 数组）。同步（inst.devices 已知，
    // 无 lux IO）。
    let devices: Vec<(String, String)> = inst
        .devices
        .iter()
        .map(|d| (d.label.clone(), d.value.clone()))
        .collect();
    let list_devices_fn = lua.create_function(move |lua, _self: Value| {
        let t = lua.create_table()?;
        for (i, (label, value)) in devices.iter().enumerate() {
            let dev = lua.create_table()?;
            dev.set("label", label.clone())?;
            dev.set("value", value.clone())?;
            t.set(i + 1, dev)?;
        }
        Ok(t)
    })?;
    ctx.set("list_devices", list_devices_fn)?;

    // --- L1+ ctx:transport() -> str：底层链路类型（tcp/serial/udp）---
    // 脚本据此选帧格式（serial → PDU+CRC16、tcp → MBAP 头、udp → 裸 RTU/自定义）。
    // 与 app_config.kind 一致；同步（配置解析产物，register 传入 &'static str）。
    // _self 吸收冒号语法隐式 self（ctx:transport()）。
    let transport_fn = lua.create_function(move |_lua, _self: Value| Ok(transport))?;
    ctx.set("transport", transport_fn)?;

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

/// 解析 rtdata cell `[ts, localTime, value]` JSON → value 字符串（index 2）。
///
/// rtdata::write 落盘格式：`serde_json::json!([timestamp, local_time, val_str])`，
/// value 始终是字符串（数字/布尔原样 `to_string`）。非 JSON 或缺 index 2 → `None`
/// （调用方返 nil，对标 mqtt_pub `parse_rtdata_hash` 的容错）。
fn parse_rtdata_value(cell: &str) -> Option<String> {
    let arr: serde_json::Value = serde_json::from_str(cell).ok()?;
    arr.get(2).and_then(|v| v.as_str()).map(String::from)
}
