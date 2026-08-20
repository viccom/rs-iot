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

mod json;

use crate::host::HostContext;
use crate::plugin::lua_core::io_engine::IoEngine;
use crate::types::{CtlMessage, InstConfig, InstId, Sample, SampleValue};
use mlua::{Lua, LuaSerdeExt, LuaString, Value};
use std::path::Path;
use tokio::sync::mpsc;

/// 运行时上报设备集（collect 与 L2 `set_value(s)` 双出口共用；flush 时同步 stat）。
///
/// 原定义在 `builtin/lua_client/mod.rs`，阶段 A 审查修复下沉 lua_core：api 层
/// `register`/`push_sample` 与薄壳主循环同为消费者，公共层反向依赖薄壳违背
/// §5.0 分层（B 阶段 luaTransform/luaNorth 复用 api 时会拖进 collector 壳）。
pub(crate) type RuntimeDevs = std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

/// mlua::Value → SampleValue（对标 transform/rhai.rs 的类型映射）。
///
/// 下沉缘由同 [`RuntimeDevs`]：collect 返回值解析（薄壳）与 L2 set_value 解析
/// （本层）共用的通用转换，属 lua_core 公共层。
pub(crate) fn lua_value_to_sample(v: &mlua::Value) -> Option<SampleValue> {
    match v {
        mlua::Value::Integer(i) => Some(SampleValue::Int(*i)),
        mlua::Value::Number(n) => Some(SampleValue::Float(*n)),
        mlua::Value::Boolean(b) => Some(SampleValue::Bool(*b)),
        mlua::Value::String(s) => Some(SampleValue::Str(s.to_str().ok()?.to_string())),
        _ => None,
    }
}

/// hub send 结果（`send_with_deadline` 调用方语义：丢样时计数、关闭时告警）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    /// 已入队（hub 正常消费中）。
    Sent,
    /// hub channel 已关（接收端 drop，实例停止/管道退出）。
    Closed,
    /// 背压超 2s 预算，样本被丢弃。
    Dropped,
}

/// 带 2s 背压超时的 hub send（Q3/附录 B 冻结面①：控制面 stop/reload/cmd 永不被
/// 数据面反压冻结）。满 channel 无消费者时 2s 后丢样返回 [`SendOutcome::Dropped`]，
/// 而非裸 `tx.send` 的无限期挂起；hub 关闭返回 [`SendOutcome::Closed`]。
///
/// collect 分支（`builtin/lua_client/mod.rs`）与 `push_sample`（set_value(s)）共用；
/// 丢样计数 `dropped` 由调用方持有并 fetch_add（flush 时落 stat `dropped_samples`）。
pub(crate) async fn send_with_deadline(tx: &mpsc::Sender<Sample>, sample: Sample) -> SendOutcome {
    match tokio::time::timeout(std::time::Duration::from_secs(2), tx.send(sample)).await {
        Ok(Ok(())) => SendOutcome::Sent,
        Ok(Err(_)) => SendOutcome::Closed,
        Err(_) => SendOutcome::Dropped,
    }
}

/// [`send_with_deadline`] 的非等待版（阶段 A 审查修复）：同一 tick 内首次 send 已
/// 超预算（hub 持续满）后，剩余样本不再逐条各等 2s——立即入队或丢弃，控制面
/// 不被数据面冻结（语义映射与 [`send_with_deadline`] 一致）。
pub(crate) fn try_send_outcome(tx: &mpsc::Sender<Sample>, sample: Sample) -> SendOutcome {
    match tx.try_send(sample) {
        Ok(()) => SendOutcome::Sent,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => SendOutcome::Dropped,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
    }
}

