//! `eipClient`：EtherNet/IP 南向采集插件（rust-ethernet-ip 1.2，罗克韦尔 AB
//! CompactLogix / ControlLogix）。
//!
//! 周期连接 PLC，按 `devices` 配置批量读 tag，组装 `Sample` 推入 hub；
//! 同时消费下行命令（`cmd="write"`）写回 tag。
//!
//! 配置（`InstConfig::app_config` JSON，缺失字段走默认）：
//! ```json
//! {
//!   "endpoint": "192.168.1.100:44818",
//!   "period": 3,
//!   "slot": null,
//!   "reconnect_secs": 5,
//!   "health_check_secs": 0,
//!   "health_tag": null,
//!   "max_packet_size": null,
//!   "max_batch_bytes": 400,
//!   "round_timeout_ms": null,
//!   "batch_max_operations": null, "batch_max_packet_size": null,
//!   "batch_packet_timeout_ms": null, "batch_continue_on_error": null,
//!   "batch_optimize_packing": null
//! }
//! ```
//! - `endpoint`：`ip:port` 直通；`host:port` / 纯主机名启动时 DNS 解析（缺省端口 44818）——
//!   库只接受 `SocketAddr`（E-15）；
//! - `health_tag`：健康检查的真实读点位（E-10），缺省取第一个可读点位；
//! - `max_batch_bytes`：单个 MSP 包的在线字节预算，默认 400（E-11/E-19）；
//! - `round_timeout_ms`：整轮采集预算，默认 `period*1000`（最小 1000，E-17）。
//!
//! 变量映射（复用 `devices`，与其他南向插件对齐）：
//! - `DeviceSpec.value` = 设备 ID（即 `rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = tag 全名（`Program:Main.Tag` / `MyTag` / `MyArray[3]` /
//!   `MyArray[0:9]` 范围 / `MyDint.5` 位寻址——读写两侧均由库自动识别 bit 路径）
//! - `ChildSpec.label` = values map 的 key（显示名；空则用 tag 名）
//! - `ChildSpec.type`：`"string"` → 走 `read_string_tag`（自定义 STRING 类型如 Str82 的
//!   批量路径解不了，须专用解码）；CIP 类型名（bool/sint/…/lreal）参与支持表校验
//!   （E-2/E-3）；其它通用值类型标签（`"float"` 等）不参与 eip 解码。
//!
//! 写命令 `data` JSON：`{"tag":"点位显示名","value":1500,"plc_type":"dint"}`。
//! `tag` 先按**点位显示名**查索引（F-1），再按已配置的 CIP 路径原样使用；都不是则拒写。
//! `plc_type` 可选（CIP 类型化写入，建议带）：bool/sint/int/dint/lint/usint/uint/
//! udint/ulint/real/lreal/string；缺省推断 bool→Bool、整数→Dint、浮点→Real、串→String。
//! 写值全部 checked（E-7）：越界/负数写无符号/非数字串/非整数值一律拒写，不再回绕或静默写 0。
//! 写结果经 `CtlRegistry::finish_write` 回流（F-2）。
//!
//! 已知限制（记录不修，见协议语义台账 §6）：
//! - **E-9**：位写 / BOOL 数组元素写是库内的"读-改-写"，非原子——与 PLC 程序或其它
//!   写者并发时会回滚同字其它位；
//! - **E-20**：批量结果按"位置"回填（`batch_exec.rs:703-744`），依赖设备保序；
//!   本插件按 tag 名回填不受该错位影响，但非 AB 设备/网关的整体错位风险仍在上游；
//! - **E-10**：点表为空（无可读点位）时健康检查退回库的 NOP（不等应答，无法证明存活）。
//!
//! **报文 trace 豁免（不接 `TraceIo`）**：EIP/CIP 虽是二进制协议，但 `rust-ethernet-ip`
//! 在 `EipClient::connect` 内部自建并独占 TCP 流（外层拿不到裸 `TcpStream` 句柄），
//! `TraceIo<T>` 无处包装；与 opcua/s7（库自管会话，非裸报文）同属豁免面。帧级诊断
//! 走库自身日志 + `GET /api/v1/logs/recent`；若未来换成可注入 IO 的库再接线。

mod failure;
mod route;
mod value;
mod write;

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, DeviceSpec, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use failure::{chunk_tags_by_bytes, classify_batch_error, classify_error, FailureClass};
use route::{array_kind, plan_point, ArrayKind, ArraySpan, ReadRoute, TypeClass};
use rust_ethernet_ip::{BatchConfig, EipClient, PlcValue, RoutePath};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use value::{json_to_sample_value, plc_to_sample, plc_with_type, sample_to_string_repr};
use write::WriteIndex;

/// EtherNet/IP 采集插件（结构体名避开库的 `EipClient`）。
#[derive(Debug, Default)]
pub struct EipClientPlugin;

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
struct EipCfg {
    /// 原始 endpoint（可能是主机名/省略端口；启动时经 [`resolve_endpoint`] 解析，E-15）。
    endpoint: String,
    period: u64,
    /// ControlLogix 背板槽号；None 走普通 connect（CompactLogix 常见）。
    slot: Option<u8>,
    /// 断线重连节流（秒）。
    reconnect_secs: u64,
    /// >0 时周期做健康检查（真实读一次，E-10）。
    health_check_secs: u64,
    /// 健康检查探针点位（E-10；缺省取第一个可读点位）。
    health_tag: Option<String>,
    /// 透传 `EipClient::set_max_packet_size`（同时作为批量打包上限，E-18）。
    max_packet_size: Option<u32>,
    /// 单个 MSP 包的在线字节预算（E-11/E-19，默认 400）。
    max_batch_bytes: usize,
    /// 整轮采集预算（毫秒，E-17）。
    round_timeout_ms: u64,
    /// BatchConfig 透传字段（None = 库默认）。
    batch_max_operations: Option<usize>,
    batch_max_packet_size: Option<usize>,
    batch_packet_timeout_ms: Option<u64>,
    batch_continue_on_error: Option<bool>,
    batch_optimize_packing: Option<bool>,
}

/// 默认单包字符串字节预算。
///
/// 库默认 504（`BatchConfig::default`），但打包只按**请求侧**估且少算 16B
/// SendRRData/CPF 固定头（E-11/E-19）——取 400 留出固定头 + 每 op 偏移表余量。
const DEFAULT_MAX_BATCH_BYTES: usize = 400;

/// 整轮预算下限（E-17）。
const MIN_ROUND_TIMEOUT_MS: u64 = 1000;

