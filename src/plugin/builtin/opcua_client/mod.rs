//! `opcuaClient`：OPC UA 南向采集插件（async-opcua 0.19，纯 Rust 加密，无 openssl）。
//!
//! 周期连接外部 UA Server，按 `devices` 配置读取 NodeId 值，组装 `Sample` 推入 hub。
//! 同时消费下行命令（`cmd="write"`）写回 Server item。
//!
//! 采集模式（OPC-9）：`acquisition_mode` = `"auto"`（缺省，先试订阅、服务不支持降级轮询）
//! | `"subscription"`（强制订阅，失败拆会话重试）| `"polling"`（纯轮询，行为同旧版）。
//! 订阅参数：`publishing_interval_ms`（1000）、`sampling_interval_ms`（0=随发布间隔）、
//! `queue_size`（1）、`subscription_watchdog_secs`（60，无任何消息含 keep-alive 即重连）。
//!
//! 配置（`InstConfig::app_config` JSON）：
//! ```json
//! {
//!   "endpoint": "opc.tcp://127.0.0.1:49320",
//!   "security_policy": "none",       // none | Basic128Rsa15 | Basic256 | Basic256Sha256
//!                                    //      | Aes128_Sha256_RsaOaep | Aes256_Sha256_RsaPss
//!                                    // 非 none 一律 SignAndEncrypt（无 Sign-only 模式）
//!   "username": null, "password": null,
//!   "period": 3,
//!   "acquisition_mode": "auto",      // auto | subscription | polling（OPC-9，缺省 auto）
//!   "request_timeout_ms": 10000,     // 单次 read/write 超时（100..120000）；超时视为会话失效，下拍重连
//!   "trust_server_certs": true,      // 默认 true（兼容）：跳过证书有效期/主机名校验；
//!                                    // security_policy=none 时通道无加密（用户名/密码明文）并 warn
//!   "verify_server": false           // true = 严格校验服务端证书（hostname/ApplicationUri/有效期）
//! }
//! ```
//! 变量映射（复用 `devices`，与 datasim 对齐）：
//! - `DeviceSpec.value` = 设备 ID（逻辑名，即 `rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = UA NodeId 字符串：`ns=2;s=v1` / `nsu=urn:xxx;s=v1`（连接后按
//!   NamespaceArray 解析）/ `i=2258`；可带数组下标 `[n]` / `[a:b]`；前缀大小写不敏感、允许空白
//! - `ChildSpec.label` = values map 的 key（显示名；空则用 NodeId）
//! - `ChildSpec.type` = 点表声明类型（写命令未带 `opc_type` 时的默认类型）
//!
//! 写命令（`cmd="write"`，`data` 为 JSON 文本）：
//! - HMI 契约：`{"tag":"点位显示名","value":1.5}`（`tag` 按 devid 内点位解析，
//!   跨 devid 显示名唯一时兜底；解析不到 → `Err`，经写回执返回 422）
//! - 旧通道：`{"node_id":"ns=2;s=v1","value":1.5}`（优先；`tag` 被忽略）
//! - `opc_type` 可选（如 `"Int32"`，也接受点表别名 `i32/f32/bool/string…`）：
//!   OPC UA 写严格类型匹配，缺省用点表 `type`，再无则按值推断
//! - 结果经 [`crate::host::CtlRegistry::finish_write`] 回执（成功/失败都能回流到 REST）

mod convert;
mod node;
mod subscription;
#[cfg(test)]
mod tests;

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use convert::{
    data_value_bad_status, data_value_to_sample, json_to_sample_value, variant_with_type,
};
use node::{parse_node_spec, resolve_write_target, NodeSpec, RawDevice, RawPoint};
use opcua::client::{Client, ClientBuilder, IdentityToken, Password, Session};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    AttributeId, EndpointDescription, MessageSecurityMode, NamespaceMap, NodeId, QualifiedName,
    ReadValueId, StatusCode, TimestampsToReturn, UserTokenPolicy, VariableId, Variant, WriteValue,
};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subscription::{
    aggregate_notifications, setup_subscription, watchdog_expired, AcquisitionMode, SubMsg,
    SubRuntime, SubscriptionParams, DEFAULT_MAX_MONITORED_ITEMS_PER_CALL,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// OPC UA 采集插件。
#[derive(Debug, Default)]
pub struct OpcuaClient;

// 表单默认值常量（审计 C1；前端 pluginSchemas.ts opcuaClient 镜像契约）。
const DEFAULT_PERIOD_SECS: u64 = 3;
/// 单次 read/write 超时默认值（OPC-10：库默认 60s 会把 select 分支堵死）。
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 10_000;
/// 超时配置下限（100ms）。
const MIN_REQUEST_TIMEOUT_MS: u64 = 100;
/// 超时配置上限（120s）。
const MAX_REQUEST_TIMEOUT_MS: u64 = 120_000;
/// 服务端未暴露 OperationLimits / 声明 0 时的分批上限（保守值，防超大请求）。
const DEFAULT_MAX_NODES_PER_READ: usize = 1_000;
/// 建连超时（D-1：OS 级 TCP connect 无超时）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// CloseSession 上限（OPC-13：断开前发 CloseSession，失败仅 warn）。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// 端点列表探测超时（OPC-14：仅错误路径附加诊断，不能拖慢重连）。
const ENDPOINT_LIST_TIMEOUT: Duration = Duration::from_secs(3);
/// 坏质量 warn 限流窗口（OPC-16：每点位每窗口一条）。
const BAD_QUALITY_WARN_WINDOW: Duration = Duration::from_secs(60);
/// 一轮超 period 的 warn 限流窗口（F-7）。
const OVER_PERIOD_WARN_WINDOW: Duration = Duration::from_secs(60);

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
struct OpcuaCfg {
    endpoint: String,
    security_policy: String,
    username: Option<String>,
    password: Option<String>,
    period: u64,
    /// 采集模式（OPC-9）：auto（缺省）= 先试订阅，服务不支持降级轮询。
    acquisition_mode: AcquisitionMode,
    /// 订阅参数（OPC-9；仅订阅路径使用）。
    subscription: SubscriptionParams,
    /// 单次 read/write 超时（毫秒）。
    request_timeout_ms: u64,
    /// 是否信任任意服务端证书（跳过有效期/主机名/ApplicationUri 校验）。
    /// 默认 true 保留旧行为；生产应设 false 并配 `verify_server=true`。
    trust_server_certs: bool,
    /// 是否严格校验 server 证书（hostname/application uri/有效期）。
    /// 默认 false：仅检查证书被信任 + 密钥长度，跳过 hostname 校验，
    /// 便于用 IP/loopback 连接（server 证书常不含这些 alt name）。
    /// 生产环境（server 证书与 endpoint 匹配）建议设 true。
    verify_server: bool,
}

fn parse_cfg(app: &serde_json::Value) -> Result<OpcuaCfg, String> {
    let endpoint = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?
        .to_string();
    let security_policy = app
        .get("security_policy")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let username = app
        .get("username")
        .and_then(|v| v.as_str())
        .map(String::from);
    let password = app
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_PERIOD_SECS)
        .max(1);
    // OPC-9：采集模式与订阅参数（夹紧 + warn 在 subscription 模块内）。
    let (acquisition_mode, subscription) = subscription::parse_subscription(app)?;
    // OPC-10：读写请求超时（库默认 60s 不可配 → 一轮读超 period 无告警、select 被堵）。
    let request_timeout_ms = match app.get("request_timeout_ms").and_then(|v| v.as_u64()) {
        Some(v) if !(MIN_REQUEST_TIMEOUT_MS..=MAX_REQUEST_TIMEOUT_MS).contains(&v) => {
            let clamped = v.clamp(MIN_REQUEST_TIMEOUT_MS, MAX_REQUEST_TIMEOUT_MS);
            tracing::warn!(
                "[opcuaClient] request_timeout_ms={v} 超出 {MIN_REQUEST_TIMEOUT_MS}..={MAX_REQUEST_TIMEOUT_MS}，夹到 {clamped}"
            );
            clamped
        }
        Some(v) => v,
        None => DEFAULT_REQUEST_TIMEOUT_MS,
    };
    // OPC-11：默认 true 保留兼容（旧配置未写该键时行为不变）。
    let trust_server_certs = app
        .get("trust_server_certs")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let verify_server = app
        .get("verify_server")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(OpcuaCfg {
        endpoint,
        security_policy,
        username,
        password,
        period,
        acquisition_mode,
        subscription,
        request_timeout_ms,
        trust_server_certs,
        verify_server,
    })
}

