//! 附件上传 + 工作区浏览（`/api/agent/attachments`、`/api/agent/workspace`）。
//!
//! reasonix serve 的 HTTP API 只解析 `@` 引用（/submit → ResolveScopedRefs），
//! 没有落盘/浏览端点（SaveXxxDataURL、ListDir 仅 desktop 进程内绑定）——
//! 本模块由网关直管 reasonix 工作区文件系统补齐，与 providers.rs 同一决策模式。
//!
//! 规则严格对齐 reasonix 源码（跨进程行为一致性是硬约束）：
//! - 限额/命名/扩展名白名单：internal/control/attachments.go（图 10MB、文件 25MB、
//!   `^\.[a-z0-9]{1,12}$` 否则 `.bin`、O_EXCL 防碰撞）；
//! - 浏览过滤清单：internal/fileref/search.go SkipEntry（目录一次一层、噪音过滤）。
//!
//! 落盘锚定 workspace_dir 而非 serve 的 cwd——@ 解析锚定 workspaceRoot，desktop
//! 版存附件前也 chdir 到 workspaceRoot（app.go withActiveWorkspaceDo）保证重合。

use std::path::{Path, PathBuf};

use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::proxy::GwState;

/// 附件图片上限（control/attachments.go maxImageAttachmentBytes）。
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// 附件文件上限（control/attachments.go maxFileAttachmentBytes）。
const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
/// 唯一文件名重试次数（control/attachments.go maxAttachmentCreateAttempts）。
const MAX_CREATE_ATTEMPTS: u32 = 1000;

/// 上传路由的请求体上限（axum DefaultBodyLimit 默认仅 2MB，会先于本模块的
/// 流式 25MB 拦截报 multipart 解析错误——路由层显式抬高，其余路由不受影响）。
pub const UPLOAD_BODY_LIMIT: usize = MAX_FILE_BYTES + 64 * 1024;

/// 附件目录（相对工作区根，正斜杠——与 reasonix @token 语法一致）。
const ATTACHMENTS_REL: &str = ".reasonix/attachments";

/// 前端展示用目录项。
#[derive(Serialize)]
pub struct WsEntry {
    pub name: String,
    pub is_dir: bool,
}

fn err(code: StatusCode, msg: &str) -> axum::response::Response {
    (code, msg.to_string()).into_response()
}

/// workspace_dir 未配置时的统一 503。
fn not_configured() -> axum::response::Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "attachments not configured: set [reasonix] workspace_dir in gateway.toml",
    )
}

/// 存储扩展名：小写、白名单 `^[.][a-z0-9]{1,12}$`，不合法回退 `.bin`
/// （attachments.go safeAttachmentExt——防可执行后缀/怪异后缀进工作区）。
fn safe_ext(orig_name: &str) -> String {
    let ext = std::path::Path::new(orig_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let dotted = format!(".{ext}");
    let bytes = dotted.as_bytes();
    let ok = bytes.len() >= 2
        && bytes.len() <= 13
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if ok {
        dotted
    } else {
        ".bin".to_string()
    }
}

/// 图片扩展名（attachments.go imageExt：png/jpg/jpeg/gif/webp）。
fn is_image_ext(ext: &str) -> bool {
    matches!(ext, ".png" | ".jpg" | ".jpeg" | ".gif" | ".webp")
}

/// 上传落盘：写 `<ws>/.reasonix/attachments/upload-<时间戳>-<序号><ext>`，
/// 返回工作区相对路径（正斜杠）。O_EXCL 重试保证并发唯一。
pub fn save_attachment(ws: &Path, orig_name: &str, bytes: &[u8]) -> Result<String, String> {
    let ext = safe_ext(orig_name);
    let limit = if is_image_ext(&ext) {
        MAX_IMAGE_BYTES
    } else {
        MAX_FILE_BYTES
    };
    if bytes.is_empty() || bytes.len() > limit {
        let kind = if is_image_ext(&ext) {
            "图片"
        } else {
            "文件"
        };
        return Err(format!(
            "{kind}大小须在 1 字节到 {} MB 之间",
            limit / (1024 * 1024)
        ));
    }
    let dir = ws.join(".reasonix").join("attachments");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建附件目录失败: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S.%6f");
    for seq in 1..=MAX_CREATE_ATTEMPTS {
        let rel = format!("{ATTACHMENTS_REL}/upload-{ts}-{seq:06}{ext}");
        let path = ws.join(&rel);
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("写附件失败: {e}")),
        };
        if let Err(e) = std::io::Write::write_all(&mut f, bytes) {
            drop(f);
            let _ = std::fs::remove_file(&path);
            return Err(format!("写附件失败: {e}"));
        }
        return Ok(rel);
    }
    Err("附件文件名冲突重试耗尽".to_string())
}

