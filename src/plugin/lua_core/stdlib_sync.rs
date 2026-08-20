//! stdlib 本地 index + 官方源同步客户端（设计 §3.5/§3.6 D1/D2，计划 T9）。
//!
//! - **本地 index**：`scripts/_stdlib/.index.json`（sync 落盘，原子随目录替换）——
//!   本地安装的模块/版本/hash，是 `stdlib.list()/info()` 只读 API、REST 来源徽标
//!   （§3.7）与 T11 manifest dry-run 版本校验的数据源。
//! - **同步客户端**（[`sync_with_fetcher`]）：GET `{upstream}/index.json` +
//!   `index.json.sig` → **ed25519 验签**（公钥 [`VERIFY_KEYS`] 编译进宿主，镜像被
//!   劫持无法注入，§3.7）→ `min_api_ver` 预检（超前版本拒装，§3.3 兼容矩阵）→
//!   pin 校验（默认锁定版本策略）→ 逐文件 sha256 对比下载 → 写 `_stdlib.tmp/` →
//!   **原子替换** `_stdlib/` → 返回 [`SyncReport`]。
//!
//! # 格式约定（定死，T13 官方源仓脚本须对齐）
//!
//! - upstream 布局 = **静态目录根**：`index.json`、`index.json.sig` 与库文件
//!   （`algo/crc16.lua` 等相对路径）同层；D2 只读镜像 = 该目录完整拷贝，改
//!   `lua_stdlib_upstream` 指向镜像即切换。
//! - `index.json.sig` = **裸 64 字节 ed25519 签名**（对 index.json 原始字节签名；
//!   容忍末尾单个 LF/CRLF——文本编辑器/CI 常见。不做通用空白 trim：签名末字节
//!   本身可能落在空白区间，通用 trim 会吃掉真签名字节）。不用 base64/hex 包装。
//! - `sha256` = 小写 hex 字符串。
//!
//! # 实现取舍
//!
//! - 文件 IO 直接同步执行（`std::fs`）：stdlib 文件量小（§3.6 D5 预算几十库），
//!   sync 是管理员低频操作，不值得 spawn_blocking 的复杂度。
//! - Windows 上 `rename` 到已存在目录会失败（ERROR_ALREADY_EXISTS），原子替换用
//!   两步换：先 `_stdlib` → `_stdlib.old.<ts>`，再 `_stdlib.tmp` → `_stdlib`，
//!   成功后删备份；第二步失败自动回滚（见 [`atomic_swap`]）。
//! - 下载经注入的 fetch 闭包（[`sync_with_fetcher`]）：生产 reqwest（[`sync`]，
//!   feature lua-http），测试本地目录副本——reqwest 不支持 `file://` 的务实解。

#[cfg(feature = "lua-http")]
use crate::plugin::lua_core::capability::CUR_API_VER;
use crate::plugin::lua_core::stdlib::SEED_MODULES;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 内置验签公钥（§3.5/§3.7：ed25519 公钥编译进 rs-iot，支持多公钥轮换预留）。
///
/// v1 = 官方源仓 rsiot-lualib 的发布公钥（T13 回填；私钥仓外保管 + CI
/// secret，绝不入任何仓）。测试 `verify_keys_official_v1_locked` 以 hex
/// 常量锁字节防手误。
// 官方钥 v1 (2026-08-19)，轮换走双钥过渡（Q17）：新钥追加进数组，全网升级后移除旧钥。
#[cfg(feature = "lua-http")]
pub(crate) const VERIFY_KEYS: &[[u8; 32]] = &[[
    0x51, 0xdb, 0x92, 0x80, 0xde, 0xbf, 0x69, 0x53, 0x7f, 0xdb, 0xed, 0xa8, 0xdf, 0x7a, 0xca, 0x3e,
    0xa9, 0x71, 0x4b, 0x11, 0x77, 0xac, 0x10, 0x44, 0x38, 0xa1, 0x99, 0x6f, 0xcf, 0xcd, 0xd9, 0x5a,
]];

/// index 单文件条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileEntry {
    /// 文件 sha256（小写 hex）。
    pub(crate) sha256: String,
    /// 该文件所属库版本（跟随 index 发布版本）。
    pub(crate) ver: String,
}

/// 本地/上游共享的 index 结构（§3.5：`_stdlib/.index.json` 与 `{upstream}/index.json` 同构）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocalIndex {
    /// index 版本（semver 字符串，本地安装的 stdlib 版本轴，§3.3）。
    pub(crate) version: String,
    /// 该版 stdlib 依赖的最低宿主 API 版本（§3.3 兼容矩阵，sync 预检）。
    #[serde(default = "default_min_api_ver")]
    pub(crate) min_api_ver: u32,
    /// 文件清单：相对路径（`algo/crc16.lua`）→ 条目。
    pub(crate) files: BTreeMap<String, FileEntry>,
}

fn default_min_api_ver() -> u32 {
    1
}

/// 同步结果报告（REST 响应体；added/updated/removed 为文件路径列表）。
#[cfg(feature = "lua-http")]
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncReport {
    pub(crate) added: Vec<String>,
    pub(crate) updated: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) version: String,
}

