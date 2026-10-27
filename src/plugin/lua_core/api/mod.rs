//! ctx 对象 + host API 注册（spec §7）。L1/L2/L3/L4/L6 在此；L5 http 在 [`http`]；测试在 [`tests`]。
//!
//! Plan 8 Task 1 拆分自原 `api.rs`（守 <500 行，原 616 行）；Task 2 加 L2 数据 API：
//!
//! - `mod.rs`（本文件）：register 编排 + L1 request / L2 数据 / L3 send_command / L4 kv /
//!   L6 log·now·time + register_log + parse_rtdata_value helper。
//! - [`http`]：L5 http_get/post + headers_to_vec + http_request。
//! - [`alarm`]：C6 fire_event/clear_event（Q12 告警，D 阶段 T4）+ AlarmState 护栏。
//! - [`tests`]：API 单测（含 L2 数据闭环）。

#[cfg(test)]
mod tests;

mod alarm;

#[cfg(feature = "lua-http")]
mod http;

mod json;

use crate::host::HostContext;
use crate::plugin::lua_core::io_engine::IoEngine;
use crate::plugin::sample_sender::SampleSender;
use crate::types::{CtlMessage, InstConfig, InstId, Sample, SampleValue};
use mlua::{Lua, LuaSerdeExt, LuaString, Value};
use std::path::Path;

/// 运行时上报设备集（collect 与 L2 `set_value(s)` 双出口共用；flush 时同步 stat）。
///
/// 原定义在 `builtin/lua_client/mod.rs`，阶段 A 审查修复下沉 lua_core：api 层
/// `register`/`push_sample` 与薄壳主循环同为消费者，公共层反向依赖薄壳违背
/// §5.0 分层（B 阶段 luaTransform/luaNorth 复用 api 时会拖进 collector 壳）。
pub(crate) type RuntimeDevs = std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

// ---------- M-3 模型点表快照（校验闸 + 模型感知 API 的数据源） ----------

/// 单 tag 模型快照（名称/类型/单位；地址已在装配期解析，脚本不感知地址语法）。
#[derive(Debug, Clone)]
pub(crate) struct TagSnap {
    pub(crate) name: String,
    pub(crate) data_type: String,
    /// 工程单位（快照预留字段：ctx:tags 当前只暴露名→类型，unit 待 API 增强时
    /// 消费——届时编译器提示移除本 expect）。
    #[expect(dead_code)]
    pub(crate) unit: Option<String>,
}

/// 单设备模型快照（devreg 条目 ∪ 模型模板，装配期 `expand` 产物）。
#[derive(Debug, Clone)]
pub(crate) struct DeviceSnap {
    pub(crate) devid: String,
    pub(crate) model_id: String,
    pub(crate) params: std::collections::BTreeMap<String, String>,
    /// 模型 tags（空 = 未定义点表——迁移空点表设备兼容放行）。
    pub(crate) tags: Vec<TagSnap>,
}

/// 模型点表快照（Arc 共享只读；Collector 装配时建一次，reload 重建）。
pub(crate) type ModelSnapshot =
    std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, DeviceSnap>>>;

/// 从装配展开产物构建快照（devid → 设备模型）。
pub(crate) fn model_snapshot(expanded: &[crate::model::expand::ExpandedDevice]) -> ModelSnapshot {
    let mut m = std::collections::BTreeMap::new();
    for ex in expanded {
        let tags = ex
            .model
            .tags
            .iter()
            .map(|t| TagSnap {
                name: t.name.clone(),
                data_type: t.data_type.clone(),
                unit: t.unit.clone(),
            })
            .collect();
        m.insert(
            ex.devid.clone(),
            DeviceSnap {
                devid: ex.devid.clone(),
                model_id: ex.model.model_id.clone(),
                params: ex.entry.params.clone(),
                tags,
            },
        );
    }
    std::sync::Arc::new(std::sync::Mutex::new(m))
}

/// 自写点记录表（Q6 自环防护，阶段 C T8）：`(devid, tag)` → 最近一次写入时刻。
/// 仅 Role::Transform（luaTransform 壳）构造 ApiCtx 时传 Some——api 层
/// `push_sample` 发送成功后记录，壳层收 Rtdata 时窗口内命中即跳过（自身输出
/// 不触发自身 on_rtdata）；Collector/North 传 None（不记录零开销）。
pub(crate) type SelfWrites = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<(String, String), std::time::Instant>>,
>;

/// 自环防护窗口（Q6，阶段 C T8）：自写记录的保留期——窗口内命中同 `(devid,tag)`
/// 的 Rtdata 视为自身输出回流，跳过不投。取 10s 覆盖 hub 积压 + pipeline 攒批 +
/// 订阅端调度延迟的最坏和。
///
/// 取舍（计划五项已定取舍第 2 条）：窗口内**其他源**（collector/别的 transform）
/// 对同 `(devid,tag)` 的并发写入会被一并静默——Q6 宿主保证的是"自身输出不触发
/// 自身"（per-instance 表），跨源可见性不受保证。
pub(crate) const SELF_LOOP_GUARD_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) use alarm::AlarmStats;

/// mlua::Value → SampleValue（对标 transform/rhai.rs 的类型映射）。
///
/// 下沉缘由同 [`RuntimeDevs`]：collect 返回值解析（薄壳）与 L2 set_value 解析
/// （本层）共用的通用转换，属 lua_core 公共层。
pub(crate) fn lua_value_to_sample(v: &mlua::Value) -> Option<SampleValue> {
    match v {
        mlua::Value::Integer(i) => Some(SampleValue::Int(*i)),
        mlua::Value::Number(n) => Some(SampleValue::Float(*n)),
        mlua::Value::Boolean(b) => Some(SampleValue::Bool(*b)),
        mlua::Value::String(s) => Some(SampleValue::Str(s.to_str().ok()?.to_string())),
        _ => None,
    }
}

