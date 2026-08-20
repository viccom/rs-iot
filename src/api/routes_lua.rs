//! Lua 实例状态/重载/stdlib REST 端点（spec §13 + 设计 §3.5/§3.6）。默认 JWT 保护。
//!
//! - `POST /api/v1/lua/reload?inst_id=<inst>` → 手动重载（stop+start，复用 InstanceManager；
//!   与 notify 自动热加载结果一致，IO 重连——spec §5.3「VM 替换 ≈ 轻量重启 instance」）
//! - `GET /api/v1/lua/scripts` → 列 `scripts/` 下有 `main.lua` 的目录名
//!   （`?kind=lib` → 列 `scripts/_lib/` 下共享库相对路径，T10/Q13）
//! - `POST /api/v1/lua/script/save`（`kind` 缺省 `script` / `lib`）→ 写脚本 main.lua
//!   或站点共享库 `scripts/_lib/<org>/<...>.lua`（D11 保留段拒绝）
//! - `GET /api/v1/lua/script/versions` / `POST /api/v1/lua/script/rollback`（同 `kind`）
//! - `GET /api/v1/lua/script?name=<n>` → 回读内容（`kind=script` 缺省 / `lib`，Q13 库读回）
//! - `GET /api/v1/lua/stdlib` → 本地 stdlib 清单（index + 种子合并，来源徽标 seed/synced；
//!   `scripts/` 模板条目不列入——安装器为 B 阶段遗留）
//! - `POST /api/v1/lua/stdlib/sync` → 官方源/镜像验签同步（body `{upstream?, pin?}`；
//!   pin 缺省锁定本地已装版本，`"auto"` 跟随 latest，§3.3；成功/失败落审计日志 §3.7；
//!   成功后批量 **VM 级 reload**（运行中实例逐一触发，失败回退旧 VM、非运行态不复活，Q11））
//!
//! 实例 reload 核心在 `plugin::lua_core::reload`（T10 移入：`_lib/` 全局 watcher
//! 与本层端点共用，保持 api → lua_core 单向依赖）。
//!
//! 实例指标已迁至统一端点 `GET /api/v1/inst/stats`（M4，见 `routes_stat`）。
//!
//! v1 用 query 参数（与 `routes_inst::IdQuery` 一致，避 axum 0.8 路径参数语法歧义）。

use crate::api::AppState;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
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
    /// kind=script（缺省）：脚本目录名（如 `"devA"`），写入 `scripts/<name>/main.lua`；
    /// kind=lib：共享库相对路径 `<org>/<...>/<file>.lua`，写入 `scripts/_lib/...`
    /// （T10/Q13）。script 侧仅允许字母/数字/下划线/短横；lib 侧另有
    /// ≥2 段 + 保留段拒绝 + `.lua` 后缀校验（见 [`parse_lib_segments`]）。
    pub name: String,
    /// 资产类型：`"script"`（缺省）| `"lib"`（站点共享库，D11 保留段拒绝）。
    pub kind: Option<String>,
    /// 文件内容（main.lua 或库 .lua）。
    pub content: String,
}

/// `POST /api/v1/lua/reload?inst_id=<inst>` → 手动重载（stop + start，spec §5.3）。
///
/// 仅对 `app_code == "luaClient"` 实例生效（端点语义）。实例不在 lux `instances` 表 → 404；
/// 非 lua 实例 → 400。核心在 [`crate::plugin::lua_core::reload::reload_lua_inst`]
/// （T10 移入 lua_core：`_lib/` 全局 watcher 共用，端点只做 HTTP 映射）；
/// handler 依赖 lua_core → lua feature 门控（路由注册同门控，见 `build_router`）。
#[cfg(feature = "lua")]
pub async fn reload(State(st): State<AppState>, Query(q): Query<InstQuery>) -> Response {
    use crate::plugin::lua_core::reload::{reload_lua_inst, ReloadOutcome};
    match reload_lua_inst(&st.lux, &st.mgr, &q.inst_id).await {
        ReloadOutcome::Ok => Json(serde_json::json!({"status": "ok"})).into_response(),
        ReloadOutcome::NotFound => err(StatusCode::NOT_FOUND, "instance not found"),
        ReloadOutcome::BadRequest(m) => err(StatusCode::BAD_REQUEST, &m),
        ReloadOutcome::StartFail(m) => err(StatusCode::INTERNAL_SERVER_ERROR, &m),
    }
}

/// `?kind=script|lib` 查询参数（资产类型，缺省 script）。
#[derive(Deserialize)]
pub struct ListQuery {
    /// `script`（缺省，列 scripts/ 有 main.lua 的目录）| `lib`（列 `_lib/` 共享库）。
    pub kind: Option<String>,
}

