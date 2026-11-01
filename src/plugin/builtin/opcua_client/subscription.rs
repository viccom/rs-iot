//! OPC-9：OPC UA Subscription（订阅 / monitored items）。
//!
//! 审查台账（`docs/designs/2026-11-01-protocol-semantics-audit.md` §4 OPC-9，原 P3
//! 能力缺口，现场必做能力升级）的实现：服务端变化推送替代周期 Read 轮询。
//!
//! 结构：
//! - 配置：[`AcquisitionMode`] / [`SubscriptionParams`]（parse_cfg 调
//!   [`parse_subscription`]，含夹紧与 warn）
//! - 回调桥：[`SubBridge`] 直接实现 `OnSubscriptionNotificationCore`——**不走**
//!   `SubscriptionCallbacks`，因为它的 blanket impl 把 `notification_data=None`
//!   的 keep-alive publish 响应静默吞掉（callbacks.rs:29 `return`），而订阅看门狗
//!   恰恰要靠 keep-alive 证明"连接活着只是没数据"。回调在 session 内部 task 上
//!   被调，只做 channel send 转发，不做任何重活。
//! - 请求构造：[`build_monitored_items`]（全部节点进**一个**订阅、跨 devid 扁平、
//!   `client_handle` 自分配——0 是库的"自动分配"保留字，必须避开）
//! - 判定纯函数：[`fallback_to_polling`]（服务不支持类错误 → auto 降级轮询）/
//!   [`watchdog_expired`]
//! - 建立流程：[`setup_subscription`]（探测 `MaxMonitoredItemsPerCall`=i=11714 分批、
//!   逐项校验 `CreatedMonitoredItem` 状态，坏点不拆会话）
//!
//! 生命周期约定：会话关闭时服务端自动清理订阅与 monitored items（规范行为），
//! 插件不显式 delete；重连后重建订阅与 handle 映射，通知通道（unbounded）复用。

use crate::types::SampleValue;
use opcua::client::{OnSubscriptionNotificationCore, Session};
use opcua::types::{
    match_extension_object_owned, DataChangeNotification, DataValue, Error, EventNotificationList,
    ExtensionObject, MonitoredItemCreateRequest, MonitoringMode, MonitoringParameters,
    NotificationMessage, StatusChangeNotification, StatusCode, TimestampsToReturn,
};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::{chunk_nodes, read_value_id, Prepared, PreparedDevice};

/// 订阅 lifetime 计数（规范要求 ≥ 3×keep-alive；120 = 6×20 留双倍余量）。
const LIFETIME_COUNT: u32 = 120;
/// keep-alive 计数（publishing_interval×20 无数据则服务端发 keep-alive；
/// 1000ms×20 = 20s ≪ 看门狗缺省 60s，留足判定余量）。
const MAX_KEEP_ALIVE_COUNT: u32 = 20;
/// 服务端未声明 `MaxMonitoredItemsPerCall` 时的分批缺省（服务端库同值，
/// constants::MAX_MONITORED_ITEMS_PER_CALL=1000）。
pub(super) const DEFAULT_MAX_MONITORED_ITEMS_PER_CALL: usize = 1_000;

// ---------- 配置 ----------

/// 采集模式（OPC-9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcquisitionMode {
    /// 先试订阅，服务不支持则本连接降级轮询（缺省）。
    Auto,
    /// 强制订阅：建立失败拆会话重试，不降级。
    Subscription,
    /// 纯轮询（既有行为，完全不变）。
    Polling,
}

impl AcquisitionMode {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "subscription" => Ok(Self::Subscription),
            "polling" => Ok(Self::Polling),
            other => Err(format!(
                "unknown acquisition_mode {s:?}（归一后 {other:?}；支持 auto/subscription/polling）"
            )),
        }
    }
}

/// 订阅参数（parse_cfg 校验后的运行值）。
#[derive(Debug, Clone, Copy)]
pub(super) struct SubscriptionParams {
    /// 发布间隔 ms（50..=60000）。
    pub(super) publishing_interval_ms: u64,
    /// 采样间隔 ms（0..=60000；0 = 随发布间隔，服务端默认语义）。
    pub(super) sampling_interval_ms: u64,
    /// 每点服务端队列长度（1..=100；丢旧策略）。
    pub(super) queue_size: u32,
    /// 看门狗秒数（10..=3600）：无任何消息（含 keep-alive）即判订阅失效。
    pub(super) watchdog_secs: u64,
}

