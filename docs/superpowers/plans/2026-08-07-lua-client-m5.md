# Lua 脚本采集 — M5（HTTP 出站 + 全审计）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 实现本计划。步骤用 checkbox（`- [ ]`）跟踪。由 `/loop` 调度（CronCreate `116fd1e9`，每 10m 一轮）。

**Goal:** 为 luaClient 补齐 spec 的 M5 能力——`ctx:http_get` / `ctx:http_post`（L5），基于 reqwest（同步语法、底层 async，同 `ctx:request` 模式），feature `lua-http` 门控（默认随 `lua` 开），全审计日志（url/method/status/耗时），5s 默认超时，1MB 响应上限。

**Architecture:** 走 rs-iot 既有「受控出站」边界（spec §12 / §7 L5）：HTTP 经 reqwest（rustls-tls，不拉 OpenSSL），feature `lua-http` 独立可关（裁 release / 减风险面）。审计走 `tracing`（与 `[luaClient]` / `[modbusClient]` 约定一致），INFO 记成功、warn 记失败/超限。延续 ctx 全局 + 冒号 `_self` 首参 + `create_async_function`（async 桥接）约定。

**Tech Stack:** reqwest 0.12（rustls-tls + json，optional）、mlua 0.12 `create_async_function`、`std::time::Instant`（计时，非 Date）。

**执行顺序：** Task 1（Cargo feature）→ Task 2（api.rs http + 测试）。Task 1 引入 reqwest 依赖（首次编译 rustls 较慢，~3-5min，预期）。

---

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `Cargo.toml` | Modify | 加 `reqwest` optional + `lua-http` feature + `default` 加 `lua-http` |
| `src/plugin/builtin/lua_client/api.rs` | Modify | `#[cfg(feature="lua-http")]` 注册 `http_get`/`http_post` + `http_request`/`headers_to_vec` helper + 审计 + 测试 |

---

## Task 1: Cargo.toml lua-http feature + reqwest 依赖

**Files:**
- Modify: `Cargo.toml`

### - [ ] Step 1：加 reqwest 依赖 + lua-http feature

`Cargo.toml` 在 `mlua` 依赖行（约第 82 行）后加 reqwest：

```toml
# Lua 脚本 HTTP 出站（lua-http feature，默认开启）：reqwest rustls-tls，不拉 OpenSSL。
# json feature 供 http_post body 序列化（v1 直接传文本 body，json feature 备用）。
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"], optional = true }
```

`[features]` 段：`default` 加 `lua-http`，并新增 `lua-http` feature（约第 84-89 行）：

```toml
[features]
default = ["transform", "lua", "lua-http"]
# L2 数据转换（rhai 脚本，默认开启）
transform = ["dep:rhai"]
# Lua 动态脚本采集（luaClient，默认开启）
lua = ["dep:mlua"]
# Lua HTTP 出站（ctx:http_get/http_post，默认随 lua 开；独立可关以裁 release / 减出站风险面）
lua-http = ["lua", "dep:reqwest"]
```

### - [ ] Step 2：验证 reqwest 编译（默认 feature）

```bash
cargo build 2>&1 | tail -10
```
Expected: 编译成功（首次拉 reqwest + rustls 编译 ~3-5min，后续缓存）。`lua-http` 在 default，默认编进。

### - [ ] Step 3：验证关 lua-http 时 http 不编（feature 门控）

```bash
cargo build --no-default-features --features lua 2>&1 | tail -10
```
Expected: 编译成功（reqwest 不拉，api.rs http 段 `#[cfg(feature="lua-http")]` 跳过）。

### - [ ] Step 4：fmt + clippy + 提交

```bash
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock
git commit -m "feat(lua): lua-http feature + reqwest 依赖（M5 HTTP 出站门控）"
```

---

## Task 2: ctx:http_get/http_post + 全审计（api.rs）

**Files:**
- Modify: `src/plugin/builtin/lua_client/api.rs`

### - [ ] Step 1：写失败测（mock HTTP server + http_get）

在 `api.rs` 的 `#[cfg(test)] mod tests` 内加（复用既有 lux/inst setup 模式，仿 `kv_set_get_roundtrip`）：

```rust
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
            let _ = s.read(&mut buf).await; // 读请求行+头（不解析）
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
        let io = spawn_io_task(Box::new(MockIoTask {
            responder: Box::new(|_| Ok(vec![])),
        }));
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
            .load(format!(
                "return ctx:http_get('http://127.0.0.1:{port}/x')"
            ))
            .eval_async()
            .await
            .expect("http_get");
        assert_eq!(resp.get::<i64>("status").unwrap(), 200);
        assert_eq!(
            resp.get::<String>("body").unwrap(),
            "{\"ok\":true}"
        );

        // http_post 同 server（再起一个连接）验 status。
        let _ = crate::db::shutdown_lux(handle).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
```

