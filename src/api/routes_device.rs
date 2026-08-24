//! device CRUD——设备**权威源 = 实例配置（`InstConfig.devices`）∪ `dev2inst`
//! 运行时补集**（D-OWN-3）。
//!
//! 历史设计：lux `devices` Hash 为独立设备注册表，但唯一写入方是 `device/set`
//! 手动 API，无 bootstrap 预置、建实例不同步写——是**孤立死表**（写方=手动 API，
//! 读方=同一 API，前端/pipeline/采集全不用）。REST 客户端读到空表而实例实际在
//! 采集（现场 bug）。已废弃该 Hash，统一到实例配置：
//!
//! - `GET /api/v1/devices`：实例配置**聚合** ∪ dev2inst 补集——lua `set_value`
//!   等运行时产生的设备（无配置点表）也可见，带 `runtime: true` + `owner_inst`
//!   徽标（前端设备树/roster 据此区分运行时设备并显示宿主）；同 devid 配置与
//!   在册并存时**配置优先**（不重复、不覆盖 children）
//! - `GET /api/v1/device/get`：聚合后过滤单设备
//! - `POST /api/v1/device/set`：**单点实例配置操作**（body 含 `inst_id` 定位——
//!   设备挂在实例下，无实例不成设备），读-改-写整包实例配置，免去整包覆盖
//! - `POST /api/v1/device/delete`：同上，从实例点表移除该设备（管理 API 语义
//!   不变：仍写实例配置，运行时设备的清理走 dev2inst 级联闭环）

use crate::api::{hget, AppState};
use crate::types::{DeviceSpec, InstConfig};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct IdQuery {
    pub dev_id: String,
}

/// `GET /api/v1/devices`：聚合所有实例配置的设备表，并入 dev2inst 运行时补集
/// `{dev_id: DeviceSpec}`。
///
/// 实例配置全集 = lux `instances` + **运行态补缺**（config.toml/代码启动的实例
/// 不落 lux，如 demo sim1——不合并则设备实际在采集而 API 返回空表，AI/前端
/// 误报"无设备"。与 routes_inst::merge_roster / cascade_delete 的 mgr 快照
/// 回退同款语义）。运行时补集（D-OWN-3）：lua `set_value` 等产生的无配置
/// 设备经 dev2inst 并入，带 `runtime: true` + `owner_inst`。
pub async fn list(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::Value::Object(aggregate(&st).await))
}

/// `GET /api/v1/device/get?dev_id=`：聚合后过滤单设备；不存在返空对象。
pub async fn get(State(st): State<AppState>, Query(q): Query<IdQuery>) -> Json<serde_json::Value> {
    let agg = aggregate(&st).await;
    match agg.get(&q.dev_id) {
        Some(v) => Json(serde_json::json!({ q.dev_id: v })),
        None => Json(serde_json::json!({})),
    }
}

/// 聚合入口（list/get 共用）：实例配置派生 ∪ dev2inst 运行时补集。
///
/// dev2inst 读失败 → warn 并退化纯配置派生（不 500）。
async fn aggregate(st: &AppState) -> serde_json::Map<String, serde_json::Value> {
    let insts = inst_configs(st).await;
    let mut agg = aggregate_from_insts(&insts);
    let reg = registry_or_empty(crate::db::dev_registry::all(&st.lux).await);
    merge_runtime_entries(&mut agg, reg);
    agg
}

/// lux instances + 运行态补缺的实例配置全集（list/get 取数入口）。
async fn inst_configs(st: &AppState) -> serde_json::Value {
    let lux = crate::api::hgetall(&st.lux, "instances").await;
    let runtime: Vec<(String, InstConfig)> = st
        .mgr
        .status()
        .keys()
        .filter_map(|id| st.mgr.config_snapshot(id).map(|c| (id.clone(), c)))
        .collect();
    merge_runtime_insts(&lux, &runtime)
}

/// 把运行态实例配置并入 lux 全集（纯函数，便于单测）。
///
/// lux 同名优先（落盘配置是持久化权威）；运行态快照只补 lux 缺的实例。
fn merge_runtime_insts(
    lux: &serde_json::Value,
    runtime: &[(String, InstConfig)],
) -> serde_json::Value {
    let mut obj = lux.as_object().cloned().unwrap_or_default();
    for (inst_id, cfg) in runtime {
        if obj.contains_key(inst_id) {
            continue;
        }
        if let Ok(v) = serde_json::to_value(cfg) {
            obj.insert(inst_id.clone(), v);
        }
    }
    serde_json::Value::Object(obj)
}