/// 安全策略字符串 → (SecurityPolicy, MessageSecurityMode)。
///
/// OPC-12：库支持的 6 种策略全部映射（None / Basic128Rsa15 / Basic256 /
/// Basic256Sha256 / Aes128_Sha256_RsaOaep / Aes256_Sha256_RsaPss）；非 none
/// 一律 `SignAndEncrypt`（不提供 Sign-only，避免"看起来加密其实不加密"）。
/// 词表大小写不敏感，也接受库的 URI 形式（取 `#` 之后段）。
fn parse_security(s: &str) -> Result<(SecurityPolicy, MessageSecurityMode), String> {
    let word: String = s
        .rsplit('#')
        .next()
        .unwrap_or(s)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let (policy, mode) = match word.as_str() {
        "" | "none" => (SecurityPolicy::None, MessageSecurityMode::None),
        "basic128rsa15" => (
            SecurityPolicy::Basic128Rsa15,
            MessageSecurityMode::SignAndEncrypt,
        ),
        "basic256" => (
            SecurityPolicy::Basic256,
            MessageSecurityMode::SignAndEncrypt,
        ),
        "basic256sha256" => (
            SecurityPolicy::Basic256Sha256,
            MessageSecurityMode::SignAndEncrypt,
        ),
        "aes128sha256rsaoaep" => (
            SecurityPolicy::Aes128Sha256RsaOaep,
            MessageSecurityMode::SignAndEncrypt,
        ),
        "aes256sha256rsapss" => (
            SecurityPolicy::Aes256Sha256RsaPss,
            MessageSecurityMode::SignAndEncrypt,
        ),
        other => {
            return Err(format!(
                "unsupported security_policy: {s:?}（归一后 {other:?}；支持 none/Basic128Rsa15/Basic256/Basic256Sha256/Aes128_Sha256_RsaOaep/Aes256_Sha256_RsaPss）"
            ));
        }
    };
    Ok((policy, mode))
}

/// OPC-11：`trust_server_certs=true` + `security_policy=none` 的明文告警（进程内仅一条）。
static PLAINTEXT_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_plaintext_once() {
    if !PLAINTEXT_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "[opcuaClient] security_policy=none 且 trust_server_certs=true：通道无加密（用户名/密码明文），且不校验服务端证书有效期/主机名——生产环境请改签名加密策略并设 trust_server_certs=false/verify_server=true"
        );
    }
}

fn identity_token(cfg: &OpcuaCfg) -> IdentityToken {
    match (&cfg.username, &cfg.password) {
        (Some(u), Some(p)) => IdentityToken::UserName(u.clone(), Password(p.clone())),
        (None, None) => IdentityToken::Anonymous,
        // D-3（2026-11-01 审查）：半配置静默降级 Anonymous 排障困难（表单漏填密码
        // 常见）——大声 warn 指出缺哪个，仍保持兼容降级不拒连。
        (Some(u), None) => {
            tracing::warn!(
                "[opcuaClient] username={u:?} 已配但 password 缺失——降级 Anonymous 连接（多为漏填，请补齐）"
            );
            IdentityToken::Anonymous
        }
        (None, Some(_)) => {
            tracing::warn!(
                "[opcuaClient] password 已配但 username 缺失——降级 Anonymous 连接（多为漏填，请补齐）"
            );
            IdentityToken::Anonymous
        }
    }
}

