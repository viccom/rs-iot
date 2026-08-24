//! mqttPub 兼容层（阶段 B T8）：旧 `[mqtt]` 配置 → 默认 luaNorth 实例的无损迁移。
//!
//! 迁移前（已删）：`src/plugin/builtin/mqtt_pub` 直启常驻——订阅 `<hwid>/command`
//! 收 CtlMessage 转发 ctl_registry，周期从 lux 抽取全部 `rtdata:<devid>` 组
//! `{hwid, devid, values, timestamp}` publish 到 `<hwid>/<devid>/datas`。
//!
//! 迁移后（本模块）：编排层（lib.rs 的 `run_app_with_shutdown`，原 MqttPub 直启处）
//! 在 `cfg.mqtt.host` 非空时——
//! 1. 落盘内置兼容脚本 [`COMPAT_SCRIPT`] 到 `scripts/<COMPAT_SCRIPT_NAME>/main.lua`
//!    （**不存在时写入，已存在不覆盖**——用户改了就是自定义脚本，热加载生效）；
//! 2. 生成默认实例配置（[`mqtt_compat_instance`]，inst_id=`mqtt-pub-default`）并持久化
//!    到 lux `instances`（REST 可见可管），`auto_start=false` 防与 lux autoStart 路径
//!    双启动；
//! 3. `mgr.start_sink` 经 InstanceManager 全套生命周期启动。
//!
//! R5 单通道锁（[`start_default_north`]）：本进程已在运行同名实例 → 不起第二通道；
//! lux 已有同名配置 → 沿用不覆盖；lux 配置 `auto_start=true` → 交 lux autoStart
//! 路径启动（仍单通道）。`cfg.mqtt.host` 空 = 显式关闭（不生成）。

use crate::config::AppConfig;
use crate::types::{AppType, HostKind, Hwid, InstConfig, PluginName};
use std::path::Path;

/// 默认实例 ID（lux instances key / 命令路由 key）。
pub(crate) const DEFAULT_INST_ID: &str = "mqtt-pub-default";

/// 兼容脚本目录名（`scripts/<名>/main.lua`；仅 [A-Za-z0-9_-] 合法）。
pub(crate) const COMPAT_SCRIPT_NAME: &str = "_builtin-north-compat";

/// 内置 mqttPub 兼容脚本（首次启动落盘，见模块文档）。
///
/// 行为逐字段对标旧 MqttPub（迁移前 `mqtt_pub/mod.rs`）：
/// - up：每设备一条发布——topic `<hwid>/<devid>/datas`、qos 0、payload
///   `{hwid, devid, values, timestamp}`；values 键为 lux hash 原始 field
///   （`<devid>.<tag>`——批量入口已剥前缀，此处拼回）、值为 cell 第 3 元原样、
///   timestamp 取该设备各 cell ts 最大值；
/// - down：`<hwid>/command`（订阅项由宿主配置生成）上的 CtlMessage JSON
///   `{instance, devid, cmd, data}` → `ctx:send_command` 转发目标实例；
///   字段缺失/类型不符静默丢弃（同旧 MqttPub 反序列化失败即忽略）；
/// - auth：无平台认证返回 nil（cfg 的 username/password/client_id 直用）。
pub(crate) const COMPAT_SCRIPT: &str = r#"-- @role mqtt-north
-- 内置 mqttPub 兼容脚本（阶段 B T8 无损迁移，默认实例 mqtt-pub-default 使用）。
-- 行为逐字段对标旧 MqttPub（src/plugin/builtin/mqtt_pub，已删）。
-- 本文件由宿主首次启动写入，已存在不覆盖——修改后即为自定义脚本（热加载生效）。
local json = require("json")

local HWID = cfg.hwid

-- MqttPub 等价：values 键 = lux hash 原始 field（<devid>.<tag>）。
-- luaNorth 批量入口已剥 <devid>. 前缀，此处拼回（db::rtdata 落盘均为前缀 field）。
local function build_item(e)
  local values = {}
  for tag, v in pairs(e.values) do
    values[e.devid .. "." .. tag] = v
  end
  local payload = json.encode({
    hwid = HWID,
    devid = e.devid,
    values = values,
    timestamp = e.ts,
  })
  return { topic = HWID .. "/" .. e.devid .. "/datas", payload = payload, qos = 0 }
end

-- 无平台认证：返回 nil → cfg 的 username/password/client_id 直用（MqttPub 同款）。
function auth(c)
  return nil
end

