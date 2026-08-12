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
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

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
/// 由 axum fallback 调用，签名只接 `Uri`（不需要 State，前端资源是编译期嵌入的常量）。
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let file = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = FrontendAsset::get(file) {
        let mime = mime_guess::from_path(file).first_or_octet_stream();
        // unwrap 安全：mime 类型字符串是合法 HeaderValue（ASCII）。
        Response::builder()
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref()).unwrap(),
            )
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
                .body(Body::from(content.data.into_owned()))
                .unwrap(),
            None => (StatusCode::NOT_FOUND, "frontend not built").into_response(),
        }
    }
}