/// 建立到 UA Server 的会话。
async fn connect(cfg: &OpcuaCfg) -> Result<(Arc<Session>, JoinHandle<StatusCode>), String> {
    let (policy, mode) = parse_security(&cfg.security_policy)?;
    if cfg.trust_server_certs && policy == SecurityPolicy::None {
        warn_plaintext_once();
    }
    let mut client = ClientBuilder::new()
        .application_name("rs-iot OPC UA client")
        .application_uri("urn:rs-iot:opcua-client")
        .create_sample_keypair(true)
        .trust_server_certs(cfg.trust_server_certs)
        .verify_server_certs(cfg.verify_server)
        // 与插件侧 tokio::time::timeout 对齐：库内部请求也要按同一预算放弃，
        // 否则超时后旧请求仍占通道（OPC-10）。
        .request_timeout(Duration::from_millis(cfg.request_timeout_ms))
        .session_retry_limit(3)
        .client()
        .map_err(|e| format!("build opcua client: {e:?}"))?;

    let endpoint: EndpointDescription = (
        cfg.endpoint.as_str(),
        policy.to_str(),
        mode,
        UserTokenPolicy::anonymous(),
    )
        .into();
    let token = identity_token(cfg);

    // D-1（2026-11-01 审查）：connect 全流程包超时——`connect_to_matching_endpoint`
    // 内部含 OS 级 TCP connect（无超时）+ 端点发现往返，`wait_for_connection` 是
    // 无时限 watch 等待；IP 配错且被 DROP 时分钟级阻塞 tick 分支，token/cmd 全停。
    let (session, event_loop) = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        client.connect_to_matching_endpoint(endpoint, token),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let mut msg = format!("connect {}: {e:?}", cfg.endpoint);
            // OPC-14：端点匹配失败时附上服务端实际暴露的安全端点（截断 10 条），
            // 现场一眼看出"服务端没开这个策略/模式"。
            if e.status() == StatusCode::BadTcpEndpointUrlInvalid {
                msg.push_str(&endpoint_list_suffix(&client, &cfg.endpoint).await);
            }
            return Err(msg);
        }
        Err(_) => {
            return Err(format!(
                "connect {}: timeout ({CONNECT_TIMEOUT:?})",
                cfg.endpoint
            ))
        }
    };
    let handle = event_loop.spawn();
    match tokio::time::timeout(CONNECT_TIMEOUT, session.wait_for_connection()).await {
        Ok(_) => {}
        Err(_) => {
            // 中止 event loop task 防 handle 泄漏（重连循环里它不会自行退出）。
            handle.abort();
            return Err(format!(
                "connect {}: wait_for_connection timeout ({CONNECT_TIMEOUT:?})",
                cfg.endpoint
            ));
        }
    }
    Ok((session, handle))
}

/// 端点匹配失败时附加服务端端点列表（OPC-14；失败静默返回空串，不掩盖原始错误）。
async fn endpoint_list_suffix(client: &Client, url: &str) -> String {
    match tokio::time::timeout(
        ENDPOINT_LIST_TIMEOUT,
        client.get_server_endpoints_from_url(url),
    )
    .await
    {
        Ok(Ok(eps)) => format_endpoint_list(&eps, 10),
        Ok(Err(e)) => {
            tracing::debug!("[opcuaClient] 获取服务端端点列表失败（诊断信息省略）: {e:?}");
            String::new()
        }
        Err(_) => {
            tracing::debug!("[opcuaClient] 获取服务端端点列表超时（诊断信息省略）");
            String::new()
        }
    }
}

/// 服务端端点摘要（security_policy/security_mode，去重，截断到 `limit` 条）。
fn format_endpoint_list(eps: &[EndpointDescription], limit: usize) -> String {
    let mut items: Vec<String> = Vec::new();
    for ep in eps {
        let item = format!("{}/{:?}", ep.security_policy_uri.as_ref(), ep.security_mode);
        if !items.contains(&item) {
            items.push(item);
        }
    }
    if items.is_empty() {
        return String::new();
    }
    let total = items.len();
    let shown: Vec<&str> = items.iter().take(limit).map(String::as_str).collect();
    let mut out = format!("（服务端安全端点 {total} 个：{}", shown.join(", "));
    if total > limit {
        out.push_str(&format!("，…{}", total - limit));
    }
    out.push('）');
    out
}

/// 读服务端 `MaxNodesPerRead`（OPC-2）。
///
/// 库没有 `session.operation_limits()` 这类封装（0.19 源码无该 API），等价做法 =
/// 读 ServerCapabilities/OperationLimits 下的标准变量 `i=11705`。读不到/为 0 →
/// [`DEFAULT_MAX_NODES_PER_READ`]（服务端未声明上限时的保守分批）。
async fn probe_max_nodes_per_read(session: &Session, timeout: Duration) -> usize {
    probe_operation_limit(
        session,
        VariableId::Server_ServerCapabilities_OperationLimits_MaxNodesPerRead,
        DEFAULT_MAX_NODES_PER_READ,
        timeout,
    )
    .await
}

/// 读服务端 `MaxMonitoredItemsPerCall`（OPC-9；标准变量 `i=11714`，探测方式同上）。
async fn probe_max_monitored_items_per_call(session: &Session, timeout: Duration) -> usize {
    probe_operation_limit(
        session,
        VariableId::Server_ServerCapabilities_OperationLimits_MaxMonitoredItemsPerCall,
        DEFAULT_MAX_MONITORED_ITEMS_PER_CALL,
        timeout,
    )
    .await
}

/// 读 OperationLimits 标准变量的通用探测（读不到/为 0 → `default`）。
async fn probe_operation_limit(
    session: &Session,
    var: VariableId,
    default: usize,
    timeout: Duration,
) -> usize {
    let node = NodeId::from(var);
    let req = [ReadValueId::from(node)];
    match tokio::time::timeout(
        timeout,
        session.read(&req, TimestampsToReturn::Neither, 0.0),
    )
    .await
    {
        Ok(Ok(results)) => results
            .first()
            .filter(|dv| data_value_bad_status(dv).is_none())
            .and_then(|dv| dv.value.as_ref())
            .and_then(variant_as_u64)
            .map(|n| n as usize)
            .filter(|n| *n > 0)
            .unwrap_or(default),
        _ => default,
    }
}

/// 服务端能力值 → u64（非数值/负数 → None）。
fn variant_as_u64(v: &Variant) -> Option<u64> {
    match v {
        Variant::Byte(x) => Some(*x as u64),
        Variant::UInt16(x) => Some(*x as u64),
        Variant::UInt32(x) => Some(*x as u64),
        Variant::UInt64(x) => Some(*x),
        Variant::SByte(x) => u64::try_from(*x).ok(),
        Variant::Int16(x) => u64::try_from(*x).ok(),
        Variant::Int32(x) => u64::try_from(*x).ok(),
        Variant::Int64(x) => u64::try_from(*x).ok(),
        _ => None,
    }
}

/// 按服务端上限切批（OPC-2；`max` 为 0/异常时按 1，绝不空批死循环）。
fn chunk_nodes(len: usize, max: usize) -> Vec<Range<usize>> {
    let max = max.max(1);
    (0..len)
        .step_by(max)
        .map(|start| start..(start + max).min(len))
        .collect()
}

/// 传输层/会话层错误（拆会话重连）；业务级单批错误不拆（OPC-2）。
fn is_transport_status(st: StatusCode) -> bool {
    st == StatusCode::BadConnectionClosed
        || st == StatusCode::BadSecureChannelClosed
        || st == StatusCode::BadSessionClosed
        || st == StatusCode::BadSessionIdInvalid
        || st == StatusCode::BadTimeout
        || st == StatusCode::BadCommunicationError
        || st == StatusCode::BadServerNotConnected
        || st == StatusCode::BadRequestInterrupted
        || st == StatusCode::BadShutdown
}

