//! 前端静态资源服务：rust-embed 嵌入 frontend/dist，SPA fallback 到 index.html。
//!
//! 路由策略（app.rs 注册顺序）：`/api/*` 走反代（auth_middleware 鉴权），其余走静态资源
//! （fallback，公开——前端 SPA 登录页本身要先加载）。
//! SPA fallback：未命中的非文件路径回退 index.html（vue-router history 模式由前端路由接管）。
//!
//! 对标 rs-iot 主 crate `src/api/frontend.rs`（rust-embed + mime_guess 模式）。

use axum::body::Body;
use axum::http::header;
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use rust_embed::RustEmbed;
use tower_http::services::ServeDir;

/// 嵌入 frontend/dist 构建产物。
///
/// 相对路径以 `CARGO_MANIFEST_DIR`（crates/rsiot-gateway/）为基准解析
/// （rust-embed 默认行为，对标 rs-iot 主 crate `src/api/frontend.rs`）。
/// `../../frontend/dist` → 仓库根 frontend/dist。
#[derive(RustEmbed)]
#[folder = "../../frontend/dist"]
struct FrontendAsset;

/// 静态资源 handler（build_router 的 fallback）：从路径取文件名，命中则返文件，
/// 未命中回退 index.html（SPA history 模式）。
///
/// 缓存策略（与 `hmi_cache_control` 同款，2026-10-28 补齐）：`assets/` 下是 vite
/// 哈希文件名产物 → immutable 一年；其余（index.html、SPA 回退）→ no-cache——
/// 否则浏览器可启发式缓存旧 index.html，升级部署后引用已删除的 hash 资产。
/// 由 axum fallback 调用，签名只接 `Uri`（不需要 State，前端资源是编译期嵌入的常量）。
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let file = if path.is_empty() { "index.html" } else { path };
    // unwrap 安全：两值均为合法 HeaderValue（ASCII）。
    let cache_value = if file.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    if let Some(content) = FrontendAsset::get(file) {
        let mime = mime_guess::from_path(file).first_or_octet_stream();
        // unwrap 安全：mime 类型字符串是合法 HeaderValue（ASCII）。
        Response::builder()
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref()).unwrap(),
            )
            .header(header::CACHE_CONTROL, HeaderValue::from_str(cache_value).unwrap())
            .body(Body::from(content.data.into_owned()))
            .unwrap()
    } else if path.contains('.') {
        // 带扩展名的路径视为静态文件缺失 → 404（防断链资源被当 HTML 返致控制台 Unexpected token）。
        // 注意：判断的是"路径任意段含点"，vue-router history 路由惯例不含点（/login /scada），
        // 含点路径基本只有静态资源（/assets/*.js），故此启发式安全。
        (StatusCode::NOT_FOUND, "not found").into_response()
    } else {
        // SPA fallback：非文件路径（如 /login）回退 index.html，交给 vue-router 处理。
        match FrontendAsset::get("index.html") {
            Some(content) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CACHE_CONTROL, HeaderValue::from_str("no-cache").unwrap())
                .body(Body::from(content.data.into_owned()))
                .unwrap(),
            None => (StatusCode::NOT_FOUND, "frontend not built").into_response(),
        }
    }
}

// ---------- /hmi hmi-studio 静态服务（可配 hmi_dist 时挂载，W2 集成） ----------

/// `/hmi` 前缀静态服务：**运行时文件系统** serve（tower-http `ServeDir`）。
///
/// 与上方 rust-embed 主前端刻意不同：hmi-studio dist 与 gateway 二进制独立
/// 部署演进（更新 HMI 不重编 gateway），故不嵌入。SPA 启发式与 `serve` 同款：
/// 未命中且路径含点 = 静态资源缺失 404；不含点 = 回 index.html（history 路由
/// 由 hmi-studio 前端接管）。未配置 hmi_dist 时 app.rs 不挂本服务。
///
/// 缓存策略（`hmi_cache_control`，审查 2026-10-27）：`assets/` 下是 vite 产物
/// 哈希文件名（内容变即换名）→ immutable 一年；其余（index.html、SPA 回退、
/// favicon 等）→ no-cache 每次回源校验——dist 重新部署后浏览器不会拿旧
/// index.html 引用已删除的 hash 资产。
pub fn hmi_router(dist: std::path::PathBuf) -> Router {
    let index_root = dist.clone();
    // 用 .fallback 而非 .not_found_service：后者把回退包进 SetStatus(404)——
    // SPA 路由会拿到 404 状态 + index.html 体（浏览器能渲染但 fetch/ok 判断、
    // PWA 可安装性全坏）。主前端 `serve` 的 SPA 回退是 200，本服务对齐。
    let dir = ServeDir::new(dist).fallback(Router::new().fallback(axum::routing::any(
        move |uri: Uri| {
            let root = index_root.clone();
            async move { hmi_spa_fallback(root, uri).await }
        },
    )));
    Router::new()
        .fallback_service(dir)
        .layer(axum::middleware::from_fn(hmi_cache_control))
}

/// `/hmi` 响应 Cache-Control（from_fn 中间件；uri 已被 nest 剥掉 `/hmi` 前缀）：
/// `assets/*` → `public, max-age=31536000, immutable`（哈希文件名，内容变即换名）；
/// 其余 → `no-cache`（可缓存但须回源校验，部署后旧引用立即失效）。
/// overriding 而非 if_not_present：ServeDir 默认不发 Cache-Control，语义由本
/// 中间件单点决定。
async fn hmi_cache_control(uri: Uri, request: axum::extract::Request, next: Next) -> Response {
    let value = if uri.path().starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    // unwrap 安全：两值均为合法 HeaderValue（ASCII）。
    let mut resp = next.run(request).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_str(value).unwrap());
    resp
}

/// SPA 回退（ServeDir 未命中时调用，uri 已被 nest 剥掉 `/hmi` 前缀）：
/// 路径含点 → 404（防 HTML 被当 .js/.css 解析，同 `serve` 注释）；否则回 index.html。
async fn hmi_spa_fallback(dist: std::path::PathBuf, uri: Uri) -> Response {
    if uri.path().contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(dist.join("index.html")).await {
        Ok(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => (StatusCode::NOT_FOUND, "hmi index.html not found").into_response(),
    }
}