/// 组 `Sample` 推 hub（`set_value`/`set_values` 共用，DRY）。
///
/// `values` 空 → 直接返回（无点不上报）。send 经 [`send_with_deadline`]（2s 背压
/// 超时，hub 关 → warn，超时 → 丢样 + `dropped` 计数；对标 `run()` collect 推 hub
/// 容错语义）。timestamp 取当前本地秒。上报的 devid 记入 `runtime_devs`
/// （运行时设备集，collect 与 L2 上报共用出口）。
async fn push_sample(
    tx: &mpsc::Sender<Sample>,
    hwid: &crate::types::Hwid,
    runtime_devs: &RuntimeDevs,
    dropped: &std::sync::atomic::AtomicU64,
    devid: String,
    values: std::collections::BTreeMap<String, SampleValue>,
) {
    if values.is_empty() {
        return;
    }
    if let Ok(mut set) = runtime_devs.lock() {
        set.insert(devid.clone());
    }
    let sample = Sample {
        hwid: hwid.clone(),
        devid,
        values,
        timestamp: chrono::Local::now().timestamp(),
    };
    match send_with_deadline(tx, sample).await {
        SendOutcome::Sent => {}
        SendOutcome::Closed => tracing::warn!("[luaClient] set_value(s): hub 关闭，值未上报"),
        SendOutcome::Dropped => {
            dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                "[luaClient] set_value(s): hub 背压超 2s，丢样（累计 {}）",
                dropped.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
}

/// [`register`] 的组装上下文（阶段 A 审查遗留收敛：9 参 → struct；B 阶段 north
/// 能力包再加字段不再扩参）。字段与原 register 参数一一对应；`inst`/
/// `scripts_root` 借用调用方（零克隆），其余为廉价句柄。
pub(crate) struct ApiCtx<'a> {
    /// 宿主上下文：提供 lux（L2/L4 读写）+ `ctl_registry`（L3 路由）。
    pub(crate) ctx_host: HostContext,
    /// 实例配置（L2 `ctx:list_devices` 返 `inst.devices`；log 前缀取 `inst_id`）。
    pub(crate) inst: &'a InstConfig,
    /// IO 引擎（L1 `ctx:request` 内部 `io.request` → IO task 独占处理，spec §8 模式 Z）。
    pub(crate) io: IoEngine,
    /// hub 发送端（L2 `set_value` 推 [`Sample`]）。
    pub(crate) tx: mpsc::Sender<Sample>,
    /// 运行时上报设备集（collect 与 L2 `set_value(s)` 双出口共用，flush 时同步
    /// stat；详见 [`RuntimeDevs`]）。
    pub(crate) runtime_devs: RuntimeDevs,
    /// 丢样计数（`push_sample` 背压超时分支 fetch_add；与 collect 分支共享同一
    /// 实例，flush 时落 stat `dropped_samples`）。
    pub(crate) dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// 底层链路类型字符串（`cfg.transport.as_str()`，"tcp"/"serial"/"udp"），供
    /// `ctx:transport()` 暴露给脚本（脚本据此选帧格式：serial 加 CRC16、tcp 加
    /// MBAP 头）。
    pub(crate) transport: &'static str,
    /// 脚本根目录（全局 `stdlib` 表读 `scripts/_stdlib/.index.json` 合并种子，T9 §3.5）。
    pub(crate) scripts_root: &'a Path,
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
/// - L4 `ctx:secret_get(ref) -> str|nil`：async，读 lux 全局 `secrets` 表（平台凭证，
///   [`crate::db::secrets`]）。恒注入 + 实例配置 `app_config.secrets` 字符串数组显式
///   授权（Q4/R3）：越权引用报错（可感知，非静默 nil）；在清单但未写 → nil；读取
///   不落日志（审计点在管理面写入/删除）。
/// - L6 `log_info/warn/err/debug(msg)`：tracing，前缀 `[lua:<inst>]`。
/// - L6 `now()`/`time()`：当前本地毫秒/秒时间戳（f64）。
/// - 全局 `stdlib` 表（T9 §3.5）：`stdlib.list()/info(mod)` 只读清单——本地 index
///   与 embed 种子合并视图（[`ApiCtx::scripts_root`] 即为其数据源）。
///
/// 组装参数见 [`ApiCtx`]（9 参收敛 struct，阶段 B T3）。
pub(crate) fn register(lua: &Lua, ac: &ApiCtx<'_>) -> mlua::Result<()> {
    let ctx = lua.create_table()?;

    // --- L1 ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err ---
    // create_async_function：ctx:request 内部 coroutine.yield 桥接 IO task（spec §8）。
    // 首参 `_self` 接收 Lua 方法调用语法 `ctx:request(...)` 隐式传入的 self（ctx 表本身），
    // 不使用——若漏掉会导致 mlua 把 ctx 表当 frame 解析（bad argument #1: Lua table to string）。
    let io_req = ac.io.clone();
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
    let name = ac.inst.inst_id.as_str().to_string();
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
    let lux_set = ac.ctx_host.lux.clone();
    let inst_set = ac.inst.inst_id.as_str().to_string();
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

    let lux_get = ac.ctx_host.lux.clone();
    let inst_get = ac.inst.inst_id.as_str().to_string();
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

    // --- L4 ctx:secret_get(ref) -> str|nil（Q4/R3/D6：实例授权清单显式授权）---
    // lux 全局 secrets 表受控读取（[crate::db::secrets]，REST 管理面 T4 写入）。
    // 函数恒注入（Q4 定案：未配置 secrets 的实例也注册——未引用零成本）；授权 =
    // 实例配置 app_config.secrets 字符串数组显式列出的 ref。审计定案：读取不落日志
    // （高频回调里打日志会刷屏；Q4 审计点是管理面写入/删除，T4 routes_secret 已做）。
    // 清单在 register 期解析一次（类型不符按空清单降级 + warn 一次，非每次调用）。
    let secret_allowed: Vec<String> = match ac.inst.app_config.get("secrets") {
        None => Vec::new(), // 可选键：未配置 = 空清单（任何引用都未授权）
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(other) => {
            tracing::warn!(
                "[lua:{}] app_config.secrets 类型不符（期望字符串数组，实 {other}），按空清单处理",
                ac.inst.inst_id
            );
            Vec::new()
        }
    };
    let lux_secret = ac.ctx_host.lux.clone();
    let secret_fn = lua.create_async_function(move |lua, (_self, r): (Value, LuaString)| {
        let lux = lux_secret.clone();
        let allowed = secret_allowed.clone();
        async move {
            let r = r.to_string_lossy().to_string();
            // 前缀防线（脚本侧）：防读任意 lux key 的企图——与 validate_ref 同
            // 规则、先于授权检查（格式非法时提示加前缀，而非误导去改清单）。
            if let Err(e) = crate::db::secrets::validate_ref(&r) {
                return Err(mlua::Error::RuntimeError(format!("secret_get: {e}")));
            }
            // 授权清单（R3 越权可感知）：不在清单 → 报错而非静默 nil（脚本作者
            // 需要知道为什么读不到；ref 非机密可入消息，value 绝不出现）。
            if !allowed.contains(&r) {
                return Err(mlua::Error::RuntimeError(format!(
                    "secret 未授权：{r}（请检查实例配置 secrets 清单）"
                )));
            }
            // 在清单但 lux 未写 → nil（配置了但没写入，脚本可判空给默认）。
            match crate::db::secrets::get(&lux, &r).await {
                Ok(Some(v)) => Ok(Value::String(lua.create_string(&v)?)),
                Ok(None) => Ok(Value::Nil),
                Err(e) => Err(mlua::Error::RuntimeError(format!("secret_get: {e}"))),
            }
        }
    })?;
    ctx.set("secret_get", secret_fn)?;

    // --- L3 ctx:send_command(inst_id, devid, cmd, data) -> bool ---
    // 经 CtlRegistry::send 投到目标实例 cmd_rx（与 MQTT/REST 写入口同总线）。
    // send 是同步 try_send → create_function（非 async）；_self 吸收冒号隐式 self。
    let ctl = ac.ctx_host.ctl_registry.clone();
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
    let tx_set = ac.tx.clone();
    let hwid_set = ac.ctx_host.hwid.clone();
    let rt_set = ac.runtime_devs.clone();
    let drop_set = ac.dropped.clone();
    let set_value_fn = lua.create_async_function(
        move |_lua, (_self, devid, tag, v): (Value, LuaString, LuaString, Value)| {
            let tx = tx_set.clone();
            let hwid = hwid_set.clone();
            let rt = rt_set.clone();
            let dropped = drop_set.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                let sv = lua_value_to_sample(&v)
                    .ok_or_else(|| mlua::Error::RuntimeError("set_value: 不支持的值类型".into()))?;
                let mut values = std::collections::BTreeMap::new();
                values.insert(tag, sv);
                push_sample(&tx, &hwid, &rt, &dropped, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_value", set_value_fn)?;

    // --- L2 ctx:set_values(devid, {tag=v,...}) → 批量推一个设备多 tag（一个 Sample）---
    // 贴合 Sample 多变量语义：on_data 解析被动帧多点一次上报（点间同时刻 timestamp）。
    // table 经 pairs 遍历 → BTreeMap；空 table 不上报。
    let tx_sv = ac.tx.clone();
    let hwid_sv = ac.ctx_host.hwid.clone();
    let rt_sv = ac.runtime_devs.clone();
    let drop_sv = ac.dropped.clone();
    let set_values_fn = lua.create_async_function(
        move |_lua, (_self, devid, table): (Value, LuaString, mlua::Table)| {
            let tx = tx_sv.clone();
            let hwid = hwid_sv.clone();
            let rt = rt_sv.clone();
            let dropped = drop_sv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let mut values = std::collections::BTreeMap::new();
                for pair in table.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    if let Some(sv) = lua_value_to_sample(&v) {
                        values.insert(k, sv);
                    }
                }
                push_sample(&tx, &hwid, &rt, &dropped, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_values", set_values_fn)?;

    // --- L2 ctx:get_value(devid, tag) -> str|nil ---
    // 读 lux HGET rtdata:<devid> field=<devid>.<tag> → [ts, localTime, value] → value_str。
    // field 已含 devid 前缀（pipeline 写入格式）则原样用，否则补 <devid>.<tag>。
    let lux_gv = ac.ctx_host.lux.clone();
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
    let lux_gd = ac.ctx_host.lux.clone();
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
    let devices: Vec<(String, String)> = ac
        .inst
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
    let transport = ac.transport;
    let transport_fn = lua.create_function(move |_lua, _self: Value| Ok(transport))?;
    ctx.set("transport", transport_fn)?;

    // --- L5 ctx:http_get/http_post（feature lua-http）---
    // 实现移 http.rs（守 <500）；此处委托 http::register 注册到同一 ctx 表。
    #[cfg(feature = "lua-http")]
    http::register(lua, &ctx, ac.inst)?;

    // --- 全局 stdlib 表（T9 §3.5）：stdlib.list()/info(mod) 只读清单 ---
    // 本地 index + embed 种子合并视图（读盘即时，sync 后 reload 即见新版）。
    lua.globals().set(
        "stdlib",
        crate::plugin::lua_core::stdlib_sync::stdlib_table(lua, ac.scripts_root)?,
    )?;

    lua.globals().set("ctx", ctx)?;
    Ok(())
}

/// builtin 单段模块分发（§3.1 Builtin 层；`script.rs` require 路由的 Builtin 分支调用）。
///
/// 返回模块值（Lua table 等）。名单 = [`crate::plugin::lua_core::capability::BUILTIN_MODULES`]
///（route 已保证 `name` 在名单内）；json 实现在 [`json`]（serde_json 桥 + 1MB 输入上限）。
pub(crate) fn builtin_module(lua: &Lua, name: &str) -> mlua::Result<Value> {
    match name {
        "json" => json::make_module(lua).map(Value::Table),
        other => Err(mlua::Error::RuntimeError(format!(
            "builtin 模块 {other} 未注册（BUILTIN_MODULES 名单外）"
        ))),
    }
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