/// 校验工作区相对路径（浏览/后续引用的沙箱第一道闸）：
/// 仅接受 `/` 分隔的相对路径；拒绝空段、`.`/`..`、反斜杠（token 语法里是
/// 转义符）、`:`（Windows 盘符/NTFS 流）、控制字符。空串 = 根。
fn validate_rel(rel: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    if rel.is_empty() {
        return Ok(out);
    }
    for seg in rel.split('/') {
        if seg.is_empty() {
            return Err("路径含空段（连续 / 或尾随 /）".to_string());
        }
        if seg == "." || seg == ".." {
            return Err("路径不允许 . 或 .. 段".to_string());
        }
        if seg.contains('\\') {
            return Err("路径仅接受 / 分隔符".to_string());
        }
        if seg.contains(':') {
            return Err("路径段不允许冒号（盘符/NTFS 流）".to_string());
        }
        if seg.bytes().any(|b| b.is_ascii_control()) {
            return Err("路径段含控制字符".to_string());
        }
        out.push(seg);
    }
    Ok(out)
}

/// fileref/search.go SkipEntry 的等价清单：两类合并为按名跳过 + 目录类。
const SKIP_ANY_NAMES: &[&str] = &[
    ".codex",
    ".DS_Store",
    ".git",
    ".npm",
    ".pnpm-store",
    "node_modules",
    "Thumbs.db",
];
const SKIP_DIR_NAMES: &[&str] = &[
    "build",
    "dist",
    "target",
    "__pycache__",
    "venv",
    ".venv",
    ".gradle",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    ".terraform",
    ".dart_tool",
];

fn skip_entry(name: &str, is_dir: bool) -> bool {
    SKIP_ANY_NAMES.contains(&name) || (is_dir && SKIP_DIR_NAMES.contains(&name))
}

/// 列工作区一层目录（目录在前、各组按名不区分大小写排序，噪音条目过滤——
/// desktop ListDir 同款）。path 为工作区相对路径（空 = 根），返回排序后条目。
pub fn list_dir(ws: &Path, rel: &str) -> Result<Vec<WsEntry>, String> {
    let target = validate_rel(rel)?;
    let abs = ws.join(&target);
    let meta = std::fs::metadata(&abs).map_err(|e| format!("读取目录失败: {e}"))?;
    if !meta.is_dir() {
        return Err(format!("`{rel}` 不是目录"));
    }
    // 沙箱第二道闸：canonicalize 后必须仍在工作区内（防符号链接逃逸）。
    let ws_canon = std::fs::canonicalize(ws).map_err(|e| format!("解析工作区根失败: {e}"))?;
    let abs_canon = std::fs::canonicalize(&abs).map_err(|e| format!("解析路径失败: {e}"))?;
    if !abs_canon.starts_with(&ws_canon) {
        return Err("路径越出工作区".to_string());
    }
    let rd = std::fs::read_dir(&abs).map_err(|e| format!("读取目录失败: {e}"))?;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type();
        let is_dir = ft.as_ref().map(|t| t.is_dir()).unwrap_or(false);
        if skip_entry(&name, is_dir) {
            continue;
        }
        if is_dir {
            dirs.push(name);
        } else if ft.as_ref().map(|t| t.is_file()).unwrap_or(false) {
            files.push(name);
        }
    }
    dirs.sort_by_key(|a| a.to_lowercase());
    files.sort_by_key(|a| a.to_lowercase());
    Ok(dirs
        .into_iter()
        .map(|name| WsEntry { name, is_dir: true })
        .chain(files.into_iter().map(|name| WsEntry {
            name,
            is_dir: false,
        }))
        .collect())
}