/// hub send 结果（`send_with_deadline` 调用方语义：丢样时计数、关闭时告警）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    /// 已入队（hub 正常消费中）。
    Sent,
    /// hub channel 已关（接收端 drop，实例停止/管道退出）。
    Closed,
    /// 背压超 2s 预算，样本被丢弃。
    Dropped,
}

/// 带 2s 背压超时的 hub send（Q3/附录 B 冻结面①：控制面 stop/reload/cmd 永不被
/// 数据面反压冻结）。满 channel 无消费者时 2s 后丢样返回 [`SendOutcome::Dropped`]，
/// 而非裸 `tx.send` 的无限期挂起；hub 关闭返回 [`SendOutcome::Closed`]。
///
/// `tx` 是 [`SampleSender`]（D-OWN-1）：经遮蔽 send 转发前登记 dev2inst——登记
/// 仅 seen miss 首次发生（微秒级），正常被 2s 预算覆盖。
///
/// collect 分支（`builtin/lua_client/mod.rs`）与 `push_sample`（set_value(s)）共用；
/// 丢样计数 `dropped` 由调用方持有并 fetch_add（flush 时落 stat `dropped_samples`）。
pub(crate) async fn send_with_deadline(tx: &SampleSender, sample: Sample) -> SendOutcome {
    match tokio::time::timeout(std::time::Duration::from_secs(2), tx.send(sample)).await {
        Ok(Ok(())) => SendOutcome::Sent,
        Ok(Err(_)) => SendOutcome::Closed,
        Err(_) => SendOutcome::Dropped,
    }
}

/// [`send_with_deadline`] 的非等待版（阶段 A 审查修复）：同一 tick 内首次 send 已
/// 超预算（hub 持续满）后，剩余样本不再逐条各等 2s——立即入队或丢弃，控制面
/// 不被数据面冻结（语义映射与 [`send_with_deadline`] 一致）。try_send 内的归属
/// 登记走 detached task（不占本路径等待，见 [`SampleSender::try_send`]）。
pub(crate) fn try_send_outcome(tx: &SampleSender, sample: Sample) -> SendOutcome {
    match tx.try_send(sample) {
        Ok(()) => SendOutcome::Sent,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => SendOutcome::Dropped,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
    }
}