fn parse_cfg(app: &serde_json::Value) -> Result<EipCfg, String> {
    let endpoint = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?
        .to_string();
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .max(1);
    let slot = app.get("slot").and_then(|v| v.as_u64()).map(|v| {
        u8::try_from(v).unwrap_or_else(|_| {
            tracing::warn!("[eipClient] slot {v} 超出 u8，按 255 处理");
            u8::MAX
        })
    });
    let max_batch_bytes = app
        .get("max_batch_bytes")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_MAX_BATCH_BYTES)
        .max(64);
    Ok(EipCfg {
        endpoint,
        period,
        slot,
        reconnect_secs: app
            .get("reconnect_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .max(1),
        health_check_secs: app
            .get("health_check_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        health_tag: app
            .get("health_tag")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        max_packet_size: app.get("max_packet_size").and_then(|v| v.as_u64()).map(v32),
        max_batch_bytes,
        // E-17：默认一轮预算 = 一个采集周期（period 已保证 ≥1），下限 1s
        round_timeout_ms: app
            .get("round_timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(period.saturating_mul(1000))
            .max(MIN_ROUND_TIMEOUT_MS),
        batch_max_operations: app
            .get("batch_max_operations")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        batch_max_packet_size: app
            .get("batch_max_packet_size")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        batch_packet_timeout_ms: app.get("batch_packet_timeout_ms").and_then(|v| v.as_u64()),
        batch_continue_on_error: app.get("batch_continue_on_error").and_then(|v| v.as_bool()),
        batch_optimize_packing: app.get("batch_optimize_packing").and_then(|v| v.as_bool()),
    })
}

/// u64 → u32 饱和（配置容错，超界取 MAX 并在解析期已无 per-field 日志面）。
fn v32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// EtherNet/IP 标准端口（E-15 主机名缺省端口）。
const DEFAULT_EIP_PORT: u16 = 44818;

/// endpoint 形态（E-15）。
///
/// 库只接受 `SocketAddr`（`addr.parse::<SocketAddr>()`，
/// `<REG>rust-ethernet-ip-1.2.0/src/client.rs:592-598`）——主机名/省略端口此前必失败。
#[derive(Debug, PartialEq, Eq)]
enum EndpointShape {
    /// 已是 `ip:port`（含 IPv6 方括号形式）→ 直通。
    Addr(String),
    /// 需 DNS 解析（`host:port` 或纯主机名）。
    Lookup {
        /// 主机名。
        host: String,
        /// 端口。
        port: u16,
    },
}

/// 纯函数部分：endpoint 字符串 → 形态（单测锁定，不做 I/O）。
fn endpoint_shape(raw: &str) -> Result<EndpointShape, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("app_config.endpoint 为空（E-15）".to_string());
    }
    if t.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(EndpointShape::Addr(t.to_string()));
    }
    match t.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(format!("endpoint '{t}' 主机名为空（E-15）"));
            }
            let port: u16 = port.parse().map_err(|_| {
                format!("endpoint '{t}' 端口 '{port}' 不是 0..=65535 的整数（E-15）")
            })?;
            Ok(EndpointShape::Lookup {
                host: host.to_string(),
                port,
            })
        }
        None => Ok(EndpointShape::Lookup {
            host: t.to_string(),
            port: DEFAULT_EIP_PORT,
        }),
    }
}

/// endpoint 解析（E-15）：`ip:port` 直通；主机名走 `tokio::net::lookup_host` 取首个地址。
///
/// 结果在启动时定格（不做每 tick 解析）；DNS 失败 = 配置期错误。
async fn resolve_endpoint(raw: &str) -> Result<String, String> {
    match endpoint_shape(raw)? {
        EndpointShape::Addr(a) => Ok(a),
        EndpointShape::Lookup { host, port } => {
            let mut addrs = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| format!("endpoint '{raw}' DNS 解析失败: {e}（E-15）"))?;
            let addr = addrs
                .next()
                .ok_or_else(|| format!("endpoint '{raw}' 无可用地址（E-15）"))?;
            Ok(addr.to_string())
        }
    }
}

/// connect 上限（A-3）：库内 `EipClient` 构造是裸 `TcpStream::connect`（无超时），
/// 黑洞地址（防火墙 DROP）会挂 OS 级 SYN 重试 ~2min，期间 token/cmd 全停——
/// 外层包 timeout，超时即 drop future（socket 一并释放）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 建立连接并应用透传配置（max_packet_size / batch 参数）。
async fn connect(cfg: &EipCfg, endpoint: &str) -> Result<EipClient, String> {
    // A-3：整体包 CONNECT_TIMEOUT（with_route_path/connect 两大构造路径同款）。
    let mut client = match cfg.slot {
        Some(slot) => {
            let fut = EipClient::with_route_path(endpoint, RoutePath::new().add_slot(slot));
            match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
                Ok(r) => r.map_err(|e| format!("connect {endpoint} slot {slot}: {e}"))?,
                Err(_) => {
                    return Err(format!(
                        "connect {endpoint} slot {slot}: timeout ({CONNECT_TIMEOUT:?})"
                    ));
                }
            }
        }
        None => match tokio::time::timeout(CONNECT_TIMEOUT, EipClient::connect(endpoint)).await {
            Ok(r) => r.map_err(|e| format!("connect {endpoint}: {e}"))?,
            Err(_) => {
                return Err(format!("connect {endpoint}: timeout ({CONNECT_TIMEOUT:?})"));
            }
        },
    };
    // E-18：`max_packet_size` 此前只进 `set_max_packet_size`（单点读写缓冲），批量打包只看
    // `BatchConfig.max_packet_size`（<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:398）
    // → 配置对批量无效。这里两处取同一值；未配置时批量按保守默认 max_batch_bytes（E-11/E-19）。
    let batch_bytes = cfg
        .max_packet_size
        .map(|v| v as usize)
        .unwrap_or(cfg.max_batch_bytes)
        .max(64);
    if let Some(sz) = cfg.max_packet_size {
        client.set_max_packet_size(sz);
    }
    // E-18：批量打包上限显式设置（库只读 BatchConfig，不读 set_max_packet_size）。
    let mut bc = BatchConfig {
        max_packet_size: batch_bytes,
        ..BatchConfig::default()
    };
    if let Some(v) = cfg.batch_max_operations {
        bc.max_operations_per_packet = v;
    }
    if let Some(v) = cfg.batch_max_packet_size {
        bc.max_packet_size = v;
    }
    if let Some(v) = cfg.batch_packet_timeout_ms {
        bc.packet_timeout_ms = v;
    }
    if let Some(v) = cfg.batch_continue_on_error {
        bc.continue_on_error = v;
    }
    if let Some(v) = cfg.batch_optimize_packing {
        bc.optimize_packet_packing = v;
    }
    client.configure_batch_operations(bc);
    Ok(client)
}

/// 采集目标（配置期预解析）：分流后的四类路径 + 健康探针。
#[derive(Debug, Default)]
struct Targets {
    /// 批量读（标量 / `Program:` 作用域 / UDT 成员 / 多维数组，E-5）。
    batch: Vec<(String, Vec<(String, String)>)>,
    /// 逐个 `read_string_tag`（自定义 STRING 类型）。
    strings: Vec<(String, Vec<(String, String)>)>,
    /// 逐个 `read_tag` 单点读：位读（`TagPath::Bit`）。
    ///
    /// A-2（2026-11-01 审查）：库的批量读对位寻址只编码**父字**、位"完全在客户端
    /// 解析"（rust-ethernet-ip tag-path lib.rs:220-231），批量解码无位掩码——
    /// `MyDint.5` 批量读到的是父字整值而非该位（静默错数据）。
    bits: Vec<(String, Vec<(String, String)>)>,
    /// 数组元素/范围（E-13/E-14）：按 (devid, base) 归并，一次范围读回整段。
    arrays: Vec<ArrayGroup>,
    /// 健康检查探针点位（E-10）：首个可读点位（配置 `health_tag` 优先，见 run）。
    health_probe: Option<String>,
}

/// 同一 (devid, base) 的数组点位归并。
#[derive(Debug)]
struct ArrayGroup {
    devid: String,
    /// 数组基名（不含下标）。
    base: String,
    /// 组内点位是否全部声明 `bool`（0xD3 歧义时用于判定 BOOL 数组，E-2）。
    all_bool: bool,
    points: Vec<ArrayPoint>,
}

/// 一个数组元素点位（`Arr[3]` 或 `Arr[0:9]`）。
#[derive(Debug)]
struct ArrayPoint {
    /// values 的 key 前缀（范围点位展开为 `label[i]`）。
    field: String,
    /// 起始下标。
    start: u32,
    /// 元素个数。
    count: u32,
    /// 配置里的原始路径（单元素兜底读原样使用）。
    path: String,
}

impl ArrayPoint {
    /// 该点位展开的 values key（`[a:b]` → `label[a..=b]`）。
    fn fields(&self) -> Vec<String> {
        if self.count == 1 {
            vec![self.field.clone()]
        } else {
            (0..self.count)
                .map(|i| format!("{}[{i}]", self.field))
                .collect()
        }
    }
}

