//! L5 HTTP 出站 API（feature `lua-http`，全审计 + 5s 默认超时 + 1MB 上限 + R2 频控）。spec §7/§12。
//!
//! Plan 8 Task 1 拆分自原 `api.rs`：`http_get`/`http_post` 注册 + `headers_to_vec`/
//! `http_request` helper。reqwest async（同 ctx:request 模式）；_self 吸收冒号隐式 self。
//!
//! T5 频控（Q3/§4.5-2）：`http_get`/`http_post` 共享一个每实例出站预算（限值查能力卡
//! `rate_per_sec`，两卡均 20/s），超限向脚本返回 `rate limited（20/s）` 错误（可感知退避）。
//!
//! 本模块整体由 `mod.rs` 的 `#[cfg(feature = "lua-http")] mod http;` 门控——feature 关闭时
//! 不参与编译，故内部项的 `#[cfg(feature = "lua-http")]` 为冗余双保险（保留与原 api.rs 一致）。

use crate::plugin::lua_core::ratelimit::RateLimiter;
use crate::types::InstConfig;
use mlua::{Lua, LuaString, Value};
use std::sync::Arc;

/// 注册 L5 `ctx:http_get` / `ctx:http_post` 到 `ctx` 表（feature lua-http）。
///
/// - `http_get(url, headers?, timeout?) -> {status, body}`：GET 请求。
/// - `http_post(url, body, headers?, timeout?) -> {status, body}`：POST 请求。
///
/// 全审计（INFO 成功 / warn 失败或超限），响应体上限 1MB，超时默认 5s。
/// R2 频控（Q3/§4.5-2/T5）：两方法共享一个每实例出站限流器（限值查能力卡 20/s），
/// 超限在发请求前即返回 `rate limited（20/s）` 错误。
#[cfg(feature = "lua-http")]
pub(super) fn register(lua: &Lua, ctx: &mlua::Table, inst: &InstConfig) -> mlua::Result<()> {
    let rl = Arc::new(RateLimiter::new(http_rate_per_sec()));
    let inst_get = inst.inst_id.as_str().to_string();
    let rl_get = rl.clone();
    let http_get_fn = lua.create_async_function(
        move |lua,
              (_self, url, headers, timeout): (
            Value,
            LuaString,
            Option<mlua::Table>,
            Option<u64>,
        )| {
            let inst = inst_get.clone();
            let rl = rl_get.clone();
            async move {
                // R2 频控先行（不触网即拒，脚本可感知退避）。
                if !rl.check() {
                    return Err(rate_limited_err());
                }
                let url = url.to_string_lossy().to_string();
                // Q3/附录 B 冻结面②：timeout（秒）宿主 clamp 1-30s（缺省 5s）。
                let timeout_secs = clamp_timeout_secs(timeout);
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
    let rl_post = rl.clone();
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
            let rl = rl_post.clone();
            async move {
                // R2 频控先行（与 http_get 共享同一出站预算）。
                if !rl.check() {
                    return Err(rate_limited_err());
                }
                let url = url.to_string_lossy().to_string();
                let body = body.to_string_lossy().to_string();
                // Q3/附录 B 冻结面②：timeout（秒）宿主 clamp 1-30s（缺省 5s）。
                let timeout_secs = clamp_timeout_secs(timeout);
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

/// 宿主侧超时参数钳制（Q3/附录 B 冻结面②，T4 接线）：缺省 5s，1-30s 封顶（秒）。
///
/// 上限查能力卡 `ctx:http_get` 的 `timeout_cap_ms`（毫秒 → 秒）——单一事实源
/// （阶段 A 审查修复：原先硬编码 30 与卡片靠约定同步，有漂移风险）。
/// 单测锁边界（0→1 / 3600→30，`tests::clamp_timeout_secs_bounds`）；下限行为级断言见
/// `tests::http_timeout_clamped_lower_bound_to_1s`（timeout=0 被抬到 1s）。
#[cfg(feature = "lua-http")]
pub(super) fn clamp_timeout_secs(t: Option<u64>) -> u64 {
    let cap_secs = crate::plugin::lua_core::capability::CAPABILITIES
        .iter()
        .find(|c| c.name == "ctx:http_get")
        .map(|c| c.timeout_cap_ms / 1000)
        .filter(|&s| s > 0)
        .unwrap_or(30);
    t.unwrap_or(5).clamp(1, cap_secs)
}

/// http R2 频控限值（次/秒/实例，Q3/§4.5-2/T5）：查能力卡 `ctx:http_get` 的
/// `rate_per_sec`——单一事实源（http_get/http_post 两卡同声明 20/s，共享一个出站
/// 预算；若未来两卡 diverge 应拆成各自限流器）。卡缺失或 0（=不限）时回落保守默认 20。
#[cfg(feature = "lua-http")]
fn http_rate_per_sec() -> u32 {
    crate::plugin::lua_core::capability::CAPABILITIES
        .iter()
        .find(|c| c.name == "ctx:http_get")
        .map(|c| c.rate_per_sec)
        .filter(|&r| r > 0)
        .unwrap_or(20)
}

/// 频控拒绝错误（消息带限值，脚本可感知退避）。
#[cfg(feature = "lua-http")]
fn rate_limited_err() -> mlua::Error {
    mlua::Error::RuntimeError(format!("rate limited（{}/s）", http_rate_per_sec()))
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
            // Lua 脚本 HTTP 响应体上限 1MiB（具名常量，审计 B6）。
            const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
            if bytes.len() > MAX_HTTP_BODY_BYTES {
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