/// 读本地 index（`scripts/_stdlib/.index.json`）；不存在/损坏返回 None（种子兜底态）。
pub(crate) fn read_index(scripts_root: &Path) -> Option<LocalIndex> {
    let bytes = std::fs::read(scripts_root.join("_stdlib").join(".index.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 验签并解析 index（§3.7 供应链对策）。
///
/// `sig_file` 为裸 64 字节 ed25519 签名（容忍末尾单个 LF/CRLF）；逐 key 试验签，
/// 任一通过即信任，随后才反序列化（fail closed：签名不覆盖的 JSON 不消费）。
#[cfg(feature = "lua-http")]
pub(crate) fn verify_index(
    bytes: &[u8],
    sig_file: &[u8],
    keys: &[[u8; 32]],
) -> Result<LocalIndex, String> {
    use ed25519_dalek::{Signature, VerifyingKey};
    // 恰 64 字节 → 原样即签名（末字节可能本身落在空白区间，不能再剥）；
    // 超出 → 只剥末尾单个 LF/CRLF（CI/编辑器常见产物），剥后须恰 64
    let raw: &[u8] = if sig_file.len() == 64 {
        sig_file
    } else {
        sig_file
            .strip_suffix(b"\r\n")
            .or_else(|| sig_file.strip_suffix(b"\n"))
            .unwrap_or(sig_file)
    };
    let arr: [u8; 64] = raw.try_into().map_err(|_| {
        format!(
            "index.json.sig 长度 {} 非 64（裸 ed25519 签名格式，见模块文档）",
            raw.len()
        )
    })?;
    let sig = Signature::from_bytes(&arr);
    let mut verified = false;
    for k in keys {
        let vk = VerifyingKey::from_bytes(k).map_err(|e| format!("内置公钥字节非法: {e}"))?;
        if vk.verify_strict(bytes, &sig).is_ok() {
            verified = true;
            break;
        }
    }
    if !verified {
        return Err(
            "index 验签失败：无内置公钥通过（镜像可能被篡改，或宿主公钥过旧，§3.7）".into(),
        );
    }
    serde_json::from_slice(bytes).map_err(|e| format!("index.json 解析失败: {e}"))
}

/// sync 主体（fetcher 与验签公钥双注入点）：生产 [`sync`] 用 reqwest +
/// [`VERIFY_KEYS`]，测试注入本地目录闭包与 dev key（官方私钥在仓外，测试
/// 无法用它签名——参数化 keys 是唯一不把 dev 钥留在生产信任锚里的走法）。
///
/// 步骤（§3.6 D1）：拉 index+sig → 验签 → `min_api_ver` 预检 → pin 校验 →
/// 逐文件 sha 对比（本地已符则复拷，否则下载+校验）→ 写 `_stdlib.tmp/`（含
/// `.index.json`）→ [`atomic_swap`] 两步换 → [`SyncReport`]。任一步失败即 Err，
/// 不动旧 `_stdlib/`（下载/sha 失败时清理 tmp）。
///
/// 进程级互斥：两管理员并发 sync 会互删对方的 `_stdlib.tmp`（阶段 A 审查
/// 修复）——低频操作，全局串行化无吞吐代价。
///
/// 文件量小（D5 预算几十库）且为管理员低频操作，`std::fs` 同步 IO 直接执行
/// （不 spawn_blocking，见模块文档"实现取舍"）。
#[cfg(feature = "lua-http")]
pub(crate) async fn sync_with_fetcher<F, Fut>(
    upstream_base: &str,
    scripts_root: &Path,
    pin: Option<&str>,
    keys: &[[u8; 32]],
    fetch: F,
) -> Result<SyncReport, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    static SYNC_MUTEX: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = SYNC_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let base = upstream_base.trim_end_matches('/');
    let idx_bytes = fetch(format!("{base}/index.json"))
        .await
        .map_err(|e| format!("拉取 index.json: {e}"))?;
    let sig_bytes = fetch(format!("{base}/index.json.sig"))
        .await
        .map_err(|e| format!("拉取 index.json.sig: {e}"))?;
    let index = verify_index(&idx_bytes, &sig_bytes, keys)?;

    // 兼容矩阵预检（§3.3）：宿主过旧拒装，保持旧版继续运行
    if index.min_api_ver > CUR_API_VER {
        return Err(format!(
            "上游 min_api_ver={} > 宿主 CUR_API_VER={}，拒装（请保持旧版或升级宿主，§3.3）",
            index.min_api_ver, CUR_API_VER
        ));
    }
    // pin 策略（§3.3 默认 pin 锁定）：版本轴不匹配拒绝
    if let Some(p) = pin {
        if index.version != p {
            return Err(format!("pin 版本不匹配：上游 {} ≠ pin {p}", index.version));
        }
    }

    let old = read_index(scripts_root);
    let stdlib_dir = scripts_root.join("_stdlib");
    let tmp = scripts_root.join("_stdlib.tmp");
    let _ = std::fs::remove_dir_all(&tmp); // 清理上次失败残留
    std::fs::create_dir_all(&tmp).map_err(|e| format!("建 _stdlib.tmp: {e}"))?;

    for (path, entry) in &index.files {
        safe_rel_path(path)?;
        let dest = tmp.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建目录 {path}: {e}"))?;
        }
        let declared = entry.sha256.to_lowercase();
        // 本地文件已符合声明 sha → 复拷（免下载）；否则下载并校验
        let local_sha = std::fs::read(stdlib_dir.join(path))
            .ok()
            .map(|b| sha256_hex(&b));
        if local_sha.as_deref() == Some(declared.as_str()) {
            std::fs::copy(stdlib_dir.join(path), &dest)
                .map_err(|e| format!("复拷本地 {path}: {e}"))?;
        } else {
            let content = fetch(format!("{base}/{path}"))
                .await
                .map_err(|e| format!("下载 {path}: {e}"))?;
            let actual = sha256_hex(&content);
            if actual != declared {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(format!(
                    "{path} sha256 不符：index 声明 {declared}，实际 {actual}（内容被篡改，拒装）"
                ));
            }
            std::fs::write(&dest, &content).map_err(|e| format!("写 {path}: {e}"))?;
        }
    }

    // .index.json 进 tmp，随目录一并原子替换（index 与文件永不失配）
    let idx_json = serde_json::to_vec(&index).map_err(|e| format!("序列化 index: {e}"))?;
    std::fs::write(tmp.join(".index.json"), &idx_json)
        .map_err(|e| format!("写 .index.json: {e}"))?;
    atomic_swap(&tmp, &stdlib_dir)?;

    // 报告：新旧 index 差集
    let old_files = old.map(|i| i.files).unwrap_or_default();
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut removed = Vec::new();
    for (p, e) in &index.files {
        match old_files.get(p) {
            None => added.push(p.clone()),
            Some(oe) if oe.sha256.to_lowercase() != e.sha256.to_lowercase() => {
                updated.push(p.clone())
            }
            _ => {}
        }
    }
    for p in old_files.keys() {
        if !index.files.contains_key(p) {
            removed.push(p.clone());
        }
    }
    Ok(SyncReport {
        added,
        updated,
        removed,
        version: index.version,
    })
}

/// 生产 sync 薄包装：reqwest fetcher（10s 超时；进程内 client 复用可后置——
/// sync 低频，每次新建 client 的成本可忽略）。
#[cfg(feature = "lua-http")]
pub(crate) async fn sync(
    upstream_base: &str,
    scripts_root: &Path,
    pin: Option<&str>,
) -> Result<SyncReport, String> {
    sync_with_fetcher(upstream_base, scripts_root, pin, VERIFY_KEYS, http_fetch).await
}

/// reqwest GET（feature lua-http；无 lua-http 编译时 sync 端点一并缺席）。
#[cfg(feature = "lua-http")]
async fn http_fetch(url: String) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("构建 HTTP client: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("GET {url} 读响应体: {e}"))?;
    Ok(body.to_vec())
}

/// index 条目路径闸门：仅接受仓库内相对 `.lua` 路径（防 index 本身被构造出穿越）。
///
/// `:` 拒绝与 loader.rs `route()` 同因：Windows 上 `Path::join` 遇盘符前缀段会
/// 整体替换基路径。此处属纵深防御（index 已验签，正常不可达此分支）。
#[cfg(feature = "lua-http")]
fn safe_rel_path(p: &str) -> Result<(), String> {
    if p.is_empty() || !p.ends_with(".lua") {
        return Err(format!("index 条目 {p:?} 非 .lua 文件"));
    }
    if p.starts_with('/') || p.contains('\\') || p.contains("..") || p.contains(':') {
        return Err(format!(
            "index 条目 {p:?} 路径违规（穿越/绝对路径/盘符冒号）"
        ));
    }
    if p.split('/').any(|seg| seg.is_empty()) {
        return Err(format!("index 条目 {p:?} 含空路径段"));
    }
    Ok(())
}

/// sha256 → 小写 hex。
#[cfg(feature = "lua-http")]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 目录原子替换（Windows 两步换）：目标存在时先 rename 到 `_stdlib.old.<ts>` 备份，
/// 再 rename tmp 就位；第二步失败自动回滚备份，成功后删备份。
///
/// Windows `rename` 到已存在目录返回 ERROR_ALREADY_EXISTS（POSIX 则是原子覆盖），
/// 故必须两步；备份名用纳秒时间戳防同秒冲突，残留的旧备份在下次 sync 前清理。
#[cfg(feature = "lua-http")]
fn atomic_swap(tmp: &Path, target: &Path) -> Result<(), String> {
    if !target.exists() {
        return std::fs::rename(tmp, target).map_err(|e| format!("_stdlib 落盘失败: {e}"));
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let backup = target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("_stdlib.old.{ts}"));
    let _ = std::fs::remove_dir_all(&backup); // 清历史残留
    std::fs::rename(target, &backup).map_err(|e| format!("旧 _stdlib 移出失败: {e}"))?;
    if let Err(e) = std::fs::rename(tmp, target) {
        let _ = std::fs::rename(&backup, target); // 回滚：新目录未就位前旧目录必须回去
        return Err(format!("_stdlib 替换失败（已回滚旧目录）: {e}"));
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(())
}

/// `stdlib` 全局表（§3.5）：`stdlib.list()` / `stdlib.info(mod)`——本地 index 与
/// embed 种子的合并只读视图（index 为准消歧，§3.6 D5）。调用时即时读盘，sync 后
/// reload 即见新版。
pub(crate) fn stdlib_table(lua: &mlua::Lua, scripts_root: &Path) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    let root_list = scripts_root.to_path_buf();
    let list_fn = lua.create_function(move |lua, ()| {
        let (_, entries) = inventory(&root_list);
        let arr = lua.create_table()?;
        for (i, e) in entries.iter().enumerate() {
            arr.set(i + 1, entry_to_lua(lua, e)?)?;
        }
        Ok(arr)
    })?;
    t.set("list", list_fn)?;
    let root_info = scripts_root.to_path_buf();
    let info_fn = lua.create_function(move |lua, name: String| {
        let (_, entries) = inventory(&root_info);
        // 支持点分模块名（algo.crc16）或文件路径（algo/crc16.lua）两种写法
        match entries
            .into_iter()
            .find(|e| e.name == name || e.path == name)
        {
            Some(e) => Ok(mlua::Value::Table(entry_to_lua(lua, &e)?)),
            None => Ok(mlua::Value::Nil),
        }
    })?;
    t.set("info", info_fn)?;
    Ok(t)
}

/// [`InventoryEntry`] → Lua 表 `{name, path, ver, source}`（种子无 ver → nil）。
fn entry_to_lua(lua: &mlua::Lua, e: &InventoryEntry) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("name", e.name.as_str())?;
    t.set("path", e.path.as_str())?;
    t.set("ver", e.ver.as_deref())?;
    t.set("source", e.source)?;
    Ok(t)
}