/// `POST /api/v1/device/set` body：`inst_id` 定位实例 + `dev_id` + `object`（DeviceSpec）。
#[derive(Deserialize)]
pub struct SetReq {
    /// 设备所属实例 ID（单点操作定位）。
    pub inst_id: String,
    /// 设备 ID（作为 DeviceSpec.value 强制一致）。
    pub dev_id: String,
    /// DeviceSpec JSON（label/children/slave_id 等；value 以 dev_id 为准）。
    pub object: serde_json::Value,
}

/// `POST /api/v1/device/set`：单点 upsert 设备到指定实例点表（读-改-写整包实例）。
///
/// 实例不存在 → 404；object 非法 DeviceSpec → 400。upsert 语义：同 dev_id 存在则
/// 替换，否则追加。**不**签发 ext_token（不改 host_kind，与 routes_inst::set 的
/// External 分支无关）。
pub async fn set(State(st): State<AppState>, Json(req): Json<SetReq>) -> Response {
    let raw = match hget(&st.lux, "instances", &req.inst_id).await {
        Some(r) => r,
        None => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    let mut cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid instance config: {e}"),
            )
        }
    };
    let mut dev: DeviceSpec = match serde_json::from_value(req.object) {
        Ok(d) => d,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid device spec: {e}"),
            )
        }
    };
    dev.value = req.dev_id.clone(); // 强制 dev_id 一致（设备 ID 由路径字段决定）
    let mut found = false;
    for d in &mut cfg.devices {
        if d.value == req.dev_id {
            *d = dev.clone();
            found = true;
            break;
        }
    }
    if !found {
        cfg.devices.push(dev);
    }
    let s = match serde_json::to_string(&cfg) {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("serialize: {e}"),
            )
        }
    };
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// `POST /api/v1/device/delete` body：`inst_id` + `dev_id`。
#[derive(Deserialize)]
pub struct DelReq {
    /// 设备所属实例 ID。
    pub inst_id: String,
    /// 设备 ID（从实例点表移除）。
    pub dev_id: String,
}

/// `POST /api/v1/device/delete`：从指定实例点表移除设备（单点）。实例不存在 → 404。
pub async fn delete(State(st): State<AppState>, Json(req): Json<DelReq>) -> Response {
    let raw = match hget(&st.lux, "instances", &req.inst_id).await {
        Some(r) => r,
        None => return err(StatusCode::NOT_FOUND, "instance not found"),
    };
    let mut cfg: InstConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid instance config: {e}"),
            )
        }
    };
    cfg.devices.retain(|d| d.value != req.dev_id);
    // 清被删设备的实时数据（避免孤儿被默认北向实例周期发布）。
    if let Err(e) = crate::db::rtdata::delete_device(&st.lux, &req.dev_id).await {
        tracing::warn!("[routes_device] delete rtdata:{} failed: {e}", req.dev_id);
    }
    let s = match serde_json::to_string(&cfg) {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("serialize: {e}"),
            )
        }
    };
    let _ = st
        .lux
        .execute("HSET", &["instances", &req.inst_id, &s])
        .await;
    Json(serde_json::json!({"status": "deleted"})).into_response()
}

/// 从实例配置表聚合设备：输入 `{inst_id: InstConfig}`，输出 `{dev_id: DeviceSpec}`。
///
/// 每个 DeviceSpec 注入 `inst_id` 字段标明归属实例（MCP/REST 客户端据此识别设备
/// 来自哪个实例）。同 dev 被多实例引用时**合并**：children 按 tag（ChildSpec.value）
/// 去重（首个实例优先），label/slave_id 取首个非空/Some，inst_id 同样首个胜出。
/// 纯函数便于单测。
fn aggregate_from_insts(insts: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    let Some(cfgs) = insts.as_object() else {
        return map;
    };
    for (inst_id, cfg) in cfgs {
        let Some(devices) = cfg.get("devices").and_then(|d| d.as_array()) else {
            continue;
        };
        for dev in devices {
            let Some(dev_id) = dev.get("value").and_then(|v| v.as_str()) else {
                continue;
            };
            if dev_id.is_empty() {
                continue;
            }
            match map.get_mut(dev_id) {
                Some(existing) => merge_children(existing, dev),
                None => {
                    let mut d = dev.clone();
                    if let Some(m) = d.as_object_mut() {
                        m.insert("inst_id".into(), serde_json::json!(inst_id));
                    }
                    map.insert(dev_id.to_string(), d);
                }
            }
        }
    }
    map
}

