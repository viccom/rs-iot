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

use crate::plugin::lua_core::capability::CUR_API_VER;
use crate::plugin::lua_core::stdlib::SEED_MODULES;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 内置验签公钥（§3.5/§3.7：ed25519 公钥编译进 rs-iot，支持多公钥轮换预留）。
///
/// v1 放确定性 dev 公钥（= `SigningKey::from_bytes(&[7u8;32])` 的 verifying key，
/// 测试 `verify_keys_contain_dev_key` 同源锁）。
// TODO(T13): 官方源仓建置后替换为官方公钥（32 字节 raw），私钥入 CI secret 绝不入仓。
pub(crate) const VERIFY_KEYS: &[[u8; 32]] = &[[
    0xea, 0x4a, 0x6c, 0x63, 0xe2, 0x9c, 0x52, 0x0a, 0xbe, 0xf5, 0x50, 0x7b, 0x13, 0x2e, 0xc5, 0xf9,
    0x95, 0x47, 0x76, 0xae, 0xbe, 0xbe, 0x7b, 0x92, 0x42, 0x1e, 0xea, 0x69, 0x14, 0x46, 0xd2, 0x2c,
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

/// sync 主体（fetcher 注入点）：生产 [`sync`] 用 reqwest，测试用本地目录闭包。
///
/// 步骤（§3.6 D1）：拉 index+sig → 验签 → `min_api_ver` 预检 → pin 校验 →
/// 逐文件 sha 对比（本地已符则复拷，否则下载+校验）→ 写 `_stdlib.tmp/`（含
/// `.index.json`）→ [`atomic_swap`] 两步换 → [`SyncReport`]。任一步失败即 Err，
/// 不动旧 `_stdlib/`（下载/sha 失败时清理 tmp）。
///
/// 文件量小（D5 预算几十库）且为管理员低频操作，`std::fs` 同步 IO 直接执行
/// （不 spawn_blocking，见模块文档"实现取舍"）。
pub(crate) async fn sync_with_fetcher<F, Fut>(
    upstream_base: &str,
    scripts_root: &Path,
    pin: Option<&str>,
    fetch: F,
) -> Result<SyncReport, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let base = upstream_base.trim_end_matches('/');
    let idx_bytes = fetch(format!("{base}/index.json"))
        .await
        .map_err(|e| format!("拉取 index.json: {e}"))?;
    let sig_bytes = fetch(format!("{base}/index.json.sig"))
        .await
        .map_err(|e| format!("拉取 index.json.sig: {e}"))?;
    let index = verify_index(&idx_bytes, &sig_bytes, VERIFY_KEYS)?;

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
    sync_with_fetcher(upstream_base, scripts_root, pin, http_fetch).await
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
fn safe_rel_path(p: &str) -> Result<(), String> {
    if p.is_empty() || !p.ends_with(".lua") {
        return Err(format!("index 条目 {p:?} 非 .lua 文件"));
    }
    if p.starts_with('/') || p.contains('\\') || p.contains("..") {
        return Err(format!("index 条目 {p:?} 路径违规（穿越/绝对路径）"));
    }
    if p.split('/').any(|seg| seg.is_empty()) {
        return Err(format!("index 条目 {p:?} 含空路径段"));
    }
    Ok(())
}

/// sha256 → 小写 hex。
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
        match entries.into_iter().find(|e| e.name == name || e.path == name) {
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
pub(crate) fn inventory(scripts_root: &Path) -> (Option<String>, Vec<InventoryEntry>) {
    let index = read_index(scripts_root);
    let synced: std::collections::HashSet<&str> = index
        .iter()
        .flat_map(|i| i.files.keys())
        .map(|s| s.as_str())
        .collect();
    let mut out: Vec<InventoryEntry> = index
        .iter()
        .flat_map(|i| i.files.iter())
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

#[cfg(test)]
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

    /// 确定性 dev keypair（与 VERIFY_KEYS 占位同源，计划 T9）。
    fn dev_keypair() -> (ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey) {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
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
    fn dir_fetcher(base: PathBuf) -> impl Fn(String) -> std::future::Ready<Result<Vec<u8>, String>> {
        move |url: String| {
            std::future::ready((|| -> Result<Vec<u8>, String> {
                let rel = url.strip_prefix(UPSTREAM).and_then(|r| r.strip_prefix('/')).ok_or_else(|| format!("测试 fetcher 只认 {UPSTREAM}/ 前缀：{url}"))?;
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
        let bytes = br#"{"version":"1.0.0","files":{"algo/crc16.lua":{"sha256":"ab","ver":"1.0.0"}}}"#;
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

    /// VERIFY_KEYS 占位 = dev 公钥（同源锁；T13 替换官方钥时此测试同步更新）。
    #[test]
    fn verify_keys_contain_dev_key() {
        let (_, vk) = dev_keypair();
        assert!(
            VERIFY_KEYS.contains(&vk.to_bytes()),
            "VERIFY_KEYS 应含 dev 公钥 {:?}",
            vk.to_bytes().map(|b| format!("{b:02x}")).join("")
        );
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
        let r = sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone())).await;
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
        write_upstream(&up, "1.0.0", 1, &[("algo/demo.lua", "return { marker = 'v1' }")], None);
        let r1 = sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone()))
            .await
            .expect("首次 sync");
        assert_eq!(r1.version, "1.0.0");
        assert_eq!(r1.added, vec!["algo/demo.lua".to_string()], "全量新增");
        assert!(r1.updated.is_empty() && r1.removed.is_empty());
        let idx = read_index(&root).expect("sync 后应有本地 index");
        assert_eq!(idx.version, "1.0.0");
        assert!(!root.join("_stdlib.tmp").exists(), "不留 tmp");

        // 二次：2.0.0 内容变更 → Windows 上 rename 需先移走旧目录（两步换）
        write_upstream(&up, "2.0.0", 1, &[("algo/demo.lua", "return { marker = 'v2' }")], None);
        let r2 = sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone()))
            .await
            .expect("二次 sync（目标已存在，两步换）");
        assert_eq!(r2.version, "2.0.0");
        assert!(r2.added.is_empty(), "无新增");
        assert_eq!(r2.updated, vec!["algo/demo.lua".to_string()], "内容变更计 updated");
        assert_eq!(
            read_index(&root).expect("index 应随目录原子替换").version,
            "2.0.0"
        );
        assert!(
            !root.join("_stdlib.tmp").exists(),
            "两步换后不留 tmp"
        );
        let old_dirs: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("_stdlib.old"))
            .collect();
        assert!(old_dirs.is_empty(), "两步换成功后应删备份，实得 {old_dirs:?}");

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
        let r = sync_with_fetcher(UPSTREAM, &root, Some("9.9.9"), dir_fetcher(up.clone())).await;
        let e = r.expect_err("pin 不匹配应拒");
        assert!(e.contains("pin 版本不匹配"), "实得：{e}");
        // pin 匹配则放行
        sync_with_fetcher(UPSTREAM, &root, Some("1.0.0"), dir_fetcher(up.clone()))
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
        let r = sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone())).await;
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
        let r = sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone())).await;
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
        sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone()))
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

    /// stdlib.list()/info() 挂进沙箱 VM：list 返回合并数组，info 支持点分名，
    /// 未知模块返回 nil。
    #[tokio::test]
    async fn stdlib_table_list_and_info() {
        let root = fresh_scripts("tbl", "");
        let up = fresh_scripts("up-tbl", "");
        write_upstream(&up, "1.0.0", 1, &[("algo/crc16.lua", "return {}")], None);
        sync_with_fetcher(UPSTREAM, &root, None, dir_fetcher(up.clone()))
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
}
