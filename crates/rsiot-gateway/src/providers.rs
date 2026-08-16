//! LLM 供应商管理（`/api/agent/providers*`）。
//!
//! 背景：reasonix serve 的 HTTP 面没有供应商 CRUD/默认模型 API（仅
//! `provider_setup.go` 的"当前模型缺 key 补 key"，loopback 限定）——供应商
//! 唯一权威源是 `REASONIX_HOME/config.toml`（`[[providers]]` + `default_model`）
//! 与 `.env`（api_key_env 指向的密钥），serve **启动时快照**配置。
//!
//! 因此本网关直接做配置面的读写：`toml_edit` 外科手术式编辑（**保留用户
//! 配置里的注释与排版**，绝不 serde 全量重写），密钥写 `.env`，变更后经
//! warden 重启 reasonix 使其生效。密钥只写不读（GET 永不回传内容）。
//!
//! 并发注记：reasonix 自身经 `config.LockUserCredentialEdits` 跨进程锁写凭据，
//! 本网关仅在"改配置→立刻重启"窗口写 `.env`，与 serve 运行期的凭据写入
//! （仅缺 key 补写路径）冲突面极小；重启后 serve 全量重读，一致性由快照语义兜底。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use toml_edit::{value, Array, DocumentMut, Item, Table};

use crate::proxy::GwState;

/// 供应商视图（密钥只报有无，不回传内容）。
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ProviderInfo {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// 条目自带缺省模型（`default` 字段；None 时 Reasonix 取 models[0]）。
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub has_key: bool,
}

/// upsert 请求（POST /api/agent/providers）。api_key 可选：留空 = 不动现有密钥。
#[derive(Debug, Deserialize)]
pub struct ProviderUpsertReq {
    pub name: String,
    /// "openai" | "anthropic"（缺省 openai 兼容）。
    #[serde(default)]
    pub kind: String,
    pub base_url: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// 缺省由 name 派生（大写、非字母数字→下划线、+_API_KEY）。
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub api_key: String,
}

/// 设默认模型请求：`model = "<provider>/<model-id>"`（新会话生效，需重启）。
#[derive(Debug, Deserialize)]
pub struct DefaultModelReq {
    pub model: String,
}

// ---- 配置文件读写（纯函数 + 薄 IO，便于单测） ----

fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}
fn env_path(home: &Path) -> PathBuf {
    home.join(".env")
}

/// 解析 .env 为行列表（reasonix .env 是裸 KEY=value，不解析引号语义）。
fn read_env_lines(home: &Path) -> std::io::Result<Vec<String>> {
    match std::fs::read_to_string(env_path(home)) {
        Ok(s) => Ok(s.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e),
    }
}

/// .env 中是否已有该 key（非空值才算已配置）。
fn env_has_key(lines: &[String], key: &str) -> bool {
    lines.iter().any(|l| {
        let t = l.trim();
        !t.starts_with('#')
            && t.len() > key.len() + 1
            && t.starts_with(key)
            && t.as_bytes()[key.len()] == b'='
            && !t[key.len() + 1..].trim().is_empty()
    })
}

/// .env 行是否为指定 key 的赋值行。
fn is_env_assign(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    !t.starts_with('#') && t.starts_with(key) && t.as_bytes().get(key.len()) == Some(&b'=')
}

/// 设置/替换 .env 的 key（原位替换保序，新增追加），原子写回。
fn env_set_key(home: &Path, key: &str, val: &str) -> Result<(), String> {
    let mut lines = read_env_lines(home).map_err(|e| format!("read .env failed: {e}"))?;
    let new_line = format!("{key}={val}");
    let replaced = lines.iter_mut().any(|l| {
        if is_env_assign(l, key) {
            *l = new_line.clone();
            true
        } else {
            false
        }
    });
    if !replaced {
        if lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
            lines.push(String::new());
        }
        lines.push(new_line);
    }
    atomic_write(&env_path(home), &(lines.join("\n") + "\n"))
}

/// 删除 .env 的 key（存在时才写回）。
fn env_remove_key(home: &Path, key: &str) -> Result<(), String> {
    let mut lines = read_env_lines(home).map_err(|e| format!("read .env failed: {e}"))?;
    let before = lines.len();
    lines.retain(|l| !is_env_assign(l, key));
    if lines.len() != before {
        atomic_write(&env_path(home), &(lines.join("\n") + "\n"))?;
    }
    Ok(())
}

/// 原子写（先临时文件再 rename，防半截配置）。
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {} failed: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {} failed: {e}", path.display()))
}