/// 把 `new` 的 children 并入 `existing`（按 tag 去重，首个实例优先）。
///
/// existing 无 children 时先建空数组；new 无 children 时 no-op。
fn merge_children(existing: &mut serde_json::Value, new: &serde_json::Value) {
    let Some(new_children) = new.get("children").and_then(|c| c.as_array()) else {
        return;
    };
    if existing.get("children").is_none() {
        existing["children"] = serde_json::json!([]);
    }
    let mut seen: std::collections::HashSet<String> = existing
        .get("children")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ch| ch.get("value").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for ch in new_children {
        let Some(tag) = ch.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        if seen.insert(tag.to_string()) {
            if let Some(arr) = existing["children"].as_array_mut() {
                arr.push(ch.clone());
            }
        }
    }
}

/// dev2inst 读结果 → 补集条目集：读失败降级为空补集（warn，不 500）——
/// 聚合退化为纯配置派生，与 routes_inst::cascade 的 dev2inst 失败退化同口径。
fn registry_or_empty(
    r: Result<Vec<(String, Vec<String>)>, crate::error::RsIotError>,
) -> Vec<(String, Vec<String>)> {
    match r {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[routes_device] dev2inst 读失败（聚合退化为纯配置派生）: {e}");
            Vec::new()
        }
    }
}