/// M-3 校验闸：devid ∈ 快照设备 ∧ tags ⊆ 模型 tags（模型 tags 非空时）。
///
/// 语义：**未建模不放行**（严格校验，决策 D-M4）；空 tags 模型 = 迁移空点表设备，
/// 兼容放行（无点表可校验，新建设备模型必有 tags）。返回 Err 消息并计数。
pub(crate) fn gate_check(
    model: &Option<ModelSnapshot>,
    devid: &str,
    tags: &[String],
    gate_rejected: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<(), String> {
    let Some(snapshot) = model else {
        return Ok(()); // 非 Collector（transform/north）：无闸
    };
    let snap = snapshot.lock().unwrap();
    let Some(dev) = snap.get(devid) else {
        gate_rejected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Err(format!(
            "设备 {devid} 未绑定到本实例（模型点表快照无此设备）——先建设备并绑定"
        ));
    };
    if dev.tags.is_empty() {
        return Ok(()); // 空模型（迁移空点表）：兼容放行
    }
    for t in tags {
        if !dev.tags.iter().any(|s| s.name == *t) {
            gate_rejected.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(format!(
                "标签 {devid}.{t} 未在模型 {} 点表（tags: {})——先建模",
                dev.model_id,
                dev.tags
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
    }
    Ok(())
}

/// 闸拒 tag 的发现队列登记（决策 D-M4"不过闸进 discoveries"，审查修复）：
/// 按模型快照筛出真正未建模的 tag（设备未绑定 → 全部登记），带值 fail-soft
/// 进队——数据本身不放行（严格闸语义不变），元数据可见供 adopt 建模。
async fn note_gate_rejected(
    lux: &lux::EmbeddedClient,
    hwid: &crate::types::Hwid,
    model: &Option<ModelSnapshot>,
    devid: &str,
    values: &std::collections::BTreeMap<String, crate::types::SampleValue>,
) {
    let Some(snapshot) = model else {
        return; // 非 Collector 无闸（gate_check 不会 Err，防御）
    };
    let rejected = {
        let snap = snapshot.lock().unwrap();
        match snap.get(devid) {
            None => values.clone(),
            Some(dev) => values
                .iter()
                .filter(|(t, _)| !dev.tags.iter().any(|s| s.name == **t))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    };
    crate::model::discovery::note_rejected(lux, hwid, devid, rejected).await;
}

/// 组 `Sample` 推 hub（`set_value`/`set_values` 共用，DRY）。
///
/// `values` 空 → 直接返回（无点不上报）。send 经 [`send_with_deadline`]（2s 背压
/// 超时，hub 关 → warn，超时 → 丢样 + `dropped` 计数；对标 `run()` collect 推 hub
/// 容错语义）。timestamp 取当前本地秒。上报的 devid 记入 `runtime_devs`
/// （运行时设备集，collect 与 L2 上报共用出口）。发送成功且 `self_writes` 为 Some
/// 时记自写点（Q6 自环防护——丢样/关闭不产生回流，不记）。
async fn push_sample(
    tx: &SampleSender,
    hwid: &crate::types::Hwid,
    runtime_devs: &RuntimeDevs,
    dropped: &std::sync::atomic::AtomicU64,
    self_writes: &Option<SelfWrites>,
    devid: String,
    values: std::collections::BTreeMap<String, SampleValue>,
) {
    if values.is_empty() {
        return;
    }
    if let Ok(mut set) = runtime_devs.lock() {
        set.insert(devid.clone());
    }
    // Q6 自环防护（T8）：发送成功才记录——键先快照（sample 构造后 devid/values
    // 已 move；None（Collector/North）不分配）。
    let sw_keys = self_writes
        .as_ref()
        .map(|_| self_write_keys(&devid, &values));
    let sample = Sample {
        hwid: hwid.clone(),
        devid,
        values,
        timestamp: chrono::Local::now().timestamp(),
    };
    match send_with_deadline(tx, sample).await {
        SendOutcome::Sent => {
            if let (Some(sw), Some(keys)) = (self_writes.as_ref(), sw_keys) {
                record_self_writes(sw, keys);
            }
        }
        SendOutcome::Closed => tracing::warn!("[luaClient] set_value(s): hub 关闭，值未上报"),
        SendOutcome::Dropped => {
            dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                "[luaClient] set_value(s): hub 背压超 2s，丢样（累计 {}）",
                dropped.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
}

/// 剥 tag 的 `<devid>.` 设备前缀（终审计杂项登记：该语义原先 4 处各自展开，收敛
/// 单一事实源）。tag 双形态（"tag" / "devid.tag"，与 Sample.values 同源）：前缀命中
/// 返回剥后 tag，不命中原样返回（不可剥形态如 devid=dev1 时的 "dev2.temp"）。
/// 消费方：本文件 [`self_write_keys`]/get_device、luaTransform 投递点、luaNorth
/// `pull_rtdata_batch`。手写字节比较而非 `strip_prefix(&format!(...))`：投递热路径
/// 逐点调用，免每次分配；`starts_with` 保证 `devid.len()` 是 tag 的字符边界，
/// `[n] == b'.'` 命中后 `n+1` 亦是边界，切片安全。
pub(crate) fn strip_dev_prefix<'a>(devid: &str, tag: &'a str) -> &'a str {
    let n = devid.len();
    if tag.len() > n && tag.as_bytes()[n] == b'.' && tag.starts_with(devid) {
        &tag[n + 1..]
    } else {
        tag
    }
}

/// 自写键快照（Q6 自环防护，T8）：`(devid,tag)` 对；tag 双形态（"tag" /
/// "devid.tag"，与 Sample.values 同源）——壳层投递路径剥 dev 前缀后查表，两种
/// 形态都记保证任一写法可命中。
fn self_write_keys(
    devid: &str,
    values: &std::collections::BTreeMap<String, SampleValue>,
) -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = values
        .keys()
        .map(|t| (devid.to_string(), t.clone()))
        .collect();
    // 第二形态仅当真的剥短了（无前缀时 helper 原样返回，长度不变）。
    keys.extend(values.keys().filter_map(|t| {
        let s = strip_dev_prefix(devid, t);
        (s.len() < t.len()).then(|| (devid.to_string(), s.to_string()))
    }));
    keys
}

/// 自写点记入（Q6 自环防护，T8）：`键 -> now`；顺带 retain 清理窗口外条目（防
/// map 无界膨胀，计划 T8 要求）：写入点数有界（窗口 10s 内的活跃点，通常几十），
/// 全量 O(n) 扫描即可，不引入后台清理任务（取舍：低频写场景下简单性优于每次
/// 多扫几十项的微开销）。
fn record_self_writes(self_writes: &SelfWrites, keys: Vec<(String, String)>) {
    if let Ok(mut m) = self_writes.lock() {
        let now = std::time::Instant::now();
        m.retain(|_, t| now.duration_since(*t) < SELF_LOOP_GUARD_WINDOW);
        for k in keys {
            m.insert(k, now);
        }
    }
}

/// 实例角色（能力包裁剪，设计 §5.2 角色与能力包矩阵）：[`register`] 按角色跳过
/// 矩阵中标记 ✗ 的能力注册点。v1 三角色（§5.0 一套沙箱多种角色）：
/// - Collector（luaClient）：全量能力（含 C1 request/transport）。
/// - North（luaNorth / mqtt-north）：无 C1——request/transport 不注册（B 阶段 T6）。
/// - Transform（luaTransform，C 阶段 T4）：C0/C2/C4/C6 ✓，C1/C3/C5 ✗——
///   request/transport/send_command/http 不注册（数据加工不出网不下行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Role {
    /// collector：全量能力（luaClient 构造 ApiCtx 的缺省值，行为零变化）。
    #[default]
    Collector,
    /// mqtt-north：§5.2 矩阵 mqtt-north 列（C0/C2/C4/C6 ✓、C1 ✗、C3 send_command
    /// 恒注入——Q5 定案 down() 下行依赖，R3 显式授权收紧排后续 task）。
    North,
    /// transform：§5.2 矩阵 transform 列（C 阶段 T4）——C0/C2/C4/C6 ✓、
    /// C1/C3/C5 ✗（订阅型数据加工：读 Rtdata 写派生点，无传输层/下行/出网）。
    /// secret_get 恒注入（Q4/R3 授权清单门控，非角色裁剪）。
    Transform,
}

/// [`register`] 的组装上下文（阶段 A 审查遗留收敛：9 参 → struct；B 阶段 north
/// 能力包再加字段不再扩参）。字段与原 register 参数一一对应；`inst`/
/// `scripts_root` 借用调用方（零克隆），其余为廉价句柄。T6 起加 [`Role`]（能力包
/// 裁剪）且 `io` 转 Option（North 无传输层不构建 IO 引擎——C1 request 是其唯一消费者）。
pub(crate) struct ApiCtx<'a> {
    /// 宿主上下文：提供 lux（L2/L4 读写）+ `ctl_registry`（L3 路由）。
    pub(crate) ctx_host: HostContext,
    /// 实例配置（L2 `ctx:list_devices` 返 `inst.devices`；log 前缀取 `inst_id`）。
    pub(crate) inst: &'a InstConfig,
    /// IO 引擎（L1 `ctx:request` 内部 `io.request` → IO task 独占处理，spec §8 模式 Z）。
    /// North 角色传 None：无传输层不 spawn IO task，C1 随 [`Role`] 裁剪不注册。
    pub(crate) io: Option<IoEngine>,
    /// hub 发送端（L2 `set_value` 推 [`Sample`]）：[`SampleSender`]——set_value(s)
    /// 产出的运行时设备同样经遮蔽 send 登记 dev2inst（D-OWN-1，lua 全角色覆盖）。
    pub(crate) tx: SampleSender,
    /// 运行时上报设备集（collect 与 L2 `set_value(s)` 双出口共用，flush 时同步
    /// stat；详见 [`RuntimeDevs`]）。
    pub(crate) runtime_devs: RuntimeDevs,
    /// 丢样计数（`push_sample` 背压超时分支 fetch_add；与 collect 分支共享同一
    /// 实例，flush 时落 stat `dropped_samples`）。
    pub(crate) dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// 自写点记录（Q6 自环防护，T8）：Some 仅 Transform——[`push_sample`] 发送
    /// 成功后记 `(devid,tag) -> Instant`；luaTransform 壳在 Rtdata 投递路径消费
    ///（窗口内命中跳过该点）。Collector/North 传 None（详见 [`SelfWrites`]）。
    pub(crate) self_writes: Option<SelfWrites>,
    /// 底层链路类型字符串（`cfg.transport.as_str()`，"tcp"/"serial"/"udp"），供
    /// `ctx:transport()` 暴露给脚本（脚本据此选帧格式：serial 加 CRC16、tcp 加
    /// MBAP 头）。North 角色不注册 transport，值不被消费（占位 "mqtt"）。
    pub(crate) transport: &'static str,
    /// 脚本根目录（全局 `stdlib` 表读 `scripts/_stdlib/.index.json` 合并种子，T9 §3.5）。
    pub(crate) scripts_root: &'a Path,
    /// 实例角色（能力包裁剪，§5.2）：缺省 Collector（luaClient 行为零变化）。
    pub(crate) role: Role,
    /// 告警护栏状态（Q12 T4，spec §3）：fire 频控 20/s + 同 key 1s 抖动表 +
    /// 频控计数。三壳各建一份（reload 复用 ac 计数不丢）；`ctx:fire_event`/
    /// `clear_event` 消费（C6 跨角色全注入），壳层 flush 读计数落 stat
    /// `alarm_rate_limited`（详见 [`alarm::AlarmState`]）。
    pub(crate) alarm: AlarmStats,
    /// 模型点表快照（M-3）：Collector 装配期展开产物——`set_value/set_values`
    /// 校验闸 + `ctx:devices()/tags()/param()` 数据源。Transform/North 传 None
    /// （加工/北向不是采集源，无闸、无模型感知 API）。
    pub(crate) model: Option<ModelSnapshot>,
    /// 校验闸拒绝计数（M-3，`set_value` 未建模上报；collect 闸用同 Arc 共享，
    /// 壳层 flush 落 stat `gate_rejected`）。
    pub(crate) gate_rejected: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// 在 `lua` 上注册 ctx 全局对象 + host 函数（`run` 调用）。
///
/// - L1 `ctx:request(frame, timeout_ms) -> resp|nil,err`：async，经 `create_async_function`
///   桥接，脚本 `ctx:request(...)` 调用时 mlua 自动驱动协程，内部 `io.request` → IO task
///   独占处理 → oneshot 回执（spec §8 模式 Z）。
/// - L2 `ctx:set_value(devid, tag, v)`：async，推 [`Sample`] 到 hub（数据上行总线，与
///   collect 同路径，pipeline 入库）；devid 由脚本显式传入（多设备支持）。
/// - L2 `ctx:set_values(devid, {tag=v,...})`：async，批量推一个设备多 tag（一个 [`Sample`]，
///   多点同时刻 timestamp，贴合 `on_data` 被动帧多点一次上报）。
/// - L2 `ctx:get_value(devid, tag) -> str|nil`：async，读 lux `rtdata:<devid>` 最新值
///   （`[ts, localTime, value]` cell → value 字符串）。
/// - L2 `ctx:get_device(devid) -> table|nil`：async，读 lux `HGETALL rtdata:<devid>`
///   → `{tag: value_str}`（设备当前全部数据点）。
/// - L2 `ctx:list_devices() -> table`：返 `inst.devices`（`{label, value}` 数组，同步）。
/// - L3 `ctx:send_command(inst, devid, cmd, data) -> bool`：经 [`crate::host::CtlRegistry`]
///   投递（`try_send` 非阻塞）。
/// - L4 `ctx:kv_set(key, v)` / `ctx:kv_get(key) -> v|nil`：async，落 lux `luakv:<inst>`
///   （[`crate::db::luakv`]），脚本本地持久状态——跨重启保留、实例间隔离。value 经
///   `serde_json::Value` 中转（Lua value <-> JSON 文本）。
/// - L4 `ctx:secret_get(ref) -> str|nil`：async，读 lux 全局 `secrets` 表（平台凭证，
///   [`crate::db::secrets`]）。恒注入 + 实例配置 `app_config.secrets` 字符串数组显式
///   授权（Q4/R3）：越权引用报错（可感知，非静默 nil）；在清单但未写 → nil；读取
///   不落日志（审计点在管理面写入/删除）。
/// - C6 `ctx:fire_event(key, level, message) -> bool|nil,err` / `ctx:clear_event(key) -> bool`：
///   async，告警触发/清除（Q12 T4，spec `2026-08-21-alarm` §2）——lux `alarms` Hash
///   （count++ 覆盖写 / HDEL 幂等）+ EventBus `Alarm` 帧（同 key 1s 抖动；fire 20/s
///   频控超限返 false；source=inst_id 宿主填脚本不可伪造）。三角色全注入（§5.2 C6
///   行 ✓✓✓✓）；实现移 [`alarm`]。
/// - L6 `log_info/warn/err/debug(msg)`：tracing，前缀 `[lua:<inst>]`。
/// - L6 `now()`/`time()`：当前本地毫秒/秒时间戳（f64）。
/// - 全局 `stdlib` 表（T9 §3.5）：`stdlib.list()/info(mod)` 只读清单——本地 index
///   与 embed 种子合并视图（[`ApiCtx::scripts_root`] 即为其数据源）。
///
/// 组装参数见 [`ApiCtx`]（9 参收敛 struct，阶段 B T3）。
pub(crate) fn register(lua: &Lua, ac: &ApiCtx<'_>) -> mlua::Result<()> {
    let ctx = lua.create_table()?;

    // --- L1 ctx:request(frame:str, timeout_ms:num) -> resp:str | nil,err ---
    // create_async_function：ctx:request 内部 coroutine.yield 桥接 IO task（spec §8）。
    // 首参 `_self` 接收 Lua 方法调用语法 `ctx:request(...)` 隐式传入的 self（ctx 表本身），
    // 不使用——若漏掉会导致 mlua 把 ctx 表当 frame 解析（bad argument #1: Lua table to string）。
    // 角色裁剪（§5.2 矩阵 mqtt-north 列无 C1，B 阶段 T6）：North 不注册 request；
    // io=None 是 North 的合法形态（C1 的唯一消费者就是 request）。
    if let (Role::Collector, Some(io_engine)) = (&ac.role, &ac.io) {
        let io_req = io_engine.clone();
        let request_fn = lua.create_async_function(
            move |lua, (_self, frame, timeout_ms): (Value, LuaString, u64)| {
                let io = io_req.clone();
                async move {
                    let bytes = frame.as_bytes().to_vec();
                    match io.request(&bytes, timeout_ms).await {
                        Ok(resp) => Ok::<_, mlua::Error>((
                            Value::String(lua.create_string(&resp)?),
                            Value::Nil,
                        )),
                        Err(e) => {
                            Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
                        }
                    }
                }
            },
        )?;
        ctx.set("request", request_fn)?;
    }

    // --- L6 log_info / log_warn / log_err / log_debug ---
    let name = ac.inst.inst_id.as_str().to_string();
    register_log(lua, &ctx, &name)?;

    // --- L6 now() / time() ---
    ctx.set(
        "now",
        lua.create_function(|_lua, _self: Value| {
            Ok(chrono::Local::now().timestamp_millis() as f64)
        })?,
    )?;
    ctx.set(
        "time",
        lua.create_function(|_lua, _self: Value| Ok(chrono::Local::now().timestamp() as f64))?,
    )?;

    // --- L4 ctx:kv_set(key, v) / ctx:kv_get(key) -> v|nil ---
    // value 经 serde_json::Value 中转（Lua value <-> JSON 文本），落 lux luakv:<inst>。
    // async（lux 操作 await）→ create_async_function；_self 吸收冒号隐式 self。
    let lux_set = ac.ctx_host.lux.clone();
    let inst_set = ac.inst.inst_id.as_str().to_string();
    let kv_set_fn =
        lua.create_async_function(move |lua, (_self, key, v): (Value, LuaString, Value)| {
            let lux = lux_set.clone();
            let inst = inst_set.clone();
            async move {
                let key = key.to_string_lossy().to_string();
                let json: serde_json::Value = lua.from_value(v)?;
                let s = serde_json::to_string(&json)
                    .map_err(|e| mlua::Error::RuntimeError(format!("kv_set 序列化: {e}")))?;
                crate::db::luakv::set(&lux, &inst, &key, &s)
                    .await
                    .map_err(|e| mlua::Error::RuntimeError(format!("kv_set: {e}")))?;
                Ok(())
            }
        })?;
    ctx.set("kv_set", kv_set_fn)?;

    let lux_get = ac.ctx_host.lux.clone();
    let inst_get = ac.inst.inst_id.as_str().to_string();
    let kv_get_fn = lua.create_async_function(move |lua, (_self, key): (Value, LuaString)| {
        let lux = lux_get.clone();
        let inst = inst_get.clone();
        async move {
            let key = key.to_string_lossy().to_string();
            match crate::db::luakv::get(&lux, &inst, &key).await {
                Ok(Some(s)) => {
                    let json: serde_json::Value = serde_json::from_str(&s)
                        .map_err(|e| mlua::Error::RuntimeError(format!("kv_get 反序列化: {e}")))?;
                    let v: Value = lua.to_value(&json)?;
                    Ok(v)
                }
                Ok(None) => Ok(Value::Nil),
                Err(e) => Err(mlua::Error::RuntimeError(format!("kv_get: {e}"))),
            }
        }
    })?;
    ctx.set("kv_get", kv_get_fn)?;

    // --- L4 ctx:secret_get(ref) -> str|nil（Q4/R3/D6：实例授权清单显式授权）---
    // lux 全局 secrets 表受控读取（[crate::db::secrets]，REST 管理面 T4 写入）。
    // 函数恒注入（Q4 定案：未配置 secrets 的实例也注册——未引用零成本）；授权 =
    // 实例配置 app_config.secrets 字符串数组显式列出的 ref。审计定案：读取不落日志
    // （高频回调里打日志会刷屏；Q4 审计点是管理面写入/删除，T4 routes_secret 已做）。
    // 清单在 register 期解析一次（类型不符按空清单降级 + warn 一次，非每次调用）。
    let secret_allowed: Vec<String> = match ac.inst.app_config.get("secrets") {
        None => Vec::new(), // 可选键：未配置 = 空清单（任何引用都未授权）
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(other) => {
            tracing::warn!(
                "[lua:{}] app_config.secrets 类型不符（期望字符串数组，实 {other}），按空清单处理",
                ac.inst.inst_id
            );
            Vec::new()
        }
    };
    let lux_secret = ac.ctx_host.lux.clone();
    let secret_fn = lua.create_async_function(move |lua, (_self, r): (Value, LuaString)| {
        let lux = lux_secret.clone();
        let allowed = secret_allowed.clone();
        async move {
            let r = r.to_string_lossy().to_string();
            // 前缀防线（脚本侧）：防读任意 lux key 的企图——与 validate_ref 同
            // 规则、先于授权检查（格式非法时提示加前缀，而非误导去改清单）。
            if let Err(e) = crate::db::secrets::validate_ref(&r) {
                return Err(mlua::Error::RuntimeError(format!("secret_get: {e}")));
            }
            // 授权清单（R3 越权可感知）：不在清单 → 报错而非静默 nil（脚本作者
            // 需要知道为什么读不到；ref 非机密可入消息，value 绝不出现）。
            if !allowed.contains(&r) {
                return Err(mlua::Error::RuntimeError(format!(
                    "secret 未授权：{r}（请检查实例配置 secrets 清单）"
                )));
            }
            // 在清单但 lux 未写 → nil（配置了但没写入，脚本可判空给默认）。
            match crate::db::secrets::get(&lux, &r).await {
                Ok(Some(v)) => Ok(Value::String(lua.create_string(&v)?)),
                Ok(None) => Ok(Value::Nil),
                Err(e) => Err(mlua::Error::RuntimeError(format!("secret_get: {e}"))),
            }
        }
    })?;
    ctx.set("secret_get", secret_fn)?;

    // --- L3 ctx:send_command(inst_id, devid, cmd, data) -> bool ---
    // 经 CtlRegistry::send 投到目标实例 cmd_rx（与 MQTT/REST 写入口同总线）。
    // send 是同步 try_send → create_function（非 async）；_self 吸收冒号隐式 self。
    // 角色裁剪（§5.2 矩阵 transform ✗，C 阶段 T4）：Transform 不注册 send_command
    //（订阅型数据加工无下行）；north 恒注入不受影响（Q5 定案 down() 下行依赖）。
    if ac.role != Role::Transform {
        let ctl = ac.ctx_host.ctl_registry.clone();
        let send_fn = lua.create_function(
            move |_lua,
                  (_self, inst_id, devid, cmd, data): (
                Value,
                LuaString,
                LuaString,
                LuaString,
                LuaString,
            )| {
                let msg = CtlMessage {
                    instance: InstId::from(inst_id.to_string_lossy().to_string()),
                    devid: devid.to_string_lossy().to_string(),
                    cmd: cmd.to_string_lossy().to_string(),
                    data: data.to_string_lossy().to_string(),
                };
                let inst_str = msg.instance.to_string();
                Ok(ctl.send(&inst_str, msg))
            },
        )?;
        ctx.set("send_command", send_fn)?;
    }

    // --- L2 ctx:set_value(devid, tag, v) → 推 Sample 到 hub（数据上行总线）---
    // devid 由脚本显式传入（多设备支持：脚本可对 ctx:list_devices() 的任一 devid 上报）。
    // value 经 lua_value_to_sample 转 SampleValue；单点便捷，等价 set_values(devid,{[tag]=v})。
    // M-3 校验闸（Collector + 快照）：devid ∈ 绑定 ∧ tag ∈ 模型 tags → 否则 lua error
    //（脚本 pcall 可捕获）+ 宿主 warn + gate_rejected 计数。
    let tx_set = ac.tx.clone();
    let hwid_set = ac.ctx_host.hwid.clone();
    let lux_set = ac.ctx_host.lux.clone();
    let rt_set = ac.runtime_devs.clone();
    let drop_set = ac.dropped.clone();
    let sw_set = ac.self_writes.clone();
    let model_set = ac.model.clone();
    let gate_set = ac.gate_rejected.clone();
    let set_value_fn = lua.create_async_function(
        move |_lua, (_self, devid, tag, v): (Value, LuaString, LuaString, Value)| {
            let tx = tx_set.clone();
            let hwid = hwid_set.clone();
            let lux_d = lux_set.clone();
            let rt = rt_set.clone();
            let dropped = drop_set.clone();
            let self_writes = sw_set.clone();
            let model = model_set.clone();
            let gate = gate_set.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                if let Err(msg) = gate_check(&model, &devid, std::slice::from_ref(&tag), &gate) {
                    // D-M4：未建模 tag 进发现队列（数据不放行），供 adopt 建模。
                    let sv = lua_value_to_sample(&v)
                        .unwrap_or_else(|| crate::types::SampleValue::Str(format!("{v:?}")));
                    let mut vals = std::collections::BTreeMap::new();
                    vals.insert(tag, sv);
                    note_gate_rejected(&lux_d, &hwid, &model, &devid, &vals).await;
                    return Err(mlua::Error::RuntimeError(msg));
                }
                let sv = lua_value_to_sample(&v)
                    .ok_or_else(|| mlua::Error::RuntimeError("set_value: 不支持的值类型".into()))?;
                let mut values = std::collections::BTreeMap::new();
                values.insert(tag, sv);
                push_sample(&tx, &hwid, &rt, &dropped, &self_writes, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_value", set_value_fn)?;

    // --- L2 ctx:set_values(devid, {tag=v,...}) → 批量推一个设备多 tag（一个 Sample）---
    // 贴合 Sample 多变量语义：on_data 解析被动帧多点一次上报（点间同时刻 timestamp）。
    // table 经 pairs 遍历 → BTreeMap；空 table 不上报。M-3 校验闸同 set_value。
    let tx_sv = ac.tx.clone();
    let hwid_sv = ac.ctx_host.hwid.clone();
    let lux_sv = ac.ctx_host.lux.clone();
    let rt_sv = ac.runtime_devs.clone();
    let drop_sv = ac.dropped.clone();
    let sw_sv = ac.self_writes.clone();
    let model_sv = ac.model.clone();
    let gate_sv = ac.gate_rejected.clone();
    let set_values_fn = lua.create_async_function(
        move |_lua, (_self, devid, table): (Value, LuaString, mlua::Table)| {
            let tx = tx_sv.clone();
            let hwid = hwid_sv.clone();
            let lux_d = lux_sv.clone();
            let rt = rt_sv.clone();
            let dropped = drop_sv.clone();
            let self_writes = sw_sv.clone();
            let model = model_sv.clone();
            let gate = gate_sv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let mut values = std::collections::BTreeMap::new();
                for pair in table.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    if let Some(sv) = lua_value_to_sample(&v) {
                        values.insert(k, sv);
                    }
                }
                let tags: Vec<String> = values.keys().cloned().collect();
                if let Err(msg) = gate_check(&model, &devid, &tags, &gate) {
                    note_gate_rejected(&lux_d, &hwid, &model, &devid, &values).await;
                    return Err(mlua::Error::RuntimeError(msg));
                }
                push_sample(&tx, &hwid, &rt, &dropped, &self_writes, devid, values).await;
                Ok(())
            }
        },
    )?;
    ctx.set("set_values", set_values_fn)?;

    // --- L2 ctx:get_value(devid, tag) -> str|nil ---
    // 读 lux HGET rtdata:<devid> field=<devid>.<tag> → [ts, localTime, value] → value_str。
    // field 已含 devid 前缀（pipeline 写入格式）则原样用，否则补 <devid>.<tag>。
    let lux_gv = ac.ctx_host.lux.clone();
    let get_value_fn = lua.create_async_function(
        move |lua, (_self, devid, tag): (Value, LuaString, LuaString)| {
            let lux = lux_gv.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let tag = tag.to_string_lossy().to_string();
                let field = if tag.contains(&devid) {
                    tag
                } else {
                    format!("{devid}.{tag}")
                };
                match lux
                    .execute_value("HGET", &[&format!("rtdata:{devid}"), &field])
                    .await
                {
                    Ok(v) => {
                        let s = match v {
                            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(&b).to_string(),
                            lux::EmbeddedValue::Simple(s) => s,
                            _ => return Ok(Value::Nil),
                        };
                        match parse_rtdata_value(&s) {
                            Some(val) => Ok(Value::String(lua.create_string(&val)?)),
                            None => Ok(Value::Nil),
                        }
                    }
                    Err(_) => Ok(Value::Nil),
                }
            }
        },
    )?;
    ctx.set("get_value", get_value_fn)?;

    // --- L2 ctx:get_device(devid) -> table|nil ---
    // 读 lux HGETALL rtdata:<devid> → {tag: value_str}（设备当前全部数据点）。
    // field=<devid>.<tag> → 剥前缀得 tag；cell=[ts,localTime,value] → value_str。
    let lux_gd = ac.ctx_host.lux.clone();
    let get_device_fn =
        lua.create_async_function(move |lua, (_self, devid): (Value, LuaString)| {
            let lux = lux_gd.clone();
            async move {
                let devid = devid.to_string_lossy().to_string();
                let key = format!("rtdata:{devid}");
                match lux.execute_value("HGETALL", &[&key]).await {
                    Ok(lux::EmbeddedValue::Array(arr)) => {
                        if arr.is_empty() {
                            // 缺省设备（lux 无 rtdata:<devid>）→ HGETALL 返空数组，返回 nil
                            // （与非 Array 分支一致语义：无数据点即设备不存在）。
                            return Ok(Value::Nil);
                        }
                        let t = lua.create_table()?;
                        let mut i = 0;
                        while i + 1 < arr.len() {
                            let field = match &arr[i] {
                                lux::EmbeddedValue::Bulk(b) => {
                                    String::from_utf8_lossy(b).into_owned()
                                }
                                lux::EmbeddedValue::Simple(s) => s.clone(),
                                _ => {
                                    i += 2;
                                    continue;
                                }
                            };
                            let raw = match &arr[i + 1] {
                                lux::EmbeddedValue::Bulk(b) => {
                                    String::from_utf8_lossy(b).into_owned()
                                }
                                lux::EmbeddedValue::Simple(s) => s.clone(),
                                _ => {
                                    i += 2;
                                    continue;
                                }
                            };
                            let tag = strip_dev_prefix(&devid, &field);
                            if let Some(v) = parse_rtdata_value(&raw) {
                                t.set(tag, v)?;
                            }
                            i += 2;
                        }
                        Ok(Value::Table(t))
                    }
                    _ => Ok(Value::Nil),
                }
            }
        })?;
    ctx.set("get_device", get_device_fn)?;

    // --- L2 ctx:list_devices() -> table ---
    // 返实例绑定设备（M-2 起绑定形态 {label=devid, value=devid} 数组；点表经
    // 模型查询——L3 ctx:tags(devid) 属 M-3 lua v2）。同步（inst.devices 已知，
    // 无 lux IO）。
    let devices: Vec<(String, String)> = ac
        .inst
        .devices
        .iter()
        .map(|d| (d.devid.clone(), d.devid.clone()))
        .collect();
    let list_devices_fn = lua.create_function(move |lua, _self: Value| {
        let t = lua.create_table()?;
        for (i, (label, value)) in devices.iter().enumerate() {
            let dev = lua.create_table()?;
            dev.set("label", label.clone())?;
            dev.set("value", value.clone())?;
            t.set(i + 1, dev)?;
        }
        Ok(t)
    })?;
    ctx.set("list_devices", list_devices_fn)?;

    // --- M-3 lua v2 模型感知 API（Collector 快照注入时注册）---
    // ctx:devices() -> [{devid=..., model=...}]：绑定设备全量（含模型 id）。
    // ctx:tags(devid) -> {name=type,...}：设备模型点表（名→类型）；未绑定/无快照 → 空表。
    // ctx:param(devid, name) -> str|nil：设备实例化参数。
    // 快照为装配期展开产物（Arc 只读）；Transform/North 无快照 → 不注册（脚本调
    // 用报 nil——能力面按角色裁剪，与 transport 同语义）。
    if let Some(snapshot) = &ac.model {
        let snap_devices = snapshot.clone();
        let devices_fn = lua.create_function(move |lua, _self: Value| {
            let t = lua.create_table()?;
            let snap = snap_devices.lock().unwrap();
            for (i, dev) in snap.values().enumerate() {
                let row = lua.create_table()?;
                row.set("devid", dev.devid.clone())?;
                row.set("model", dev.model_id.clone())?;
                t.set(i + 1, row)?;
            }
            Ok(t)
        })?;
        ctx.set("devices", devices_fn)?;

        let snap_tags = snapshot.clone();
        let tags_fn = lua.create_function(move |lua, (_self, devid): (Value, String)| {
            let t = lua.create_table()?;
            let snap = snap_tags.lock().unwrap();
            if let Some(dev) = snap.get(&devid) {
                for tag in &dev.tags {
                    t.set(tag.name.clone(), tag.data_type.clone())?;
                }
            }
            Ok(t)
        })?;
        ctx.set("tags", tags_fn)?;

        let snap_param = snapshot.clone();
        let param_fn =
            lua.create_function(move |lua, (_self, devid, name): (Value, String, String)| {
                let snap = snap_param.lock().unwrap();
                let v = snap.get(&devid).and_then(|d| d.params.get(&name)).cloned();
                match v {
                    Some(s) => Ok(Value::String(lua.create_string(&s)?)),
                    None => Ok(Value::Nil),
                }
            })?;
        ctx.set("param", param_fn)?;
    }

    // --- L1+ ctx:transport() -> str：底层链路类型（tcp/serial/udp）---
    // 脚本据此选帧格式（serial → PDU+CRC16、tcp → MBAP 头、udp → 裸 RTU/自定义）。
    // 与 app_config.kind 一致；同步（配置解析产物，register 传入 &'static str）。
    // _self 吸收冒号语法隐式 self（ctx:transport()）。
    // 角色裁剪（§5.2 矩阵 mqtt-north 列无 C1，B 阶段 T6）：North 不注册 transport
    //（north 无传输语义——MQTT 连接层在宿主，T7 连接序列）。
    if ac.role == Role::Collector {
        let transport = ac.transport;
        let transport_fn = lua.create_function(move |_lua, _self: Value| Ok(transport))?;
        ctx.set("transport", transport_fn)?;
    }

    // --- C6 ctx:fire_event/clear_event（Q12 T4，api/alarm.rs）---
    // 三角色全注入（§5.2 C6 行 ✓✓✓✓，无角色门）：判定场景可能在任一角色。
    // fire：校验 → 20/s 频控 → lux count++ → 同 key 1s 抖动决定 publish；
    // clear：幂等恒发 Clear 帧。护栏/审计详见 [`alarm`] 模块文档。
    alarm::register(lua, &ctx, ac)?;

    // --- L5 ctx:http_get/http_post（feature lua-http）---
    // 实现移 http.rs（守 <500）；此处委托 http::register 注册到同一 ctx 表。
    // 角色裁剪（§5.2 矩阵 C5，C 阶段 T4）：collector ✓ north ✓ transform ✗——
    // 订阅型数据加工不出网。
    #[cfg(feature = "lua-http")]
    if ac.role != Role::Transform {
        http::register(lua, &ctx, ac.inst)?;
    }

    // --- 全局 stdlib 表（T9 §3.5）：stdlib.list()/info(mod) 只读清单 ---
    // 本地 index + embed 种子合并视图（读盘即时，sync 后 reload 即见新版）。
    lua.globals().set(
        "stdlib",
        crate::plugin::lua_core::stdlib_sync::stdlib_table(lua, ac.scripts_root)?,
    )?;

    lua.globals().set("ctx", ctx)?;
    Ok(())
}

/// builtin 单段模块分发（§3.1 Builtin 层；`script.rs` require 路由的 Builtin 分支调用）。
///
/// 返回模块值（Lua table 等）。名单 = [`crate::plugin::lua_core::capability::BUILTIN_MODULES`]
///（route 已保证 `name` 在名单内）；json 实现在 [`json`]（serde_json 桥 + 1MB 输入上限）。
pub(crate) fn builtin_module(lua: &Lua, name: &str) -> mlua::Result<Value> {
    match name {
        "json" => json::make_module(lua).map(Value::Table),
        other => Err(mlua::Error::RuntimeError(format!(
            "builtin 模块 {other} 未注册（BUILTIN_MODULES 名单外）"
        ))),
    }
}

/// 注册 log_info/warn/err/debug 到 ctx 表（tracing，前缀 [lua:<name>]）。
///
/// `tracing::Level` 是 struct + 关联常量（非 enum），不可 `as u8`/`match` 分发；
/// 闭包捕获 `Level`（`Copy`）后用 `==` 比较显式调对应宏（info!/warn!/error!/debug!
/// 是不同宏，无法按 level 一阶化）。
fn register_log(lua: &Lua, ctx: &mlua::Table, name: &str) -> mlua::Result<()> {
    for (key, level) in [
        ("log_info", tracing::Level::INFO),
        ("log_warn", tracing::Level::WARN),
        ("log_err", tracing::Level::ERROR),
        ("log_debug", tracing::Level::DEBUG),
    ] {
        let n = name.to_string();
        let f = lua.create_function(move |_lua, (_self, msg): (Value, LuaString)| {
            let msg = msg.to_string_lossy().to_string();
            if level == tracing::Level::INFO {
                tracing::info!("[lua:{n}] {msg}");
            } else if level == tracing::Level::WARN {
                tracing::warn!("[lua:{n}] {msg}");
            } else if level == tracing::Level::ERROR {
                tracing::error!("[lua:{n}] {msg}");
            } else {
                tracing::debug!("[lua:{n}] {msg}");
            }
            Ok(())
        })?;
        ctx.set(key, f)?;
    }
    Ok(())
}

/// 解析 rtdata cell `[ts, localTime, value]` JSON → value 字符串（index 2）。
///
/// rtdata::write 落盘格式：`serde_json::json!([timestamp, local_time, val_str])`，
/// value 始终是字符串（数字/布尔原样 `to_string`）。非 JSON 或缺 index 2 → `None`
/// （调用方返 nil，对标 mqtt_pub `parse_rtdata_hash` 的容错）。
fn parse_rtdata_value(cell: &str) -> Option<String> {
    let arr: serde_json::Value = serde_json::from_str(cell).ok()?;
    arr.get(2).and_then(|v| v.as_str()).map(String::from)
}