/// 解析订阅相关配置键（夹紧 + warn，风格对齐 request_timeout_ms）。
pub(super) fn parse_subscription(
    app: &serde_json::Value,
) -> Result<(AcquisitionMode, SubscriptionParams), String> {
    let mode = AcquisitionMode::parse(
        app.get("acquisition_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("auto"),
    )?;
    let clamp = |key: &str, default: u64, min: u64, max: u64| -> u64 {
        let v = app.get(key).and_then(|v| v.as_u64()).unwrap_or(default);
        if v < min || v > max {
            let clamped = v.clamp(min, max);
            tracing::warn!("[opcuaClient] {key}={v} 超出 {min}..={max}，夹到 {clamped}");
            clamped
        } else {
            v
        }
    };
    Ok((
        mode,
        SubscriptionParams {
            publishing_interval_ms: clamp("publishing_interval_ms", 1_000, 50, 60_000),
            sampling_interval_ms: clamp("sampling_interval_ms", 0, 0, 60_000),
            queue_size: clamp("queue_size", 1, 1, 100) as u32,
            watchdog_secs: clamp("subscription_watchdog_secs", 60, 10, 3_600),
        },
    ))
}

// ---------- 回调桥 ----------

/// session 内部 task → run 循环的消息（只做转发，闭包内不做重活）。
pub(super) enum SubMsg {
    /// 数据变化（`client_handle` 由 run 循环经映射解析成 devid/field——
    /// 映射随连接重建，回调侧不持有可变状态，避免跨线程锁）。
    Data {
        client_handle: u32,
        value: DataValue,
    },
    /// keep-alive（无数据的 publish 响应；看门狗活性证据）。
    KeepAlive,
    /// 订阅状态变化（BadTimeout=服务端超时删订阅 等）。
    Status(StatusCode),
}

/// [`SubMsg`] 的回调侧发送者。
pub(super) struct SubBridge {
    /// 通道关闭（run 循环已退出）时 send 失败直接忽略——回调无处报错也不该 panic。
    pub(super) tx: mpsc::UnboundedSender<SubMsg>,
}

impl OnSubscriptionNotificationCore for SubBridge {
    fn on_subscription_notification(
        &mut self,
        notification: NotificationMessage,
        _monitored_items: opcua::client::MonitoredItemMap<'_>,
    ) {
        // keep-alive：notification_data 为 None/空。不走 SubscriptionCallbacks 的
        // blanket impl（它在此处直接 return，看门狗将永远看不到 keep-alive）。
        let Some(data) = notification.notification_data else {
            let _ = self.tx.send(SubMsg::KeepAlive);
            return;
        };
        if data.is_empty() {
            let _ = self.tx.send(SubMsg::KeepAlive);
            return;
        }
        for obj in data {
            match_extension_object_owned!(obj,
                v: DataChangeNotification => {
                    for notif in v.monitored_items.into_iter().flatten() {
                        let _ = self.tx.send(SubMsg::Data {
                            client_handle: notif.client_handle,
                            value: notif.value,
                        });
                    }
                },
                v: StatusChangeNotification => {
                    let _ = self.tx.send(SubMsg::Status(v.status));
                },
                v: EventNotificationList => {
                    // 本插件只订阅 Value 属性数据变化，事件通知不应出现；
                    // 静默忽略（Future: event 订阅需求出现时在此扩展）。
                    let _ = v;
                },
                _ => {}
            )
        }
    }
}

// ---------- 请求构造 ----------

/// `client_handle` 分配器（每连接重建）。
///
/// 从 1 开始：0 是库的"自动分配"保留字（service.rs:2013 `if client_handle == 0`），
/// 插件必须自持唯一 handle 才能建立 handle→(devid,field) 映射。
pub(super) struct HandleAllocator {
    next: u32,
}

impl HandleAllocator {
    pub(super) fn new() -> Self {
        Self { next: 1 }
    }

    pub(super) fn next_handle(&mut self) -> u32 {
        let h = self.next;
        self.next = self.next.saturating_add(1);
        h
    }
}

/// 全部设备的点位 → 扁平 monitored item 请求列表 + handle 映射。
///
/// - 全部节点进一个订阅（跨 devid 共享发布节奏与 keep-alive 心跳）
/// - `item_to_monitor` 复用读路径的 [`read_value_id`]（数组下标范围 OPC-15 同样生效）
/// - 采样间隔 0 = 随订阅发布间隔（服务端语义），不额外造拍
pub(super) fn build_monitored_items(
    devices: &[PreparedDevice],
    sampling_interval_ms: f64,
    queue_size: u32,
    alloc: &mut HandleAllocator,
) -> (
    Vec<MonitoredItemCreateRequest>,
    HashMap<u32, (String, String)>,
) {
    let mut items = Vec::new();
    let mut handles = HashMap::new();
    for dev in devices {
        for (field, spec) in &dev.points {
            let handle = alloc.next_handle();
            items.push(MonitoredItemCreateRequest {
                item_to_monitor: read_value_id(spec),
                monitoring_mode: MonitoringMode::Reporting,
                requested_parameters: MonitoringParameters {
                    client_handle: handle,
                    sampling_interval: sampling_interval_ms,
                    filter: ExtensionObject::null(),
                    queue_size,
                    discard_oldest: true,
                },
            });
            handles.insert(handle, (dev.devid.clone(), field.clone()));
        }
    }
    (items, handles)
}

// ---------- 聚合 ----------

/// 一批通知（已 drain）按 devid 聚合。
///
/// - 同点多次通知后到覆盖（queue>1 或跨 publish 时"最新值胜出"）
/// - 值转换走 [`super::convert::data_value_to_sample`]：坏质量/无值 → `Null`
///   （OPC-1 语义），`bad_keys`（"devid.field"）供 run 循环限流 warn + 计数
/// - 未知 handle（上一连接残留通知）丢弃：重连后 handle 数字可能复用，
///   映射以本连接为准
pub(super) fn aggregate_notifications(
    batch: Vec<(u32, DataValue)>,
    handles: &HashMap<u32, (String, String)>,
) -> (BTreeMap<String, BTreeMap<String, SampleValue>>, Vec<String>) {
    let mut by_dev: BTreeMap<String, BTreeMap<String, SampleValue>> = BTreeMap::new();
    let mut bad_keys = Vec::new();
    for (handle, dv) in batch {
        let Some((devid, field)) = handles.get(&handle) else {
            continue;
        };
        // 坏质量判定与轮询路径同口径（坏状态或无 value）
        if super::convert::data_value_bad_status(&dv).is_some() || dv.value.is_none() {
            bad_keys.push(format!("{devid}.{field}"));
        }
        let value = super::convert::data_value_to_sample(&dv);
        by_dev
            .entry(devid.clone())
            .or_default()
            .insert(field.clone(), value);
    }
    (by_dev, bad_keys)
}

// ---------- 判定纯函数 ----------

/// 订阅建立失败是否降级轮询（仅 auto 模式 + 服务不支持类错误）。
///
/// - `subscription` 模式永不降级（显式要求，失败拆会话重试）
/// - 非"服务不支持"错误（传输/超时/业务）不降级——降级掩盖不了网络问题
pub(super) fn fallback_to_polling(mode: AcquisitionMode, err: &Error) -> bool {
    mode == AcquisitionMode::Auto && is_service_unsupported_status(err.status())
}

/// "服务端不支持订阅"类状态码（Bad_OutOfService 按台账口径并入：源不工作时
/// 反复重建订阅无意义，轮询同样失败但能按拍重试）。
fn is_service_unsupported_status(st: StatusCode) -> bool {
    st == StatusCode::BadServiceUnsupported
        || st == StatusCode::BadNotSupported
        || st == StatusCode::BadNotImplemented
        || st == StatusCode::BadOutOfService
}

/// 看门狗判定：距上次活动**严格超过**窗口才算过期（恰好等于时容忍一拍，
/// keep-alive 的到达抖动为 publishing_interval×keep_alive_count）。
pub(super) fn watchdog_expired(now: Instant, last_activity: Instant, timeout: Duration) -> bool {
    now.duration_since(last_activity) > timeout
}

// ---------- 建立流程 ----------

/// 订阅模式每连接运行态。
pub(super) struct SubRuntime {
    pub(super) subscription_id: u32,
    /// client_handle → (devid, field)。
    pub(super) handles: HashMap<u32, (String, String)>,
    /// 建立期被服务端拒绝的点（"devid.field" + 状态码），warn/计数用。
    pub(super) rejected: Vec<(String, StatusCode)>,
}

/// 建立失败（区分超时与服务端错误：超时不参与降级判定，直接拆会话）。
pub(super) enum SetupFailure {
    Timeout(&'static str),
    Error { stage: &'static str, err: Error },
}

impl SetupFailure {
    /// 可读错误文本（tick_fail / warn 用）。
    pub(super) fn message(&self) -> String {
        match self {
            Self::Timeout(stage) => format!("{stage} timeout (request budget)"),
            Self::Error { stage, err } => format!("{stage} failed: {err}"),
        }
    }

    /// 服务端错误（降级判定用；超时返回 None）。
    pub(super) fn error(&self) -> Option<&Error> {
        match self {
            Self::Timeout(_) => None,
            Self::Error { err, .. } => Some(err),
        }
    }
}

/// 建订阅 + 全量 monitored items（连接成功后调用）。
///
/// 逐项校验 [`opcua::types::MonitoredItemCreateResult::status_code`]：坏点
/// warn（调用方限流）+ 计数，不拆会话；**全部**点位被拒视为配置级失败
/// （订阅永远无数据，等看门狗不如立即失败可见化）。
pub(super) async fn setup_subscription(
    session: &Session,
    prepared: &Prepared,
    params: &SubscriptionParams,
    notify_tx: mpsc::UnboundedSender<SubMsg>,
    max_per_call: usize,
    request_timeout: Duration,
) -> Result<SubRuntime, SetupFailure> {
    // 一个订阅承载全部节点；lifetime=120 ≥ 3×keepalive=20（规范要求），
    // publishing_enabled=true，max_notifications_per_publish=0（不限）。
    let sub_id = match tokio::time::timeout(
        request_timeout,
        session.create_subscription(
            Duration::from_millis(params.publishing_interval_ms),
            LIFETIME_COUNT,
            MAX_KEEP_ALIVE_COUNT,
            0,
            0,
            true,
            SubBridge { tx: notify_tx },
        ),
    )
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            return Err(SetupFailure::Error {
                stage: "create_subscription",
                err: e,
            })
        }
        Err(_) => return Err(SetupFailure::Timeout("create_subscription")),
    };

    let mut alloc = HandleAllocator::new();
    let (items, handles) = build_monitored_items(
        &prepared.devices,
        params.sampling_interval_ms as f64,
        params.queue_size,
        &mut alloc,
    );
    let mut accepted: HashMap<u32, (String, String)> = HashMap::new();
    let mut rejected: Vec<(String, StatusCode)> = Vec::new();
    // 空点表也保留订阅：keep-alive 仍喂看门狗，与轮询模式"空表 tick_ok"对齐。
    for chunk in chunk_nodes(items.len(), max_per_call) {
        let batch = items[chunk].to_vec();
        // OPC-3 同口径：Sample.timestamp 为接收时刻，不向服务端要时间戳。
        match tokio::time::timeout(
            request_timeout,
            session.create_monitored_items(sub_id, TimestampsToReturn::Neither, batch.clone()),
        )
        .await
        {
            Ok(Ok(results)) => {
                if results.len() != batch.len() {
                    // 服务端违约（结果数必须与请求一致）：按服务错误处理，不猜配对。
                    return Err(SetupFailure::Error {
                        stage: "create_monitored_items",
                        err: Error::new(
                            StatusCode::BadUnexpectedError,
                            format!("results {} != requested {}", results.len(), batch.len()),
                        ),
                    });
                }
                for (req, created) in batch.iter().zip(results.iter()) {
                    let handle = req.requested_parameters.client_handle;
                    let status = created.result.status_code;
                    if status.is_good() {
                        accepted.insert(handle, handles.get(&handle).cloned().unwrap_or_default());
                    } else {
                        let (devid, field) = handles.get(&handle).cloned().unwrap_or_default();
                        rejected.push((format!("{devid}.{field}"), status));
                    }
                }
            }
            Ok(Err(e)) => {
                return Err(SetupFailure::Error {
                    stage: "create_monitored_items",
                    err: e,
                })
            }
            Err(_) => return Err(SetupFailure::Timeout("create_monitored_items")),
        }
    }
    let total_points: usize = prepared.devices.iter().map(|d| d.points.len()).sum();
    if total_points > 0 && accepted.is_empty() {
        return Err(SetupFailure::Error {
            stage: "create_monitored_items",
            err: Error::new(
                StatusCode::BadNodeIdUnknown,
                format!(
                    "all {total_points} monitored items rejected: {:?}",
                    rejected
                        .iter()
                        .map(|(k, st)| format!("{k}={st}"))
                        .collect::<Vec<_>>()
                ),
            ),
        });
    }
    Ok(SubRuntime {
        subscription_id: sub_id,
        handles: accepted,
        rejected,
    })
}