fn load_doc(home: &Path) -> Result<DocumentMut, String> {
    let raw = std::fs::read_to_string(config_path(home))
        .map_err(|e| format!("read config.toml failed: {e}"))?;
    raw.parse::<DocumentMut>()
        .map_err(|e| format!("parse config.toml failed: {e}"))
}

fn save_doc(home: &Path, doc: &DocumentMut) -> Result<(), String> {
    atomic_write(&config_path(home), &doc.to_string())
}

/// 读全量状态：供应商列表 + default_model。
pub fn read_state(home: &Path) -> Result<(Vec<ProviderInfo>, String), String> {
    let doc = load_doc(home)?;
    let env_lines = read_env_lines(home).map_err(|e| format!("read .env failed: {e}"))?;
    let mut out = Vec::new();
    if let Some(arr) = doc.get("providers").and_then(Item::as_array_of_tables) {
        for t in arr.iter() {
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let api_key_env = t
                .get("api_key_env")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push(ProviderInfo {
                name,
                kind: t
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                base_url: t
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                models: t
                    .get("models")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|i| i.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                default: t
                    .get("default")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                has_key: !api_key_env.is_empty() && env_has_key(&env_lines, &api_key_env),
                api_key_env,
            });
        }
    }
    let dm = doc
        .get("default_model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok((out, dm))
}

/// api_key_env 缺省派生：provider 名 → 大写、非字母数字→下划线、压连续下划线、+_API_KEY。
pub fn derive_key_env(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_');
    format!("{}_API_KEY", if s.is_empty() { "PROVIDER" } else { s })
}

/// upsert 供应商条目（原位改，保留注释/排版）；带 api_key 时写 .env。
/// 返回是否新建条目。
pub fn upsert_provider(home: &Path, req: &ProviderUpsertReq) -> Result<bool, String> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err("provider name is required".into());
    }
    if req.base_url.trim().is_empty() {
        return Err("base_url is required".into());
    }
    let models: Vec<String> = req
        .models
        .iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    if models.is_empty() {
        return Err("at least one model is required".into());
    }
    let mut doc = load_doc(home)?;
    let arr = doc
        .get_mut("providers")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "config.toml 缺 [[providers]] 数组（reasonix 配置损坏）".to_string())?;
    let kind = if req.kind.trim().is_empty() {
        "openai".to_string()
    } else {
        req.kind.trim().to_string()
    };
    let key_env = if req.api_key_env.trim().is_empty() {
        derive_key_env(name)
    } else {
        req.api_key_env.trim().to_string()
    };

    let mut created = false;
    let target: &mut Table = {
        let mut found = None;
        for t in arr.iter_mut() {
            if t.get("name").and_then(|v| v.as_str()) == Some(name) {
                found = Some(t);
                break;
            }
        }
        match found {
            Some(t) => t,
            None => {
                created = true;
                arr.push(Table::new());
                let last = arr.len() - 1;
                arr.iter_mut().nth(last).expect("just pushed")
            }
        }
    };
    target["name"] = value(name);
    target["kind"] = value(kind);
    target["base_url"] = value(req.base_url.trim());
    let mut ma = Array::new();
    for m in &models {
        ma.push(m.as_str());
    }
    target["models"] = value(ma);
    target["api_key_env"] = value(key_env.clone());
    save_doc(home, &doc)?;
    if !req.api_key.trim().is_empty() {
        env_set_key(home, &key_env, req.api_key.trim())?;
    }
    Ok(created)
}

/// 删除供应商条目 + 其 .env 密钥。条目不存在返 Err。
pub fn delete_provider(home: &Path, name: &str) -> Result<(), String> {
    let mut doc = load_doc(home)?;
    let arr = doc
        .get_mut("providers")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "config.toml 缺 [[providers]] 数组".to_string())?;
    let key_env = arr
        .iter()
        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(|t| {
            t.get("api_key_env")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let before = arr.len();
    arr.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
    if arr.len() == before {
        return Err(format!("provider {name:?} not found"));
    }
    save_doc(home, &doc)?;
    if let Some(k) = key_env {
        if !k.is_empty() {
            env_remove_key(home, &k)?;
        }
    }
    Ok(())
}

/// 设默认模型（`<provider>/<model>`）。校验供应商存在且模型在其列表内
/// （reasonix 侧另取条目 default/models[0]，此处严格校验防配置写坏）。
/// 若被删模型恰是 default_model，调用方应先/后再设默认——本函数只管写。
pub fn set_default_model(home: &Path, model_ref: &str) -> Result<(), String> {
    let (provider, model) = model_ref
        .split_once('/')
        .ok_or_else(|| format!("model ref must be \"<provider>/<model>\", got {model_ref:?}"))?;
    let (provs, _) = read_state(home)?;
    let p = provs
        .iter()
        .find(|p| p.name == provider)
        .ok_or_else(|| format!("unknown provider {provider:?}"))?;
    if !p.models.iter().any(|m| m == model) {
        return Err(format!(
            "model {model:?} not in provider {provider:?} models: {:?}",
            p.models
        ));
    }
    let mut doc = load_doc(home)?;
    doc["default_model"] = value(model_ref);
    save_doc(home, &doc)
}

// ---- HTTP handlers（/api/agent/providers*，JWT 已由网关层1 保护） ----

fn err(code: StatusCode, msg: &str) -> axum::response::Response {
    (code, msg.to_string()).into_response()
}

/// home 目录未配置时的统一 503。
fn not_configured() -> axum::response::Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "provider management not configured: set [reasonix] home_dir in gateway.toml",
    )
}