/// 日志限流：同 key 在窗口内只放行一次（OPC-16 / F-7）。
///
/// 键来自点表节点集/固定诊断 key，随节点数有界；另按窗口周期裁剪过期项防长期增长。
struct RateLimit {
    window: Duration,
    last: std::collections::HashMap<String, Instant>,
    last_prune: Instant,
}

impl RateLimit {
    fn new(window: Duration) -> Self {
        Self {
            window,
            last: std::collections::HashMap::new(),
            last_prune: Instant::now(),
        }
    }

    /// `true` = 本次允许打日志；`false` = 窗口内重复，抑制。
    fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        self.prune_if_due(now);
        let fresh = match self.last.get(key) {
            Some(t) => now.duration_since(*t) >= self.window,
            None => true,
        };
        if fresh {
            self.last.insert(key.to_string(), now);
        }
        fresh
    }

    fn prune_if_due(&mut self, now: Instant) {
        let prune_every = self.window.max(Duration::from_secs(1)).saturating_mul(10);
        if now.duration_since(self.last_prune) < prune_every {
            return;
        }
        self.last
            .retain(|_, t| now.duration_since(*t) < self.window);
        self.last_prune = now;
    }
}

/// 一轮采集的结果判定（OPC-1/F-9）。
///
/// 整轮全部点位坏质量 → `Err("all nodes bad quality")`（tick_fail，且调用方不投递
/// 空 Sample）；否则若有读失败把错误透传；部分点坏由 `Null` 标记，仍算成功。
fn round_status(
    total_points: usize,
    good_points: usize,
    round_err: Option<&str>,
) -> Result<(), String> {
    if total_points > 0 && good_points == 0 {
        return Err("all nodes bad quality".to_string());
    }
    match round_err {
        Some(e) => Err(e.to_string()),
        None => Ok(()),
    }
}

/// 连接后按命名空间表解析点表（`nsu=` 需要）。
struct PreparedDevice {
    devid: String,
    points: Vec<(String, NodeSpec)>,
}

struct Prepared {
    devices: Vec<PreparedDevice>,
    max_nodes_per_read: usize,
}

fn build_prepared(
    raw: &[RawDevice],
    namespaces: Option<&NamespaceMap>,
    max_nodes_per_read: usize,
    inst_id: &str,
) -> Prepared {
    let devices = raw
        .iter()
        .map(|dev| {
            let points = dev
                .points
                .iter()
                .filter_map(|p| match parse_node_spec(&p.raw, namespaces) {
                    Ok(spec) => Some((p.field.clone(), spec)),
                    Err(e) => {
                        tracing::warn!(
                            "[opcuaClient] inst {inst_id} dev {} 点 {} 地址无效（跳过该点）: {e}",
                            dev.devid,
                            p.field
                        );
                        None
                    }
                })
                .collect();
            PreparedDevice {
                devid: dev.devid.clone(),
                points,
            }
        })
        .collect();
    Prepared {
        devices,
        max_nodes_per_read,
    }
}

/// 点表地址是否为 `nsu=<URI>;…` 形式（忽略大小写与空白）。
///
/// 用于判断 NamespaceArray 读失败是否构成采集失败：纯 `ns=` 配置不依赖该表。
fn uses_nsu(raw: &str) -> bool {
    raw.trim()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
        .starts_with("nsu=")
}

/// 第 `connects` 次成功连接对应的重连计数（OPC-13：首连不算重连）。
fn reconnects_after_connect(connects: u64) -> u64 {
    connects.saturating_sub(1)
}

/// 一轮耗时是否超过采集周期（F-7 告警判据）。
fn is_over_period(elapsed: Duration, period: Duration) -> bool {
    elapsed > period
}

/// 读请求项（含数组下标范围，OPC-15）。
fn read_value_id(spec: &NodeSpec) -> ReadValueId {
    ReadValueId {
        node_id: spec.node_id.clone(),
        attribute_id: AttributeId::Value as u32,
        index_range: spec.index_range.clone(),
        data_encoding: QualifiedName::null(),
    }
}

/// 写请求项（含数组下标范围，OPC-15）。
fn write_value(spec: &NodeSpec, variant: Variant) -> WriteValue {
    WriteValue {
        node_id: spec.node_id.clone(),
        attribute_id: AttributeId::Value as u32,
        index_range: spec.index_range.clone(),
        value: opcua::types::DataValue {
            value: Some(variant),
            ..Default::default()
        },
    }
}

/// 发 CloseSession 再丢弃会话（OPC-13：断连前尽量礼貌关闭，失败仅 warn）。
async fn disconnect_session(s: &Session) {
    match tokio::time::timeout(CLOSE_TIMEOUT, s.disconnect()).await {
        Ok(Ok(())) => tracing::debug!("[opcuaClient] CloseSession 已发送"),
        Ok(Err(e)) => tracing::warn!("[opcuaClient] CloseSession 失败（仍丢弃会话）: {e:?}"),
        Err(_) => {
            tracing::warn!("[opcuaClient] CloseSession 超时（{CLOSE_TIMEOUT:?}，仍丢弃会话）")
        }
    }
}

/// 关闭会话 + 中止事件循环（读失败/退出路径统一入口）。
async fn close_session(
    session: &mut Option<Arc<Session>>,
    ev_handle: &mut Option<JoinHandle<StatusCode>>,
    inst_id: &str,
) {
    if let Some(s) = session.take() {
        tracing::info!("[opcuaClient] inst {inst_id} closing session");
        disconnect_session(&s).await;
    }
    if let Some(h) = ev_handle.take() {
        h.abort();
    }
}