/// POST /api/agent/attachments（multipart，字段名 `file`，可多字段逐个落盘）。
/// 返回 `{files:[{name,path}]}`——前端把 path 以 `@` 引用插入消息。
pub async fn upload(State(st): State<Arc<GwState>>, mut mp: Multipart) -> axum::response::Response {
    if st.reasonix.workspace_dir.is_empty() {
        return not_configured();
    }
    let ws = PathBuf::from(&st.reasonix.workspace_dir);
    let mut saved: Vec<serde_json::Value> = Vec::new();
    loop {
        let mut field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("解析 multipart 失败: {e}"),
                )
            }
        };
        if field.name().unwrap_or("") != "file" {
            continue;
        }
        let Some(orig) = field.file_name().map(str::to_string) else {
            continue;
        };
        // 流式累积 + 硬顶（先按 25MB 拦截；落盘时再按图片类收紧到 10MB）。
        // chunk 错误（请求体超限/客户端中断）显式中止——静默吞掉会把截断的
        // 部分字节当完整附件落盘（实测事故：DefaultBodyLimit 2MB 时代留下的残件）。
        let mut buf: Vec<u8> = Vec::new();
        let mut overflow = false;
        let mut read_err: Option<String> = None;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len() + chunk.len() > MAX_FILE_BYTES {
                        overflow = true;
                        break;
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    read_err = Some(format!("读取附件数据失败: {e}"));
                    break;
                }
            }
        }
        if let Some(e) = read_err {
            return err(StatusCode::BAD_REQUEST, &e);
        }
        if overflow {
            return err(StatusCode::PAYLOAD_TOO_LARGE, "附件超过 25 MB 上限");
        }
        match save_attachment(&ws, &orig, &buf) {
            Ok(path) => saved.push(serde_json::json!({ "name": orig, "path": path })),
            Err(e) => return err(StatusCode::BAD_REQUEST, &e),
        }
    }
    if saved.is_empty() {
        return err(StatusCode::BAD_REQUEST, "缺少名为 file 的附件字段");
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "files": saved })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct WorkspaceQuery {
    #[serde(default)]
    pub path: String,
}

