//! ctx 对象 + host API 注册（spec §7）。Plan 1：L1 request（async）+ L6 log/now/time；
//! L2 get_value/set_value/get_device/list_devices/config/dump_comm/crc 后续 plan 扩。

use crate::host::HostContext;
use crate::plugin::builtin::lua_client::io_engine::IoEngine;
use crate::types::{CtlMessage, InstConfig, InstId};
use mlua::{Lua, LuaSerdeExt, LuaString, Value};

/// 在 `lua` 上注册 ctx 全局对象 + host 函数（Task 6 的 run 调用）。
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
/// `ctx_host` 自本 task（L4 kv）起用于 lux 读写；L2 get_value 等后续 plan 亦经此
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

    // --- L5 ctx:http_get/http_post（feature lua-http，全审计 + 5s 默认超时 + 1MB 上限）---
    // reqwest async（同 ctx:request 模式）；_self 吸收冒号隐式 self；timeout 秒（默认 5）。
    #[cfg(feature = "lua-http")]
    {
        let inst_get = inst.inst_id.as_str().to_string();
        let http_get_fn = lua.create_async_function(
            move |lua,
                  (_self, url, headers, timeout): (
                Value,
                LuaString,
                Option<mlua::Table>,
                Option<u64>,
            )| {
                let inst = inst_get.clone();
                async move {
                    let url = url.to_string_lossy().to_string();
                    let timeout_secs = timeout.unwrap_or(5).max(1);
                    let hdrs = headers_to_vec(headers)?;
                    let (status, body) =
                        http_request(&inst, "GET", &url, None, hdrs, timeout_secs).await?;
                    let t = lua.create_table()?;
                    t.set("status", status)?;
                    t.set("body", body)?;
                    Ok(t)
                }
            },
        )?;
        ctx.set("http_get", http_get_fn)?;

        let inst_post = inst.inst_id.as_str().to_string();
        let http_post_fn = lua.create_async_function(
            move |lua,
                  (_self, url, body, headers, timeout): (
                Value,
                LuaString,
                LuaString,
                Option<mlua::Table>,
                Option<u64>,
            )| {
                let inst = inst_post.clone();
                async move {
                    let url = url.to_string_lossy().to_string();
                    let body = body.to_string_lossy().to_string();
                    let timeout_secs = timeout.unwrap_or(5).max(1);
                    let hdrs = headers_to_vec(headers)?;
                    let (status, resp_body) =
                        http_request(&inst, "POST", &url, Some(body), hdrs, timeout_secs).await?;
                    let t = lua.create_table()?;
                    t.set("status", status)?;
                    t.set("body", resp_body)?;
                    Ok(t)
                }
            },
        )?;
        ctx.set("http_post", http_post_fn)?;
    }

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

/// Lua headers table → `Vec<(name,value)>`（`Send`，跨 await 安全）。
///
/// mlua `Table` 跨 await 持有不一定 `Send`，故在闭包内（await 前）同步转 `Vec`。
#[cfg(feature = "lua-http")]
fn headers_to_vec(headers: Option<mlua::Table>) -> mlua::Result<Vec<(String, String)>> {
    let mut v = Vec::new();
    if let Some(h) = headers {
        for pair in h.pairs::<String, LuaString>() {
            let (k, val) = pair?;
            v.push((k, val.to_string_lossy().to_string()));
        }
    }
    Ok(v)
}