impl ArrayGroup {
    /// 覆盖全部组内点位的读范围（E-13/E-14：一个 base 一次范围读）。
    fn span(&self) -> ArraySpan {
        let start = self.points.iter().map(|p| p.start).min().unwrap_or(0);
        let end = self
            .points
            .iter()
            .map(|p| p.start.saturating_add(p.count))
            .max()
            .unwrap_or(start);
        ArraySpan {
            base: self.base.clone(),
            start,
            count: end.saturating_sub(start).max(1),
        }
    }
}

/// 点位分流（配置期，E-2/E-3/E-4/E-5/E-6/E-14/E-16 的校验入口）。
///
/// 任一非法点位 → `Err`（实例启动失败并给出台账条目），不再"带病采集"。
fn parse_targets(devices: &[DeviceSpec]) -> Result<Targets, String> {
    let mut t = Targets::default();
    // (devid, base) → arrays 下标
    let mut array_idx: HashMap<(String, String), usize> = HashMap::new();
    for dev in devices {
        let mut batch = Vec::new();
        let mut strings = Vec::new();
        let mut bits = Vec::new();
        for c in &dev.children {
            if c.value.is_empty() {
                continue;
            }
            let field = if c.label.is_empty() {
                c.value.clone()
            } else {
                c.label.clone()
            };
            let route = plan_point(&c.value, &c.r#type)
                .map_err(|e| format!("devid '{}' 点位 '{}': {e}", dev.value, c.value))?;
            let is_bool = matches!(
                route::declared_type_class(&c.r#type),
                TypeClass::Cip("bool")
            );
            match route {
                ReadRoute::Batch => batch.push((field, c.value.clone())),
                ReadRoute::StringTag => strings.push((field, c.value.clone())),
                ReadRoute::Bit { .. } => bits.push((field, c.value.clone())),
                ReadRoute::Array(span) => {
                    let key = (dev.value.clone(), span.base.clone());
                    let idx = match array_idx.get(&key) {
                        Some(i) => *i,
                        None => {
                            t.arrays.push(ArrayGroup {
                                devid: dev.value.clone(),
                                base: span.base.clone(),
                                all_bool: true,
                                points: Vec::new(),
                            });
                            array_idx.insert(key, t.arrays.len() - 1);
                            t.arrays.len() - 1
                        }
                    };
                    let g = &mut t.arrays[idx];
                    g.all_bool &= is_bool;
                    g.points.push(ArrayPoint {
                        field,
                        start: span.start,
                        count: span.count,
                        path: c.value.clone(),
                    });
                }
            }
        }
        if !batch.is_empty() {
            t.batch.push((dev.value.clone(), batch));
        }
        if !strings.is_empty() {
            t.strings.push((dev.value.clone(), strings));
        }
        if !bits.is_empty() {
            t.bits.push((dev.value.clone(), bits));
        }
    }
    // E-10：健康探针 = 第一个可读点位（优先批量标量——最稳定的常规点位）
    t.health_probe = t
        .batch
        .first()
        .and_then(|(_, p)| p.first().map(|(_, tag)| tag.clone()))
        .or_else(|| {
            t.bits
                .first()
                .and_then(|(_, p)| p.first().map(|(_, tag)| tag.clone()))
        })
        .or_else(|| {
            t.strings
                .first()
                .and_then(|(_, p)| p.first().map(|(_, tag)| tag.clone()))
        })
        .or_else(|| {
            t.arrays
                .first()
                .and_then(|g| g.points.first().map(|p| format!("{}[{}]", g.base, p.start)))
        });
    Ok(t)
}

/// tick 耗时超过 period 的告警限流窗口（F-7，2026-11-01 协议语义审查）。
const OVERRUN_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// tick 超时告警状态（F-7）。
///
/// `MissedTickBehavior::Skip` 会静默丢拍：一轮轮询慢于 period 时采集频率悄悄低于
/// 配置值而无人知晓。这里按 [`OVERRUN_WARN_INTERVAL`] 限流 warn（防每拍刷屏），
/// 计数进 tracker extra（`tick_overrun_count`）供运维观察。
#[derive(Debug, Default)]
struct OverrunWarn {
    /// 累计超时次数（进 extra，不受限流影响）。
    count: u64,
    /// 上次告警时刻（限流窗口起点）。
    last: Option<std::time::Instant>,
}

impl OverrunWarn {
    /// 记录一次超时；返回本次是否应打 warn（首拍或距上次 ≥ 窗口）。
    fn record(&mut self, now: std::time::Instant) -> bool {
        self.count = self.count.saturating_add(1);
        let due = self
            .last
            .is_none_or(|t| now.duration_since(t) >= OVERRUN_WARN_INTERVAL);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// 跨轮累计计数（进 StatTracker extra）。
#[derive(Debug, Default)]
struct RoundCounters {
    /// 点级读失败次数（有值点变 Null）。
    point_failures: u64,
    /// 无解码分支 / 0xD3 歧义被跳过的点数（E-2/E-3）。
    unsupported_points: u64,
    /// 批量降半重试次数（E-11）。
    batch_retries: u64,
    /// 超预算截断轮数（E-17）。
    truncated_rounds: u64,
}

impl RoundCounters {
    /// 落 StatTracker extra（运维可见）。
    fn publish(&self, tracker: &mut crate::plugin::stat_tracker::StatTracker) {
        tracker.extra_insert("point_failures", serde_json::json!(self.point_failures));
        tracker.extra_insert(
            "unsupported_points",
            serde_json::json!(self.unsupported_points),
        );
        tracker.extra_insert("batch_retries", serde_json::json!(self.batch_retries));
        tracker.extra_insert("round_truncated", serde_json::json!(self.truncated_rounds));
    }
}

/// 一轮采集结果。
#[derive(Debug)]
enum RoundOutcome {
    /// 完成（可能有部分点级失败，已写 Null）。
    Ok,
    /// 传输层失效（连接不可用）：须拆连接重建；本轮已采数据已投递（E-1/E-17）。
    Transport(String),
    /// 超整轮预算被截断：连接保留；本轮已采数据已投递（E-17）。
    Truncated,
}

/// 单点失败（分类 + 可读原因）。
#[derive(Debug)]
struct PointFailure {
    class: FailureClass,
    msg: String,
    /// 该失败来自"整包没发出"——需要单点读隔离坏点（否则无法定位是哪一条）。
    isolate: bool,
}

/// 一轮采集的上下文与聚合状态。
struct Round<'a> {
    inst_id: &'a str,
    /// 本轮预算截止时刻（E-17）。
    deadline: tokio::time::Instant,
    /// 单包字节预算（E-11/E-19）。
    max_batch_bytes: usize,
    /// devid → (field → value)。
    per_dev: BTreeMap<String, BTreeMap<String, SampleValue>>,
    /// 传输层失败原因（Some → 本轮收尾时拆连接重建）。
    transport: Option<String>,
    /// 超预算截断过。
    truncated: bool,
    /// 只告警一次的 key 集合（跨轮复用）。
    warned: &'a mut HashSet<String>,
    counters: &'a mut RoundCounters,
    /// 数组类型缓存（E-13：首个 tick 探测后不再重复）。
    probes: &'a mut HashMap<String, ArrayProbe>,
}

/// 数组 base 的类型探测结果（E-13 缓存值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayProbe {
    /// 标签属性读到的元素类型码（1 次往返，库内亦缓存）。
    Type(u16),
    /// 属性不可读 → 退回逐元素 `read_tag`（库单点路径自带 BOOL 数组探测）。
    PerElement,
}

impl Round<'_> {
    /// 本轮预算是否已耗尽（E-17：按步骤边界检查，已采数据不丢）。
    fn expired(&self) -> bool {
        tokio::time::Instant::now() >= self.deadline
    }

    /// 记一条 value。
    fn insert(&mut self, devid: &str, field: &str, v: SampleValue) {
        self.per_dev
            .entry(devid.to_string())
            .or_default()
            .insert(field.to_string(), v);
    }

    /// 记传输层失效（保留首个原因）。
    fn note_transport(&mut self, msg: String) {
        if self.transport.is_none() {
            self.transport = Some(msg);
        }
    }

    /// 点级失败处置（F-3 坏质量 + E-1/E-3 分类处置）。
    ///
    /// 一律写 `SampleValue::Null`（rtdata/历史跳过该 field，保留上次有效值与时间戳），
    /// 而不是静默丢点让下游误以为旧值仍新鲜。
    fn fail_point(&mut self, devid: &str, field: &str, tag: &str, class: FailureClass, msg: &str) {
        match class {
            FailureClass::Transport => {
                self.note_transport(format!("tag '{tag}': {msg}"));
            }
            FailureClass::UnsupportedType => {
                self.counters.unsupported_points =
                    self.counters.unsupported_points.saturating_add(1);
                self.insert(devid, field, SampleValue::Null);
                self.warn_once(&format!("unsupported:{tag}"), || {
                    format!("tag '{tag}' 读回类型无解码分支（E-3），跳过（Null）: {msg}")
                });
            }
            FailureClass::BatchTooLarge | FailureClass::Point => {
                self.counters.point_failures = self.counters.point_failures.saturating_add(1);
                self.insert(devid, field, SampleValue::Null);
                tracing::warn!(
                    "[eipClient] inst {} tag '{tag}' read failed: {msg}",
                    self.inst_id
                );
            }
        }
    }

    /// 同一 key 只告警一次（防每 tick 刷屏，E-2/E-3/E-13）。
    fn warn_once(&mut self, key: &str, build: impl FnOnce() -> String) {
        if self.warned.insert(key.to_string()) {
            tracing::warn!("[eipClient] inst {} {}", self.inst_id, build());
        }
    }

    /// 批量读标量 tag（E-1/E-11）：跨 devid 打平后按字节预算切包。
    async fn collect_batch(&mut self, client: &mut EipClient, targets: &Targets) {
        if targets.batch.is_empty() {
            return;
        }
        // tag → [(devid, field)]（A-5：同一物理 tag 可挂多 devid——模型模板×params
        // 展开下两设备引用同一物理 tag 是现实场景，此前单值 map 后插覆盖先插，后者静默无数据）
        let mut by_tag: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
        for (dev, pairs) in &targets.batch {
            for (field, tag) in pairs {
                by_tag
                    .entry(tag.as_str())
                    .or_default()
                    .push((dev.as_str(), field.as_str()));
            }
        }
        let names: Vec<&str> = by_tag.keys().copied().collect();
        for chunk in chunk_tags_by_bytes(&names, self.max_batch_bytes) {
            if self.expired() {
                self.truncated = true;
                return;
            }
            let results = self.read_batch_level(client, &chunk).await;
            for (tag, res) in results {
                let Some(pairs) = by_tag.get(tag.as_str()) else {
                    continue;
                };
                match res {
                    Ok(v) => {
                        let sample = plc_to_sample(&v);
                        for &(dev, field) in pairs {
                            self.insert(dev, field, sample.clone());
                        }
                    }
                    Err(f) => {
                        for &(dev, field) in pairs {
                            self.fail_point(dev, field, &tag, f.class, &f.msg);
                        }
                    }
                }
            }
            if self.transport.is_some() {
                return;
            }
        }
    }

    /// 读一批 tag（E-1/E-11 三段式）：整包 → 包太大降半重试一次 → 单点隔离。
    ///
    /// 每个请求 tag 都必然有结论（库少返回结果时补"未返回"失败），调用方据此写值或 Null。
    async fn read_batch_level(
        &mut self,
        client: &mut EipClient,
        tags: &[&str],
    ) -> HashMap<String, Result<PlcValue, PointFailure>> {
        let mut out: HashMap<String, Result<PlcValue, PointFailure>> = HashMap::new();
        let mut isolate_all = false;
        match client.read_tags_batch(tags).await {
            Ok(results) => {
                for (tag, res) in results {
                    match res {
                        Ok(v) => {
                            out.insert(tag, Ok(v));
                        }
                        Err(e) => {
                            let class = classify_batch_error(&e);
                            if class == FailureClass::Transport {
                                // E-1：组级失败被库降级成点级 NetworkError——必须拆连接，
                                // 否则流被毒化后所有请求在 ensure_stream_usable() 永久失败。
                                self.note_transport(format!("批量读 '{tag}': {e}"));
                                return out;
                            }
                            out.insert(
                                tag,
                                Err(PointFailure {
                                    class,
                                    msg: e.to_string(),
                                    isolate: false,
                                }),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                let class = classify_error(&e);
                if class == FailureClass::Transport {
                    self.note_transport(format!("批量读: {e}"));
                    return out;
                }
                // 整包没发出（如某 tag 路径非法 → 库在构造阶段就 Err）：
                // 逐点兜底隔离坏点，不因一个坏 tag 丢整包（既有 per-tag 容错意图）。
                isolate_all = true;
                for t in tags {
                    out.insert(
                        (*t).to_string(),
                        Err(PointFailure {
                            class,
                            msg: e.to_string(),
                            isolate: true,
                        }),
                    );
                }
            }
        }

        // ② "包太大" 的点降半重试一次（E-11：0x06 Partial Transfer / MSP 打包被拒）
        if !isolate_all {
            let mut too_large: Vec<String> = out
                .iter()
                .filter(|(_, r)| matches!(r, Err(f) if f.class == FailureClass::BatchTooLarge))
                .map(|(t, _)| t.clone())
                .collect();
            if !too_large.is_empty() {
                self.counters.batch_retries = self.counters.batch_retries.saturating_add(1);
                too_large.sort_unstable();
                let mid = too_large.len().div_ceil(2);
                for half in [&too_large[..mid], &too_large[mid..]] {
                    if half.is_empty() {
                        continue;
                    }
                    if self.expired() {
                        self.truncated = true;
                        return out;
                    }
                    let half_refs: Vec<&str> = half.iter().map(String::as_str).collect();
                    match client.read_tags_batch(&half_refs).await {
                        Ok(results) => {
                            for (tag, res) in results {
                                match res {
                                    Ok(v) => {
                                        out.insert(tag, Ok(v));
                                    }
                                    Err(e) => {
                                        let class = classify_batch_error(&e);
                                        if class == FailureClass::Transport {
                                            self.note_transport(format!(
                                                "批量读（降半） '{tag}': {e}"
                                            ));
                                            return out;
                                        }
                                        out.insert(
                                            tag,
                                            Err(PointFailure {
                                                class,
                                                msg: e.to_string(),
                                                isolate: false,
                                            }),
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let class = classify_error(&e);
                            if class == FailureClass::Transport {
                                self.note_transport(format!("批量读（降半）: {e}"));
                                return out;
                            }
                            // 半包仍失败：保留原分类，交给单点兜底
                        }
                    }
                }
            }
        }

        // ③ 单点兜底：库单点路径自带 0x06 重组与 BOOL 数组探测（client.rs:1197-1230）
        let fallback: Vec<String> = out
            .iter()
            .filter(
                |(_, r)| matches!(r, Err(f) if f.class == FailureClass::BatchTooLarge || f.isolate),
            )
            .map(|(t, _)| t.clone())
            .collect();
        for tag in fallback {
            if self.expired() {
                self.truncated = true;
                return out;
            }
            match client.read_tag(&tag).await {
                Ok(v) => {
                    out.insert(tag, Ok(v));
                }
                Err(e) => {
                    let class = classify_error(&e);
                    if class == FailureClass::Transport {
                        self.note_transport(format!("单点读 '{tag}': {e}"));
                        return out;
                    }
                    out.insert(
                        tag,
                        Err(PointFailure {
                            class,
                            msg: e.to_string(),
                            isolate: false,
                        }),
                    );
                }
            }
        }

        // ④ 兜底补全：库少返回结果时不静默丢点
        for t in tags {
            out.entry((*t).to_string()).or_insert_with(|| {
                Err(PointFailure {
                    class: FailureClass::Point,
                    msg: "库未返回该点的结果".to_string(),
                    isolate: false,
                })
            });
        }
        out
    }

    /// STRING 标量（`read_string_tag`：自定义 STRING 类型批量路径解不了）。
    async fn collect_strings(&mut self, client: &mut EipClient, targets: &Targets) {
        for (dev, pairs) in &targets.strings {
            for (field, tag) in pairs {
                if self.expired() {
                    self.truncated = true;
                    return;
                }
                match client.read_string_tag(tag).await {
                    Ok(s) => self.insert(dev, field, SampleValue::Str(s)),
                    Err(e) => {
                        let class = classify_error(&e);
                        let msg = e.to_string();
                        self.fail_point(dev, field, tag, class, &msg);
                        if self.transport.is_some() {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// 位读（A-2/E-5）：必须单点，批量路径只编码父字。
    async fn collect_bits(&mut self, client: &mut EipClient, targets: &Targets) {
        for (dev, pairs) in &targets.bits {
            for (field, tag) in pairs {
                if self.expired() {
                    self.truncated = true;
                    return;
                }
                match client.read_tag(tag).await {
                    Ok(v) => {
                        let sample = plc_to_sample(&v);
                        self.insert(dev, field, sample);
                    }
                    Err(e) => {
                        let class = classify_error(&e);
                        let msg = e.to_string();
                        self.fail_point(dev, field, tag, class, &msg);
                        if self.transport.is_some() {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// 数组元素/范围读（E-13/E-14）：按 base 归并，一次范围读回整段。
    async fn collect_arrays(&mut self, client: &mut EipClient, targets: &Targets) {
        for grp in &targets.arrays {
            if self.expired() {
                self.truncated = true;
                return;
            }
            let span = grp.span();
            // 归并跨度过大（点位分散）→ 退回逐元素，避免一次读几万个元素
            if span.count > route::MAX_RANGE_ELEMS {
                self.warn_once(&format!("span:{}", grp.base), || {
                    format!(
                        "数组 '{}' 点位跨度 {} 超过上限 {}，退回逐元素读（E-13）",
                        grp.base,
                        span.count,
                        route::MAX_RANGE_ELEMS
                    )
                });
                self.collect_array_elements(client, grp).await;
                if self.transport.is_some() {
                    return;
                }
                continue;
            }
            // E-13：base 类型只在首个 tick 探测一次（此后本连接内零探测）
            let cached = self.probes.get(&grp.base).copied();
            let probe = match cached {
                Some(p) => p,
                None => {
                    let p = match client.get_tag_attributes(&grp.base).await {
                        Ok(attrs) => ArrayProbe::Type(attrs.data_type),
                        Err(e) => {
                            let class = classify_error(&e);
                            if class == FailureClass::Transport {
                                self.note_transport(format!("数组探测 '{}': {e}", grp.base));
                                return;
                            }
                            self.warn_once(&format!("probe:{}", grp.base), || {
                                format!(
                                    "数组 '{}' 属性读失败（{e}），退回逐元素读（元素类型未知）",
                                    grp.base
                                )
                            });
                            ArrayProbe::PerElement
                        }
                    };
                    self.probes.insert(grp.base.clone(), p);
                    p
                }
            };
            match probe {
                ArrayProbe::Type(code) => match array_kind(code) {
                    ArrayKind::Scalar => self.collect_array_range(client, grp, &span).await,
                    ArrayKind::AmbiguousDword => {
                        if grp.all_bool {
                            // 声明 bool 的数组元素：库单点读按 BOOL 数组取位（正确语义）
                            self.collect_array_elements(client, grp).await;
                        } else {
                            // E-2：0xD3 兼作 DWORD —— 无法区分元素语义，宁可无值不静默错值
                            self.counters.unsupported_points =
                                self.counters.unsupported_points.saturating_add(
                                    grp.points.iter().map(|p| u64::from(p.count)).sum(),
                                );
                            self.warn_once(&format!("dword-array:{}", grp.base), || {
                                format!(
                                    "数组 '{}' 类型码 0xD3（BOOL 数组 / DWORD 数组同码，台账 E-2）：\
                                     点位未声明 bool，无法区分元素语义 → 整组跳过（Null）；\
                                     DWORD 数组元素需等上游修复",
                                    grp.base
                                )
                            });
                            self.null_group(grp);
                        }
                    }
                    ArrayKind::PerElement => self.collect_array_elements(client, grp).await,
                    ArrayKind::Unsupported => {
                        self.counters.unsupported_points = self
                            .counters
                            .unsupported_points
                            .saturating_add(grp.points.iter().map(|p| u64::from(p.count)).sum());
                        self.warn_once(&format!("unsupported-array:{}", grp.base), || {
                            format!(
                                "数组 '{}' 元素类型 {} 无解码分支（E-3）→ 整组跳过（Null）",
                                grp.base,
                                route::type_code_name(code)
                            )
                        });
                        self.null_group(grp);
                    }
                },
                ArrayProbe::PerElement => self.collect_array_elements(client, grp).await,
            }
            if self.transport.is_some() {
                return;
            }
        }
    }

    /// 整组写 Null（跳过类处置，E-2/E-3）。
    fn null_group(&mut self, grp: &ArrayGroup) {
        for p in &grp.points {
            for field in p.fields() {
                self.insert(&grp.devid, &field, SampleValue::Null);
            }
        }
    }

    /// 范围读一个数组组（E-14：`read_array_range` 一次读 N 元素）。
    async fn collect_array_range(
        &mut self,
        client: &mut EipClient,
        grp: &ArrayGroup,
        span: &ArraySpan,
    ) {
        match client
            .read_array_range(&grp.base, span.start, span.count)
            .await
        {
            Ok(values) => {
                for p in &grp.points {
                    for (i, field) in p.fields().into_iter().enumerate() {
                        let idx = (p.start - span.start) as usize + i;
                        match values.get(idx) {
                            Some(v) => {
                                let sample = plc_to_sample(v);
                                self.insert(&grp.devid, &field, sample);
                            }
                            None => {
                                self.counters.point_failures =
                                    self.counters.point_failures.saturating_add(1);
                                self.insert(&grp.devid, &field, SampleValue::Null);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let class = classify_error(&e);
                if class == FailureClass::Transport {
                    self.note_transport(format!("数组范围读 '{}': {e}", grp.base));
                    return;
                }
                self.counters.point_failures = self
                    .counters
                    .point_failures
                    .saturating_add(grp.points.iter().map(|p| u64::from(p.count)).sum());
                tracing::warn!(
                    "[eipClient] inst {} 数组 '{}' 范围读失败: {e}",
                    self.inst_id,
                    grp.base
                );
                self.null_group(grp);
            }
        }
    }

    /// 逐元素读（BOOL 数组 / 结构数组 / 属性不可读的兜底）。
    async fn collect_array_elements(&mut self, client: &mut EipClient, grp: &ArrayGroup) {
        for p in &grp.points {
            for (i, field) in p.fields().into_iter().enumerate() {
                if self.expired() {
                    self.truncated = true;
                    return;
                }
                let tag = if p.count == 1 {
                    // 单元素：保留配置里的原路径（`Arr[3]`）
                    p.path.clone()
                } else {
                    // 范围点位逐元素兜底：`base[start+i]`
                    match p.path.rsplit_once('[') {
                        Some((base, _)) => format!("{base}[{}]", p.start + i as u32),
                        None => continue,
                    }
                };
                match client.read_tag(&tag).await {
                    Ok(v) => {
                        let sample = plc_to_sample(&v);
                        self.insert(&grp.devid, &field, sample);
                    }
                    Err(e) => {
                        let class = classify_error(&e);
                        let msg = e.to_string();
                        self.fail_point(&grp.devid, &field, &tag, class, &msg);
                        if self.transport.is_some() {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// 投递本轮已采数据并给出结论（E-1/E-17：任何提前退出都不丢已采数据）。
    async fn finish(self, ctx: &HostContext, tx: &SampleSender) -> RoundOutcome {
        let now = chrono::Local::now().timestamp();
        for (devid, values) in self.per_dev {
            if values.is_empty() {
                continue;
            }
            let sample = Sample {
                hwid: ctx.hwid.clone(),
                devid,
                values,
                timestamp: now,
            };
            if tx.send(sample).await.is_err() {
                break; // hub 关闭
            }
        }
        if let Some(msg) = self.transport {
            return RoundOutcome::Transport(msg);
        }
        if self.truncated {
            return RoundOutcome::Truncated;
        }
        RoundOutcome::Ok
    }
}

/// E-10：周期性健康检查——**真实读一次**（`health_tag` 或第一个可读点位）。
///
/// 库的 `check_health_detailed` 只发 24B 全零 NOP 且**不等应答**
/// （<REG>rust-ethernet-ip-1.2.0/src/client.rs:3609-3623），无法证明 PLC 存活。
/// 只有点表全空（没有可读点位）或探针点本身点级失败时才退回它——此时保留其局限：
/// NOP 只能证明 TCP 可写，PLC 侧会话/标签表状态未知。
///
/// 返回 `false` = 连接判定失效（调用方拆连接重建）。
async fn health_check(
    client: &mut EipClient,
    health_tag: Option<&str>,
    targets: &Targets,
    inst_id: &str,
    warned: &mut HashSet<String>,
) -> bool {
    let probe = health_tag
        .map(str::to_string)
        .or_else(|| targets.health_probe.clone());
    if let Some(tag) = probe {
        match client.read_tag(&tag).await {
            Ok(_) => return true,
            Err(e) => match classify_error(&e) {
                FailureClass::Transport => {
                    tracing::warn!("[eipClient] inst {inst_id} 健康探针 '{tag}' 传输层失败: {e}");
                    return false;
                }
                _ => {
                    if warned.insert(format!("health-probe:{tag}")) {
                        tracing::warn!(
                            "[eipClient] inst {inst_id} 健康探针 '{tag}' 点级失败: {e}（连接仍在；\
                             建议配置 app_config.health_tag 指向稳定点位）"
                        );
                    }
                }
            },
        }
    }
    // 无可读点位 / 探针点级失败 → 退回库的 NOP 健康检查（局限见函数注释）
    match client.check_health_detailed().await {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!("[eipClient] inst {inst_id} 健康检查（NOP）报告会话失效，拆连接重建");
            false
        }
        Err(e) => {
            tracing::warn!("[eipClient] inst {inst_id} 健康检查失败: {e}");
            false
        }
    }
}

#[async_trait]
impl Source for EipClientPlugin {
    fn name(&self) -> &'static str {
        "eipClient"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: SampleSender,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        let inst_id = inst.inst_id.clone();
        let cfg = match parse_cfg(&inst.app_config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[eipClient] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // E-15：endpoint 启动时解析一次（主机名 → SocketAddr）。
        let endpoint = match resolve_endpoint(&cfg.endpoint).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("[eipClient] inst {inst_id} endpoint 非法: {e}");
                return;
            }
        };
        // M-2 模型驱动装配：绑定展开（模板 addr × devreg params → 绝对 tag 路径）。
        let expanded = match crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("[eipClient] inst {inst_id} 绑定展开失败（设备未注册？）: {e}");
                return;
            }
        };
        // 点位分流 + 支持表校验（E-2/E-3/E-4/E-5/E-6/E-14/E-16）：非法配置实例启动失败。
        let targets = match parse_targets(&expanded) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("[eipClient] inst {inst_id} 点位配置非法: {e}");
                return;
            }
        };
        // F-1：写目标索引（显示名 → CIP 路径）。
        let write_index = WriteIndex::from_devices(&expanded);
        // F-2：声明本实例支持写回执——REST 写入口据此在窗口内等待 handle_write 的业务
        // 结果（成功 200 / 明确失败 422 / 无回报 504）。装配失败提前返回的实例不声明能力。
        ctx.ctl_registry.declare_write_ack(inst_id.as_str());

        let period = Duration::from_secs(cfg.period);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase = Duration::from_millis(crate::ticker::phase_ms(inst_id.as_str(), period));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // StatTracker（M4 实例指标）+ flush ticker。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut reconnects: u64 = 0;
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst_id.as_str());
        // F-7：tick 超 period 的告警限流状态。
        let mut overrun = OverrunWarn::default();
        let mut counters = RoundCounters::default();
        // 只告警一次的 key（E-2/E-3/E-13）与数组类型缓存（E-13，随连接重建清空）。
        let mut warned: HashSet<String> = HashSet::new();
        let mut array_probes: HashMap<String, ArrayProbe> = HashMap::new();
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        // 断线即 None（对标 opcuaClient session=None）；下个 tick 重连（节流）。
        let mut client: Option<EipClient> = None;
        let mut last_connect_attempt = tokio::time::Instant::now()
            .checked_sub(Duration::from_secs(cfg.reconnect_secs))
            .unwrap_or_else(tokio::time::Instant::now);
        let mut last_health_check = std::time::Instant::now();
        let round_timeout = Duration::from_millis(cfg.round_timeout_ms);

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[eipClient] inst {inst_id} stopping");
                    break;
                }
                msg = cmd_rx.recv() => {
                    // 通道关闭（stop 先 unregister 再 cancel）即退出——此前 None 空过会让
                    // select 立即重 Poll 进入热循环（与 modbus/opcua 的 D-2 修复对齐）。
                    let Some(m) = msg else { break };
                    let result = if m.cmd == "write" {
                        handle_write(&mut client, &m.data, &m.devid, &write_index).await
                    } else {
                        Err(format!("实例不支持命令 '{}'（仅 write）", m.cmd))
                    };
                    if let Err(e) = &result {
                        tracing::warn!(
                            "[eipClient] inst {inst_id} cmd {} devid {} 失败: {e}",
                            m.cmd,
                            m.devid
                        );
                    }
                    // F-2：成败都回流（request_id=None 时 finish_write 是 no-op）。
                    ctx.ctl_registry
                        .finish_write(m.request_id.as_deref(), result);
                }
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                }
                _ = ticker.tick() => {
                    if client.is_none() {
                        let now = tokio::time::Instant::now();
                        if now.duration_since(last_connect_attempt)
                            < Duration::from_secs(cfg.reconnect_secs)
                        {
                            continue; // 重连节流窗口内，跳过本 tick
                        }
                        last_connect_attempt = now;
                        match connect(&cfg, &endpoint).await {
                            Ok(c) => {
                                reconnects += 1;
                                tracker.extra_insert(
                                    "session_reconnect_count",
                                    serde_json::json!(reconnects),
                                );
                                tracing::info!(
                                    "[eipClient] inst {inst_id} connected {endpoint}"
                                );
                                client = Some(c);
                                // E-13：数组类型缓存随连接重建清空（新连接标签表可能变化）
                                array_probes.clear();
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[eipClient] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() {
                                    tracker.flush().await;
                                }
                                continue;
                            }
                        }
                    }
                    let Some(c) = client.as_mut() else {
                        continue;
                    };
                    let started = std::time::Instant::now();

                    // E-10：健康检查（真实读一次；失败即判定连接失效）
                    let mut healthy = true;
                    if cfg.health_check_secs > 0
                        && last_health_check.elapsed()
                            >= Duration::from_secs(cfg.health_check_secs)
                    {
                        last_health_check = std::time::Instant::now();
                        healthy = health_check(
                            c,
                            cfg.health_tag.as_deref(),
                            &targets,
                            inst_id.as_str(),
                            &mut warned,
                        )
                        .await;
                    }

                    let outcome = if healthy {
                        // E-17：整轮预算（按步骤边界检查，已采数据在 finish 里照常投递）
                        let deadline = tokio::time::Instant::now() + round_timeout;
                        let mut round = Round {
                            inst_id: inst_id.as_str(),
                            deadline,
                            max_batch_bytes: cfg.max_batch_bytes,
                            per_dev: BTreeMap::new(),
                            transport: None,
                            truncated: false,
                            warned: &mut warned,
                            counters: &mut counters,
                            probes: &mut array_probes,
                        };
                        round.collect_batch(c, &targets).await;
                        if round.transport.is_none() {
                            round.collect_strings(c, &targets).await;
                        }
                        if round.transport.is_none() {
                            round.collect_bits(c, &targets).await;
                        }
                        if round.transport.is_none() {
                            round.collect_arrays(c, &targets).await;
                        }
                        round.finish(&ctx, &tx).await
                    } else {
                        RoundOutcome::Transport("健康检查判定连接失效".to_string())
                    };

                    // F-7：一轮超过 period（MissedTickBehavior::Skip 会静默丢拍）限流告警
                    let elapsed = started.elapsed();
                    if elapsed >= period {
                        if overrun.record(std::time::Instant::now()) {
                            tracing::warn!(
                                "[eipClient] inst {inst_id} 本轮采集 {elapsed:?} ≥ period {period:?}：\
                                 MissedTickBehavior::Skip 会静默丢拍，实际频率低于配置（累计 {} 次）",
                                overrun.count
                            );
                        }
                        tracker.extra_insert(
                            "tick_overrun_count",
                            serde_json::json!(overrun.count),
                        );
                    }

                    match outcome {
                        RoundOutcome::Ok => tracker.tick_ok(elapsed.as_millis() as u64),
                        RoundOutcome::Truncated => {
                            counters.truncated_rounds =
                                counters.truncated_rounds.saturating_add(1);
                            tracing::warn!(
                                "[eipClient] inst {inst_id} 本轮超出 {round_timeout:?} 预算，\
                                 已截断（已采数据已投递，E-17）"
                            );
                            tracker.tick_ok(elapsed.as_millis() as u64);
                        }
                        RoundOutcome::Transport(msg) => {
                            // 传输级失败（连接断/超时/封装状态码）：拆 session 下 tick 重连。
                            // 点级失败在 collect 内 per-tag 容错，不会走到这里。
                            tracker.tick_fail(&format!("transport failure: {msg}"));
                            client = None;
                            array_probes.clear();
                            tracing::warn!(
                                "[eipClient] inst {inst_id} 传输层失败（{msg}），拆连接重建"
                            );
                        }
                    }
                    counters.publish(&mut tracker);
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }

        // 优雅注销 session（best-effort，失败仅 warn）。
        if let Some(mut c) = client {
            if let Err(e) = c.unregister_session().await {
                tracing::warn!("[eipClient] inst {inst_id} unregister failed: {e}");
            }
        }
        tracing::info!("[eipClient] inst {inst_id} exited");
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// 处理 write 命令：`data` = JSON `{"tag":"点位显示名","value":123,"plc_type":"dint"}`。
///
/// - **F-1**：`tag` 先按点位显示名查索引（[`WriteIndex`]），再按已配置 CIP 路径原样用，
///   都不是则拒写（不得拿显示名当路径写）；
/// - **F-2**：返回业务结果，调用方经 `finish_write` 回流 REST；
/// - **E-6**：STRING 数组元素拒写（库缺结构 handle，等上游修复）；
/// - **E-7**：写值全量 checked（越界/负数写无符号/非数字串拒写）；
/// - **E-9**（记录不修）：位写 / BOOL 数组元素写在库内是读-改-写，非原子。
async fn handle_write(
    client: &mut Option<EipClient>,
    data: &str,
    devid: &str,
    index: &WriteIndex,
) -> Result<(), String> {
    let Some(c) = client.as_mut() else {
        return Err("实例未连接 PLC，写命令丢弃".to_string());
    };
    let req: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("写命令 JSON 非法: {e}"))?;
    let tag = req
        .get("tag")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "写命令缺少 tag（HMI 发点位显示名）".to_string())?;
    let json_value = req
        .get("value")
        .ok_or_else(|| "写命令缺少 value".to_string())?;
    let value = json_to_sample_value(json_value);
    if matches!(value, SampleValue::Null) {
        return Err("写命令 value 为空（null）".to_string());
    }
    // F-1：显示名 → CIP 路径
    let path = index.resolve(devid, tag)?;
    let plc_type = req.get("plc_type").and_then(|v| v.as_str());
    let wants_string = matches!(
        plc_type.map(str::trim).filter(|s| !s.is_empty()),
        Some(t) if t.eq_ignore_ascii_case("string")
    ) || (plc_type.is_none() && matches!(value, SampleValue::Str(_)));

    // E-6：STRING 数组元素写会命中库的数组元素 workaround（缺 2B 结构 handle）→ 拒写。
    if wants_string && route::hits_array_element_workaround(&path) {
        return Err(format!(
            "拒写 '{path}'：STRING 数组元素写走库的数组 workaround（`to_bytes()` 无结构 handle）\
             且无公开正确路径（台账 E-6，等上游修复）；标量 STRING 与 `Arr[0].Member` 不受影响"
        ));
    }

    // A-1（2026-11-01 审查）：`plc_type=string` + 非 Str 值此前走 `unreachable!`
    // 直接 panic 杀实例——改为可读串化后正常写 STRING tag；未知 plc_type 拒写（A-6）。
    let result = if wants_string {
        c.write_string_tag(&path, &sample_to_string_repr(&value))
            .await
    } else if route::needs_single_read(&path) {
        // 位路径（`Tag.5`）：库只接受 `PlcValue::Bool`（E-4；位写是读-改-写，E-9 非原子）。
        // HMI 推按钮发 `1`/`0` 也走这条 conversion（checked，非法串/缺值仍拒写）。
        match value::to_bool(&value) {
            Ok(b) => c.write_tag(&path, PlcValue::Bool(b)).await,
            Err(e) => return Err(format!("写 '{path}' 拒写：{e}")),
        }
    } else {
        // E-7：转换失败（越界/非法串）即拒写，不回绕不静默写 0
        let pv = plc_with_type(&value, plc_type)?;
        c.write_tag(&path, pv).await
    };
    match result {
        Ok(()) => {
            tracing::debug!("[eipClient] write '{path}' ok（devid {devid}, tag '{tag}'）");
            Ok(())
        }
        Err(e) => {
            if classify_error(&e) == FailureClass::Transport {
                // 传输级失败（含 E-12 封装状态码）：拆 session，下次 tick 重连。
                *client = None;
            }
            Err(format!("写 '{path}' 失败: {e}"))
        }
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "eipClient",
        create: || Box::new(EipClientPlugin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChildSpec;

    fn child(label: &str, tag: &str, ty: &str) -> ChildSpec {
        ChildSpec {
            label: label.into(),
            value: tag.into(),
            r#type: ty.into(),
            props: None,
        }
    }

    /// E-5：位读必须单点，`Program:`/UDT 成员/单元素数组可进批量。
    #[test]
    fn parse_targets_routes_by_path_shape() {
        let devices = vec![DeviceSpec {
            value: "d1".into(),
            children: vec![
                child("bit5", "MyDint.5", "dint"),
                child("plain", "PlainTag", ""),
                child("s", "MyString", "string"),
                child("m", "Udt.Member", ""),
                child("prog", "Program:MainProgram.Tag", ""),
                child("arr", "Arr[3]", ""),
            ],
            ..Default::default()
        }];
        let t = parse_targets(&devices).expect("合法点表");
        assert_eq!(
            t.batch,
            vec![(
                "d1".into(),
                vec![
                    ("plain".into(), "PlainTag".into()),
                    ("m".into(), "Udt.Member".into()),
                    ("prog".into(), "Program:MainProgram.Tag".into()),
                ]
            )],
            "无点标量 / UDT 成员 / Program 作用域都进批量（E-5）"
        );
        assert_eq!(
            t.strings,
            vec![("d1".into(), vec![("s".into(), "MyString".into())])],
            "string 类型进专用路径"
        );
        assert_eq!(
            t.bits,
            vec![("d1".into(), vec![("bit5".into(), "MyDint.5".into())])],
            "位读单点（E-5）"
        );
        // Arr[3] 归数组组（E-13/E-14），不再是 per_tag
        assert_eq!(t.arrays.len(), 1);
        assert_eq!(t.arrays[0].base, "Arr");
        assert_eq!(t.arrays[0].points.len(), 1);
        assert_eq!(t.arrays[0].points[0].start, 3);
        assert_eq!(t.arrays[0].points[0].count, 1);
        assert_eq!(
            t.health_probe.as_deref(),
            Some("PlainTag"),
            "E-10 探针取首个可读点"
        );
    }

    /// E-14：`[a:b]` 范围点位展开成 label[i]，同 base 归并成一个组。
    #[test]
    fn parse_targets_groups_array_ranges() {
        let devices = vec![DeviceSpec {
            value: "d1".into(),
            children: vec![
                child("wave", "Wave[0:4]", "real"),
                child("wave9", "Wave[9]", "real"),
                child("other", "Other[2]", "dint"),
            ],
            ..Default::default()
        }];
        let t = parse_targets(&devices).expect("合法点表");
        assert_eq!(t.arrays.len(), 2, "两个不同 base → 两个组");
        let wave = t.arrays.iter().find(|g| g.base == "Wave").expect("Wave 组");
        assert_eq!(wave.points.len(), 2);
        assert_eq!(wave.points[0].fields().len(), 5, "范围 [0:4] 展开 5 个 key");
        assert_eq!(wave.points[0].fields()[0], "wave[0]");
        assert_eq!(wave.points[0].fields()[4], "wave[4]");
        // 归并跨度覆盖全部点位：0..=9 → start 0 / count 10（一次范围读，E-13/E-14）
        let span = wave.span();
        assert_eq!((span.start, span.count), (0, 10));
    }

    /// E-2/E-3/E-4/E-6/E-16：非法点位配置期即 Err（实例启动失败，不再带病采集）。
    #[test]
    fn parse_targets_rejects_unsupported_points() {
        let cases = [
            (child("w", "WordTag", "word"), "E-3"),
            (child("dw", "DwArr[0]", "dword"), "E-2"),
            (child("b", "Word.40", "dint"), "E-4"),
            (child("i", "AlarmWord.3", "int"), "E-4"),
            (child("s", "StrArr[0]", "string"), "E-6"),
            (child("io", "Local:1:I.Data.0", ""), "E-16"),
        ];
        for (c, tag_id) in cases {
            let devices = vec![DeviceSpec {
                value: "d1".into(),
                children: vec![c.clone()],
                ..Default::default()
            }];
            let err = parse_targets(&devices).expect_err("必须拒绝");
            assert!(
                err.contains(tag_id),
                "点位 '{}' 的错误需指向 {tag_id}: {err}",
                c.value
            );
        }
    }

    #[test]
    fn parse_cfg_defaults_and_validation() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"192.168.1.10:44818"})).unwrap();
        assert_eq!(cfg.endpoint, "192.168.1.10:44818");
        assert_eq!(cfg.period, 3);
        assert!(cfg.slot.is_none());
        assert_eq!(cfg.reconnect_secs, 5);
        assert_eq!(cfg.health_check_secs, 0);
        assert!(cfg.health_tag.is_none());
        assert!(cfg.max_packet_size.is_none());
        assert!(cfg.batch_max_operations.is_none());
        // E-11/E-19：批量字节预算默认 400；E-17：整轮预算默认 period*1000
        assert_eq!(cfg.max_batch_bytes, DEFAULT_MAX_BATCH_BYTES);
        assert_eq!(cfg.round_timeout_ms, 3000);

        let full = parse_cfg(&serde_json::json!({
            "endpoint": "plc:44818", "period": 0, "slot": 3,
            "reconnect_secs": 10, "health_check_secs": 60, "health_tag": "Heartbeat",
            "max_packet_size": 4000, "max_batch_bytes": 900,
            "round_timeout_ms": 0, "batch_max_operations": 20,
            "batch_max_packet_size": 504, "batch_packet_timeout_ms": 2000,
            "batch_continue_on_error": true, "batch_optimize_packing": false
        }))
        .unwrap();
        assert_eq!(full.period, 1, "period 0 clamped to 1");
        assert_eq!(full.slot, Some(3));
        assert_eq!(full.reconnect_secs, 10);
        assert_eq!(full.health_check_secs, 60);
        assert_eq!(full.health_tag.as_deref(), Some("Heartbeat"));
        assert_eq!(full.max_packet_size, Some(4000));
        assert_eq!(full.max_batch_bytes, 900);
        assert_eq!(full.round_timeout_ms, MIN_ROUND_TIMEOUT_MS, "0 → 下限 1000");
        assert_eq!(full.batch_max_operations, Some(20));
        assert_eq!(full.batch_max_packet_size, Some(504));
        assert_eq!(full.batch_packet_timeout_ms, Some(2000));
        assert_eq!(full.batch_continue_on_error, Some(true));
        assert_eq!(full.batch_optimize_packing, Some(false));

        assert!(
            parse_cfg(&serde_json::json!({})).is_err(),
            "missing endpoint errors"
        );
    }

    /// E-15：endpoint 形态判定（纯函数；DNS 在 `resolve_endpoint` 里做）。
    #[test]
    fn endpoint_shape_rules() {
        assert_eq!(
            endpoint_shape("192.168.1.10:44818").unwrap(),
            EndpointShape::Addr("192.168.1.10:44818".into())
        );
        assert_eq!(
            endpoint_shape("[::1]:44818").unwrap(),
            EndpointShape::Addr("[::1]:44818".into())
        );
        assert_eq!(
            endpoint_shape("plc.local:1234").unwrap(),
            EndpointShape::Lookup {
                host: "plc.local".into(),
                port: 1234
            }
        );
        assert_eq!(
            endpoint_shape("plc.local").unwrap(),
            EndpointShape::Lookup {
                host: "plc.local".into(),
                port: DEFAULT_EIP_PORT
            },
            "纯主机名补默认端口 44818（E-15）"
        );
        assert!(endpoint_shape("  ").is_err(), "空 endpoint 必须 Err");
        assert!(endpoint_shape(":44818").is_err(), "空主机名必须 Err");
        assert!(
            endpoint_shape("plc.local:http").is_err(),
            "非数字端口必须 Err"
        );
    }

    /// E-13：同 base 多点位归并成一个范围（一次范围读回整段）。
    #[test]
    fn array_group_span_covers_all_points() {
        let devices = vec![DeviceSpec {
            value: "d1".into(),
            children: vec![child("a", "Arr[2]", "dint"), child("b", "Arr[7]", "dint")],
            ..Default::default()
        }];
        let t = parse_targets(&devices).expect("合法点表");
        let span = t.arrays[0].span();
        assert_eq!((span.start, span.count), (2, 6), "2..=7 一次读回（E-13）");
        assert!(!t.arrays[0].all_bool);
    }

    /// F-7：超时告警限流（首拍/窗口到期才告警，计数始终累加）。
    #[test]
    fn overrun_warn_is_rate_limited() {
        let mut o = OverrunWarn::default();
        let t0 = std::time::Instant::now();
        assert!(o.record(t0), "首次必须告警");
        assert!(!o.record(t0), "窗口内不重复告警");
        assert_eq!(o.count, 2, "计数不受限流影响");
        assert!(o.record(t0 + OVERRUN_WARN_INTERVAL), "窗口到期后再次告警");
    }
}