/// 清单条目（`stdlib.list()` 与 REST `GET /lua/stdlib` 共用；source = §3.7 来源徽标）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InventoryEntry {
    /// 点分模块名（`algo.crc16`）。
    pub(crate) name: String,
    /// 文件相对路径（index 键，`algo/crc16.lua`）。
    pub(crate) path: String,
    /// 库版本（种子无版本轴 → None）。
    pub(crate) ver: Option<String>,
    /// 来源徽标："synced"（index 安装）| "seed"（embed 种子兜底）。
    pub(crate) source: &'static str,
}

/// 本地清单：index 文件（source="synced"，含 ver）+ 种子差集（source="seed"）。
///
/// 本地 index 存在时以 index 为准消歧（§3.6 D5）——同名文件种子不再重复列出；
/// index 缺失/损坏 → 纯种子视图（内网冷启动兜底态）。返回 (版本, 条目)。
///
/// `scripts/` 前缀条目（官方脚本模板，D1 分发面）不列入：模板不是 require 可达
/// 的 stdlib 模块（首段 `scripts` 非保留段，路由进不了 `_stdlib/`），列出即假模块
/// （阶段 A 审查修复）。模板安装器（装 `scripts/<name>/` + `@source` 徽标，Q18）
/// 登记为 B 阶段遗留——现状模板仅分发落盘 `_stdlib/scripts/` 供查阅。
pub(crate) fn inventory(scripts_root: &Path) -> (Option<String>, Vec<InventoryEntry>) {
    let index = read_index(scripts_root);
    let synced: std::collections::HashSet<&str> = index
        .iter()
        .flat_map(|i| i.files.keys())
        .filter(|p| !p.starts_with("scripts/"))
        .map(|s| s.as_str())
        .collect();
    let mut out: Vec<InventoryEntry> = index
        .iter()
        .flat_map(|i| i.files.iter())
        .filter(|(p, _)| !p.starts_with("scripts/"))
        .map(|(path, e)| InventoryEntry {
            name: path.strip_suffix(".lua").unwrap_or(path).replace('/', "."),
            path: path.clone(),
            ver: Some(e.ver.clone()),
            source: "synced",
        })
        .collect();
    for seed in SEED_MODULES {
        let path = format!("{seed}.lua");
        if !synced.contains(path.as_str()) {
            out.push(InventoryEntry {
                name: seed.replace('/', "."),
                path,
                ver: None,
                source: "seed",
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    (index.map(|i| i.version), out)
}

/// 测试整体挂 lua-http：主体（验签/sync/atomic_swap）均在 `sync_with_fetcher`
/// 门控内，default features 含 lua-http，CI/本地默认跑全量覆盖。
#[cfg(all(test, feature = "lua-http"))]
mod tests {
    use super::*;
    use crate::plugin::lua_core::config::parse_cfg;
    use crate::plugin::lua_core::engine::new_vm;
    use crate::plugin::lua_core::script::{load_main, LoadCtx};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 测试 upstream 基址（假 scheme：dir_fetcher 按前缀映射到本地目录）。
    const UPSTREAM: &str = "testup";

    /// 确定性 dev keypair（测试签名用；官方钥 v1 已回填 VERIFY_KEYS，dev 钥
    /// 只在测试里经注入的 keys 参数使用，绝不进生产信任锚——它能被任何人
    /// 重derive，留在 VERIFY_KEYS 里等于供应链后门）。
    fn dev_keypair() -> (ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    /// 测试注入的验签公钥集（dev key 单钥，喂 [`sync_with_fetcher`] 的 keys 参数）。
    fn dev_keys() -> [[u8; 32]; 1] {
        [dev_keypair().1.to_bytes()]
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 造本地 upstream 目录：写文件 + index.json（真实 sha，`lie_sha_for` 声明假 sha）
    /// + index.json.sig（dev key 裸 64 字节 + 尾随换行，锁容忍逻辑）。
    fn write_upstream(
        dir: &Path,
        version: &str,
        min_api_ver: u32,
        files: &[(&str, &str)],
        lie_sha_for: Option<&str>,
    ) {
        use ed25519_dalek::Signer;
        fs::create_dir_all(dir).unwrap();
        let mut files_json = serde_json::Map::new();
        for (p, content) in files {
            fs::create_dir_all(dir.join(Path::new(p).parent().unwrap())).unwrap();
            fs::write(dir.join(p), content).unwrap();
            let sha = if Some(*p) == lie_sha_for {
                "0".repeat(64)
            } else {
                sha256_hex(content.as_bytes())
            };
            files_json.insert(p.to_string(), json!({"sha256": sha, "ver": version}));
        }
        let idx = json!({"version": version, "min_api_ver": min_api_ver, "files": files_json});
        let bytes = serde_json::to_vec(&idx).unwrap();
        fs::write(dir.join("index.json"), &bytes).unwrap();
        let (sk, _) = dev_keypair();
        let mut sig_file = sk.sign(&bytes).to_bytes().to_vec();
        sig_file.push(b'\n');
        fs::write(dir.join("index.json.sig"), &sig_file).unwrap();
    }

    /// 本地目录 fetcher：URL `testup/<rel>` → 读 `base/<rel>`（reqwest 不支持
    /// file:// 的务实替代，见 [`sync_with_fetcher`] 文档）。
    fn dir_fetcher(
        base: PathBuf,
    ) -> impl Fn(String) -> std::future::Ready<Result<Vec<u8>, String>> {
        move |url: String| {
            std::future::ready((|| -> Result<Vec<u8>, String> {
                let rel = url
                    .strip_prefix(UPSTREAM)
                    .and_then(|r| r.strip_prefix('/'))
                    .ok_or_else(|| format!("测试 fetcher 只认 {UPSTREAM}/ 前缀：{url}"))?;
                fs::read(base.join(rel)).map_err(|e| format!("读 {rel}: {e}"))
            })())
        }
    }

    /// 唯一临时 scripts 根（防并发测试互扰），含 devA/main.lua（require algo.demo）。
    fn fresh_scripts(tag: &str, main_lua: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "lua-stdlibsync-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("devA")).unwrap();
        fs::write(p.join("devA/main.lua"), main_lua).unwrap();
        p
    }

    /// T9 计划 Step1-1：index 验签通过；篡改 1 字节拒（镜像被劫持场景，§3.7）。
    #[test]
    fn index_verifies_and_rejects_tamper() {
        let (sk, vk) = dev_keypair();
        use ed25519_dalek::Signer;
        let bytes =
            br#"{"version":"1.0.0","files":{"algo/crc16.lua":{"sha256":"ab","ver":"1.0.0"}}}"#;
        let sig = sk.sign(bytes).to_bytes();
        let idx = verify_index(bytes, &sig, &[vk.to_bytes()]).expect("正确签名应通过");
        assert_eq!(idx.version, "1.0.0");
        assert_eq!(idx.min_api_ver, 1, "缺省 min_api_ver=1");
        assert_eq!(idx.files.len(), 1);
        // 篡改 index 内容（签名不再覆盖）→ 拒
        let mut bad = bytes.to_vec();
        bad[3] ^= 1;
        assert!(
            verify_index(&bad, &sig, &[vk.to_bytes()]).is_err(),
            "篡改 1 字节应验签失败"
        );
        // 非 64 字节签名 → 拒（格式闸门）
        assert!(verify_index(bytes, b"short", &[vk.to_bytes()]).is_err());
    }

    /// T13 官方钥回填锁：VERIFY_KEYS[0] = rsiot-lualib v1 发布公钥（hex 常量与
    /// 源仓 pub.key 同源；轮换时追加新钥并同步更新此锁，Q17 双钥过渡）。
    #[test]
    fn verify_keys_official_v1_locked() {
        const OFFICIAL_V1_HEX: &str =
            "51db9280debf69537fdbeda8df7aca3ea9714b1177ac104438a1996fcfcdd95a";
        let got: String = VERIFY_KEYS[0].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            got, OFFICIAL_V1_HEX,
            "官方钥 v1 字节被改动须两侧同步（rsiot-lualib pub.key）"
        );
        assert_eq!(VERIFY_KEYS.len(), 1, "v1 期单钥；追加钥须走 Q17 双钥过渡");
    }

    /// T9 计划 Step1-2：index 声明 sha 与文件实际不符 → Err 含 "sha256"（供应链闸门）。
    #[tokio::test]
    async fn sync_rejects_bad_file_hash() {
        let up = fresh_scripts("up-badsha", ""); // 复用唯一目录生成
        write_upstream(
            &up,
            "1.0.0",
            1,
            &[("algo/demo.lua", "return { marker = 'x' }")],
            Some("algo/demo.lua"),
        );
        let root = fresh_scripts("badsha", "");
        let r =
            sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone())).await;
        let e = r.expect_err("坏 sha 应拒");
        assert!(e.contains("sha256"), "错误应含 sha256，实得：{e}");
        assert!(!root.join("_stdlib.tmp").exists(), "失败后应清理 tmp 目录");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// T9 原子替换（Windows 两步换是关键路径）：首次 sync 落文件 + `.index.json`，
    /// 二次 sync（新版本 + 内容变更）走"目标已存在"分支照样替换成功；替换后
    /// `read_index` 返回新版本且 require 命中 `_stdlib/` 文件（script.rs 文件优先）。
    #[tokio::test]
    async fn sync_atomic_swap_and_require() {
        let up = fresh_scripts("up-swap", "");
        let root = fresh_scripts("swap", "marker = require('algo.demo').marker");

        // 首次：1.0.0
        write_upstream(
            &up,
            "1.0.0",
            1,
            &[("algo/demo.lua", "return { marker = 'v1' }")],
            None,
        );
        let r1 = sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone()))
            .await
            .expect("首次 sync");
        assert_eq!(r1.version, "1.0.0");
        assert_eq!(r1.added, vec!["algo/demo.lua".to_string()], "全量新增");
        assert!(r1.updated.is_empty() && r1.removed.is_empty());
        let idx = read_index(&root).expect("sync 后应有本地 index");
        assert_eq!(idx.version, "1.0.0");
        assert!(!root.join("_stdlib.tmp").exists(), "不留 tmp");

        // 二次：2.0.0 内容变更 → Windows 上 rename 需先移走旧目录（两步换）
        write_upstream(
            &up,
            "2.0.0",
            1,
            &[("algo/demo.lua", "return { marker = 'v2' }")],
            None,
        );
        let r2 = sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone()))
            .await
            .expect("二次 sync（目标已存在，两步换）");
        assert_eq!(r2.version, "2.0.0");
        assert!(r2.added.is_empty(), "无新增");
        assert_eq!(
            r2.updated,
            vec!["algo/demo.lua".to_string()],
            "内容变更计 updated"
        );
        assert_eq!(
            read_index(&root).expect("index 应随目录原子替换").version,
            "2.0.0"
        );
        assert!(!root.join("_stdlib.tmp").exists(), "两步换后不留 tmp");
        let old_dirs: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("_stdlib.old"))
            .collect();
        assert!(
            old_dirs.is_empty(),
            "两步换成功后应删备份，实得 {old_dirs:?}"
        );

        // require 命中 `_stdlib/algo/demo.lua`（script.rs 文件优先于种子）
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(root.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        assert_eq!(
            lua.load("return marker").eval::<String>().unwrap(),
            "v2",
            "require 应取到替换后的新文件"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// pin 策略（§3.3 默认 pin 锁定）：上游版本 ≠ pin → 拒（"pin 版本不匹配"）。
    #[tokio::test]
    async fn sync_rejects_pin_mismatch() {
        let up = fresh_scripts("up-pin", "");
        write_upstream(&up, "1.0.0", 1, &[("algo/demo.lua", "return {}")], None);
        let root = fresh_scripts("pin", "");
        let r = sync_with_fetcher(
            UPSTREAM,
            &root,
            Some("9.9.9"),
            &dev_keys(),
            dir_fetcher(up.clone()),
        )
        .await;
        let e = r.expect_err("pin 不匹配应拒");
        assert!(e.contains("pin 版本不匹配"), "实得：{e}");
        // pin 匹配则放行
        sync_with_fetcher(
            UPSTREAM,
            &root,
            Some("1.0.0"),
            &dev_keys(),
            dir_fetcher(up.clone()),
        )
        .await
        .expect("pin 匹配应成功");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// 兼容矩阵预检（§3.3）：上游 min_api_ver > 宿主 CUR_API_VER → 拒装。
    #[tokio::test]
    async fn sync_rejects_future_min_api_ver() {
        let up = fresh_scripts("up-apiver", "");
        write_upstream(&up, "1.0.0", 99, &[("algo/demo.lua", "return {}")], None);
        let root = fresh_scripts("apiver", "");
        let r =
            sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone())).await;
        let e = r.expect_err("超前 min_api_ver 应拒装");
        assert!(e.contains("拒装"), "实得：{e}");
        assert!(read_index(&root).is_none(), "拒装不留 index");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// 篡改 index（签名不覆盖）经 sync 全链路也拒——验签是 sync 的第一道闸。
    #[tokio::test]
    async fn sync_rejects_tampered_index() {
        let up = fresh_scripts("up-tamper", "");
        write_upstream(&up, "1.0.0", 1, &[("algo/demo.lua", "return {}")], None);
        // 篡改 index.json 一个字节（保留原签名）
        let bytes = fs::read(up.join("index.json")).unwrap();
        let mut bad = bytes.clone();
        bad[10] ^= 1;
        fs::write(up.join("index.json"), &bad).unwrap();
        let root = fresh_scripts("tamper", "");
        let r =
            sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone())).await;
        assert!(r.is_err(), "篡改 index 应被验签拒");
        assert!(!root.join("_stdlib").exists(), "拒后不落 _stdlib");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// inventory（stdlib.list() 数据源）：无 index → 全种子 source="seed"；有 index →
    /// index 文件 "synced"（含 ver）+ 种子差集 "seed"（D5：index 为准消歧）。
    #[tokio::test]
    async fn inventory_merges_index_and_seed() {
        let root = fresh_scripts("inv", "");
        // 冷启动：无 index，仅 4 个种子
        let (v0, e0) = inventory(&root);
        assert_eq!(v0, None);
        assert_eq!(e0.len(), SEED_MODULES.len(), "全部种子");
        assert!(e0.iter().all(|e| e.source == "seed" && e.ver.is_none()));

        // sync 后：algo/crc16.lua 转 synced（种子同名让位），其余仍 seed
        let up = fresh_scripts("up-inv", "");
        write_upstream(&up, "1.0.0", 1, &[("algo/crc16.lua", "return {}")], None);
        sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone()))
            .await
            .expect("sync");
        let (v1, e1) = inventory(&root);
        assert_eq!(v1.as_deref(), Some("1.0.0"));
        assert_eq!(e1.len(), SEED_MODULES.len(), "1 synced + 3 seed");
        let crc = e1.iter().find(|e| e.path == "algo/crc16.lua").unwrap();
        assert_eq!(crc.source, "synced");
        assert_eq!(crc.ver.as_deref(), Some("1.0.0"));
        assert_eq!(crc.name, "algo.crc16", "name 为点分模块名");
        assert!(e1.iter().filter(|e| e.source == "seed").count() == 3);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// 阶段 A 审查修复：`scripts/` 前缀条目（官方脚本模板，D1 分发面）不列入
    /// inventory——模板非 require 可达模块（首段 `scripts` 非保留段），列出即假模块。
    #[tokio::test]
    async fn inventory_skips_script_templates() {
        let root = fresh_scripts("inv-tpl", "");
        let up = fresh_scripts("up-inv-tpl", "");
        write_upstream(
            &up,
            "1.0.0",
            1,
            &[
                ("algo/crc16.lua", "return {}"),
                ("scripts/modbus-like/main.lua", "function collect() end"),
            ],
            None,
        );
        sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone()))
            .await
            .expect("sync");
        // 文件已分发落盘（D1 分发面成立，安装器为 B 阶段遗留）
        assert!(root.join("_stdlib/scripts/modbus-like/main.lua").exists());
        let (_, entries) = inventory(&root);
        assert!(
            entries.iter().all(|e| !e.path.starts_with("scripts/")),
            "模板条目不应列入 stdlib 清单，实得 {entries:?}"
        );
        assert_eq!(
            entries.len(),
            SEED_MODULES.len(),
            "1 synced（crc16）+ 种子差集"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// 阶段 A 审查修复：index 条目含盘符冒号 → 拒（Windows `Path::join` 前缀
    /// 替换逃逸，与 loader.rs `route()` 同因；验签后正常不可达，纵深防御）。
    #[test]
    fn safe_rel_path_rejects_colon() {
        assert!(safe_rel_path("C:evil.lua").is_err());
        assert!(safe_rel_path("algo/C:x.lua").is_err());
        assert!(safe_rel_path("algo/crc16.lua").is_ok());
    }

    /// stdlib.list()/info() 挂进沙箱 VM：list 返回合并数组，info 支持点分名，
    /// 未知模块返回 nil。
    #[tokio::test]
    async fn stdlib_table_list_and_info() {
        let root = fresh_scripts("tbl", "");
        let up = fresh_scripts("up-tbl", "");
        write_upstream(&up, "1.0.0", 1, &[("algo/crc16.lua", "return {}")], None);
        sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone()))
            .await
            .expect("sync");

        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        lua.globals()
            .set("stdlib", stdlib_table(&lua, &root).unwrap())
            .unwrap();
        let n: i64 = lua.load("return #stdlib.list()").eval().unwrap();
        assert_eq!(n as usize, SEED_MODULES.len(), "list = index + 种子差集");
        let src: String = lua
            .load(r#"return stdlib.info("algo.crc16").source"#)
            .eval()
            .unwrap();
        assert_eq!(src, "synced");
        let seed_src: String = lua
            .load(r#"local e = stdlib.info("codec.basexx"); return e.source"#)
            .eval()
            .unwrap();
        assert_eq!(seed_src, "seed");
        assert!(
            lua.load(r#"return stdlib.info("nope.nope") == nil"#)
                .eval::<bool>()
                .unwrap(),
            "未知模块 info 返回 nil"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// T9 hashings/emqx 在线分发（机制层）：三段嵌套保留段路径
    /// （`algo/hashings/<x>.lua`）sync 落 `_stdlib/` 后，沙箱 require 按点分
    /// 模块名路由可达，且 **`_stdlib/` 内模块间 require**（hashings 算法件
    /// require `algo.hashings.uintn` 的依赖闭包形态）与 selftest 约定成立。
    /// 真实 hashings 源码的向量验证见 `official_dist_hashings_emqx_local`
    /// （ignore，走官方 dist 全链路）。
    #[tokio::test]
    async fn sync_nested_stdlib_module_dependency() {
        let up = fresh_scripts("up-nested", "");
        write_upstream(
            &up,
            "1.1.0",
            1,
            &[
                (
                    "algo/hashings/dep.lua",
                    "local M = {}\nM.tag = 'dep'\nfunction M.selftest() return M.tag == 'dep' end\nreturn M\n",
                ),
                (
                    "algo/hashings/user.lua",
                    "local dep = require('algo.hashings.dep')\nlocal M = {}\nfunction M.selftest() return dep.selftest() and dep.tag == 'dep' end\nreturn M\n",
                ),
                (
                    "cloud/emqx.lua",
                    "local M = {}\nfunction M.topic_data(hw, devid) return hw..'/'..devid..'/datas' end\nfunction M.selftest() return M.topic_data('a','b') == 'a/b/datas' end\nreturn M\n",
                ),
            ],
            None,
        );
        let root = fresh_scripts("nested", "em = require('cloud.emqx')");
        let r = sync_with_fetcher(UPSTREAM, &root, None, &dev_keys(), dir_fetcher(up.clone()))
            .await
            .expect("sync 嵌套路径");
        assert_eq!(r.added.len(), 3, "三文件全量新增：{r:?}");
        assert!(
            root.join("_stdlib/algo/hashings/dep.lua").exists(),
            "嵌套路径应落 _stdlib/algo/hashings/"
        );

        // 沙箱 VM：跨模块 require + selftest 约定 + cloud 保留段路由
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(root.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        assert!(
            lua.load("return require('algo.hashings.user').selftest()")
                .eval::<bool>()
                .unwrap(),
            "user → dep 的 _stdlib 内模块间 require 应成立"
        );
        let ok: bool = lua
            .load(r#"return select(1, pcall(require, 'algo.hashings.nope'))"#)
            .eval()
            .unwrap();
        assert!(!ok, "未同步模块 pcall 应失败（miss 即错，永不回落）");
        assert_eq!(
            lua.load("return em.topic_data('gw01', 'dev42')")
                .eval::<String>()
                .unwrap(),
            "gw01/dev42/datas",
            "cloud 保留段 require 应取同步文件"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&up);
    }

    /// T9 hashings/emqx 真实源码端到端（本地手工跑，默认 ignore）：真实官方
    /// dist（rsiot-lualib 仓 `./gen_index.sh 1.1.0 && ./sign_index.sh` 产物，
    /// 官方私钥签名）经 dir_fetcher（无需 HTTP 服务）→ `sync_with_fetcher`
    /// 注入 **生产 VERIFY_KEYS** 验签全链路落 `_stdlib/` → 沙箱 VM 逐模块
    /// `selftest()`（NIST/RFC 已知向量）。跑法：
    ///
    /// ```text
    /// cd rsiot-lualib && ./gen_index.sh 1.1.0 && ./sign_index.sh
    /// cargo test -p rs-iot --features lua,lua-http -- --ignored official_dist
    /// ```
    ///
    /// 依赖源仓 dist 存在（`../rsiot-lualib`，CI 无此仓）故 ignore，不进默认
    /// 套件——本地发布前必跑（等价 e2e_sync_official_dist_over_http 的免端口版）。
    #[cfg(feature = "lua-http")]
    #[tokio::test]
    #[ignore = "需本地 rsiot-lualib dist（见测试文档注释）"]
    async fn official_dist_hashings_emqx_local() {
        let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rsiot-lualib/dist");
        assert!(
            dist.join("index.json").exists(),
            "缺官方 dist：{}（先在源仓跑 ./gen_index.sh 1.1.0 && ./sign_index.sh）",
            dist.display()
        );
        let root = fresh_scripts("official-dist", "");

        // 生产信任锚 VERIFY_KEYS 验官方签名（同时锁 pub.key 与 VERIFY_KEYS 同源）
        let report = sync_with_fetcher(
            UPSTREAM,
            &root,
            None,
            VERIFY_KEYS,
            dir_fetcher(dist.clone()),
        )
        .await
        .expect("官方 dist 验签→逐文件 sha→原子落盘");
        assert_eq!(report.version, "1.1.0");
        assert_eq!(report.added.len(), 11, "T9 后全量：{report:?}");

        // 沙箱 VM 两档指令预算（cfg.hook_instr_limit，护栏参数本就宿主可配）：
        //   默认 1M 档——轻算法（sha256 单次 ~0.4M 指令）生产默认护栏内可用；
        //   提高档 4M ——hmac = 3 次哈希（sha1 inner+outer+data 块），实测
        //     mlua 5.4 下 >1M 默认护栏（纯 Lua 元表运算密度所致）——T10 OneNET
        //     token（hmac_sha1）生产使用须调高 hook_instr_limit，见任务汇报。
        let mk_vm = |limit: u64| {
            let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x","hook_instr_limit": limit}))
                .unwrap();
            new_vm(&cfg).unwrap()
        };
        let lctx = LoadCtx {
            scripts_root: Arc::from(root.as_path()),
            script_name: Arc::<str>::from("devA"),
        };

        // 默认预算档：sha256 selftest 过（轻算法护栏内可用的验收锚点）
        let lua_default = mk_vm(1_000_000);
        load_main(&lua_default, &lctx).unwrap();
        assert!(
            lua_default
                .load("return require('algo.hashings.sha256').selftest()")
                .eval::<bool>()
                .unwrap(),
            "sha256 selftest 应在默认 1M 指令护栏内通过"
        );

        // 提高档：全部新模块 selftest（已知向量）+ emqx 薄配方 helper
        let lua = mk_vm(4_000_000);
        load_main(&lua, &lctx).unwrap();
        for m in [
            "algo.hashings.uintn",
            "algo.hashings.md5",
            "algo.hashings.sha1",
            "algo.hashings.sha256",
            "algo.hashings.hmac",
            "cloud.emqx",
        ] {
            assert!(
                lua.load(format!("return require('{m}').selftest()"))
                    .eval::<bool>()
                    .unwrap_or_else(|e| panic!("{m} selftest 应通过：{e}")),
                "{m} selftest 向量失败"
            );
        }
        let topic: String = lua
            .load(r#"return require('cloud.emqx').topic_data('gw01', 'dev42')"#)
            .eval()
            .unwrap();
        assert_eq!(topic, "gw01/dev42/datas");
        assert!(
            lua.load(r#"return require('cloud.emqx').auth({}) == nil"#)
                .eval::<bool>()
                .unwrap(),
            "EMQX 裸 broker：auth 恒 nil（凭证走 broker ACL）"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// T13 端到端验收（本地手工跑，默认 ignore）：真实官方 dist（rsiot-lualib
    /// 仓 `./gen_index.sh && ./sign_index.sh` 产物，官方 v1 私钥签名）经本机
    /// 静态 HTTP 服务 → **生产 reqwest 路径** [`sync`]（内置 VERIFY_KEYS 验签）
    /// 全链路落 `_stdlib/`。跑法：
    ///
    /// ```text
    /// cd rsiot-lualib && ./gen_index.sh && ./sign_index.sh
    /// python -m http.server 8765 -d dist &          # 或任意静态服务
    /// cargo test -p rs-iot --features lua,lua-http -- --ignored e2e_sync
    /// ```
    ///
    /// 篡改场景（镜像劫持，§3.7）：改 dist/ 任一 .lua 一字节后重跑，本测试
    /// 应在 sha256 闸门失败并输出「内容被篡改，拒装」——失败输出即验收证据。
    ///
    /// 依赖外部资源（源仓 dist + 本机端口）故 ignore，不进默认套件/CI。
    #[cfg(feature = "lua-http")]
    #[tokio::test]
    #[ignore = "需本地 rsiot-lualib dist + 静态 HTTP 服务（见测试文档注释）"]
    async fn e2e_sync_official_dist_over_http() {
        let upstream = std::env::var("RSIOT_E2E_UPSTREAM")
            .unwrap_or_else(|_| "http://127.0.0.1:8765".to_string());
        let root = fresh_scripts("e2e-official", "crcm = require('algo.crc16')");

        let report = sync(&upstream, &root, None)
            .await
            .expect("官方 dist 全链路同步（验签→逐文件 sha→原子落盘）");
        assert_eq!(report.version, "1.1.0");
        assert_eq!(report.added.len(), 11, "T9 后全量新增：{report:?}");

        // a) _stdlib/ 落位 + 本地 index 版本
        for p in [
            "algo/crc16.lua",
            "algo/hashings/hmac.lua",
            "algo/hashings/md5.lua",
            "algo/hashings/sha1.lua",
            "algo/hashings/sha256.lua",
            "algo/hashings/uintn.lua",
            "cloud/emqx.lua",
            "codec/basexx.lua",
            "util/retry.lua",
            "util/tablex.lua",
            "scripts/modbus-like/main.lua",
        ] {
            assert!(root.join("_stdlib").join(p).exists(), "_stdlib 缺 {p}");
        }
        assert_eq!(read_index(&root).expect("read_index").version, "1.1.0");

        // b) inventory 徽标：crc16 转 synced（其余仍 seed）
        let (ver, entries) = inventory(&root);
        assert_eq!(ver.as_deref(), Some("1.0.0"));
        let crc = entries
            .iter()
            .find(|e| e.path == "algo/crc16.lua")
            .expect("inventory 应含 crc16");
        assert_eq!(crc.source, "synced");
        assert_eq!(crc.ver.as_deref(), Some("1.0.0"));

        // c) require("algo.crc16") 取同步文件版（非种子）——行为证明：给本地
        //    _stdlib 文件追加种子没有的字段，新 VM require 应可见（文件优先）
        let p = root.join("_stdlib/algo/crc16.lua");
        let src = fs::read_to_string(&p).unwrap();
        let patched = src.replacen(
            "\nreturn M",
            "\nM.e2e_synced_file = true -- e2e 注入：种子无此字段\nreturn M",
            1,
        );
        assert_ne!(patched, src, "crc16.lua 应恰含一处 \\nreturn M 供注入");
        fs::write(&p, patched).unwrap();
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x"})).unwrap();
        let lua = new_vm(&cfg).unwrap();
        let lctx = LoadCtx {
            scripts_root: Arc::from(root.as_path()),
            script_name: Arc::<str>::from("devA"),
        };
        load_main(&lua, &lctx).unwrap();
        assert!(
            lua.load("return crcm.e2e_synced_file == true")
                .eval::<bool>()
                .unwrap(),
            "require 应读到 _stdlib/ 文件版（种子无 e2e_synced_file 字段）"
        );
        // 同步文件版行为完好：官方 selftest 向量仍过
        assert!(
            lua.load("return crcm.selftest()").eval::<bool>().unwrap(),
            "同步下来的 crc16 应通过官方 selftest"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
