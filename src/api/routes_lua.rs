//! Lua 实例状态/重载 REST 端点（spec §13）。默认 JWT 保护。
//!
//! - `POST /api/v1/lua/reload?inst_id=<inst>` → 手动重载（stop+start，复用 InstanceManager；
//!   与 notify 自动热加载结果一致，IO 重连——spec §5.3「VM 替换 ≈ 轻量重启 instance」）
//! - `GET /api/v1/lua/scripts` → 列 `scripts/` 下有 `main.lua` 的目录名
//!
//! 实例指标已迁至统一端点 `GET /api/v1/inst/stats`（M4，见 `routes_stat`）。
//!
//! v1 用 query 参数（与 `routes_inst::IdQuery` 一致，避 axum 0.8 路径参数语法歧义）。

use crate::api::{str_from_value, AppState};
use crate::types::InstConfig;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use lux::EmbeddedValue;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// `?inst_id=<inst>` 查询参数。
#[derive(Deserialize)]
pub struct InstQuery {
    /// 实例 ID。
    pub inst_id: String,
}

/// `POST /api/v1/lua/script/save` 请求体。
#[derive(Deserialize)]
pub struct SaveScriptReq {
    /// 脚本目录名（如 `"devA"`），写入 `scripts/<name>/main.lua`。
    /// 仅允许字母/数字/下划线/短横，拒路径分隔符与 `..`（防逃逸）。
    pub name: String,
    /// main.lua 文件内容。
    pub content: String,
}

/// `POST /api/v1/lua/reload?inst_id=<inst>` → 手动重载（stop + start，spec §5.3）。
///
/// 仅对 `app_code == "luaClient"` 实例生效（端点语义）。实例不在 lux `instances` 表 → 404；
/// 非 lua 实例 → 400。
pub async fn reload(State(st): State<AppState>, Query(q): Query<InstQuery>) -> Response {
    let raw = match st
        .lux
        .execute_value("HGET", &["instances", &q.inst_id])
        .await
    {
        Ok(v) if !matches!(v, EmbeddedValue::Nil) => str_from_value(&v),
        _ => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    let cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid config: {e}")),
    };
    if cfg.app_code.as_ref() != "luaClient" {
        return err(StatusCode::BAD_REQUEST, "not a luaClient instance");
    }
    let _ = st.mgr.stop(&q.inst_id).await;
    if let Err(e) = st.mgr.start_source(cfg).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// `GET /api/v1/lua/scripts` → `{ "scripts": ["devA", ...] }`（cwd/scripts 下有 main.lua 的目录）。
pub async fn list_scripts(State(_st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "scripts": scan_scripts(Path::new("scripts")) }))
}

/// `POST /api/v1/lua/script/save` → 写 `scripts/<name>/main.lua`（自动建目录）。
///
/// AI 写脚本入口（MCP `save_script` 工具 + 浮动面板"写脚本并运行"都走此）。
/// **路径白名单**：`name` 仅允许 `[A-Za-z0-9_-]`，拒路径分隔符/`..`/绝对路径
/// （对标 v1 §六安全边界 + zacp `resolveInWorkspace`）。写后**不自动 reload**——
/// 调用方按需 `POST /lua/reload?inst_id=` 或靠 notify 热加载（luaClient 监听 scripts/）。
pub async fn save_script(State(_st): State<AppState>, Json(req): Json<SaveScriptReq>) -> Response {
    match resolve_script_path(&req.name) {
        Ok(dir) => {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("create dir: {e}"),
                );
            }
            let main_lua = dir.join("main.lua");
            if let Err(e) = std::fs::write(&main_lua, &req.content) {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("write main.lua: {e}"),
                );
            }
            Json(serde_json::json!({
                "status": "ok",
                "path": main_lua.display().to_string(),
            }))
            .into_response()
        }
        Err(msg) => err(StatusCode::BAD_REQUEST, msg),
    }
}

/// 校验脚本名合法性并解析为 `scripts/<name>` 绝对路径。
///
/// 纯函数便于单测。规则：
/// - 非空，仅 `[A-Za-z0-9_-]`（拒 `/`、`\`、`:`、空格、`..`、`.` 开头等）。
/// - 拼接后用 `Path::components` 复核——不得含 `..` 或 `Component::RootDir`（双保险）。
///
/// 返回绝对路径（cwd/scripts/<name>），调用方 join("main.lua")。
fn resolve_script_path(name: &str) -> Result<PathBuf, &'static str> {
    if name.is_empty() {
        return Err("script name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("script name may only contain [A-Za-z0-9_-]");
    }
    let dir = PathBuf::from("scripts").join(name);
    // 双保险：字符白名单已排除 `..`/分隔符，这里再验 components 防 future regression。
    if dir
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
    {
        return Err("script name must not escape scripts/ directory");
    }
    Ok(dir)
}

/// 扫描 `root` 下有 `main.lua` 的子目录名（排序）。纯函数便于单测（不依赖 cwd）。
fn scan_scripts(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && p.join("main.lua").exists() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

// ---- 局部类型/工具 ----
type StatusCode = axum::http::StatusCode;
type Response = axum::response::Response;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_picks_dirs_with_main_lua() {
        let tmp = std::env::temp_dir().join(format!("lua-scripts-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("devA")).unwrap();
        std::fs::write(tmp.join("devA/main.lua"), "function collect() end").unwrap();
        std::fs::create_dir_all(tmp.join("empty")).unwrap(); // 无 main.lua，不列
        let got = scan_scripts(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(got, vec!["devA".to_string()]);
    }

    #[test]
    fn scan_missing_root_is_empty() {
        assert!(scan_scripts(Path::new("/no/such/rs-iot-lua-scripts-xyz")).is_empty());
    }

    // ---- resolve_script_path 路径白名单（安全关键，规则7）----

    #[test]
    fn resolve_accepts_simple_name() {
        let p = resolve_script_path("devA").unwrap();
        assert_eq!(p, PathBuf::from("scripts").join("devA"));
    }

    #[test]
    fn resolve_accepts_alnum_underscore_hyphen() {
        assert!(resolve_script_path("dev_1-2").is_ok());
        assert!(resolve_script_path("ABC123").is_ok());
    }

    #[test]
    fn resolve_rejects_empty() {
        assert_eq!(
            resolve_script_path("").unwrap_err(),
            "script name must not be empty"
        );
    }

    /// 验证意图（规则7）：路径逃逸必须拒绝——AI 误操作或恶意输入不得写出 scripts/ 外。
    #[test]
    fn resolve_rejects_path_traversal() {
        for bad in ["..", "../etc", "a/b", "a\\b", "/abs", ".", "a b", "a:b"] {
            assert!(
                resolve_script_path(bad).is_err(),
                "should reject traversal/separator/space in name: {bad:?}"
            );
        }
    }
}