/// `GET /api/v1/lua/scripts` → `{ "scripts": ["devA", ...] }`（cwd/scripts 下有
/// main.lua 的目录）；`?kind=lib` → `{ "libs": ["viccom/hex.lua", ...] }`
/// （`scripts/_lib/` 下 .lua 相对路径，Recursive，T10/Q13）。
pub async fn list_scripts(State(_st): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    match q.kind.as_deref() {
        None | Some("script") => Json(serde_json::json!({
            "scripts": scan_scripts(Path::new("scripts"))
        }))
        .into_response(),
        Some("lib") => Json(serde_json::json!({
            "libs": scan_libs(&Path::new("scripts").join("_lib"))
        }))
        .into_response(),
        Some(_) => err(
            StatusCode::BAD_REQUEST,
            "kind must be \"script\" or \"lib\"",
        ),
    }
}

/// `POST /api/v1/lua/script/save` → 写 `scripts/<name>/main.lua`（kind=script 缺省）
/// 或 `scripts/_lib/<org>/<...>.lua`（kind=lib，T10/Q13），自动建目录。
///
/// AI 写脚本入口（MCP `save_script` 工具 + 浮动面板"写脚本并运行"都走此）。
/// **路径白名单**：script 侧 `name` 仅允许 `[A-Za-z0-9_-]`；lib 侧 ≥2 段、首段
/// 非保留段（D11）、段白名单同规则（拒路径分隔符/`..`/绝对路径——对标 v1 §六
/// 安全边界 + zacp `resolveInWorkspace`）。
/// **版本管理**：覆盖前若旧文件存在则 copy 到 `<目标父目录>/versions/<unix_ts>.lua`
/// （script → `scripts/<name>/versions/`；lib → org 直下文件为
/// `scripts/_lib/<org>/versions/`，嵌套库为其所在目录，对齐快照目录惯例）。
/// 写后**不自动 reload**——script 靠 per-instance notify 热加载或手动 reload；
/// lib 由 `_lib/` 全局 watcher 触发全实例 reload（Q15，计划 T10）。
pub async fn save_script(State(_st): State<AppState>, Json(req): Json<SaveScriptReq>) -> Response {
    match req.kind.as_deref() {
        None | Some("script") => match resolve_script_path(&req.name) {
            Ok(dir) => match write_with_versioning(&dir.join("main.lua"), &req.content) {
                Ok(version_archived) => Json(serde_json::json!({
                    "status": "ok",
                    "kind": "script",
                    "path": dir.join("main.lua").display().to_string(),
                    "version_archived": version_archived,
                }))
                .into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
            },
            Err(msg) => err(StatusCode::BAD_REQUEST, msg),
        },
        Some("lib") => match save_lib_under(Path::new("scripts"), &req.name, &req.content) {
            Ok((path, version_archived)) => Json(serde_json::json!({
                "status": "ok",
                "kind": "lib",
                "path": path.display().to_string(),
                "version_archived": version_archived,
            }))
            .into_response(),
            Err(SaveLibError::Invalid(msg)) => err(StatusCode::BAD_REQUEST, msg),
            Err(SaveLibError::Io(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
        },
        Some(_) => err(
            StatusCode::BAD_REQUEST,
            "kind must be \"script\" or \"lib\"",
        ),
    }
}

/// `?name=<n>` 查询参数（脚本目录名 / lib 相对路径）。
#[derive(Deserialize)]
pub struct NameQuery {
    /// kind=script（缺省）：脚本目录名（仅 `[A-Za-z0-9_-]`，白名单同 save）；
    /// kind=lib：共享库相对路径 `<org>/<...>/<file>.lua`（校验同 save）。
    pub name: String,
    /// 资产类型：`"script"`（缺省）| `"lib"`。
    pub kind: Option<String>,
}

/// `POST /api/v1/lua/script/rollback` 请求体。
#[derive(Deserialize)]
pub struct RollbackReq {
    /// 脚本目录名（kind=script）或共享库相对路径（kind=lib）。
    pub name: String,
    /// 历史版本时间戳（unix 秒，对应 versions/<ts>.lua 文件名）。
    pub ts: u64,
    /// 资产类型：`"script"`（缺省）| `"lib"`。
    pub kind: Option<String>,
}

/// `GET /api/v1/lua/script?name=<n>` → `{name, content, size, mtime?}`（回读内容）。
///
/// kind=script（缺省）：`scripts/<name>/main.lua`；kind=lib：`scripts/_lib/<org>/...lua`
/// （阶段 A 审查修复——Q13 前端「库」tab 复用编辑器须有读回入口，原先库可写不可读）。
/// `name` 经白名单（同 save），防路径逃逸。文件不存在 → 404。
pub async fn get_script(State(_st): State<AppState>, Query(q): Query<NameQuery>) -> Response {
    let path = match asset_read_path(q.kind.as_deref(), &q.name) {
        Ok(p) => p,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let size = content.len();
            let mtime = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            Json(serde_json::json!({
                "name": q.name,
                "kind": q.kind.unwrap_or_else(|| "script".into()),
                "content": content,
                "size": size,
                "mtime": mtime,
            }))
            .into_response()
        }
        Err(_) => err(StatusCode::NOT_FOUND, "file not found"),
    }
}

/// [`get_script`] 的路径解析主体（纯函数便于单测）：kind=script →
/// `scripts/<name>/main.lua`；kind=lib → 共享库文件本体（`resolve_lib_path` 白名单）。
fn asset_read_path(kind: Option<&str>, name: &str) -> Result<PathBuf, &'static str> {
    match kind {
        None | Some("script") => Ok(resolve_script_path(name)?.join("main.lua")),
        Some("lib") => resolve_lib_path(name),
        Some(_) => Err("kind must be \"script\" or \"lib\""),
    }
}