/// GET /api/agent/workspace?path=<相对路径> → `{path, entries:[{name,is_dir}]}`。
pub async fn workspace(
    State(st): State<Arc<GwState>>,
    Query(q): Query<WorkspaceQuery>,
) -> axum::response::Response {
    if st.reasonix.workspace_dir.is_empty() {
        return not_configured();
    }
    let ws = PathBuf::from(&st.reasonix.workspace_dir);
    match list_dir(&ws, q.path.trim_end_matches('/')) {
        Ok(entries) => (
            StatusCode::OK,
            Json(serde_json::json!({ "path": q.path, "entries": entries })),
        )
            .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_ws(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rsiot-gw-attach-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ── save_attachment ──────────────────────────────────────────────

    #[test]
    fn saves_and_returns_forward_slash_rel() {
        let ws = tmp_ws("save");
        let rel = save_attachment(&ws, "泵房照片.png", b"pngbytes").unwrap();
        assert!(rel.starts_with(".reasonix/attachments/upload-"));
        assert!(rel.ends_with(".png"));
        assert!(ws.join(".reasonix").join("attachments").is_dir());
        let written = std::fs::read(ws.join(&rel)).unwrap();
        assert_eq!(written, b"pngbytes");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn rejects_oversized_by_category() {
        let ws = tmp_ws("size");
        let big_img = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(save_attachment(&ws, "a.png", &big_img).is_err());
        let ok_img = vec![0u8; MAX_IMAGE_BYTES];
        assert!(save_attachment(&ws, "a.png", &ok_img).is_ok());
        // 非图片走 25MB 档：11MB 通过。
        let big_file = vec![0u8; MAX_IMAGE_BYTES + 1024];
        assert!(save_attachment(&ws, "b.zip", &big_file).is_ok());
        let too_big = vec![0u8; MAX_FILE_BYTES + 1];
        assert!(save_attachment(&ws, "b.zip", &too_big).is_err());
        assert!(save_attachment(&ws, "c.bin", b"").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn sanitizes_extension() {
        assert_eq!(safe_ext("data.csv"), ".csv");
        assert_eq!(safe_ext("noext"), ".bin");
        assert_eq!(safe_ext("weird.Exe!"), ".bin");
        assert_eq!(safe_ext("UPPER.PNG"), ".png");
        let ws = tmp_ws("ext");
        let rel = save_attachment(&ws, "weird.Exe!", b"x").unwrap();
        assert!(rel.ends_with(".bin"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn concurrent_unique_names() {
        let ws = tmp_ws("uniq");
        let mut rels = Vec::new();
        for _ in 0..3 {
            rels.push(save_attachment(&ws, "same.txt", b"x").unwrap());
        }
        let set: std::collections::HashSet<_> = rels.iter().collect();
        assert_eq!(set.len(), 3, "同名重传必须得到不同落盘名: {rels:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    // ── validate_rel ─────────────────────────────────────────────────

    #[test]
    fn validate_rel_rules() {
        assert!(validate_rel("").unwrap().as_os_str().is_empty());
        assert_eq!(
            validate_rel("docs/a.md").unwrap(),
            PathBuf::from("docs").join("a.md")
        );
        assert!(validate_rel("..").is_err());
        assert!(validate_rel("a/../b").is_err());
        assert!(validate_rel("a/./b").is_err());
        assert!(validate_rel("C:/x").is_err());
        assert!(validate_rel("a\\b").is_err());
        assert!(validate_rel("a//b").is_err());
        assert!(validate_rel("a/").is_err());
    }

    // ── list_dir ─────────────────────────────────────────────────────

    #[test]
    fn lists_dirs_first_filtered_sorted() {
        let ws = tmp_ws("list");
        for d in ["docs", "zdir", ".git", "node_modules", "dist", "target"] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        std::fs::write(ws.join("beta.md"), b"").unwrap();
        std::fs::write(ws.join("Alpha.txt"), b"").unwrap();
        std::fs::write(ws.join(".DS_Store"), b"").unwrap();
        let es = list_dir(&ws, "").unwrap();
        let names: Vec<(&str, bool)> = es.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
        assert_eq!(
            names,
            vec![
                ("docs", true),
                ("zdir", true),
                ("Alpha.txt", false),
                ("beta.md", false)
            ],
            "目录在前、各组不区分大小写排序、噪音目录与 .DS_Store 过滤"
        );
        let sub = list_dir(&ws, "docs").unwrap();
        assert!(sub.is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn list_dir_sandbox_rejects_escape() {
        let ws = tmp_ws("sandbox");
        // 词法层：.. / 绝对 / 反斜杠全拒。
        assert!(list_dir(&ws, "../other").is_err());
        assert!(list_dir(&ws, "a/..").is_err());
        // 符号链接逃逸：canonicalize 前缀校验兜底（Windows 需开发者模式建链，容错跳过）。
        let outside = ws.parent().unwrap().join("rsiot-gw-attach-outside");
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&outside, ws.join("jump"));
            assert!(list_dir(&ws, "jump").is_err());
        }
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn list_dir_missing_is_error() {
        let ws = tmp_ws("missing");
        assert!(list_dir(&ws, "nope").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
