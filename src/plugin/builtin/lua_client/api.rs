//! ctx 对象 + host API 注册（spec §7）。Plan 1：L1 request（async）+ L6 log/now/time；
//! L2 get_value/set_value/get_device/list_devices/config/dump_comm/crc 后续 plan 扩。

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::InstConfig;
use mlua::{Lua, LuaString, Value};

/// 在 `lua` 上注册 ctx 全局对象 + host 函数（Task 6 的 run 调用）。
///
/// - L1 `ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err`：async，经
///   `create_async_function` 桥接，脚本 `ctx:request(...)` 调用时 mlua 自动驱动协程，
///   内部 `io.request` → IO task 独占处理 → oneshot 回执（spec §8 模式 Z）。
/// - L6 `log_info/warn/err/debug(msg)`：tracing，前缀 `[lua:<inst>]`。
/// - L6 `now()`/`time()`：当前本地毫秒/秒时间戳（f64）。
///
/// `_ctx_host` Plan 1 未用（L2 get_value 等后续 plan 经 lux 读取设备最新值/实例配置
/// 时使用），保留参数以稳定 Task 6 调用签名。
pub(super) fn register(
    lua: &Lua,
    _ctx_host: HostContext, // Plan 1 未用（L2 get_value 等后续 plan 用），保留参数
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
    use crate::plugin::builtin::lua_client::io_engine::{spawn_io_task, MockIoTask};
    use crate::types::{AppType, DeviceSpec, HostKind, InstConfig, PluginName};
    use tokio_util::sync::CancellationToken;

    /// 冒烟测：register 成功 + ctx 暴露 request/log_info/now/time（且都是 function）。
    #[tokio::test]
    async fn register_exposes_ctx_api() {
        let lua = Lua::new();
        let io = spawn_io_task(Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])),
        }));
        // 起一个测试 lux 拿 EmbeddedClient（HostContext 需要；端口 0 由 OS 分配，独立
        // 临时 data_dir 防并行冲突——同 tests/common 模式）。
        let data_dir = std::env::temp_dir()
            .join(format!("lua-api-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let lux_cfg = crate::db::build_lux_config(&cfg);
        let handle = lux::run_with_config(lux_cfg).await.expect("lux");
        let ctx_host = HostContext {
            lux: handle.client(),
            hwid: crate::types::Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: CtlRegistry::new(),
            bridge_registry: BridgeRegistry::new(),
        };
        let inst = InstConfig {
            inst_id: "it".into(),
            inst_name: "it".into(),
            app_code: PluginName::new("luaClient"),
            app_type: AppType::South,
            auto_start: false,
            app_config: serde_json::json!({"script":"a","endpoint":"x"}),
            devices: vec![DeviceSpec {
                label: "d".into(),
                value: "d".into(),
                children: vec![],
            }],
            host_kind: HostKind::Builtin,
        };
        register(&lua, ctx_host, &inst, io).expect("register");
        for api in [
            "request",
            "log_info",
            "log_warn",
            "log_err",
            "log_debug",
            "now",
            "time",
        ] {
            let src = format!("return type(ctx.{api}) == 'function'");
            let is_fn: bool = lua.load(&src).eval().expect(api);
            assert!(is_fn, "ctx.{api} 应是 function");
        }
        // 冒号语法冒烟：ctx:now() / ctx:log_info(...) 应工作（_self 首参吸收冒号隐式 self）。
        let now: f64 = lua
            .load("return ctx:now()")
            .eval()
            .expect("ctx:now 冒号调用");
        assert!(now > 0.0);
        let _: () = lua
            .load("ctx:log_info('hello')")
            .exec()
            .expect("ctx:log_info 冒号调用");

        // 优雅关闭 lux（先 SAVE 再 shutdown），并清理临时 data_dir。
        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