/// `GET /api/v1/lua/script/versions?name=<n>` → `{versions: [{ts, size}]}`（列历史版本）。
///
/// kind=script（缺省）扫 `scripts/<n>/versions/`；kind=lib 扫目标库文件父目录下
/// `versions/`。按 ts 降序（新→旧）。无目录或空 → 空数组。
pub async fn list_versions(State(_st): State<AppState>, Query(q): Query<NameQuery>) -> Response {
    let versions_dir = match q.kind.as_deref() {
        None | Some("script") => match resolve_script_path(&q.name) {
            Ok(dir) => dir.join("versions"),
            Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
        },
        Some("lib") => match resolve_lib_path(&q.name) {
            Ok(p) => match p.parent() {
                Some(d) => d.join("versions"),
                None => return err(StatusCode::BAD_REQUEST, "lib path has no parent"),
            },
            Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
        },
        Some(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "kind must be \"script\" or \"lib\"",
            )
        }
    };
    Json(serde_json::json!({ "versions": list_version_files(&versions_dir) })).into_response()
}

/// `POST /api/v1/lua/script/rollback` → 复制 `versions/<ts>.lua` → 目标文件
/// （script：`scripts/<n>/main.lua`；lib：`scripts/_lib/...`。不自动 reload——
/// script 靠热加载/手动，lib 靠 `_lib/` 全局 watcher，同 save）。
///
/// 回滚前，当前文件同样先存一个版本快照（保留回滚点，可再回滚回去）。
/// 版本文件不存在 → 404。
pub async fn rollback_script(
    State(_st): State<AppState>,
    Json(req): Json<RollbackReq>,
) -> Response {
    let target = match req.kind.as_deref() {
        None | Some("script") => match resolve_script_path(&req.name) {
            Ok(dir) => dir.join("main.lua"),
            Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
        },
        Some("lib") => match resolve_lib_path(&req.name) {
            Ok(p) => p,
            Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
        },
        Some(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "kind must be \"script\" or \"lib\"",
            )
        }
    };
    match rollback_target(&target, req.ts) {
        Ok(()) => Json(serde_json::json!({
            "status": "ok",
            "kind": req.kind.unwrap_or_else(|| "script".into()),
            "rolled_back_to": req.ts,
            "path": target.display().to_string(),
        }))
        .into_response(),
        Err(RollbackErr::NotFound) => err(StatusCode::NOT_FOUND, "version file not found"),
        Err(RollbackErr::Io(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// 回滚核心（script/lib 两 kind 共用，T10 抽出）：`<target 父>/versions/<ts>.lua`
/// → target。
///
/// 顺序（Important 修复保持，见 git 史 rollback_script）：**先读目标版本内容，再
/// archive 当前版本**——原顺序（先 archive 后 read）在「同一秒内 save 后立即
/// rollback」场景自毁（archive 用整秒时间戳生成同名 versions/<ts>.lua 覆盖掉回滚
/// 源，读到的是自己，回滚静默无效且目标快照永久丢失）。archive 失败仅 warn 不
/// 中断（快照丢失只意味着本次回滚点无法再回滚回去，主回滚仍应完成）。
fn rollback_target(target: &Path, ts: u64) -> Result<(), RollbackErr> {
    let Some(parent) = target.parent() else {
        return Err(RollbackErr::Io("target has no parent dir".into()));
    };
    let ver = parent.join("versions").join(format!("{ts}.lua"));
    if !ver.exists() {
        return Err(RollbackErr::NotFound);
    }
    let content =
        std::fs::read_to_string(&ver).map_err(|e| RollbackErr::Io(format!("read version: {e}")))?;
    if let Err(e) = archive_version(target) {
        tracing::warn!("[lua] rollback archive version failed (回滚点快照丢失): {e}");
    }
    std::fs::write(target, &content).map_err(|e| RollbackErr::Io(format!("write file: {e}")))?;
    Ok(())
}

/// 回滚错误分类：NotFound → 404；Io → 500。
#[derive(Debug)]
enum RollbackErr {
    /// 版本文件不存在。
    NotFound,
    /// 文件系统错误（消息随原因）。
    Io(String),
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
    // 保留目录名（阶段 A 审查修复）：`_` 在白名单内，不拒则 REST 可创建
    // `scripts/_lib/main.lua` 与共享库/官方镜像目录冲突、混入脚本列表。
    if matches!(name, "_lib" | "_stdlib") {
        return Err("script name reserved (_lib/_stdlib)");
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

// ---- T10：kind=lib 站点共享库路径校验与保存核心（Q13/D11，函数级可测）----

/// lib 路径段字符白名单（与 script 名同规则：结构性排除 `..`/`.`/分隔符/空格）。
fn lib_seg_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 保留段判定（D11 封闭集，单一事实源 `loader::RESERVED_TOP`）。lua feature 关闭时
/// require 路由层整体缺席、保留段语义无从谈起 → false（kind=lib 退化为纯文件
/// 管理不拒保留段；标准构建 lua 默认开启，拒绝路径有测试锁定）。
fn is_reserved_lib_top(seg: &str) -> bool {
    #[cfg(feature = "lua")]
    {
        crate::plugin::lua_core::loader::RESERVED_TOP.contains(&seg)
    }
    #[cfg(not(feature = "lua"))]
    {
        // lua 关闭时无路由层，seg 不参与判定；显式消耗防 unused 警告。
        let _ = seg;
        false
    }
}

/// 校验 kind=lib 相对路径并解析为段列表（T10/Q13，等价 script 侧
/// `resolve_script_path` 的白名单角色；即计划所称 `validate_lib_path`）。
///
/// 形态 `<org>/<...>/<file>.lua`：
/// - **≥2 段**（D11：单段名是私有层命名，进不了共享层）；
/// - 首段=作者/组织，**∉ 官方保留段**（`RESERVED_TOP`——防第三方投毒与 `_stdlib/`
///   命名空间混淆，loader 同源封闭集）；
/// - 防穿越同 script 规则：段字符白名单 `[A-Za-z0-9_-]`（结构性排除 `..`/`\`/空段
///   /空格），仅末段带 `.lua` 后缀；
/// - 返回段列表（含末段扩展名），调用方 join 到 `<scripts_root>/_lib/` 下。
fn parse_lib_segments(name: &str) -> Result<Vec<&str>, &'static str> {
    if name.is_empty() {
        return Err("lib path must not be empty");
    }
    if name.contains('\\') {
        return Err("lib path may not contain backslash");
    }
    let segs: Vec<&str> = name.split('/').collect();
    if segs.len() < 2 {
        return Err("lib path must have >= 2 segments: <org>/<name>.lua");
    }
    let first = segs[0];
    if !lib_seg_ok(first) {
        return Err("lib org segment may only contain [A-Za-z0-9_-]");
    }
    if is_reserved_lib_top(first) {
        return Err("lib org segment collides with reserved stdlib namespace");
    }
    // 中间段（嵌套库子目录）：纯白名单，无扩展名。
    for s in &segs[1..segs.len() - 1] {
        if !lib_seg_ok(s) {
            return Err("lib path segment may only contain [A-Za-z0-9_-]");
        }
    }
    // 末段：stem + .lua 后缀（stem 同白名单）。
    let last = segs[segs.len() - 1];
    let Some(stem) = last.strip_suffix(".lua") else {
        return Err("lib path must end with .lua");
    };
    if !lib_seg_ok(stem) {
        return Err("lib file name may only contain [A-Za-z0-9_-]");
    }
    Ok(segs)
}

/// kind=lib 路径解析（端点用，cwd 相对 `scripts` 基）：`<org>/<...>.lua` →
/// `scripts/_lib/<org>/<...>.lua`。
fn resolve_lib_path(name: &str) -> Result<PathBuf, &'static str> {
    lib_path_under(Path::new("scripts"), name)
}

/// [`resolve_lib_path`] 的可注入根版本（`scripts_root` 参数化，供单测用临时目录，
/// 不污染仓库 cwd/scripts）。
fn lib_path_under(scripts_root: &Path, name: &str) -> Result<PathBuf, &'static str> {
    let segs = parse_lib_segments(name)?;
    // 双保险：段白名单已结构性排除 `..`/分隔符，components 复核防 future regression
    // （同 resolve_script_path——检查对象是 name 派生的相对部分；scripts_root 可为
    // 注入的绝对临时根，其 Prefix/RootDir 前缀属调用方可控输入，不在防逃逸范围）。
    let mut rel = PathBuf::from("_lib");
    for s in &segs {
        rel = rel.join(s);
    }
    if rel
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
    {
        return Err("lib path must not escape scripts/_lib directory");
    }
    Ok(scripts_root.join(rel))
}

/// kind=lib 保存错误分类：Invalid → 400（路径校验）；Io → 500（文件系统）。
#[derive(Debug)]
enum SaveLibError {
    /// 路径校验失败（白名单/保留段/形态）。
    Invalid(&'static str),
    /// 文件系统错误（消息随原因）。
    Io(String),
}

/// 保存核心（script/lib 两 kind 共用，T10 抽出）：建父目录 → 旧文件存版本快照
/// （archive 失败仅 warn 继续，防版本失败阻塞保存——同原 save_script 语义）→
/// 写新内容。返回是否存了快照。
fn write_with_versioning(target: &Path, content: &str) -> Result<bool, String> {
    let parent = target
        .parent()
        .ok_or_else(|| "target has no parent dir".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    let archived = match archive_version(target) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("[lua] archive version failed (继续覆盖): {e}");
            false
        }
    };
    std::fs::write(target, content).map_err(|e| format!("write file: {e}"))?;
    Ok(archived)
}

/// kind=lib 保存核心（纯 fs，`scripts_root` 参数化便于单测；save 端点 lib 分支的
/// 函数级主体）：路径校验 → 建目录 → 版本快照 → 写入。
/// 返回 `(目标路径, 是否存了快照)`。校验先于任何 fs 副作用（拒绝时不建 `_lib/`）。
fn save_lib_under(
    scripts_root: &Path,
    name: &str,
    content: &str,
) -> Result<(PathBuf, bool), SaveLibError> {
    let target = lib_path_under(scripts_root, name).map_err(SaveLibError::Invalid)?;
    let archived = write_with_versioning(&target, content).map_err(SaveLibError::Io)?;
    Ok((target, archived))
}

/// 扫描 `lib_root`（`scripts/_lib/`）下全部 `.lua` 文件，返回相对路径
/// `"<org>/<...>/<file>.lua"`（排序）。Recursive（共享库可多级子目录）；
/// **排除 versions/ 目录**（版本快照不是库资产，对齐 scan_scripts 排除惯例）。
/// 纯函数便于单测（不依赖 cwd）。
fn scan_libs(lib_root: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if p.is_dir() {
                if name == "versions" {
                    continue;
                }
                walk(&p, &format!("{prefix}{name}/"), out);
            } else if name.ends_with(".lua") {
                out.push(format!("{prefix}{name}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(lib_root, "", &mut out);
    out.sort();
    out
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

// ---- stdlib 清单/同步（设计 §3.5/§3.6 D1；lua_core 依赖故 feature 门控）----

/// `GET /api/v1/lua/stdlib` → `{version, files: [{name, path, ver, source}]}`。
///
/// 本地 index（`scripts/_stdlib/.index.json`）与 embed 种子的合并视图（index 为准
/// 消歧，§3.6 D5）；`source` = 来源徽标 "synced"（验签安装）/ "seed"（内网冷启动
/// 兜底），`version` = 本地安装版本轴（无 index → null，§3.7）。scripts 根与
/// save/scan 端点一致取 cwd 相对 `scripts`。
#[cfg(feature = "lua")]
pub async fn stdlib_list(State(_st): State<AppState>) -> Json<serde_json::Value> {
    Json(stdlib_list_payload(Path::new("scripts")))
}

/// [`stdlib_list`] 的纯函数主体（无 AppState 依赖，便于单测——本仓无 REST
/// router 级测试基建，逻辑收敛于此做函数级覆盖）。
#[cfg(feature = "lua")]
fn stdlib_list_payload(scripts_root: &Path) -> serde_json::Value {
    let (version, files) = crate::plugin::lua_core::stdlib_sync::inventory(scripts_root);
    let files: Vec<serde_json::Value> = files
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
        .collect();
    serde_json::json!({ "version": version, "files": files })
}

/// `POST /api/v1/lua/stdlib/sync` 请求体（两字段均可省；body 亦可省——不带 JSON
/// content-type 的请求整体取缺省值）。
#[derive(Deserialize, Default)]
pub struct SyncReq {
    /// upstream 基址（省 = `AppConfig::lua_stdlib_upstream`，内网镜像改配置切 D2）。
    pub upstream: Option<String>,
    /// 版本策略（§3.3）：显式版本 = 校验上游恰为该版；`"auto"` = 显式跟随 latest；
    /// 省 = 锁定（`lua_stdlib_pin` 配置键 > 本地已装版本，首装取 latest）。
    pub pin: Option<String>,
}

/// 解析生效 pin（§3.3 默认锁定，阶段 A 审查修复）：body 显式 > 配置键 >
/// 锁定本地已装版本（`_stdlib/.index.json` 的 version；无 index 的首装 → None 跟随
/// latest）。返回 (生效 pin, 语义描述) 供响应与审计日志。
#[cfg(feature = "lua-http")]
fn resolve_effective_pin(
    body_pin: Option<&str>,
    cfg_pin: Option<&String>,
    scripts_root: &Path,
) -> (Option<String>, String) {
    match body_pin {
        Some("auto") => (None, "auto（显式跟随 latest）".into()),
        Some(p) => (Some(p.to_string()), format!("body 显式 pin {p}")),
        None => match cfg_pin
            .cloned()
            .or_else(|| read_installed_version(scripts_root))
        {
            Some(v) => (Some(v.clone()), format!("锁定 {v}（默认 pin 策略）")),
            None => (None, "首装（无本地 index，跟随 latest）".into()),
        },
    }
}

/// 本地已装 stdlib 版本（`_stdlib/.index.json`；缺失 → None）。
#[cfg(feature = "lua-http")]
fn read_installed_version(scripts_root: &Path) -> Option<String> {
    crate::plugin::lua_core::stdlib_sync::read_index(scripts_root).map(|i| i.version)
}

/// `POST /api/v1/lua/stdlib/sync` → 验签同步官方源/镜像（§3.6 D1）+ 全实例 reload（Q11）。
///
/// 流程：拉 index+sig → ed25519 验签 → min_api_ver/pin 预检 → 逐文件 sha256 →
/// `_stdlib.tmp/` → 原子替换 → 返回 `SyncReport`，随后全实例 reload
/// （`lua_core::reload::reload_all_lua_instances`，T10 移入的公共 helper）触发全部
/// 运行中 luaClient 实例的 VM 级热加载（失败自动回退旧 VM，Q11）。任一步失败 →
/// 502（upstream/验签域错误，本地 `_stdlib/` 未动）。成功/失败均落审计日志（§3.7）。
/// 需要 reqwest（feature lua-http 门控，无此 feature 端点缺席）。
#[cfg(feature = "lua-http")]
pub async fn stdlib_sync(State(st): State<AppState>, body: Option<Json<SyncReq>>) -> Response {
    let Json(req) = body.unwrap_or_default();
    let upstream = req
        .upstream
        .clone()
        .unwrap_or_else(|| st.config.lua_stdlib_upstream.clone());
    let (pin, pin_desc) = resolve_effective_pin(
        req.pin.as_deref(),
        st.config.lua_stdlib_pin.as_ref(),
        Path::new("scripts"),
    );
    // §3.7 同步审计日志：who 由 JWT 访问日志侧承载，此处落 what/when/result。
    tracing::info!("[lua][audit] stdlib sync 开始：upstream={upstream}，pin 策略={pin_desc}");
    match crate::plugin::lua_core::stdlib_sync::sync(
        &upstream,
        Path::new("scripts"),
        pin.as_deref(),
    )
    .await
    {
        Ok(report) => {
            let reloaded =
                crate::plugin::lua_core::reload::reload_all_lua_instances(&st.lux, &st.mgr).await;
            tracing::info!(
                "[lua][audit] stdlib sync 成功：{upstream} → v{}（+{}/~{}/-{}），reload 触发 {} 个运行中实例",
                report.version,
                report.added.len(),
                report.updated.len(),
                report.removed.len(),
                reloaded.len()
            );
            Json(serde_json::json!({
                "status": "ok",
                "upstream": upstream,
                "pin": pin,
                "report": report,
                "reloaded": reloaded,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::warn!("[lua][audit] stdlib sync 失败：{upstream}（{pin_desc}）：{e}");
            err(StatusCode::BAD_GATEWAY, &e)
        }
    }
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

    /// 阶段 A 审查修复：保留目录名不可作为脚本名（白名单含 `_`，须显式拒）。
    #[test]
    fn resolve_rejects_reserved_dir_names() {
        for bad in ["_lib", "_stdlib"] {
            assert!(resolve_script_path(bad).is_err(), "保留目录名应拒：{bad}");
        }
    }

    // ---- get_script 资产读回路径（阶段 A 审查修复：lib 可写即可读，Q13）----

    #[test]
    fn asset_read_path_script_and_lib() {
        assert_eq!(
            asset_read_path(None, "devA").unwrap(),
            PathBuf::from("scripts").join("devA").join("main.lua")
        );
        assert_eq!(
            asset_read_path(Some("script"), "devA").unwrap(),
            PathBuf::from("scripts").join("devA").join("main.lua")
        );
        assert_eq!(
            asset_read_path(Some("lib"), "viccom/hex.lua").unwrap(),
            PathBuf::from("scripts")
                .join("_lib")
                .join("viccom")
                .join("hex.lua")
        );
        assert!(asset_read_path(Some("bogus"), "x").is_err());
        // 白名单仍生效：lib 单段 / 撞保留段 / 穿越照拒
        assert!(asset_read_path(Some("lib"), "foo.lua").is_err());
        assert!(asset_read_path(Some("lib"), "algo/evil.lua").is_err());
        assert!(asset_read_path(Some("lib"), "../evil.lua").is_err());
    }

    // ---- sync 生效 pin 解析（§3.3 默认锁定，阶段 A 审查修复）----

    #[cfg(feature = "lua-http")]
    #[test]
    fn effective_pin_resolution_order() {
        let tmp = std::env::temp_dir().join(format!("lua-pin-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("_stdlib")).unwrap();
        std::fs::write(
            tmp.join("_stdlib/.index.json"),
            r#"{"version":"1.2.0","files":{}}"#,
        )
        .unwrap();

        // body "auto" 显式跟随 latest（最高优先）
        let (p, _) = resolve_effective_pin(Some("auto"), Some(&"1.0.0".into()), &tmp);
        assert_eq!(p, None);
        // body 显式版本 > 配置键 / 本地已装
        let (p, _) = resolve_effective_pin(Some("2.0.0"), Some(&"1.0.0".into()), &tmp);
        assert_eq!(p.as_deref(), Some("2.0.0"));
        // 省 body → 配置键持久化 pin
        let (p, _) = resolve_effective_pin(None, Some(&"1.0.0".into()), &tmp);
        assert_eq!(p.as_deref(), Some("1.0.0"));
        // 省 body 且无配置 → 锁定本地已装版本（默认 pin 语义）
        let (p, _) = resolve_effective_pin(None, None, &tmp);
        assert_eq!(p.as_deref(), Some("1.2.0"));
        // 首装（无本地 index）→ 跟随 latest
        let empty = std::env::temp_dir().join(format!("lua-pin-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let (p, _) = resolve_effective_pin(None, None, &empty);
        assert_eq!(p, None);

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&empty);
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

    // ---- stdlib 清单载荷（GET /api/v1/lua/stdlib 的纯函数主体；本仓无 REST
    //      router 级测试基建，端点逻辑收敛于此做函数级覆盖）----

    /// 验证意图（§3.6 D5/§3.7 徽标）：无 index → 全种子 "seed"、version null；
    /// 有 index → index 条目 "synced"（含 ver），种子差集仍 "seed"，同名让位 index。
    #[cfg(feature = "lua")]
    #[test]
    fn stdlib_list_payload_seed_and_synced_badges() {
        let seeds = crate::plugin::lua_core::stdlib::SEED_MODULES.len();
        let tmp = fresh_tmp("stdlib-list");

        // 冷启动：无 _stdlib/.index.json → 纯种子视图
        let payload = stdlib_list_payload(&tmp);
        assert_eq!(payload["version"], serde_json::Value::Null);
        let files = payload["files"].as_array().unwrap();
        assert_eq!(files.len(), seeds, "全部 embed 种子");
        assert!(files.iter().all(|f| f["source"] == "seed"));
        assert!(
            files.iter().all(|f| f["ver"].is_null()),
            "种子无版本轴 → ver null"
        );

        // 有 index：algo/crc16.lua 转 synced（种子同名让位），其余仍 seed
        std::fs::create_dir_all(tmp.join("_stdlib/algo")).unwrap();
        std::fs::write(
            tmp.join("_stdlib/.index.json"),
            r#"{"version":"1.0.0","min_api_ver":1,"files":{"algo/crc16.lua":{"sha256":"ab","ver":"1.0.0"}}}"#,
        )
        .unwrap();
        let payload = stdlib_list_payload(&tmp);
        assert_eq!(payload["version"], "1.0.0");
        let files = payload["files"].as_array().unwrap();
        assert_eq!(files.len(), seeds, "1 synced + 种子差集");
        let crc = files
            .iter()
            .find(|f| f["path"] == "algo/crc16.lua")
            .expect("index 条目应在列");
        assert_eq!(crc["source"], "synced");
        assert_eq!(crc["ver"], "1.0.0");
        assert_eq!(crc["name"], "algo.crc16");
        assert_eq!(
            files.iter().filter(|f| f["source"] == "seed").count(),
            seeds - 1,
            "同名种子让位 index（消歧）"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- T10：kind=lib 站点共享库（Q13/Q15，D11 保留段拒绝）----
    // 本仓无 REST router 级测试基建（T9 先例）：端点逻辑收敛为纯函数在此做函数级
    // 覆盖，端点层为编译级 + clippy；四断言对应计划 Task 10 Step 1。

    /// 四断言之「合法路径」：`<org>/<name>.lua` ≥2 段 → `scripts/_lib/<org>/<name>.lua`；
    /// 第三段起任意深度（§3.1 共享层嵌套库）。
    #[test]
    fn lib_path_accepts_org_slash_name() {
        assert_eq!(
            resolve_lib_path("viccom/hex.lua").unwrap(),
            PathBuf::from("scripts/_lib/viccom/hex.lua")
        );
        assert_eq!(
            resolve_lib_path("viccom/calcex/poller.lua").unwrap(),
            PathBuf::from("scripts/_lib/viccom/calcex/poller.lua")
        );
    }

    /// 四断言之「保留段拒绝」（D11 封闭集）：首段撞官方保留段必须拒——否则第三方可
    /// 写入与 `_stdlib/` 同名的假命名空间，require 语义混淆。遍历封闭集全量断言，
    /// 与 loader::RESERVED_TOP 单一事实源联动。
    #[cfg(feature = "lua")]
    #[test]
    fn lib_path_rejects_reserved_top() {
        for r in crate::plugin::lua_core::loader::RESERVED_TOP {
            assert!(
                resolve_lib_path(&format!("{r}/evil.lua")).is_err(),
                "保留段首段应被拒：{r}"
            );
        }
    }

    /// 四断言之「单段拒绝」（D11 共享层 ≥2 段）：单段 `foo.lua` 是私有层命名，
    /// 进不了 `_lib/`。
    #[test]
    fn lib_path_rejects_single_segment() {
        assert!(resolve_lib_path("foo.lua").is_err());
    }

    /// 四断言之「防穿越/形态」：`..`、反斜杠、绝对路径、非 `.lua` 后缀、空段、
    /// 空格、段内点号——与 script 侧白名单同规则。
    #[test]
    fn lib_path_rejects_traversal_and_bad_forms() {
        for bad in [
            "",
            "../evil.lua",
            "viccom/../evil.lua",
            "..\\evil.lua",
            "viccom\\hex.lua",
            "/abs/hex.lua",
            "viccom/.lua",
            "viccom/hex.txt",
            "viccom/hex",
            "viccom/",
            "viccom/a.b/c.lua",
            "vi ccom/hex.lua",
        ] {
            assert!(resolve_lib_path(bad).is_err(), "应拒绝：{bad:?}");
        }
    }

    /// 四断言之「保存落位 + 版本快照」（save_lib_under，函数级等价 save 端点 lib 分支）：
    /// 文件落 `scripts/_lib/<org>/<name>.lua`；覆盖前旧内容进
    /// `_lib/<org>/versions/<ts>.lua`（对齐 script 快照目录惯例）。
    #[test]
    fn lib_save_writes_file_and_archives_version() {
        let tmp = fresh_tmp("lib-save");
        let root = tmp.join("scripts");

        // 首存：落位 + 不产生版本
        let (path, archived) = save_lib_under(&root, "viccom/hex.lua", "V1").unwrap();
        assert_eq!(path, root.join("_lib/viccom/hex.lua"));
        assert!(!archived, "首存（无旧文件）不存版本");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "V1");

        // 二存：旧内容进 _lib/<org>/versions/，新内容落位
        let (_, archived) = save_lib_under(&root, "viccom/hex.lua", "V2").unwrap();
        assert!(archived, "覆盖应存版本快照");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "V2");
        let vers = list_version_files(&root.join("_lib/viccom/versions"));
        assert_eq!(vers.len(), 1, "应产生 1 个版本快照");
        let ts = vers[0]["ts"].as_u64().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("_lib/viccom/versions").join(format!("{ts}.lua")))
                .unwrap(),
            "V1",
            "快照应是覆盖前的旧内容"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 非法 lib 名在保存核心同门被拒（校验先于任何 fs 副作用）。
    #[test]
    fn lib_save_rejects_invalid_names() {
        let tmp = fresh_tmp("lib-save-reject");
        for bad in ["foo.lua", "algo/evil.lua", "../evil.lua"] {
            assert!(save_lib_under(&tmp, bad, "x").is_err(), "应拒绝：{bad}");
        }
        assert!(!tmp.join("_lib").exists(), "拒绝的保存不应建 _lib 目录");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// T10 kind=lib 列表（scan_libs）：Recursive 收集 `.lua` 相对路径（排序），
    /// 排除 versions/ 快照目录与非 lua 文件。
    #[test]
    fn scan_libs_lists_lua_files_excluding_versions() {
        let tmp = fresh_tmp("lib-scan");
        let lib = tmp.join("_lib");
        std::fs::create_dir_all(lib.join("viccom/deep")).unwrap();
        std::fs::write(lib.join("viccom/hex.lua"), "x").unwrap();
        std::fs::write(lib.join("viccom/deep/mod.lua"), "x").unwrap();
        std::fs::create_dir_all(lib.join("acme/versions")).unwrap();
        std::fs::write(lib.join("acme/versions/123.lua"), "old").unwrap(); // 快照不列
        std::fs::write(lib.join("acme/notes.txt"), "x").unwrap(); // 非 lua 不列

        let got = scan_libs(&lib);
        assert_eq!(
            got,
            vec![
                "viccom/deep/mod.lua".to_string(),
                "viccom/hex.lua".to_string()
            ],
            "字典序 + 排除 versions/快照与非 lua"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// scan_libs 缺目录 → 空（站点无共享库是常态）。
    #[test]
    fn scan_libs_missing_root_is_empty() {
        assert!(scan_libs(Path::new("/no/such/rs-iot-lua-libs-xyz")).is_empty());
    }

    /// T10 kind=lib 回滚核心（rollback_target，script/lib 两 kind 共用）：
    /// `versions/<ts>.lua` → 目标文件；回滚前当前内容存快照（保留回滚点）；
    /// 版本不存在 → NotFound。
    #[test]
    fn rollback_target_restores_lib_version() {
        let tmp = fresh_tmp("lib-rollback");
        let target = tmp.join("_lib/viccom/hex.lua");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "OLD").unwrap();
        std::fs::create_dir_all(tmp.join("_lib/viccom/versions")).unwrap();
        std::fs::write(tmp.join("_lib/viccom/versions/100.lua"), "ANCIENT").unwrap();

        rollback_target(&target, 100).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ANCIENT");

        // 回滚点：OLD 应作为新快照存在（可再回滚回去）
        let vers = list_version_files(&tmp.join("_lib/viccom/versions"));
        let newest = vers[0]["ts"].as_u64().unwrap(); // 降序，最新在前
        let snap = std::fs::read_to_string(
            tmp.join("_lib/viccom/versions")
                .join(format!("{newest}.lua")),
        )
        .unwrap();
        assert_eq!(snap, "OLD", "回滚前当前内容应存快照");

        // 版本不存在 → NotFound 分类（端点映射 404）
        assert!(matches!(
            rollback_target(&target, 999),
            Err(RollbackErr::NotFound)
        ));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