/// 发一次 HTTP 请求，返回 `(status, body)`。全审计（INFO 成功 / warn 失败或超限），
/// 响应体上限 1MB（防 OOM；流式 limit 为后续优化）。超时由 `timeout_secs` 控制。
#[cfg(feature = "lua-http")]
async fn http_request(
    inst: &str,
    method: &str,
    url: &str,
    body: Option<String>,
    headers: Vec<(String, String)>,
    timeout_secs: u64,
) -> mlua::Result<(u16, String)> {
    let start = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| mlua::Error::RuntimeError(format!("http client: {e}")))?;
    let mut req = match method {
        "GET" => client.get(url),
        "POST" => client.post(url).body(body.unwrap_or_default()),
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "http: 不支持的方法 {other}"
            )))
        }
    };
    for (k, v) in &headers {
        req = req.header(k, v.as_str());
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| mlua::Error::RuntimeError(format!("http body: {e}")))?;
            if bytes.len() > 1024 * 1024 {
                let ms = start.elapsed().as_millis();
                tracing::warn!(
                    "[lua:{inst}] HTTP {method} {url} -> body 过大 ({}B > 1MB, {ms}ms)",
                    bytes.len()
                );
                return Err(mlua::Error::RuntimeError("http: 响应体超 1MB".into()));
            }
            let ms = start.elapsed().as_millis();
            tracing::info!("[lua:{inst}] HTTP {method} {url} -> {status} ({ms}ms)");
            Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
        }
        Err(e) => {
            let ms = start.elapsed().as_millis();
            tracing::warn!("[lua:{inst}] HTTP {method} {url} -> FAIL {e} ({ms}ms)");
            Err(mlua::Error::RuntimeError(format!("http {method}: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
    use crate::plugin::builtin::lua_client::io_engine::{spawn_io_task, IoEngine, MockIoTask};
    use crate::types::{AppType, DeviceSpec, HostKind, InstConfig, PluginName};
    use tokio_util::sync::CancellationToken;

    /// 测试用：spawn_io_task 配丢弃被动帧的 on_data channel（api 单测不测 on_data）。
    fn spawn_mock_io() -> IoEngine {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        spawn_io_task(
            Box::new(MockIoTask {
                responder: Box::new(|_| Ok(vec![])),
            }),
            tx,
        )
    }

    /// 冒烟测：register 成功 + ctx 暴露 request/log_info/now/time（且都是 function）。
    #[tokio::test]
    async fn register_exposes_ctx_api() {
        let lua = Lua::new();
        let io = spawn_mock_io();
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

    /// kv_set 后 kv_get 命中；类型 round-trip（number/string/bool）；缺省返回 nil。
    #[tokio::test]
    async fn kv_set_get_roundtrip() {
        let lua = mlua::Lua::new();
        let io = spawn_mock_io();
        let data_dir = std::env::temp_dir()
            .join(format!("lua-kv-{}", std::process::id()))
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
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
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

        // kv_set/kv_get 是 async function（底层落 lux），用 exec_async/eval_async 驱动。
        lua.load("ctx:kv_set('counter', 42)")
            .exec_async()
            .await
            .expect("kv_set");
        lua.load("ctx:kv_set('name', 'alpha')")
            .exec_async()
            .await
            .expect("kv_set str");
        let n: i64 = lua
            .load("return ctx:kv_get('counter')")
            .eval_async()
            .await
            .expect("kv_get num");
        assert_eq!(n, 42);
        let s: String = lua
            .load("return ctx:kv_get('name')")
            .eval_async()
            .await
            .expect("kv_get str");
        assert_eq!(s, "alpha");
        let nil: mlua::Value = lua
            .load("return ctx:kv_get('missing')")
            .eval_async()
            .await
            .expect("kv_get nil");
        assert_eq!(nil, mlua::Value::Nil, "缺省 key 返回 nil");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// send_command 经 CtlRegistry 投到目标实例；未注册实例返回 false。
    #[tokio::test]
    async fn send_command_routes_to_target() {
        let ctl = CtlRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::types::CtlMessage>(8);
        ctl.register("target", tx);

        let lua = mlua::Lua::new();
        let io = spawn_mock_io();
        let data_dir = std::env::temp_dir()
            .join(format!("lua-send-{}", std::process::id()))
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
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        let ctx_host = HostContext {
            lux: handle.client(),
            hwid: crate::types::Hwid::new("it"),
            shutdown: CancellationToken::new(),
            ctl_registry: ctl.clone(), // 共享：lua send_command → target cmd_rx
            bridge_registry: BridgeRegistry::new(),
        };
        let inst = InstConfig {
            inst_id: "src".into(),
            inst_name: "src".into(),
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

        // 命中目标实例。
        let ok: bool = lua
            .load("return ctx:send_command('target', 'dev1', 'write', '{\"v\":1}')")
            .eval_async()
            .await
            .expect("send_command eval");
        assert!(ok, "目标实例已注册应返回 true");
        let m = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("超时")
            .expect("应收到 CtlMessage");
        assert_eq!(m.instance.as_str(), "target");
        assert_eq!(m.devid, "dev1");
        assert_eq!(m.cmd, "write");
        assert_eq!(m.data, "{\"v\":1}");

        // 未注册实例 → false。
        let miss: bool = lua
            .load("return ctx:send_command('nope', 'd', 'write', '{}')")
            .eval_async()
            .await
            .expect("eval");
        assert!(!miss, "未注册实例应返回 false");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// http_get 返回 {status, body}；经 mock HTTP server 验证。
    #[cfg(feature = "lua-http")]
    #[tokio::test]
    async fn http_get_returns_status_and_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        // mock HTTP server：读请求后回 200 + JSON body。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = s.read(&mut buf).await;
            let body = r#"{"ok":true}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes()).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let lua = mlua::Lua::new();
        let io = spawn_mock_io();
        let data_dir = std::env::temp_dir()
            .join(format!("lua-http-{}", std::process::id()))
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
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
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

        let resp: mlua::Table = lua
            .load(format!("return ctx:http_get('http://127.0.0.1:{port}/x')"))
            .eval_async()
            .await
            .expect("http_get");
        assert_eq!(resp.get::<i64>("status").unwrap(), 200);
        assert_eq!(resp.get::<String>("body").unwrap(), "{\"ok\":true}");

        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
