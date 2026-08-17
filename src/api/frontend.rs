//! 静态前端（rust-embed 嵌入）。`/apitest` 提供 apiTest 页面。

use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/api/static/"]
struct Asset;

/// `/apitest` → index.html
pub async fn index_redirect() -> Response {
    serve("index.html")
}

/// `/scada` → SCADA 采集器监控终端（amber-phosphor 工业控制柜风格）。
pub async fn scada_terminal() -> Response {
    serve("scada.html")
}

/// `/` → 重定向到 `/apitest`
pub async fn root_redirect() -> Response {
    Redirect::to("/apitest").into_response()
}

fn serve(name: &str) -> Response {
    match Asset::get(name) {
        Some(content) => {
            let mime = mime_guess::from_path(name).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.to_vec(),
            )
                .into_response()
        }
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}