-- 每设备一条发布：topic <hwid>/<devid>/datas、qos 0。
function up(batch)
  local out = {}
  for _, e in ipairs(batch) do
    out[#out + 1] = build_item(e)
  end
  return out
end

-- <hwid>/command（订阅项由宿主配置）上的 CtlMessage JSON → 转发目标实例。
-- 字段缺失/类型不符静默丢弃（同旧 MqttPub serde 反序列化失败即忽略）。
function down(topic, payload)
  local ok, msg = pcall(json.decode, payload)
  if not ok or type(msg) ~= "table" then return end
  if type(msg.instance) ~= "string" or type(msg.devid) ~= "string"
    or type(msg.cmd) ~= "string" or type(msg.data) ~= "string" then
    return
  end
  ctx:send_command(msg.instance, msg.devid, msg.cmd, msg.data)
end
"#;

/// `[mqtt]` 配置 → 默认 luaNorth 实例配置（纯函数，单测覆盖）。
///
/// `host` 空（含空白）→ `None`（显式关闭默认北向）；非空 → 映射：
/// host/port/username/password 原样；`publish_interval_secs` → `period` 与
/// `batch_max_secs`（周期收拢语义对齐 MqttPub 的发布节拍）；`client_id` 固定
/// `hwid`（对齐 MqttPub 的 `MqttOptions::new(hwid, …)`）；`subscribe` 固定
/// `[<hwid>/command]`（MqttPub 的命令订阅）。`auto_start=false`：编排层直接
/// start_sink，避免 lux autoStart 路径二次启动（R5）。
pub(crate) fn mqtt_compat_instance(
    cfg: &AppConfig,
    hwid: &Hwid,
    scripts_root: &Path,
) -> Option<InstConfig> {
    if cfg.mqtt.host.trim().is_empty() {
        return None;
    }
    let interval = cfg.mqtt.publish_interval_secs;
    Some(InstConfig {
        inst_id: DEFAULT_INST_ID.into(),
        inst_name: "MQTT north default (mqttPub compat)".into(),
        app_code: PluginName::new("luaNorth"),
        app_type: AppType::North,
        auto_start: false,
        app_config: serde_json::json!({
            "script": COMPAT_SCRIPT_NAME,
            "host": cfg.mqtt.host,
            "port": cfg.mqtt.port,
            "username": cfg.mqtt.username,
            "password": cfg.mqtt.password,
            "client_id": hwid.as_str(),
            "period": interval,
            "batch_max_secs": interval,
            "subscribe": [format!("{hwid}/command")],
            "scripts_root": scripts_root.to_string_lossy(),
        }),
        devices: Vec::new(),
        host_kind: HostKind::Builtin,
    })
}

/// 落盘兼容脚本：`main.lua` 不存在时写入 [`COMPAT_SCRIPT`]，已存在不覆盖。
///
/// 每次启动幂等调用（用户删文件可自愈；用户改文件即自定义，不碰）。
pub(crate) fn ensure_compat_script(scripts_root: &Path) -> Result<(), String> {
    let dir = scripts_root.join(COMPAT_SCRIPT_NAME);
    let path = dir.join("main.lua");
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    std::fs::write(&path, COMPAT_SCRIPT)
        .map_err(|e| format!("写入兼容脚本 {} 失败：{e}", path.display()))?;
    tracing::info!("[mqtt] 内置兼容脚本已落盘 {}", path.display());
    Ok(())
}

/// [`start_default_north`] 的结果（编排层记日志用）。
#[derive(Debug)]
pub(crate) enum DefaultNorth {
    /// `[mqtt].host` 空——不生成默认北向实例（显式关闭逃生口）。
    Disabled,
    /// 生成（脚本落盘 + lux 持久化）并 start_sink 成功。
    Started,
    /// 本进程已在运行同名实例——R5 单通道锁拦截二次启动。
    AlreadyRunning,
    /// lux 已有同名配置且 `auto_start=true`——交 lux autoStart 路径启动（仍单通道）。
    DeferredToAutoStart,
    /// 失败（脚本落盘 / lux 读写 / start_sink 任一环节，msg 供 warn 日志）。
    Failed(String),
}

/// 编排层接线（lib.rs 原 MqttPub 直启处的替代）：R5 单通道锁 + 默认实例生成/沿用
/// + `start_sink`。流程见模块文档；决策点：
/// - 本进程已运行（mgr 可见同名实例）→ [`DefaultNorth::AlreadyRunning`]；
/// - lux 已有同名配置 → 沿用（用户自定义生效，不覆盖）；`auto_start=true` 时交
///   try_start_from_lux 启动且本函数不直接 start（防双启动）；
/// - lux 无 → ensure 脚本 + 生成 + 持久化 + start_sink。
pub(crate) async fn start_default_north(
    cfg: &AppConfig,
    hwid: &Hwid,
    scripts_root: &Path,
    lux: &lux::EmbeddedClient,
    mgr: &crate::plugin::instance::InstanceManager,
) -> DefaultNorth {
    if cfg.mqtt.host.trim().is_empty() {
        return DefaultNorth::Disabled;
    }
    // R5 单通道锁：本进程已在运行同名实例（含 Stopped——用户本进程内显式停过，
    // 不自动复活）→ 不起第二通道。
    if mgr.status().contains_key(DEFAULT_INST_ID) {
        tracing::warn!("[mqtt] 实例 {DEFAULT_INST_ID} 已在运行——跳过二次启动（R5 单通道锁）");
        return DefaultNorth::AlreadyRunning;
    }
    // 兼容脚本幂等落盘（已存在不覆盖；用户删文件自愈）。
    if let Err(e) = ensure_compat_script(scripts_root) {
        return DefaultNorth::Failed(e);
    }
    // lux 已有同名配置 → 沿用（用户自定义生效，不覆盖）；auto_start=true 时交
    // try_start_from_lux 启动（本函数不直接 start，防双启动——仍单通道）。
    let existing = match crate::api::hget(lux, "instances", DEFAULT_INST_ID).await {
        Some(raw) => match serde_json::from_str::<InstConfig>(&raw) {
            Ok(inst) => Some(inst),
            Err(e) => {
                tracing::warn!("[mqtt] lux 既有 {DEFAULT_INST_ID} 配置不可解析（{e}），重新生成");
                None
            }
        },
        None => None,
    };
    let inst = match existing {
        Some(inst) => {
            tracing::info!("[mqtt] lux 已有 {DEFAULT_INST_ID} 配置，沿用不覆盖（自定义生效）");
            if inst.auto_start {
                tracing::info!(
                    "[mqtt] {DEFAULT_INST_ID} auto_start=true——交 lux autoStart 路径启动（R5 单通道）"
                );
                return DefaultNorth::DeferredToAutoStart;
            }
            inst
        }
        None => {
            let Some(inst) = mqtt_compat_instance(cfg, hwid, scripts_root) else {
                return DefaultNorth::Disabled;
            };
            let raw = serde_json::to_string(&inst).unwrap_or_default();
            if let Err(e) = lux
                .execute("HSET", &["instances", DEFAULT_INST_ID, &raw])
                .await
            {
                return DefaultNorth::Failed(format!("持久化实例配置失败：{e}"));
            }
            tracing::info!(
                "[mqtt] 生成默认 luaNorth 实例 {DEFAULT_INST_ID}（broker {}:{}，脚本 {COMPAT_SCRIPT_NAME}）",
                cfg.mqtt.host,
                cfg.mqtt.port
            );
            inst
        }
    };
    match mgr.start_sink(inst).await {
        Ok(()) => DefaultNorth::Started,
        Err(e) => DefaultNorth::Failed(format!("start_sink 失败：{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{BridgeRegistry, CtlRegistry, HostContext};
    use crate::plugin::lua_core as core_;
    use crate::plugin::sample_sender::SampleSender;
    use crate::types::{CtlMessage, Hwid, Sample, SampleValue};
    use core_::script::LoadCtx;
    use mlua::LuaSerdeExt;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::plugin::builtin::lua_north::{config as north_config, contract};

    // ===== 测试基建（照 contract.rs T7 测试模式）=====

    /// 起独立 lux（port 0 + 独立 temp data_dir），返回 (client, data_dir)。
    async fn test_lux(tag: &str) -> (lux::EmbeddedClient, String) {
        let data_dir = std::env::temp_dir()
            .join(format!("lua-compat-{tag}-{}-{tag}", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        let cfg = AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        (handle.client(), data_dir)
    }

    /// 独立 temp scripts 根（先清再建，测试间互不污染）。
    fn temp_scripts_root(tag: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("lua-compat-script-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 构造 HostContext（T7 同款），返回 (ctx, ctl_registry 共享句柄)。
    fn host_ctx(
        client: &lux::EmbeddedClient,
        hwid: &str,
        token: &CancellationToken,
    ) -> (HostContext, CtlRegistry) {
        let metrics = Arc::new(crate::process_metrics::ProcessMetrics::new());
        let ctl = CtlRegistry::new(metrics.clone());
        let ctx = HostContext {
            lux: client.clone(),
            hwid: Hwid::new(hwid),
            shutdown: token.clone(),
            ctl_registry: ctl.clone(),
            bridge_registry: BridgeRegistry::new(),
            metrics,
            trace_registry: crate::io_trace::TraceRegistry::new(),
            event_tx: crate::event_bus::EventBus::dummy(),
        };
        (ctx, ctl)
    }

    /// 测试用 AppConfig（[mqtt] 全字段显式）。
    fn test_app_cfg() -> AppConfig {
        AppConfig {
            mqtt: crate::config::MqttConfig {
                host: "127.0.0.1".into(),
                port: 1883,
                username: Some("u".into()),
                password: Some("p".into()),
                publish_interval_secs: 7,
            },
            ..Default::default()
        }
    }

    /// 组装加载了兼容脚本的 north VM（照 lua_north::run 的组装顺序：沙箱 →
    /// register(North) → cfg 表 → load_main → manifest dry-run）。
    fn compat_vm(
        inst: &InstConfig,
        scripts_root: &Path,
        ctx: &HostContext,
    ) -> mlua::Result<mlua::Lua> {
        let ncfg = north_config::parse_north_cfg(&inst.app_config).expect("app_config 合法");
        let lua = core_::engine::new_vm_sandboxed(ncfg.hook_instr_limit, ncfg.mem_limit_mb)?;
        let (stub_tx, _stub_rx) = mpsc::channel::<Sample>(16);
        let ac = core_::api::ApiCtx {
            ctx_host: ctx.clone(),
            inst,
            io: None,
            tx: SampleSender::untracked(stub_tx),
            runtime_devs: Default::default(),
            dropped: Default::default(),
            transport: "mqtt",
            scripts_root,
            role: core_::api::Role::North,
            self_writes: None, // North 无 EventBus 订阅面，无自环（T8）
            alarm: Default::default(),
            model: None,
            gate_rejected: Default::default(),
        };
        core_::api::register(&lua, &ac)?;
        let cfg_v = ncfg.to_script_cfg(ctx.hwid.as_str(), inst.inst_id.as_str());
        let t: mlua::Value = lua.to_value(&cfg_v)?;
        lua.globals().set("cfg", t)?;
        let lctx = LoadCtx {
            scripts_root: Arc::from(scripts_root),
            script_name: Arc::<str>::from(COMPAT_SCRIPT_NAME),
        };
        core_::script::load_main(&lua, &lctx)?;
        // manifest dry-run 一并跑（@role mqtt-north → auth+up 必需的契约证明）
        let stdlib_ver = core_::stdlib_sync::read_index(scripts_root).map(|i| i.version);
        core_::manifest::dry_run_main(
            &lua,
            scripts_root,
            COMPAT_SCRIPT_NAME,
            stdlib_ver.as_deref(),
        )
        .map_err(mlua::Error::RuntimeError)?;
        Ok(lua)
    }

    // ===== 兼容脚本要素（粗粒度防误删）=====

    /// manifest 契约头 + 四回调要素（auth/up/down + send_command + 数据 topic）。
    #[test]
    fn compat_script_declares_contract_elements() {
        assert!(
            COMPAT_SCRIPT.contains("@role mqtt-north"),
            "manifest 角色头"
        );
        assert!(COMPAT_SCRIPT.contains("function auth"), "auth（必需回调）");
        assert!(COMPAT_SCRIPT.contains("function up"), "up（必需回调）");
        assert!(COMPAT_SCRIPT.contains("function down"), "down（命令下行）");
        assert!(COMPAT_SCRIPT.contains("ctx:send_command"), "命令转发");
        assert!(COMPAT_SCRIPT.contains("/datas"), "数据 topic");
    }

    // ===== mqtt_compat_instance（纯函数）=====

    /// host 空/空白 → None（显式关闭）。
    #[test]
    fn compat_instance_none_when_host_empty() {
        let mut cfg = test_app_cfg();
        cfg.mqtt.host = String::new();
        assert!(
            mqtt_compat_instance(&cfg, &Hwid::new("hw"), Path::new("scripts")).is_none(),
            "空串 → None"
        );
        cfg.mqtt.host = "   ".into();
        assert!(
            mqtt_compat_instance(&cfg, &Hwid::new("hw"), Path::new("scripts")).is_none(),
            "空白串 → None"
        );
    }

    /// host 非空 → 实例字段与 app_config 映射逐项断言，且产物可被 parse_north_cfg 解析。
    #[test]
    fn compat_instance_maps_mqtt_config() {
        let cfg = test_app_cfg();
        let hwid = Hwid::new("hw-9");
        let inst = mqtt_compat_instance(&cfg, &hwid, Path::new("scripts")).expect("host 非空");
        assert_eq!(inst.inst_id.as_ref(), DEFAULT_INST_ID);
        assert_eq!(inst.app_code.as_ref(), "luaNorth");
        assert!(matches!(inst.app_type, AppType::North));
        assert!(!inst.auto_start, "auto_start=false 防 lux autoStart 双启动");
        assert!(inst.devices.is_empty());
        assert!(matches!(inst.host_kind, HostKind::Builtin));
        let ac = &inst.app_config;
        assert_eq!(ac["script"], COMPAT_SCRIPT_NAME);
        assert_eq!(ac["host"], "127.0.0.1");
        assert_eq!(ac["port"], 1883);
        assert_eq!(ac["username"], "u");
        assert_eq!(ac["password"], "p");
        assert_eq!(
            ac["client_id"], "hw-9",
            "client_id 对齐旧 MqttPub（= hwid）"
        );
        assert_eq!(ac["period"], 7, "publish_interval_secs → period");
        assert_eq!(
            ac["batch_max_secs"], 7,
            "publish_interval_secs → batch_max_secs"
        );
        assert_eq!(ac["subscribe"], serde_json::json!(["hw-9/command"]));
        assert_eq!(ac["scripts_root"], "scripts");
        north_config::parse_north_cfg(ac).expect("产物应为合法 NorthCfg");
    }

    // ===== ensure_compat_script =====

    /// 不存在时写入；已存在不覆盖（用户自定义生效）。
    #[test]
    fn ensure_compat_script_writes_once_and_never_overwrites() {
        let tmp = temp_scripts_root("ensure");
        ensure_compat_script(&tmp).unwrap();
        let p = tmp.join(COMPAT_SCRIPT_NAME).join("main.lua");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            COMPAT_SCRIPT,
            "首次写入"
        );
        // 用户自定义后不再覆盖
        std::fs::write(&p, "-- custom").unwrap();
        ensure_compat_script(&tmp).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "-- custom",
            "已存在不覆盖"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ===== payload 等价（本 task 验收线）=====

    /// 旧 MqttPub（T8 删除前 `src/plugin/builtin/mqtt_pub`）payload 组装的保真快照——
    /// 等价性断言的对照基准。照抄迁移前 `publish_rtdata`/`parse_rtdata_hash` 的组装
    /// 逻辑，不随新代码演化（它是"当时行为"的锁定）。
    mod legacy_mqtt_pub {
        fn bulk_str(v: &lux::EmbeddedValue) -> Option<String> {
            match v {
                lux::EmbeddedValue::Bulk(b) => Some(String::from_utf8_lossy(b).to_string()),
                lux::EmbeddedValue::Simple(s) => Some(s.clone()),
                _ => None,
            }
        }

        /// HGETALL 返回 → (values map, 最新 ts)，照抄 parse_rtdata_hash。
        fn parse_rtdata_hash(
            arr: &[lux::EmbeddedValue],
        ) -> (serde_json::Map<String, serde_json::Value>, i64) {
            let mut values = serde_json::Map::new();
            let mut latest_ts: i64 = 0;
            let mut i = 0;
            while i + 1 < arr.len() {
                let (Some(field), Some(cell_raw)) = (bulk_str(&arr[i]), bulk_str(&arr[i + 1]))
                else {
                    i += 2;
                    continue;
                };
                // cell = [ts, localTime, value]
                if let Ok(cell) = serde_json::from_str::<Vec<serde_json::Value>>(&cell_raw) {
                    if cell.len() >= 3 {
                        if let Some(ts) = cell[0].as_i64() {
                            latest_ts = latest_ts.max(ts);
                        }
                        values.insert(field, cell[2].clone());
                    }
                } else {
                    values.insert(field, serde_json::Value::String(cell_raw));
                }
                i += 2;
            }
            (values, latest_ts)
        }

        /// KEYS rtdata:* → 逐设备 (topic, payload)，照抄 publish_rtdata 组装段。
        pub(super) async fn publish_payloads(
            client: &lux::EmbeddedClient,
            hwid: &str,
        ) -> Vec<(String, serde_json::Value)> {
            let keys = match client.execute_value("KEYS", &["rtdata:*"]).await {
                Ok(lux::EmbeddedValue::Array(a)) => a,
                _ => return Vec::new(),
            };
            let mut out = Vec::new();
            for k in keys {
                let Some(key) = bulk_str(&k) else { continue };
                let devid = key.strip_prefix("rtdata:").unwrap_or(&key).to_string();
                let arr = match client.execute_value("HGETALL", &[&key]).await {
                    Ok(lux::EmbeddedValue::Array(a)) => a,
                    _ => continue,
                };
                let (values, latest_ts) = parse_rtdata_hash(&arr);
                out.push((
                    format!("{hwid}/{devid}/datas"),
                    serde_json::json!({
                        "hwid": hwid,
                        "devid": devid,
                        "values": serde_json::Value::Object(values),
                        "timestamp": latest_ts,
                    }),
                ));
            }
            out
        }
    }

    /// 兼容脚本经 T7 flush 管线（pull → up → Publish）产出的 topic/payload 与旧
    /// MqttPub 组装逐设备逐字段一致（解析后的 JSON 结构相等，与字段序无关）。
    #[tokio::test]
    async fn compat_up_payload_matches_legacy_mqtt_pub() {
        let (client, dir) = test_lux("payload").await;
        let hwid = "hw-x";
        // devA：两点不同 ts（验 max）+ 一个非 [ts,localTime,value] 的 legacy cell
        //（验"原样字符串保留"分支）；devB：单点字符串值。
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("temp".to_string(), SampleValue::Float(25.5));
        vals.insert("hum".to_string(), SampleValue::Int(60));
        crate::db::rtdata::write(&client, hwid, "devA", &vals, 1_700_000_100)
            .await
            .unwrap();
        client
            .execute("HSET", &["rtdata:devA", "devA.rawnote", "not-json"])
            .await
            .unwrap();
        let mut vals2 = std::collections::BTreeMap::new();
        vals2.insert("p".to_string(), SampleValue::Str("on".into()));
        crate::db::rtdata::write(&client, hwid, "devB", &vals2, 1_700_000_200)
            .await
            .unwrap();

        // 对照组：旧 MqttPub 组装
        let legacy = legacy_mqtt_pub::publish_payloads(&client, hwid).await;
        assert_eq!(legacy.len(), 2, "两设备各一条：{legacy:?}");

        // 脚本侧：真实 T7 flush 管线
        let tmp = temp_scripts_root("payload");
        ensure_compat_script(&tmp).unwrap();
        let inst =
            mqtt_compat_instance(&test_app_cfg(), &Hwid::new(hwid), &tmp).expect("host 非空");
        let token = CancellationToken::new();
        let (ctx, _ctl) = host_ctx(&client, hwid, &token);
        let lua = compat_vm(&inst, &tmp, &ctx).expect("兼容脚本应加载并通过 dry-run");
        let ncfg = north_config::parse_north_cfg(&inst.app_config).unwrap();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<contract::MqttCmd>(64);
        let mut buf = Vec::new();
        let out = contract::flush_batch(&lua, &client, &ncfg, &mut buf, &cmd_tx)
            .await
            .expect("flush 应成功");
        assert!(
            matches!(&out, contract::FlushOutcome::Up { parse_fail: 0 }),
            "up() 应零解析失败"
        );
        let items = match cmd_rx.recv().await.expect("应收到 Publish 命令") {
            contract::MqttCmd::Publish(items) => items,
            _ => panic!("期望 MqttCmd::Publish"),
        };
        assert_eq!(
            items.len(),
            2,
            "每设备一条：{:?}",
            items.iter().map(|i| i.topic.clone()).collect::<Vec<_>>()
        );

        // topic 集合一致（顺序与 lux KEYS 序无关，排序后比）
        let mut got: Vec<&str> = items.iter().map(|i| i.topic.as_str()).collect();
        got.sort();
        let mut exp: Vec<&str> = legacy.iter().map(|(t, _)| t.as_str()).collect();
        exp.sort();
        assert_eq!(got, exp, "topic 逐一致");

        // payload 逐字段一致（解析后结构相等）+ qos 0
        for it in &items {
            let expected = &legacy
                .iter()
                .find(|(t, _)| *t == it.topic)
                .expect("topic 应在对照组")
                .1;
            let actual: serde_json::Value =
                serde_json::from_slice(&it.payload).expect("payload 应为合法 JSON");
            assert_eq!(&actual, expected, "payload 逐字段一致：{}", it.topic);
            assert_eq!(it.qos, rumqttc::QoS::AtMostOnce, "qos 0 对齐 MqttPub");
        }

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// down()：合法 CtlMessage JSON → send_command 投到目标实例；非法 JSON / 字段
    /// 缺失 → 静默丢弃（同旧 MqttPub 反序列化失败即忽略）。
    #[tokio::test]
    async fn compat_down_routes_ctlmessage_and_drops_invalid() {
        let (client, dir) = test_lux("down").await;
        let hwid = "hw-x";
        let (ctx, ctl) = host_ctx(&client, hwid, &CancellationToken::new());
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CtlMessage>(8);
        ctl.register("target", cmd_tx);

        let tmp = temp_scripts_root("down");
        ensure_compat_script(&tmp).unwrap();
        let inst =
            mqtt_compat_instance(&test_app_cfg(), &Hwid::new(hwid), &tmp).expect("host 非空");
        let lua = compat_vm(&inst, &tmp, &ctx).expect("兼容脚本应加载");

        // 合法 CtlMessage → 转发目标实例
        let valid = br#"{"instance":"target","devid":"dev1","cmd":"write","data":"{\"v\":1}"}"#;
        contract::call_down(&lua, "hw-x/command", valid)
            .await
            .expect("down 不应报错");
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv())
            .await
            .expect("目标实例应收到命令")
            .expect("通道不应关闭");
        assert_eq!(got.instance.as_ref(), "target");
        assert_eq!(got.devid, "dev1");
        assert_eq!(got.cmd, "write");
        assert_eq!(got.data, "{\"v\":1}");

        // 非 JSON payload → 丢弃、不报错
        contract::call_down(&lua, "hw-x/command", b"not-json")
            .await
            .expect("非法 payload 不应报错");
        assert!(cmd_rx.try_recv().is_err(), "不应投递");

        // 字段缺失（旧 MqttPub serde 反序列化失败即忽略）→ 丢弃
        contract::call_down(&lua, "hw-x/command", br#"{"instance":"target"}"#)
            .await
            .expect("缺字段 payload 不应报错");
        assert!(cmd_rx.try_recv().is_err(), "缺字段不应投递");

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== 编排层（R5 单通道锁 + 生成/持久化/start_sink）=====

    /// 首次：脚本落盘 + lux 持久化 + start_sink（实例 Running、stat 落痕证明脚本
    /// 加载成功——连失败端口 127.0.0.1:1 无需真 broker）；二次调用被 R5 锁拦截；
    /// host 空 → Disabled。
    #[tokio::test]
    async fn start_default_north_generates_persists_starts_and_locks() {
        let (client, dir) = test_lux("orch").await;
        let token = CancellationToken::new();
        let (ctx, _ctl) = host_ctx(&client, "hw-o", &token);
        let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(8);
        let mgr = crate::plugin::instance::InstanceManager::new(ctx.clone(), hub_tx);

        let tmp = temp_scripts_root("orch");
        let mut cfg = test_app_cfg();
        cfg.mqtt.port = 1; // 连失败端口即可
        cfg.mqtt.publish_interval_secs = 1;
        let hwid = Hwid::new("hw-o");

        match start_default_north(&cfg, &hwid, &tmp, &client, &mgr).await {
            DefaultNorth::Started => {}
            other => panic!("首次应 Started：{other:?}"),
        }

        // lux 已持久化（REST 可见），auto_start=false（防 lux autoStart 双启动）
        let raw = crate::api::hget(&client, "instances", DEFAULT_INST_ID)
            .await
            .expect("lux 应有实例配置");
        let inst: InstConfig = serde_json::from_str(&raw).expect("配置应可反序列化");
        assert_eq!(inst.app_code.as_ref(), "luaNorth");
        assert!(!inst.auto_start);
        // 兼容脚本已落盘且为内置版
        let script = std::fs::read_to_string(tmp.join(COMPAT_SCRIPT_NAME).join("main.lua"))
            .expect("脚本应已写入");
        assert_eq!(script, COMPAT_SCRIPT);
        // InstanceManager 状态 Running
        assert!(matches!(
            mgr.status().get(DEFAULT_INST_ID),
            Some(crate::plugin::instance::InstState::Running)
        ));

        // 实例存活 + 脚本加载成功的证明：stat 落痕且 script_version 有效（加载失败
        // 的实例在 tracker 创建前即退出，永远不会写 stat）
        let mut stat_ok = false;
        for _ in 0..40 {
            if let Some(st) = crate::db::stat::read(&client, DEFAULT_INST_ID)
                .await
                .unwrap()
            {
                let ver = st
                    .extra
                    .get("script_version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if ver != "unknown" && (st.status == "running" || st.status == "degraded") {
                    stat_ok = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(
            stat_ok,
            "luaNorth 应加载兼容脚本并存活（stat:{DEFAULT_INST_ID}）"
        );

        // 二次调用：R5 单通道锁
        match start_default_north(&cfg, &hwid, &tmp, &client, &mgr).await {
            DefaultNorth::AlreadyRunning => {}
            other => panic!("二次调用应 AlreadyRunning：{other:?}"),
        }

        // host 空 → Disabled（不生成、不启动）
        let mut cfg_off = cfg.clone();
        cfg_off.mqtt.host = String::new();
        assert!(matches!(
            start_default_north(&cfg_off, &hwid, &tmp, &client, &mgr).await,
            DefaultNorth::Disabled
        ));

        mgr.stop_all().await;
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// lux 已有同名配置 → 沿用不覆盖（用户自定义生效）。
    #[tokio::test]
    async fn start_default_north_uses_existing_config_without_overwrite() {
        let (client, dir) = test_lux("existing").await;
        let token = CancellationToken::new();
        let (ctx, _ctl) = host_ctx(&client, "hw-e", &token);
        let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(8);
        let mgr = crate::plugin::instance::InstanceManager::new(ctx.clone(), hub_tx);

        let tmp = temp_scripts_root("existing");
        // 预写用户自定义配置（broker 不同）
        let custom = serde_json::json!({
            "inst_id": DEFAULT_INST_ID,
            "inst_name": "自定义北向",
            "app_code": "luaNorth",
            "app_type": "north",
            "auto_start": false,
            "app_config": {
                "script": COMPAT_SCRIPT_NAME,
                "host": "custom.example.com",
                "port": 8883,
                "period": 3,
                "batch_max_secs": 3,
                "subscribe": ["custom/cmd"],
                "scripts_root": tmp.to_str().unwrap(),
            },
            "devices": [],
            "host_kind": "builtin",
        });
        client
            .execute("HSET", &["instances", DEFAULT_INST_ID, &custom.to_string()])
            .await
            .unwrap();

        let cfg = test_app_cfg();
        let hwid = Hwid::new("hw-e");
        match start_default_north(&cfg, &hwid, &tmp, &client, &mgr).await {
            DefaultNorth::Started => {}
            other => panic!("既有配置沿用 + 启动应 Started：{other:?}"),
        }
        let raw = crate::api::hget(&client, "instances", DEFAULT_INST_ID)
            .await
            .unwrap();
        let inst: InstConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            inst.app_config["host"], "custom.example.com",
            "用户自定义配置不被覆盖"
        );

        mgr.stop_all().await;
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// lux 既有配置 auto_start=true → 交 lux autoStart 路径启动，本函数不直接
    /// start_sink（R5：仍单通道）。
    #[tokio::test]
    async fn start_default_north_defers_when_autostart_true() {
        let (client, dir) = test_lux("defer").await;
        let token = CancellationToken::new();
        let (ctx, _ctl) = host_ctx(&client, "hw-d", &token);
        let (hub_tx, _hub_rx) = mpsc::channel::<Sample>(8);
        let mgr = crate::plugin::instance::InstanceManager::new(ctx.clone(), hub_tx);

        let tmp = temp_scripts_root("defer");
        let custom = serde_json::json!({
            "inst_id": DEFAULT_INST_ID,
            "app_code": "luaNorth",
            "app_type": "north",
            "auto_start": true,
            "app_config": {
                "script": COMPAT_SCRIPT_NAME,
                "host": "127.0.0.1",
                "port": 1,
                "scripts_root": tmp.to_str().unwrap(),
            },
            "devices": [],
            "host_kind": "builtin",
        });
        client
            .execute("HSET", &["instances", DEFAULT_INST_ID, &custom.to_string()])
            .await
            .unwrap();

        let cfg = test_app_cfg();
        let hwid = Hwid::new("hw-d");
        match start_default_north(&cfg, &hwid, &tmp, &client, &mgr).await {
            DefaultNorth::DeferredToAutoStart => {}
            other => panic!("auto_start=true 应 DeferredToAutoStart：{other:?}"),
        }
        assert!(
            !mgr.status().contains_key(DEFAULT_INST_ID),
            "本函数不应直接 start（由 try_start_from_lux 启动）"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
