//! L5 HTTP 出站 API（feature `lua-http`，全审计 + 5s 默认超时 + 1MB 上限）。spec §7/§12。
//!
//! Plan 8 Task 1 拆分自原 `api.rs`：`http_get`/`http_post` 注册 + `headers_to_vec`/
//! `http_request` helper。reqwest async（同 ctx:request 模式）；_self 吸收冒号隐式 self。
//!
//! 本模块整体由 `mod.rs` 的 `#[cfg(feature = "lua-http")] mod http;` 门控——feature 关闭时
//! 不参与编译，故内部项的 `#[cfg(feature = "lua-http")]` 为冗余双保险（保留与原 api.rs 一致）。

use crate::types::InstConfig;
use mlua::{Lua, LuaString, Value};

/// 注册 L5 `ctx:http_get` / `ctx:http_post` 到 `ctx` 表（feature lua-http）。
///
/// - `http_get(url, headers?, timeout?) -> {status, body}`：GET 请求。
/// - `http_post(url, body, headers?, timeout?) -> {status, body}`：POST 请求。
///
/// 全审计（INFO 成功 / warn 失败或超限），响应体上限 1MB，超时默认 5s。
#[cfg(feature = "lua-http")]
pub(super) fn register(lua: &Lua, ctx: &mlua::Table, inst: &InstConfig) -> mlua::Result<()> {
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
