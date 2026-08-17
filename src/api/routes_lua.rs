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
use std::time::{SystemTime, UNIX_EPOCH};

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
/// （对标 v1 §六安全边界 + zacp `resolveInWorkspace`）。
/// **版本管理**：覆盖 main.lua 前，若旧文件存在则 copy 到
/// `scripts/<name>/versions/<unix_ts>.lua`（保留历史版本，支持回滚）。写后**不自动
/// reload**——调用方按需 `POST /lua/reload?inst_id=` 或靠 notify 热加载（luaClient
/// 监听 scripts/；versions/ 子目录放历史快照，不触发 main.lua 重载）。
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
            // 版本管理：覆盖前存历史快照（旧文件存在才存，首存不存）。
            let version_archived = match archive_version(&main_lua) {
                Ok(true) => true,   // 已存历史版本
                Ok(false) => false, // 首存（无旧文件）
                Err(e) => {
                    tracing::warn!("[lua] archive version failed (继续覆盖): {e}");
                    false
                }
            };
            if let Err(e) = std::fs::write(&main_lua, &req.content) {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("write main.lua: {e}"),
                );
            }
            Json(serde_json::json!({
                "status": "ok",
                "path": main_lua.display().to_string(),
                "version_archived": version_archived,
            }))
            .into_response()
        }
        Err(msg) => err(StatusCode::BAD_REQUEST, msg),
    }
}

/// `?name=<n>` 查询参数（脚本目录名）。
#[derive(Deserialize)]
pub struct NameQuery {
    /// 脚本目录名（仅 `[A-Za-z0-9_-]`，白名单同 save）。
    pub name: String,
}

/// `POST /api/v1/lua/script/rollback` 请求体。
#[derive(Deserialize)]
pub struct RollbackReq {
    /// 脚本目录名。
    pub name: String,
    /// 历史版本时间戳（unix 秒，对应 versions/<ts>.lua 文件名）。
    pub ts: u64,
}

/// `GET /api/v1/lua/script?name=<n>` → `{name, content, size, mtime?}`（回读脚本内容）。
///
/// 前端编辑已有脚本入口。`name` 经白名单（同 save），防路径逃逸。文件不存在 → 404。
pub async fn get_script(State(_st): State<AppState>, Query(q): Query<NameQuery>) -> Response {
    match resolve_script_path(&q.name) {
        Ok(dir) => {
            let main_lua = dir.join("main.lua");
            match std::fs::read_to_string(&main_lua) {
                Ok(content) => {
                    let size = content.len();
                    let mtime = std::fs::metadata(&main_lua)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    Json(serde_json::json!({
                        "name": q.name,
                        "content": content,
                        "size": size,
                        "mtime": mtime,
                    }))
                    .into_response()
                }
                Err(_) => err(StatusCode::NOT_FOUND, "script main.lua not found"),
            }
        }
        Err(msg) => err(StatusCode::BAD_REQUEST, msg),
    }
}

/// `GET /api/v1/lua/script/versions?name=<n>` → `{versions: [{ts, size}]}`（列历史版本）。
///
/// 扫 `scripts/<n>/versions/` 下 `<unix_ts>.lua` 文件，按 ts 降序（新→旧）。无目录或空 → 空数组。
pub async fn list_versions(State(_st): State<AppState>, Query(q): Query<NameQuery>) -> Response {
    match resolve_script_path(&q.name) {
        Ok(dir) => {
            let versions = list_version_files(&dir.join("versions"));
            Json(serde_json::json!({ "versions": versions })).into_response()
        }
        Err(msg) => err(StatusCode::BAD_REQUEST, msg),
    }
}

/// `POST /api/v1/lua/script/rollback` → 复制 `versions/<ts>.lua` → main.lua（不自动 reload）。
///
/// 回滚前，当前 main.lua 同样先存一个版本快照（保留回滚点，可再回滚回去）。
/// 版本文件不存在 → 404。
pub async fn rollback_script(
    State(_st): State<AppState>,
    Json(req): Json<RollbackReq>,
) -> Response {
    match resolve_script_path(&req.name) {
        Ok(dir) => {
            let ver = dir.join("versions").join(format!("{}.lua", req.ts));
            if !ver.exists() {
                return err(StatusCode::NOT_FOUND, "version file not found");
            }
            let main_lua = dir.join("main.lua");
            // 审查 Important 修复：先读目标版本内容，再 archive 当前版本。原顺序
            // （先 archive 后 read）在「同一秒内 save 后立即 rollback」场景自毁——
            // archive 用整秒时间戳生成同名 versions/<ts>.lua 覆盖掉回滚源，读到
            // 的是自己，回滚静默无效且目标快照永久丢失。顺序对调确定性消除。
            let content = match std::fs::read_to_string(&ver) {
                Ok(c) => c,
                Err(e) => {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("read version: {e}"),
                    );
                }
            };
            // 回滚也存当前版本快照（保留回滚点，可再回滚回去）。
            // archive 失败仅 warn 不中断——回滚主流程不受快照失败影响（快照丢失
            // 只意味着本次回滚点无法再回滚回去，主回滚仍应完成）。
            if let Err(e) = archive_version(&main_lua) {
                tracing::warn!("[lua] rollback archive version failed (回滚点快照丢失): {e}");
            }
            if let Err(e) = std::fs::write(&main_lua, &content) {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("write main.lua: {e}"),
                );
            }
            Json(serde_json::json!({
                "status": "ok",
                "rolled_back_to": req.ts,
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

/// 当前 unix 秒（版本文件名 + 前端展示用）。SystemTime 单机单调，足够。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 将 `main.lua` 存为历史版本快照到 `<main_lua 父目录>/versions/<unix_ts>.lua`。
///
/// - `main_lua` 不存在（首存）→ `Ok(false)`，不创建版本。
/// - 存在 → 建 versions 子目录，copy 旧内容。同秒多次保存覆盖同一 ts 文件（可接受——
///   去重最新版本）。
/// - IO 失败 → `Err`，调用方决定是否中断（save_script 仅 warn 继续，防版本失败阻塞保存）。
///
/// 纯文件系统操作，与 lux 解耦（脚本本就是文件资产，与 notify 热加载路径一致）。
fn archive_version(main_lua: &Path) -> std::io::Result<bool> {
    if !main_lua.exists() {
        return Ok(false);
    }
    let parent = main_lua.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "main.lua has no parent dir",
        )
    })?;
    let versions_dir = parent.join("versions");
    std::fs::create_dir_all(&versions_dir)?;
    let ts = unix_now();
    let dest = versions_dir.join(format!("{ts}.lua"));
    std::fs::copy(main_lua, &dest)?;
    Ok(true)
}

/// 列 `<versions_dir>/` 下的版本快照：`[{ts, size}]` 按 ts 降序（新→旧）。
///
/// 仅识别 `<纯数字>.lua` 文件（archive_version 产物），忽略其他。空目录/不存在 → 空 Vec。
fn list_version_files(versions_dir: &Path) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(versions_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        // 仅认 <数字>.lua（archive_version 写入格式），其他文件忽略。
        let Some(stem) = p
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let ext_is_lua = p.extension().and_then(|x| x.to_str()) == Some("lua");
        if !ext_is_lua {
            continue;
        }
        let size = std::fs::metadata(&p).map(|m| m.len() as usize).unwrap_or(0);
        out.push(serde_json::json!({ "ts": stem, "size": size }));
    }
    // 降序：新版本在前（前端版本历史列表自然顺序）。
    out.sort_by(|a, b| b["ts"].as_u64().cmp(&a["ts"].as_u64()));
    out
}