/// 变更后经 warden 重启 reasonix（serve 启动快照配置）。失败不阻断响应——
/// 返回 restarted=false 让 UI 提示手动重启。
async fn restart_reasonix(st: &GwState) -> (bool, Option<String>) {
    let url = format!(
        "{}/api/v1/services/{}/restart",
        st.reasonix.warden_url.trim_end_matches('/'),
        st.reasonix.service_name()
    );
    match st
        .client
        .post(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => (true, None),
        Ok(r) => (
            false,
            Some(format!("warden restart returned {}", r.status())),
        ),
        Err(e) => (false, Some(format!("warden restart request failed: {e}"))),
    }
}

/// GET /api/agent/providers → {default_model, providers:[...]}（密钥只报有无）。
pub async fn list(State(st): State<Arc<GwState>>) -> axum::response::Response {
    if st.reasonix.home_dir.is_empty() {
        return not_configured();
    }
    let home = PathBuf::from(&st.reasonix.home_dir);
    match read_state(&home) {
        Ok((providers, default_model)) => {
            Json(serde_json::json!({ "default_model": default_model, "providers": providers }))
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// POST /api/agent/providers → upsert（新建 201 / 更新 200），随后触发重启。
pub async fn upsert(
    State(st): State<Arc<GwState>>,
    Json(req): Json<ProviderUpsertReq>,
) -> axum::response::Response {
    if st.reasonix.home_dir.is_empty() {
        return not_configured();
    }
    let home = PathBuf::from(&st.reasonix.home_dir);
    let created = match upsert_provider(&home, &req) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let (restarted, warn) = restart_reasonix(&st).await;
    let code = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (
        code,
        Json(serde_json::json!({ "saved": true, "created": created, "restarted": restarted, "restart_error": warn })),
    )
        .into_response()
}

/// DELETE /api/agent/providers/{name} → 删除条目 + 密钥，随后触发重启。
pub async fn delete(
    State(st): State<Arc<GwState>>,
    AxumPath(name): AxumPath<String>,
) -> axum::response::Response {
    if st.reasonix.home_dir.is_empty() {
        return not_configured();
    }
    let home = PathBuf::from(&st.reasonix.home_dir);
    if let Err(e) = delete_provider(&home, &name) {
        return err(StatusCode::NOT_FOUND, &e);
    }
    let (restarted, warn) = restart_reasonix(&st).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": name, "restarted": restarted, "restart_error": warn })),
    )
        .into_response()
}

/// POST /api/agent/providers/default → 设新会话默认模型，随后触发重启。
pub async fn set_default(
    State(st): State<Arc<GwState>>,
    Json(req): Json<DefaultModelReq>,
) -> axum::response::Response {
    if st.reasonix.home_dir.is_empty() {
        return not_configured();
    }
    let home = PathBuf::from(&st.reasonix.home_dir);
    if let Err(e) = set_default_model(&home, req.model.trim()) {
        return err(StatusCode::BAD_REQUEST, &e);
    }
    let (restarted, warn) = restart_reasonix(&st).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "default_model": req.model, "restarted": restarted, "restart_error": warn })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gw-prov-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 带注释与 default_model 的最小 reasonix 配置（模拟现场 18K 用户配置的缩影）。
        std::fs::write(
            config_path(&dir),
            "# user config header comment\nconfig_version = 6\ndefault_model = \"deepseek/deepseek-v4-flash\"\n\n# deepseek provider\n[[providers]]\nname = \"deepseek\"\nkind = \"anthropic\"\nbase_url = \"https://api.deepseek.com/anthropic\"\nmodels = [\"deepseek-v4-flash\"]\napi_key_env = \"DEEPSEEK_API_KEY\"\n",
        )
        .unwrap();
        std::fs::write(env_path(&dir), "# secrets\nDEEPSEEK_API_KEY=sk-123\n").unwrap();
        dir
    }

    fn upsert_req(name: &str, models: &[&str], key: &str) -> ProviderUpsertReq {
        ProviderUpsertReq {
            name: name.into(),
            kind: "openai".into(),
            base_url: "https://api.example.com/v1".into(),
            models: models.iter().map(|s| s.to_string()).collect(),
            api_key_env: String::new(),
            api_key: key.into(),
        }
    }

    #[test]
    fn read_state_masks_key_and_reports_has_key() {
        let home = setup_home("read");
        let (provs, dm) = read_state(&home).unwrap();
        assert_eq!(dm, "deepseek/deepseek-v4-flash");
        assert_eq!(provs.len(), 1);
        assert!(provs[0].has_key);
        assert_eq!(provs[0].api_key_env, "DEEPSEEK_API_KEY");
        // 序列化后不含密钥内容（结构里根本没有该字段——编译期保证）。
        let json = serde_json::to_string(&provs[0]).unwrap();
        assert!(!json.contains("sk-123"));
    }

    #[test]
    fn upsert_new_provider_preserves_comments_and_existing() {
        let home = setup_home("add");
        assert!(upsert_provider(
            &home,
            &upsert_req("minimax", &["MiniMax-M3", "abab6.5s"], "mmk-1")
        )
        .unwrap());
        let raw = std::fs::read_to_string(config_path(&home)).unwrap();
        // 原注释与既有条目保留。
        assert!(raw.contains("# user config header comment"));
        assert!(raw.contains("# deepseek provider"));
        assert!(raw.contains("name = \"deepseek\""));
        // 新条目 + 派生 key_env + .env 写入。
        assert!(raw.contains("name = \"minimax\""));
        let env = std::fs::read_to_string(env_path(&home)).unwrap();
        assert!(env.contains("MINIMAX_API_KEY=mmk-1"));
        assert!(env.contains("DEEPSEEK_API_KEY=sk-123"), "既有密钥不受影响");
        let (provs, _) = read_state(&home).unwrap();
        assert_eq!(provs.len(), 2);
        assert!(provs.iter().find(|p| p.name == "minimax").unwrap().has_key);
    }

    #[test]
    fn upsert_existing_updates_models_without_key_touch() {
        let home = setup_home("update");
        let mut req = upsert_req("deepseek", &["deepseek-v4-flash", "deepseek-pro"], "");
        req.api_key_env = "DEEPSEEK_API_KEY".into();
        assert!(!upsert_provider(&home, &req).unwrap(), "同名应为更新非新建");
        let (provs, _) = read_state(&home).unwrap();
        let ds = provs.iter().find(|p| p.name == "deepseek").unwrap();
        assert_eq!(ds.models, vec!["deepseek-v4-flash", "deepseek-pro"]);
        assert!(ds.has_key, "api_key 留空不动现有密钥");
    }

    #[test]
    fn delete_removes_entry_and_env_key() {
        let home = setup_home("del");
        upsert_provider(&home, &upsert_req("minimax", &["MiniMax-M3"], "mmk-1")).unwrap();
        delete_provider(&home, "minimax").unwrap();
        let (provs, _) = read_state(&home).unwrap();
        assert_eq!(provs.len(), 1);
        let env = std::fs::read_to_string(env_path(&home)).unwrap();
        assert!(!env.contains("MINIMAX_API_KEY"));
        assert!(env.contains("DEEPSEEK_API_KEY=sk-123"));
        assert!(delete_provider(&home, "nope").is_err());
    }

    #[test]
    fn set_default_model_validates_and_writes() {
        let home = setup_home("default");
        set_default_model(&home, "deepseek/deepseek-v4-flash").unwrap();
        assert!(set_default_model(&home, "deepseek/no-such-model").is_err());
        assert!(set_default_model(&home, "nope/m1").is_err());
        assert!(set_default_model(&home, "nodot").is_err());
        upsert_provider(&home, &upsert_req("minimax", &["MiniMax-M3"], "")).unwrap();
        set_default_model(&home, "minimax/MiniMax-M3").unwrap();
        let raw = std::fs::read_to_string(config_path(&home)).unwrap();
        assert!(raw.contains("default_model = \"minimax/MiniMax-M3\""));
    }

    #[test]
    fn derive_key_env_sanitizes() {
        assert_eq!(derive_key_env("CLIProxyAPI"), "CLIPROXYAPI_API_KEY");
        assert_eq!(derive_key_env("my-provider 2"), "MY_PROVIDER_2_API_KEY");
    }
}