#[async_trait]
impl Source for OpcuaClient {
    fn name(&self) -> &'static str {
        "opcuaClient"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: SampleSender,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        let inst_id = inst.inst_id.to_string();
        let cfg = match parse_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[opcuaClient] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // M-2 模型驱动装配：绑定展开（模板 addr × devreg params → 绝对 NodeId）。
        let expanded = match crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("[opcuaClient] inst {inst_id} 绑定展开失败（设备未注册？）: {e}");
                return;
            }
        };
        // 原始点表（连接前）：field / 原始地址 / 声明类型。NodeId 在连接后按
        // 服务端 NamespaceArray 解析（`nsu=` 必需）。
        let raw_devices: Vec<RawDevice> = expanded
            .iter()
            .map(|dev| RawDevice {
                devid: dev.value.clone(),
                points: dev
                    .children
                    .iter()
                    .map(|c| RawPoint {
                        field: if c.label.is_empty() {
                            c.value.clone()
                        } else {
                            c.label.clone()
                        },
                        raw: c.value.clone(),
                        declared_type: c.r#type.clone(),
                    })
                    .collect(),
            })
            .collect();

        let period = Duration::from_secs(cfg.period);
        let request_timeout = Duration::from_millis(cfg.request_timeout_ms);
        // 点表是否依赖命名空间表（nsu=）：决定 NamespaceArray 读失败是否算采集失败。
        let needs_nsu = raw_devices
            .iter()
            .flat_map(|d| d.points.iter())
            .any(|p| uses_nsu(&p.raw));
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase =
            std::time::Duration::from_millis(crate::ticker::phase_ms(inst_id.as_str(), period));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // StatTracker（M4 实例指标）+ flush ticker（周期落 lux stat:<inst>）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        // F-2：声明本实例支持写回执（REST 写入口据此等待业务结果）。
        ctx.ctl_registry.declare_write_ack(&inst_id);
        let mut connects: u64 = 0;
        let mut bad_quality_total: u64 = 0;
        let mut request_timeouts_total: u64 = 0;
        let mut over_period_total: u64 = 0;
        let mut bad_warn = RateLimit::new(BAD_QUALITY_WARN_WINDOW);
        let mut over_warn = RateLimit::new(OVER_PERIOD_WARN_WINDOW);
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst_id.as_str());
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        // OPC-9 订阅模式运行态。
        // mode_eff：本连接生效模式——auto 降级后为 Polling，重连成功时重置
        //（服务端升级/更换后自动恢复订阅，不自锁在轮询）。
        let mut mode_eff = cfg.acquisition_mode;
        // 通知通道跨连接复用（回调桥只持 sender；旧会话 drop 时 sender 随之释放）。
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<SubMsg>();
        let mut sub: Option<SubRuntime> = None;
        let mut subscription_fallbacks: u64 = 0;
        let mut subscription_bad_items: u64 = 0;
        let mut sub_idle_ticks: u64 = 0;
        let watchdog = Duration::from_secs(cfg.subscription.watchdog_secs);
        let mut last_activity = Instant::now();
        let mut saw_activity = false;

        let mut session: Option<Arc<Session>> = None;
        let mut ev_handle: Option<JoinHandle<StatusCode>> = None;
        let mut namespaces: Option<NamespaceMap> = None;
        let mut prepared: Option<Prepared> = None;

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[opcuaClient] inst {inst_id} stopping");
                    break;
                }
                msg = cmd_rx.recv() => {
                    // D-2（2026-11-01 审查）：通道关闭（stop 先 unregister 再 cancel）
                    // 即退出——此前 None 空过会让 select 立即重 Poll 进入热循环
                    //（与 modbus `Evt::Cmd(None) => break` 对齐）。
                    let Some(m) = msg else { break };
                    if m.cmd == "write" {
                        // F-1/F-2：HMI `{tag,value}` 与旧 `{node_id,value}` 统一解析；
                        // 成败都经回执回流（不再只 warn）。
                        let outcome = handle_write(
                            &mut session,
                            &m.data,
                            &m.devid,
                            &raw_devices,
                            namespaces.as_ref(),
                            request_timeout,
                        )
                        .await;
                        if outcome.timed_out {
                            // OPC-10：写超时计数（读超时在 tick 路径计数）
                            request_timeouts_total += 1;
                        }
                        if let Err(e) = &outcome.result {
                            tracing::warn!("[opcuaClient] inst {inst_id} write 失败: {e}");
                        }
                        ctx.ctl_registry
                            .finish_write(m.request_id.as_deref(), outcome.result);
                        if session.is_none() {
                            // 写超时/传输错误：会话已作废（handle_write 内已尝试 CloseSession），
                            // 停事件循环并清连接期状态，下拍重连。
                            if let Some(h) = ev_handle.take() {
                                h.abort();
                            }
                            prepared = None;
                            namespaces = None;
                            // 订阅随会话失效（服务端自清），本地映射一并作废
                            sub = None;
                        }
                    } else {
                        tracing::info!("[opcuaClient] inst {inst_id} unknown cmd: {}", m.cmd);
                    }
                }
                // OPC-9：订阅通知（guard：仅订阅已建立时启用；通道关闭见 None 分支）
                msg = notify_rx.recv(), if sub.is_some() => {
                    let (mut batch, mut statuses) = (Vec::new(), Vec::new());
                    match msg {
                        Some(SubMsg::Data { client_handle, value }) => {
                            batch.push((client_handle, value));
                        }
                        Some(SubMsg::KeepAlive) => {}
                        Some(SubMsg::Status(st)) => statuses.push(st),
                        None => {
                            // 订阅已建立但发送端全没了 = 会话内部订阅状态已销毁；
                            // 拆会话走重连（防 recv 空转热循环，也不静默滞留）
                            tracing::warn!(
                                "[opcuaClient] inst {inst_id} 订阅通知通道关闭（会话订阅状态已销毁），拆会话重连"
                            );
                            close_session(&mut session, &mut ev_handle, &inst_id).await;
                            prepared = None;
                            namespaces = None;
                            sub = None;
                            continue;
                        }
                    }
                    // 连续 drain 同批通知（一次 publish 的多点变化聚成尽量少的 Sample）
                    loop {
                        match notify_rx.try_recv() {
                            Ok(SubMsg::Data { client_handle, value }) => {
                                batch.push((client_handle, value))
                            }
                            Ok(SubMsg::KeepAlive) => {}
                            Ok(SubMsg::Status(st)) => statuses.push(st),
                            Err(_) => break,
                        }
                    }
                    // 任何消息（数据/keep-alive/状态）都证明订阅活着
                    saw_activity = true;
                    last_activity = Instant::now();
                    if !batch.is_empty() {
                        let (by_dev, bad_keys) = {
                            let Some(rt) = sub.as_ref() else { continue };
                            aggregate_notifications(batch, &rt.handles)
                        };
                        bad_quality_total += bad_keys.len() as u64;
                        for key in &bad_keys {
                            if bad_warn.allow(key) {
                                tracing::warn!(
                                    "[opcuaClient] inst {inst_id} 点 {key} 坏质量（订阅通知，同点位 60s 内仅记一条）"
                                );
                            }
                        }
                        // OPC-3 同口径：接收时刻；每 devid 一个 Sample（F-9：values 至少 1 项）
                        let now_ts = chrono::Local::now().timestamp();
                        let mut send_failed = false;
                        for (devid, values) in by_dev {
                            let sample = Sample {
                                hwid: ctx.hwid.clone(),
                                devid,
                                values,
                                timestamp: now_ts,
                            };
                            if tx.send(sample).await.is_err() {
                                send_failed = true;
                                break;
                            }
                        }
                        if send_failed {
                            // hub 已关闭：实例退出（与轮询路径同口径）
                            break;
                        }
                    }
                    // 状态变化：非 Good（BadTimeout=服务端删订阅 等）→ 拆会话重连
                    for st in statuses {
                        if st.is_good() {
                            tracing::debug!(
                                "[opcuaClient] inst {inst_id} 订阅状态变化: {st}"
                            );
                            continue;
                        }
                        let msg = format!("subscription status change: {st}");
                        tracing::warn!(
                            "[opcuaClient] inst {inst_id} {msg}，拆会话重连（服务端已终止订阅）"
                        );
                        tracker.tick_fail(&msg);
                        close_session(&mut session, &mut ev_handle, &inst_id).await;
                        prepared = None;
                        namespaces = None;
                        sub = None;
                        break;
                    }
                }
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                }
                _ = ticker.tick() => {
                    let started = Instant::now();
                    if session.is_none() {
                        match connect(&cfg).await {
                            Ok((s, h)) => {
                                // OPC-13：首次连接不算重连（旧实现把首连计入 reconnects）。
                                connects += 1;
                                let reconnects = reconnects_after_connect(connects);
                                tracker.extra_insert(
                                    "session_reconnect_count",
                                    serde_json::json!(reconnects),
                                );
                                tracing::info!("[opcuaClient] inst {inst_id} connected {}", cfg.endpoint);
                                // 命名空间表（nsu= 解析必需）。读不到不拆会话：
                                // 纯 ns= 点位照常采集；配置含 nsu= 点位时记 tick_fail
                                // （避免"点位全被跳过但实例看着健康"的假健康）。
                                let ns_result = tokio::time::timeout(
                                    request_timeout,
                                    s.read_namespace_array(),
                                )
                                .await;
                                let ns_err = match ns_result {
                                    Ok(Ok(map)) => {
                                        namespaces = Some(map);
                                        None
                                    }
                                    Ok(Err(e)) => Some(format!("read namespace array failed: {e:?}")),
                                    Err(_) => {
                                        request_timeouts_total += 1;
                                        Some(format!(
                                            "read namespace array timeout ({}ms)",
                                            cfg.request_timeout_ms
                                        ))
                                    }
                                };
                                if let Some(ref e) = ns_err {
                                    tracing::warn!(
                                        "[opcuaClient] inst {inst_id} {e}（ns= 点位照常采集；nsu= 点位本会话不可用）"
                                    );
                                    if needs_nsu {
                                        tracker.tick_fail(e);
                                    }
                                }
                                // OPC-2：按服务端能力分批（不把全部节点塞一个请求）。
                                let max_nodes =
                                    probe_max_nodes_per_read(&s, request_timeout).await;
                                tracker.extra_insert(
                                    "max_nodes_per_read",
                                    serde_json::json!(max_nodes),
                                );
                                prepared = Some(build_prepared(
                                    &raw_devices,
                                    namespaces.as_ref(),
                                    max_nodes,
                                    &inst_id,
                                ));
                                session = Some(s);
                                ev_handle = Some(h);
                                // OPC-9：订阅模式（auto 先试订阅）——建订阅 + 全量
                                // monitored items；失败按模式裁决（降级/拆会话）。
                                if mode_eff != AcquisitionMode::Polling {
                                    // 重连重置生效模式：auto 每个新连接重试订阅
                                    //（服务端升级/更换后自愈，不锁死在轮询）。
                                    mode_eff = cfg.acquisition_mode;
                                    // 通道复用：清上一连接残留通知（旧 handle 映射已作废）
                                    while notify_rx.try_recv().is_ok() {}
                                    let max_mi = probe_max_monitored_items_per_call(
                                        session.as_ref().expect("just set"),
                                        request_timeout,
                                    )
                                    .await;
                                    tracker.extra_insert(
                                        "max_monitored_items_per_call",
                                        serde_json::json!(max_mi),
                                    );
                                    let setup = setup_subscription(
                                        session.as_ref().expect("just set"),
                                        prepared.as_ref().expect("just set"),
                                        &cfg.subscription,
                                        notify_tx.clone(),
                                        max_mi,
                                        request_timeout,
                                    )
                                    .await;
                                    match setup {
                                        Ok(rt) => {
                                            subscription_bad_items += rt.rejected.len() as u64;
                                            for (key, st) in &rt.rejected {
                                                if bad_warn.allow(key) {
                                                    tracing::warn!(
                                                        "[opcuaClient] inst {inst_id} 点 {key} monitored item 被拒 {st}（同点位 60s 内仅记一条；会话保留）"
                                                    );
                                                }
                                            }
                                            tracing::info!(
                                                "[opcuaClient] inst {inst_id} subscription={} 建立（{} 点，{} 点被拒）",
                                                rt.subscription_id,
                                                rt.handles.len(),
                                                rt.rejected.len()
                                            );
                                            sub = Some(rt);
                                            // 建立本身即活动：首个心跳不判 idle
                                            last_activity = Instant::now();
                                            saw_activity = true;
                                            tracker.extra_insert(
                                                "subscription_bad_items",
                                                serde_json::json!(subscription_bad_items),
                                            );
                                        }
                                        Err(f) => {
                                            let downgrade = f.error().is_some_and(|e| {
                                                subscription::fallback_to_polling(mode_eff, e)
                                            });
                                            if downgrade {
                                                // auto + 服务不支持：本连接降级轮询（会话保留）
                                                subscription_fallbacks += 1;
                                                tracker.extra_insert(
                                                    "subscription_fallbacks",
                                                    serde_json::json!(subscription_fallbacks),
                                                );
                                                tracing::warn!(
                                                    "[opcuaClient] inst {inst_id} 服务端不支持订阅（{}），本连接降级轮询",
                                                    f.message()
                                                );
                                                mode_eff = AcquisitionMode::Polling;
                                                sub = None;
                                            } else {
                                                // subscription 模式 / 非支持类错误：失败可见化 + 拆会话重试
                                                let msg =
                                                    format!("subscription setup failed: {}", f.message());
                                                tracker.tick_fail(&msg);
                                                tracing::warn!(
                                                    "[opcuaClient] inst {inst_id} {msg}，拆会话重试"
                                                );
                                                close_session(
                                                    &mut session,
                                                    &mut ev_handle,
                                                    &inst_id,
                                                )
                                                .await;
                                                prepared = None;
                                                namespaces = None;
                                                sub = None;
                                                if tracker.should_flush() {
                                                    tracker.flush().await;
                                                }
                                                continue;
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[opcuaClient] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() {
                                    tracker.flush().await;
                                }
                                continue;
                            }
                        }
                    }
                    if session.is_none() {
                        // connect 失败 / 订阅建立失败：本拍已 tick_fail + flush
                        continue;
                    }
                    if sub.is_some() {
                        // ---------- OPC-9 订阅模式心跳 ----------
                        // F-7 overrun 警告不适用：本模式无周期采集拍，服务端慢表现为
                        // keep-alive/数据间隔变长，由看门狗统一判定，不再按拍读。
                        if saw_activity {
                            tracker.tick_ok(period.as_millis() as u64);
                            saw_activity = false;
                            sub_idle_ticks = 0;
                        } else {
                            sub_idle_ticks += 1;
                        }
                        tracker.extra_insert(
                            "subscription_idle_ticks",
                            serde_json::json!(sub_idle_ticks),
                        );
                        tracker.extra_insert(
                            "bad_quality",
                            serde_json::json!(bad_quality_total),
                        );
                        tracker.extra_insert(
                            "request_timeouts",
                            serde_json::json!(request_timeouts_total),
                        );
                        tracker.extra_insert(
                            "subscription_fallbacks",
                            serde_json::json!(subscription_fallbacks),
                        );
                        tracker.extra_insert(
                            "subscription_bad_items",
                            serde_json::json!(subscription_bad_items),
                        );
                        // 看门狗：无任何消息（含 keep-alive）超窗 → 订阅失效，拆会话
                        //（走现有重连路径重建订阅+映射）。检测粒度 = 心跳拍 period。
                        if watchdog_expired(Instant::now(), last_activity, watchdog) {
                            let msg = format!(
                                "subscription watchdog: {}s 无任何消息（含 keep-alive），订阅失效",
                                cfg.subscription.watchdog_secs
                            );
                            tracker.tick_fail(&msg);
                            tracing::warn!("[opcuaClient] inst {inst_id} {msg}");
                            close_session(&mut session, &mut ev_handle, &inst_id).await;
                            prepared = None;
                            namespaces = None;
                            sub = None;
                        }
                        if tracker.should_flush() {
                            tracker.flush().await;
                        }
                        continue;
                    }
                    let Some(s) = session.clone() else { continue };
                    let mut total_points = 0usize;
                    let mut good_points = 0usize;
                    let mut round_err: Option<String> = None;
                    let mut session_lost = false;
                    let mut send_failed = false;
                    if let Some(prep) = prepared.as_ref() {
                        'read: for dev in &prep.devices {
                            if dev.points.is_empty() {
                                continue;
                            }
                            total_points += dev.points.len();
                            let mut values: BTreeMap<String, SampleValue> = BTreeMap::new();
                            let mut dev_good = 0usize;
                            for chunk in chunk_nodes(dev.points.len(), prep.max_nodes_per_read) {
                                let batch = &dev.points[chunk.clone()];
                                let nodes: Vec<ReadValueId> =
                                    batch.iter().map(|(_, spec)| read_value_id(spec)).collect();
                                // OPC-3：不请求时间戳（Both→Neither）——Sample.timestamp
                                // 明确为**接收时刻**，省带宽也消除"请求了源时间戳却不用"的矛盾。
                                let res = tokio::time::timeout(
                                    request_timeout,
                                    s.read(&nodes, TimestampsToReturn::Neither, 0.0),
                                )
                                .await;
                                match res {
                                    Ok(Ok(results)) => {
                                        if results.len() != nodes.len() {
                                            tracing::warn!(
                                                "[opcuaClient] inst {inst_id} dev {} 读响应项数不符: 请求 {} 返回 {}（多余/缺失项按坏质量处理）",
                                                dev.devid,
                                                nodes.len(),
                                                results.len()
                                            );
                                        }
                                        for ((field, _), dv) in batch.iter().zip(results.iter()) {
                                            let bad_status = data_value_bad_status(dv);
                                            if bad_status.is_some() || dv.value.is_none() {
                                                // OPC-1：坏质量/无值不再静默丢弃 → 显式 Null；
                                                // OPC-16：每点位 60s 只 warn 一条。
                                                bad_quality_total += 1;
                                                if bad_warn.allow(&format!("{}.{field}", dev.devid)) {
                                                    match bad_status {
                                                        Some(st) => tracing::warn!(
                                                            "[opcuaClient] inst {inst_id} dev {} 点 {field} 坏质量 {st}（同点位 60s 内仅记一条）",
                                                            dev.devid
                                                        ),
                                                        None => tracing::warn!(
                                                            "[opcuaClient] inst {inst_id} dev {} 点 {field} 无 value（同点位 60s 内仅记一条）",
                                                            dev.devid
                                                        ),
                                                    }
                                                }
                                                values.insert(field.clone(), SampleValue::Null);
                                            } else {
                                                dev_good += 1;
                                                good_points += 1;
                                                values.insert(field.clone(), data_value_to_sample(dv));
                                            }
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        let st = e.status();
                                        if is_transport_status(st) {
                                            // 传输层错误才拆会话（OPC-2）
                                            round_err = Some(format!("read failed: {e:?}"));
                                            session_lost = true;
                                            break 'read;
                                        }
                                        // 单批业务失败：本批记 Null，不拆会话
                                        tracing::warn!(
                                            "[opcuaClient] inst {inst_id} dev {} 读批失败（{} 点，保留会话）: {e:?}",
                                            dev.devid,
                                            batch.len()
                                        );
                                        round_err = Some(format!("read batch failed: {e:?}"));
                                        for (field, _) in batch {
                                            values.insert(field.clone(), SampleValue::Null);
                                        }
                                    }
                                    Err(_) => {
                                        // OPC-10：超时视为会话失效（下拍重连）并计数
                                        request_timeouts_total += 1;
                                        round_err = Some(format!(
                                            "read timeout ({}ms)",
                                            cfg.request_timeout_ms
                                        ));
                                        session_lost = true;
                                        break 'read;
                                    }
                                }
                            }
                            // F-9：设备内全部点位坏质量 → 不投递（空/全 Null Sample 无信息量）
                            if dev_good == 0 {
                                tracing::debug!(
                                    "[opcuaClient] inst {inst_id} dev {} 本拍无有效值（全部坏质量），不投递 Sample",
                                    dev.devid
                                );
                                continue;
                            }
                            let sample = Sample {
                                hwid: ctx.hwid.clone(),
                                devid: dev.devid.clone(),
                                values,
                                // OPC-3：接收时刻（服务端时间戳未请求，客户端时钟即口径）
                                timestamp: chrono::Local::now().timestamp(),
                            };
                            if tx.send(sample).await.is_err() {
                                send_failed = true;
                                break 'read;
                            }
                        }
                    }
                    if session_lost {
                        close_session(&mut session, &mut ev_handle, &inst_id).await;
                        prepared = None;
                        namespaces = None;
                    }
                    if send_failed {
                        // hub 已关闭：实例应退出（与 modbus/s7 同口径）
                        break;
                    }
                    // 汇总指标
                    tracker.extra_insert("bad_quality", serde_json::json!(bad_quality_total));
                    tracker.extra_insert(
                        "request_timeouts",
                        serde_json::json!(request_timeouts_total),
                    );
                    let elapsed = started.elapsed();
                    if is_over_period(elapsed, period) {
                        over_period_total += 1;
                        if over_warn.allow("tick_over_period") {
                            tracing::warn!(
                                "[opcuaClient] inst {inst_id} 一轮耗时 {elapsed:?} 超过采集周期 {period:?}（MissedTickBehavior::Skip 会丢拍：请检查点表规模/服务端响应/request_timeout_ms）"
                            );
                        }
                    }
                    tracker.extra_insert(
                        "tick_over_period_count",
                        serde_json::json!(over_period_total),
                    );
                    let outcome = if session_lost {
                        // 会话已因传输错误/超时作废：本轮失败（错误文本优先）
                        Err(round_err.unwrap_or_else(|| "session lost".to_string()))
                    } else {
                        round_status(total_points, good_points, round_err.as_deref())
                    };
                    match outcome {
                        Ok(()) => tracker.tick_ok(elapsed.as_millis() as u64),
                        Err(e) => tracker.tick_fail(&e),
                    }
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }

        close_session(&mut session, &mut ev_handle, &inst_id).await;
        tracing::info!("[opcuaClient] inst {inst_id} exited");

        // 统一退出处理（cancelled 触发；stat 落 stopped）。
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// 写命令结果 + 是否因超时导致会话作废（OPC-10 计数用）。
struct WriteOutcome {
    result: Result<(), String>,
    timed_out: bool,
}

/// 处理 write 命令（F-1 目标契约 / F-2 结果回执 / OPC-6 类型校验 / OPC-10 超时）。
///
/// `data` = JSON `{"tag":"点位显示名","value":1.5,"opc_type":"Int32"}`（或旧
/// `{"node_id":"ns=2;s=v1",…}`）。返回 `Err` 时调用方把失败文本经写回执送回 REST。
async fn handle_write(
    session: &mut Option<Arc<Session>>,
    data: &str,
    msg_devid: &str,
    devices: &[RawDevice],
    namespaces: Option<&NamespaceMap>,
    request_timeout: Duration,
) -> WriteOutcome {
    let mut timed_out = false;
    let result = handle_write_inner(
        session,
        data,
        msg_devid,
        devices,
        namespaces,
        request_timeout,
        &mut timed_out,
    )
    .await;
    WriteOutcome { result, timed_out }
}

async fn handle_write_inner(
    session: &mut Option<Arc<Session>>,
    data: &str,
    msg_devid: &str,
    devices: &[RawDevice],
    namespaces: Option<&NamespaceMap>,
    request_timeout: Duration,
    timed_out: &mut bool,
) -> Result<(), String> {
    let Some(s) = session.as_ref().cloned() else {
        return Err("write 失败：未连接（OPC UA 会话未建立）".to_string());
    };
    let req: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("write 命令 JSON 解析失败: {e}"))?;
    // 命令层契约（F-1 目标解析 / OPC-6 类型校验 / OPC-7 点表默认类型 /
    // OPC-15 下标范围 / OPC-17 null 拒写）：纯函数，单测覆盖。
    let wv = build_write_request(&req, msg_devid, devices, namespaces)?;
    let node = wv.node_id.clone();
    match tokio::time::timeout(request_timeout, s.write(&[wv])).await {
        Ok(Ok(results)) => {
            let mut failures: Vec<String> = Vec::new();
            for (i, st) in results.iter().enumerate() {
                // OPC-6：每节点 StatusCode 累计，任一失败 → Err（含节点与状态码）
                if !st.is_good() {
                    failures.push(format!("#{i}: {st}"));
                }
            }
            if results.len() != 1 {
                failures.push(format!("响应项数 {} != 1", results.len()));
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "write 失败（节点 {node}）：{}",
                    failures.join("; ")
                ))
            }
        }
        Ok(Err(e)) => {
            if is_transport_status(e.status()) {
                // 传输层错误 → 会话作废（下拍重连）；尽量先 CloseSession
                disconnect_session(&s).await;
                *session = None;
            }
            Err(format!("write 失败（节点 {node}）: {e:?}"))
        }
        Err(_) => {
            // OPC-10：请求超时视为会话失效（此前裸 await 最长阻塞 60s）
            *timed_out = true;
            disconnect_session(&s).await;
            *session = None;
            Err(format!(
                "write 超时（{}ms，节点 {node}），会话作废下拍重连",
                request_timeout.as_millis()
            ))
        }
    }
}

/// 命令 JSON → 写请求（不触碰网络，便于单测）。
///
/// 契约：`{"tag":显示名,"value":…,"opc_type":可选}`（旧 `{"node_id":…}` 优先）。
/// `opc_type` 缺省用点表声明类型（[`node::resolve_write_target`]），再无则按值推断。
fn build_write_request(
    req: &serde_json::Value,
    msg_devid: &str,
    devices: &[RawDevice],
    namespaces: Option<&NamespaceMap>,
) -> Result<WriteValue, String> {
    let target = resolve_write_target(
        devices,
        msg_devid,
        req.get("node_id").and_then(|v| v.as_str()),
        req.get("tag").and_then(|v| v.as_str()),
        namespaces,
    )?;
    let value_json = req
        .get("value")
        .ok_or_else(|| "write 缺少 value 字段".to_string())?;
    let value = json_to_sample_value(value_json);
    // OPC-17：null 值不得写成 "null"/Variant::Empty
    if matches!(value, SampleValue::Null) {
        return Err("write value 为 null（无有效值），拒写".to_string());
    }
    // OPC-7：命令 opc_type 优先，缺省用点表声明类型
    let opc_type = req
        .get("opc_type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| target.opc_type.clone());
    let variant = variant_with_type(&value, opc_type.as_deref())?;
    Ok(write_value(&target.spec, variant))
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "opcuaClient",
        create: || Box::new(OpcuaClient),
    }
}