/// 扫描 `root` 下有 `main.lua` 的子目录名（排序）。纯函数便于单测（不依赖 cwd）。
///
/// **排除 `versions` 子目录**：archive_version 在每个脚本目录下建 versions/ 存历史快照，
/// 不应作为脚本名列出（虽然 versions/ 内无 main.lua 天然不匹配，此处显式排除双保险）。
fn scan_scripts(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && p.join("main.lua").exists() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name == "versions" {
                        continue;
                    }
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

    // ---- 版本管理：archive_version / list_version_files / scan 排除 versions ----

    /// 唯一临时脚本目录（防并发测试互相干扰）。
    fn fresh_tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "lua-ver-{}-{}-{tag}",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 验证意图：save 前存历史快照，旧内容进 versions/，新内容写 main.lua。
    #[test]
    fn archive_version_preserves_old_before_overwrite() {
        let tmp = fresh_tmp("archive");
        let main = tmp.join("main.lua");
        std::fs::write(&main, "OLD").unwrap();

        let archived = archive_version(&main).unwrap();
        assert!(archived, "旧文件存在应存版本");

        // 模拟覆盖
        std::fs::write(&main, "NEW").unwrap();
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "NEW");

        // versions/ 下应有 1 个 <ts>.lua，内容是 OLD
        let vers = list_version_files(&tmp.join("versions"));
        assert_eq!(vers.len(), 1);
        let ts = vers[0]["ts"].as_u64().unwrap();
        let saved =
            std::fs::read_to_string(tmp.join("versions").join(format!("{ts}.lua"))).unwrap();
        assert_eq!(saved, "OLD", "版本快照应是覆盖前的旧内容");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn archive_version_first_save_skips() {
        let tmp = fresh_tmp("firstsave");
        let main = tmp.join("main.lua");
        // 首存：main.lua 不存在，不创建版本。
        let archived = archive_version(&main).unwrap();
        assert!(!archived, "首存（无旧文件）不存版本");
        assert!(!tmp.join("versions").exists(), "首存不建 versions 目录");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 验证意图：scan_scripts 不得把 versions 当脚本名列出。
    #[test]
    fn scan_excludes_versions_subdir() {
        let tmp = fresh_tmp("scan");
        // scripts/devA + scripts/devA/versions/<ts>.lua（但 versions 无 main.lua）
        std::fs::create_dir_all(tmp.join("devA")).unwrap();
        std::fs::write(tmp.join("devA/main.lua"), "x").unwrap();
        std::fs::create_dir_all(tmp.join("devA/versions")).unwrap();
        std::fs::write(tmp.join("devA/versions/123.lua"), "old").unwrap();
        // 陷阱：versions 里放 main.lua（误操作），scan 仍应排除
        std::fs::write(tmp.join("devA/versions/main.lua"), "trap").unwrap();

        let got = scan_scripts(&tmp);
        assert_eq!(got, vec!["devA".to_string()], "versions 不应作为脚本名列出");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 验证意图：list_version_files 只认 <数字>.lua，忽略其他文件，降序排列。
    #[test]
    fn list_versions_filters_and_sorts_desc() {
        let tmp = fresh_tmp("list");
        let vd = tmp.join("versions");
        std::fs::create_dir_all(&vd).unwrap();
        std::fs::write(vd.join("100.lua"), "a").unwrap();
        std::fs::write(vd.join("200.lua"), "b").unwrap();
        std::fs::write(vd.join("notatimestamp.lua"), "c").unwrap(); // 非数字，忽略
        std::fs::write(vd.join("150.txt"), "d").unwrap(); // 非 lua，忽略

        let vers = list_version_files(&vd);
        let ts: Vec<u64> = vers.iter().filter_map(|v| v["ts"].as_u64()).collect();
        assert_eq!(ts, vec![200, 100], "降序 + 只认数字.lua");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