### - [ ] Step 2：run，确认失败

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::api::tests::http_get_returns_status_and_body
```
Expected: FAIL（`ctx.http_get` 不存在 → Lua nil call）。

### - [ ] Step 3：实现 http_get/http_post + helper

`register` 内 `send_command` 段**之后**、`lua.globals().set("ctx", ctx)` **之前** 加（`#[cfg(feature = "lua-http")]` 门控）：

```rust
    // --- L5 ctx:http_get/http_post（feature lua-http，全审计 + 5s 默认超时 + 1MB 上限）---
    // reqwest async（同 ctx:request 模式）；_self 吸收冒号隐式 self；timeout 秒（默认 5）。
    #[cfg(feature = "lua-http")]
    {
        let inst_get = inst.inst_id.as_str().to_string();
        let http_get_fn = lua.create_async_function(
            move |lua, (_self, url, headers, timeout): (
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
            move |lua, (_self, url, body, headers, timeout): (
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
```

在 `register_log` 函数**之后**（`api.rs` 末尾、`#[cfg(test)] mod tests` 之前）加两个 helper（均 `#[cfg(feature = "lua-http")]`）：

```rust
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
```

### - [ ] Step 4：run 测试通过

```bash
CARGO_BUILD_JOBS=1 cargo test --lib plugin::builtin::lua_client::api
```
Expected: PASS（http_get_returns_status_and_body + 既有 register_exposes/kv/send_command 全绿）。

### - [ ] Step 5：fmt + clippy + 提交

```bash
cargo fmt --all --check && CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings
git add src/plugin/builtin/lua_client/api.rs
git commit -m "feat(lua): L5 ctx:http_get/http_post（reqwest + 全审计 + 1MB 上限）"
```

---

## Self-Review（写完后自检）

**1. Spec coverage（M5 = HTTP 出站 + 审计）：**
- ✅ `ctx:http_get(url, headers?, timeout?)` / `http_post(url, body, headers?, timeout?)` → `{status,body}`（spec §7 L5）→ Task 2
- ✅ reqwest 底层 async + create_async_function（spec §12 line 331）→ Task 2
- ✅ feature `lua-http` 默认随 lua 开、独立可关（spec §12 line 332 / §16）→ Task 1
- ✅ 全审计（INFO url/method/status/耗时；失败也记）（spec §12 line 333 / §7 L5 line 252）→ Task 2 http_request
- ✅ 默认超时 5s（spec §12 line 334）→ Task 2（`unwrap_or(5)`）
- ✅ 响应体上限 1MB（spec §12 line 334）→ Task 2（`bytes.len() > 1024*1024`）
- ✅ v1 不做白名单（spec §12 line 335）—— 不实现，仅审计追溯

**2. Placeholder scan：** 无 TBD/TODO；每步代码完整可编译；reqwest 首次编译慢已注明。

**3. Type consistency：** `http_request(inst, method, url, body?, headers, timeout_secs) -> (u16, String)` 全链一致；`headers_to_vec(Option<Table>) -> Vec<(String,String)>`（Send，跨 await）；`_self: Value` 首参约定（与 request/kv/send_command 一致）；返回 table `{status, body}`（get 測 `i64`/`String`）。

**已知简化（非占位，注明后续）：**
- reqwest `Client` per-call new（MVP；共享 Client 连池后置）
- body 上限 `bytes().await` 后 check（非流式 limit；超大响应理论 OOM 风险，靠审计 + lua-http 可关兜底；流式 limit 后置）
- `app_config.http_timeout_secs` 未接（Lua 传 timeout 覆盖；config 字段后置）

---

## Execution Handoff

本计划由 `/loop`（CronCreate `116fd1e9`）驱动，每轮按 **superpowers:subagent-driven-development** 执行下一个未勾选 task：fresh implementer + 两阶段 review（spec 合规 → 代码质量），TDD 红→绿→重构，每 task 一提交。CI 门槛（fmt + clippy -D warnings + test --all-targets --all-features，Windows CARGO_BUILD_JOBS=1）全绿才算 task 完成。API 503 间歇不稳时控制器接手（plan 含完整代码 + cargo 不依赖 LLM API）。Task 1+2 完成 → `finishing-a-development-branch` 合并 → 写 Plan 4（M6 热加载 + on_data）plan。

建分支：`feature/lua-m5`（从 master `136e9a9`）。