/// 把 dev2inst 运行时补集并入聚合（纯函数，便于单测）。
///
/// 配置派生优先：聚合已含的 devid 跳过（不重复、不覆盖 children）；仅补配置
/// 没出现的运行时设备，形态 `{"label": devid, "value": devid, "children": [],
/// "runtime": true, "owner_inst": "<宿主列表：单宿主即首项，多宿主逗号串>"}`。
/// 空 owners 条目跳过（owner_inst 空串无意义）。
fn merge_runtime_entries(
    agg: &mut serde_json::Map<String, serde_json::Value>,
    registry: Vec<(String, Vec<String>)>,
) {
    for (devid, owners) in registry {
        if owners.is_empty() || agg.contains_key(&devid) {
            continue; // 空 owners 无意义；配置在册以配置为准
        }
        agg.insert(
            devid.clone(),
            serde_json::json!({
                "label": devid,
                "value": devid,
                "children": [],
                "runtime": true,
                "owner_inst": owners.join(","),
            }),
        );
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
    use serde_json::json;

    fn insts_fixture() -> serde_json::Value {
        json!({
            "mb-1": {
                "inst_id": "mb-1", "app_code": "modbusClient", "app_type": "south",
                "app_config": {}, "auto_start": true, "host_kind": "builtin",
                "devices": [
                    {"label": "设备A", "value": "devA", "children": [
                        {"label": "温度", "value": "temp", "type": "float"},
                        {"label": "湿度", "value": "humidity", "type": "float"}
                    ]},
                    {"label": "设备B", "value": "devB", "children": [
                        {"label": "转速", "value": "speed", "type": "i16"}
                    ]}
                ]
            },
            "mb-2": {
                "inst_id": "mb-2", "app_code": "modbusClient", "app_type": "south",
                "app_config": {}, "auto_start": true, "host_kind": "builtin",
                "devices": [
                    {"label": "", "value": "devA", "children": [
                        {"label": "温度2", "value": "temp", "type": "float"},
                        {"label": "压力", "value": "pressure", "type": "u16"}
                    ]}
                ]
            }
        })
    }

    /// 验证意图（规则7）：聚合必须从实例配置派生——devices Hash 是孤立死表，
    /// 从它读 = 查不到实例实际采集的设备（现场 bug 根因）。
    #[test]
    fn aggregate_collects_all_instances_devices() {
        let agg = aggregate_from_insts(&insts_fixture());
        assert!(agg.contains_key("devA"));
        assert!(agg.contains_key("devB"));
        // devA 被两实例引用 → children 合并去重（temp 只一次，pressure 并入）。
        let dev_a = &agg["devA"];
        let tags: Vec<&str> = dev_a["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["value"].as_str())
            .collect();
        assert_eq!(
            tags,
            vec!["temp", "humidity", "pressure"],
            "合并去重+保留首个实例顺序"
        );
        assert_eq!(dev_a["label"], "设备A", "label 取首个非空");
    }

    /// 归属注入（C 修复）：聚合输出的设备带 `inst_id` 字段标明归属实例——
    /// REST/MCP 客户端据此识别设备来自哪个实例。同 dev 多实例引用时首个胜出。
    #[test]
    fn aggregate_injects_inst_id_ownership() {
        let agg = aggregate_from_insts(&insts_fixture());
        assert_eq!(agg["devA"]["inst_id"], "mb-1", "devA 首个来源实例为 mb-1");
        assert_eq!(agg["devB"]["inst_id"], "mb-1", "devB 归属 mb-1");
        // 同 dev 被 mb-2 引用 → 仍保持首个 inst_id（合并语义：首个实例优先）。
        assert_eq!(agg["devA"]["inst_id"], "mb-1", "合并后归属不漂移");
    }

    #[test]
    fn aggregate_skips_empty_dev_id_and_non_array() {
        let insts = json!({
            "x": {"devices": [{"value": "", "children": []}, {"label": "no-value"}]},
            "y": {"devices": "not-array"}
        });
        let agg = aggregate_from_insts(&insts);
        assert!(agg.is_empty(), "空 dev_id 与非数组均跳过");
    }

    #[test]
    fn aggregate_non_object_insts_returns_empty() {
        assert!(aggregate_from_insts(&json!([1, 2])).is_empty());
    }

    #[test]
    fn merge_children_dedups_by_tag() {
        let mut existing = json!({"value": "d", "children": [{"value": "a", "type": "float"}]});
        let new = json!({"value": "d", "children": [
            {"value": "a", "type": "float"},
            {"value": "b", "type": "i16"}
        ]});
        merge_children(&mut existing, &new);
        let tags: Vec<&str> = existing["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["value"].as_str())
            .collect();
        assert_eq!(tags, vec!["a", "b"], "重复 tag 跳过，新 tag 追加");
    }

    #[test]
    fn merge_children_creates_array_when_missing() {
        let mut existing = json!({"value": "d"});
        let new = json!({"children": [{"value": "a"}]});
        merge_children(&mut existing, &new);
        assert_eq!(existing["children"].as_array().unwrap().len(), 1);
    }

    /// 运行态补缺（现场 bug）：config.toml/代码启动的实例（demo sim1）不落 lux
    /// `instances`——不合并进聚合全集，设备实际在采集而 /devices 返回空表，
    /// AI/前端误报"无设备"。合并语义：lux 同名优先，运行态只补缺。
    #[test]
    fn merge_runtime_insts_fills_gaps_lux_wins() {
        use crate::types::{AppType, HostKind};
        let lux = json!({
            "mb-1": {"inst_id": "mb-1", "app_code": "modbusClient", "app_type": "south",
                     "devices": [{"value": "devLux", "children": []}]}
        });
        let sim1 = InstConfig {
            inst_id: "sim1".into(),
            inst_name: String::new(),
            app_code: "dataSim".into(),
            app_type: AppType::South,
            auto_start: true,
            app_config: serde_json::json!({}),
            devices: vec![DeviceSpec {
                label: "demo".into(),
                value: "devA".into(),
                children: vec![crate::types::ChildSpec {
                    label: "温度".into(),
                    value: "temp".into(),
                    r#type: "float".into(),
                    props: None,
                }],
                slave_id: None,
            }],
            host_kind: HostKind::Builtin,
        };
        // mb-1 也在运行态（快照 devices 不同）→ lux 优先不被覆盖；sim1 补入。
        let mb1_snap = InstConfig {
            inst_id: "mb-1".into(),
            inst_name: String::new(),
            app_code: "modbusClient".into(),
            app_type: AppType::South,
            auto_start: true,
            app_config: serde_json::json!({}),
            devices: vec![],
            host_kind: HostKind::Builtin,
        };
        let merged = merge_runtime_insts(&lux, &[("sim1".into(), sim1), ("mb-1".into(), mb1_snap)]);

        let agg = aggregate_from_insts(&merged);
        assert!(agg.contains_key("devA"), "运行态实例(sim1)的设备并入聚合");
        assert_eq!(agg["devA"]["inst_id"], "sim1");
        assert!(agg.contains_key("devLux"), "lux 实例设备仍在");
        assert!(
            agg["devLux"]["children"].as_array().unwrap().is_empty(),
            "mb-1 同名 lux 优先，运行态空 devices 不覆盖"
        );
    }

    // ===== D-OWN-3：dev2inst 运行时补集并入聚合 =====

    /// 配置设备 + dev2inst 运行时设备（无配置）→ 输出含两者；运行时设备带
    /// `runtime: true` 与 `owner_inst`（单宿主即首项，多宿主逗号串）。
    #[test]
    fn merge_runtime_entries_adds_runtime_badge() {
        let mut agg = aggregate_from_insts(&insts_fixture()); // devA + devB
        merge_runtime_entries(
            &mut agg,
            vec![
                ("rtdev1".into(), vec!["lua-1".into()]),
                ("rtdev2".into(), vec!["inst-a".into(), "inst-b".into()]),
            ],
        );
        assert_eq!(agg.len(), 4, "配置 2 台 + 运行时 2 台 = 4");
        let rt = &agg["rtdev1"];
        assert_eq!(rt["label"], "rtdev1", "运行时设备 label 回退为 devid");
        assert_eq!(rt["value"], "rtdev1");
        assert_eq!(
            rt["children"].as_array().unwrap().len(),
            0,
            "运行时设备无配置点表，children 空数组"
        );
        assert_eq!(rt["runtime"], true, "运行时徽标");
        assert_eq!(rt["owner_inst"], "lua-1", "单宿主 owner_inst 即首项");
        assert_eq!(agg["rtdev2"]["owner_inst"], "inst-a,inst-b", "多宿主逗号串");
        // 配置设备不带新键（向后兼容：前端 monitor/roster 输出零变化）。
        assert!(agg["devA"].get("runtime").is_none());
        assert!(agg["devA"].get("owner_inst").is_none());
    }

    /// 同 devid 配置与在册并存 → 以配置为准：不重复、不覆盖 children、
    /// 不注入 runtime/owner_inst（配置是持久化权威，dev2inst 只是观测补集）。
    #[test]
    fn merge_runtime_entries_config_wins() {
        let mut agg = aggregate_from_insts(&insts_fixture());
        let before = agg["devA"].clone();
        merge_runtime_entries(&mut agg, vec![("devA".into(), vec!["inst-x".into()])]);
        assert_eq!(agg.len(), 2, "同 devid 不产生重复条目");
        assert_eq!(
            agg["devA"], before,
            "配置设备整体不被覆盖（含 children/inst_id）"
        );
        assert!(
            agg["devA"].get("runtime").is_none(),
            "在册不改变配置设备形态"
        );
    }

    /// 空 owners 条目跳过（owner_inst 空串无意义，防御畸形态）。
    #[test]
    fn merge_runtime_entries_skips_empty_owners() {
        let mut agg = aggregate_from_insts(&insts_fixture());
        merge_runtime_entries(&mut agg, vec![("ghost".into(), vec![])]);
        assert_eq!(agg.len(), 2, "空 owners 条目不入聚合");
    }

    /// dev2inst 读失败（`registry_or_empty` 映射 Err → 空补集）→ 聚合退化纯配置
    /// 派生：空补集 merge 后聚合与纯配置输出一致（list/get 返回 Json 不 500）。
    #[test]
    fn registry_read_failure_degrades_to_config_only() {
        let err = crate::error::RsIotError::Io(std::io::Error::other("dev2inst unreadable"));
        let degraded = registry_or_empty(Err(err));
        assert!(degraded.is_empty(), "读失败映射为空补集");
        let mut agg = aggregate_from_insts(&insts_fixture());
        merge_runtime_entries(&mut agg, degraded);
        assert_eq!(
            agg,
            aggregate_from_insts(&insts_fixture()),
            "退化 = 纯配置派生"
        );
    }

    /// 读成功时 `registry_or_empty` 原样透传（不吞正常数据）。
    #[test]
    fn registry_or_empty_passes_ok_through() {
        let reg = vec![("rtdev1".into(), vec!["lua-1".into()])];
        assert_eq!(registry_or_empty(Ok(reg.clone())), reg);
    }
}
