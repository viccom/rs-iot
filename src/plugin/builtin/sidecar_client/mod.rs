//! `sidecarClient`：外部插件 bridge（进程外南向接入，任意语言）。
//!
//! 外部进程（Go/Python/C/...）作为南向采集插件，经 TCP 接入 rs-iot：
//! - acceptor 监听单端口（`config.bridge`），accept 后读 Hello 帧 → `ext_token` 校验 →
//!   按 `inst_id` 把裸 `TcpStream` 分发到对应 sidecarClient task（每外部实例一个 task）。
//! - task 收 stream 后 `tokio::io::split` + `FramedRead`/`FramedWrite`（长度前缀帧），
//!   发 HelloAck + ConfigPush，进入数据/命令/心跳循环。
//! - 数据上行 `DataUp` → `Sample` → hub → pipeline → rtdata（与内置 Source 同构）。
//! - 写命令 `cmd_rx` → `Command` 帧 → 外部 → `CommandAck`。
//! - 心跳超时 / 连接断 → 失联，回等新连接（支持外部重连）；失联期间堆积写命令在重连前丢弃。
//!
//! 外部插件=纯运行时：配置/数据/状态全在 rs-iot，外部仅本地缓存数据待补传（断连容错）。
//! lux 不对外暴露（方案 C）：外部只连 bridge，认证/授权集中在 rs-iot。
//!
//! 协议帧：`[4B BE length][1B type][JSON body]`（`LengthDelimitedCodec` length 前缀）。
//! 详见 `docs/reference/external-plugin-protocol.md`。

use crate::db::ext_token;
use crate::host::{BridgeConn, BridgeRegistry, HostContext};
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, HostKind, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

// ---- 帧类型字节 ----
const T_HELLO: u8 = 1;
const T_HELLO_ACK: u8 = 2;
const T_CONFIG_PUSH: u8 = 3;
const T_DATA_UP: u8 = 4;
const T_COMMAND: u8 = 5;
const T_COMMAND_ACK: u8 = 6;
const T_HEARTBEAT: u8 = 7;

/// 宿主支持的协议版本集合（**集合匹配，非精确匹配**）。
///
/// - v2：`devices[{devid, model, tags[]}]`（M-3 模型点表）。
/// - v3：v2 + 顶层 `app_config`（连接参数）+ `devices[].bind`（设备级协议参数）。
///
/// v3 只增字段，v2 客户端忽略未知字段即可兼容——故两者并存，旧客户端不被拒。
/// 新增版本时只往本表追加，**不移除**（移除即拒连存量外部进程）。
const SUPPORTED_VERSIONS: &[&str] = &["2", "3"];
/// 当前版本（新建示例/文档以此为准；仅用于提示文案与测试）。
const CURRENT_VERSION: &str = "3";

/// `Hello.ver` 缺省值（v2 时代客户端可能不发 ver——按 v2 处理，与协议文档一致）。
const DEFAULT_VER: &str = "2";
/// 单帧最大字节数（codec 上限，容纳大配置 push）。
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// task 侧心跳超时缺省（`app_config.heartbeat_timeout_secs` 可覆盖）。
/// 引用 config 单源常量（审计 B6：与 bridge 服务端默认同一来源，防两端漂移）。
const HB_TIMEOUT_DEFAULT_SECS: u64 = crate::config::DEFAULT_HEARTBEAT_TIMEOUT_SECS;

/// 不下发给外部插件的 `app_config` 键（宿主内部语义）。
///
/// - `secrets`：脚本侧 `ctx:secret_get` 授权清单，宿主内部概念；
/// - `heartbeat_timeout_secs`：已通过 `HelloAck.heartbeat_secs` 下发，重复无意义。
///
/// **排除清单而非白名单**：`app_config` 是"任意 JSON，由各插件自行解析"，宿主不该
/// 理解每个协议的键（OPC DA 的 `prog_id` vs Modbus 的 `serial_port`）。排除清单是
/// 封闭可审计的；白名单开放且必然遗漏。
const APP_CONFIG_HOST_KEYS: &[&str] = &["secrets", "heartbeat_timeout_secs"];

fn default_ver() -> String {
    DEFAULT_VER.into()
}

// ---- 外部 → bridge 帧载荷 ----

#[derive(Debug, Deserialize)]
struct Hello {
    inst_id: String,
    token: String,
    #[serde(default = "default_ver")]
    ver: String,
}

#[derive(Debug, Deserialize)]
struct DataUpSample {
    tag: String,
    value: serde_json::Value,
    ts: i64,
}

#[derive(Debug, Deserialize)]
struct DataUp {
    devid: String,
    samples: Vec<DataUpSample>,
}

#[derive(Debug, Deserialize)]
struct CommandAck {
    #[allow(dead_code)]
    cmd_id: u64,
    ok: bool,
    #[serde(default)]
    err: Option<String>,
}

/// 解析后的接收帧（bridge 侧）。
enum Recv {
    Hello(Hello),
    DataUp(DataUp),
    CommandAck(CommandAck),
    Heartbeat,
}

/// 解析 wire payload（`[type][json]`）为接收帧。
fn decode(payload: &[u8]) -> Result<Recv, String> {
    if payload.is_empty() {
        return Err("empty frame".into());
    }
    let typ = payload[0];
    let body = &payload[1..];
    let s = std::str::from_utf8(body).map_err(|e| format!("utf8: {e}"))?;
    match typ {
        T_HELLO => Ok(Recv::Hello(
            serde_json::from_str(s).map_err(|e| format!("hello: {e}"))?,
        )),
        T_DATA_UP => Ok(Recv::DataUp(
            serde_json::from_str(s).map_err(|e| format!("dataup: {e}"))?,
        )),
        T_COMMAND_ACK => Ok(Recv::CommandAck(
            serde_json::from_str(s).map_err(|e| format!("cmdack: {e}"))?,
        )),
        T_HEARTBEAT => Ok(Recv::Heartbeat),
        other => Err(format!("unsupported frame type {other}")),
    }
}

/// 组 wire payload（`[type][json]`，无 length 前缀；length 由 codec 或 `write_one_frame` 加）。
fn encode_payload(typ: u8, body: &impl Serialize) -> Vec<u8> {
    let mut v = vec![typ];
    let _ = serde_json::to_writer(&mut v, body);
    v
}

/// JSON 值 → SampleValue（与 modbus/opcua 对齐）。
fn json_to_sample_value(v: &serde_json::Value) -> SampleValue {
    match v {
        serde_json::Value::Bool(b) => SampleValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SampleValue::Int(i)
            } else {
                SampleValue::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => SampleValue::Str(s.clone()),
        other => SampleValue::Str(other.to_string()),
    }
}

/// 读一帧（`[4B BE len][payload]`），握手用（acceptor）。
///
/// **契约**：此处手写的 length-prefix 必须与 `make_codec` 的 `LengthDelimitedCodec`
/// 默认（4 字节大端 u32 + max_frame_length）保持一致——改 codec 配置要同步本函数，
/// 否则 acceptor 解析的 Hello 帧与 task 端 codec 不匹配、外部全连不上。
/// 单次 `timeout(dur)` 包整个读（len + body），总超时 = dur（非两段独立 2×dur）。
async fn read_one_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
    dur: Duration,
) -> Option<Vec<u8>> {
    match timeout(dur, async {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf).await.ok()?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > max {
            return None;
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await.ok()?;
        Some(buf)
    })
    .await
    {
        Ok(Some(buf)) => Some(buf),
        _ => None,
    }
}

/// 写一帧（`[4B BE len][payload]`），握手应答用（acceptor）。
async fn write_one_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> bool {
    let len = (payload.len() as u32).to_be_bytes();
    w.write_all(&len).await.is_ok() && w.write_all(payload).await.is_ok()
}

fn make_codec() -> LengthDelimitedCodec {
    let mut b = LengthDelimitedCodec::builder();
    b.max_frame_length(MAX_FRAME);
    b.new_codec()
}

/// 从 `app_config` 读心跳超时覆盖（`heartbeat_timeout_secs`）。
fn parse_hb_timeout(app: &serde_json::Value, default: Duration) -> Duration {
    app.get("heartbeat_timeout_secs")
        .and_then(|v| v.as_u64())
        .map_or(default, Duration::from_secs)
}

/// 设备点表 → ConfigPush `devices[]` 条目（v2/v3 共用；纯函数）。
///
/// `tags[].addr` 是装配期 `addr::resolve` 后的绝对地址；纯软 tag（模型 addr=None）
/// 回退为 tag 名——与 `ExpandedDevice::to_device_spec` 同口径。
fn push_devices(
    expanded: &[crate::model::expand::ExpandedDevice],
    ver: &str,
) -> Vec<serde_json::Value> {
    let with_bind = ver == "3";
    expanded
        .iter()
        .map(|ex| {
            let tags: Vec<serde_json::Value> = ex
                .model
                .tags
                .iter()
                .map(|t| {
                    let addr = match &t.addr {
                        Some(a) => crate::model::addr::resolve(a, &ex.entry.params)
                            .unwrap_or_else(|_| a.clone()),
                        None => t.name.clone(), // 纯软 tag：addr=tag 名
                    };
                    serde_json::json!({
                        "name": t.name,
                        "type": t.data_type,
                        "addr": addr,
                        "unit": t.unit,
                        "rw": if t.rw == crate::model::types::TagRw::R { "r" } else { "rw" },
                    })
                })
                .collect();
            let mut dev = serde_json::json!({
                "devid": ex.devid,
                "model": ex.model.model_id,
                "tags": tags,
            });
            // v3：设备级协议参数（OPC DA group / 更新速率、Modbus unit id 等）原样下发。
            // 空 bind 也发（{} 而非缺键）——外部按存在性判断能力，形态稳定优于省略。
            if with_bind {
                if let Some(obj) = dev.as_object_mut() {
                    obj.insert("bind".into(), serde_json::json!(ex.bind));
                }
            }
            dev
        })
        .collect()
}

/// `app_config` → 下发给外部插件的视图（v3；纯函数）。
///
/// 排除 [`APP_CONFIG_HOST_KEYS`]（宿主内部键），其余原样。非对象（null/标量）→ `{}`，
/// 降低外部解析分支。**不脱敏**：password 在调用方已解析成明文或字面值——
/// bridge 通道承载明文凭证是既定取舍（同机默认 bind_host=127.0.0.1；跨机须 TLS）。
fn push_app_config(app: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = app.as_object() else {
        return serde_json::json!({});
    };
    let mut out = serde_json::Map::with_capacity(obj.len());
    for (k, v) in obj {
        if APP_CONFIG_HOST_KEYS.contains(&k.as_str()) {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    serde_json::Value::Object(out)
}

/// 组装 ConfigPush 载荷（纯函数；v2/v3 分支的唯一实现点）。
///
/// - v2：`{inst_id, v, devices[]}`——与 M-3 形态逐字节一致（存量外部进程零影响）。
/// - v3：v2 + `app_config`（已解析凭证）+ `devices[].bind`。
///
/// `resolved_app_config` 由调用方传入（已做过 `secret:` 解析），本函数不做 IO。
fn build_config_push(
    inst_id: &str,
    ver: &str,
    expanded: &[crate::model::expand::ExpandedDevice],
    resolved_app_config: Option<&serde_json::Value>,
) -> serde_json::Value {
    let devices = push_devices(expanded, ver);
    let v: u8 = ver.parse().unwrap_or(2);
    let mut out = serde_json::json!({
        "inst_id": inst_id,
        "v": v,
        "devices": devices,
    });
    if ver == "3" {
        if let (Some(obj), Some(app)) = (out.as_object_mut(), resolved_app_config) {
            obj.insert("app_config".into(), push_app_config(app));
        }
    }
    out
}

/// 组装 v3 下发的 `app_config`：解析顶层 `password` 的 `secret:` 引用（IO）。
///
/// 只处理**顶层 `password` 键**（与 north 的 `NorthCfg.password` 同口径，也是
/// `redact_inst` 唯一脱敏的键）。其余键原样——嵌套对象里的凭证语义由协议自定，
/// 宿主不猜。
///
/// 失败处理（与 north 一致，不静默降级）：
/// - ref 非法 / lux 读失败 → warn + 置 `null`（外部收到"无凭证"，连接失败可感知）；
/// - 在清单但未写入 → `null`（配置了没写值，等同缺失）。
///
/// **不因凭证缺失而拒连**：外部插件可能需要重试或降级为匿名，由它自己决定；
/// 宿主只保证"下发的就是真实可用状态"，不替它做连接决策。
async fn resolve_app_config_for_push(
    app: &serde_json::Value,
    lux: &lux::EmbeddedClient,
) -> serde_json::Value {
    let Some(obj) = app.as_object() else {
        return serde_json::json!({});
    };
    let mut out = obj.clone();
    let pw = out
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    if let Some(pw) = pw {
        match crate::db::secrets::resolve_ref(Some(&pw), lux).await {
            Ok(Some(plain)) => {
                out.insert("password".into(), serde_json::json!(plain));
            }
            Ok(None) => {
                tracing::warn!(
                    "[sidecarClient] app_config.password 引用的 secret 未写入——下发 null（外部按无凭证处理）"
                );
                out.insert("password".into(), serde_json::Value::Null);
            }
            Err(e) => {
                tracing::warn!("[sidecarClient] app_config.password 解析失败：{e}——下发 null");
                out.insert("password".into(), serde_json::Value::Null);
            }
        }
    }
    serde_json::Value::Object(out)
}

/// 外部插件 bridge 采集插件（无状态工厂标记，每实例 task 内持有自己的连接）。
#[derive(Debug, Default)]
pub struct SidecarClient;

#[async_trait]
impl Source for SidecarClient {
    fn name(&self) -> &'static str {
        "sidecarClient"
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

        // 准入校验：必须是 External（防御误配）。
        if inst.host_kind != HostKind::External {
            tracing::warn!("[sidecarClient] inst {inst_id} host_kind != External, refuse to run");
            return;
        }

        let hb_timeout = parse_hb_timeout(
            &inst.app_config,
            Duration::from_secs(HB_TIMEOUT_DEFAULT_SECS),
        );
        let hb_check = hb_timeout / 2;

        // 连接 slot：acceptor 分发来的连接（stream + Hello 版本）经此进入 task。容量 1
        // （外部一次一个连接；重连时若 task 未取走上一个，dispatch 报 Full，acceptor
        // 回 ack 让外部稍后重试）。
        let (slot_tx, mut slot_rx) = mpsc::channel::<BridgeConn>(1);
        ctx.bridge_registry.register(&inst_id, slot_tx);

        // observed StatTracker（M4）：外部进程不写 lux，bridge 观察派生指标——
        // 收 DataUp → tick_ok（duration=0，observed 无耗时概念）、连接/心跳 → extra、
        // 断联 → degraded。语义是「rs-iot 观察到的外部活跃度」，非外部自报。
        let mut tracker =
            crate::plugin::stat_tracker::StatTracker::new(inst_id.clone(), ctx.lux.clone());
        let mut reconnects: u64 = 0;

        let mut next_cmd_id: u64 = 1;
        // M-3 DataUp 校验闸拒绝计数（flush 落 stat `gate_rejected`）。
        let gate_rejected: std::sync::Arc<std::sync::atomic::AtomicU64> = Default::default();

        'outer: loop {
            let (stream, peer_ver) = tokio::select! {
                _ = token.cancelled() => break 'outer,
                s = slot_rx.recv() => match s {
                    Some(c) => (c.stream, c.ver),
                    None => break 'outer,
                },
            };

            // 连接建立：记 reconnect（含首次）+ connected=true，status=running。
            // observed 语义——此处是「bridge 拿到外部连接」的事件。
            reconnects += 1;
            tracker.extra_insert("reconnect_count", serde_json::json!(reconnects));
            tracker.extra_insert("connected", serde_json::json!(true));
            tracker.set_status("running");

            // 丢弃失联期间堆积的旧写命令（工业写命令"最新值生效"语义，不补发旧值）。
            while let Ok(stale) = cmd_rx.try_recv() {
                tracing::warn!(
                    "[sidecarClient] inst {inst_id} drop stale cmd during disconnect: {}",
                    stale.cmd
                );
            }

            // stream → FramedRead（读）/ FramedWrite（写）分离，避免 select! 内 borrow 冲突。
            let (read, write) = tokio::io::split(stream);
            let mut reader = FramedRead::new(read, make_codec());
            let mut writer = FramedWrite::new(write, make_codec());

            // 握手：HelloAck + ConfigPush。
            let ack = serde_json::json!({
                "ok": true,
                "msg": "",
                "heartbeat_secs": hb_timeout.as_secs(),
            });
            if writer
                .send(Bytes::from(encode_payload(T_HELLO_ACK, &ack)))
                .await
                .is_err()
            {
                continue;
            }
            // M-3 模型驱动装配：绑定展开 → 完整产物（devreg+模型）存会话级快照——
            // ConfigPush 载荷组装 + DataUp 校验闸共用。展开失败拒连。
            let expanded = match crate::model::expand::expand_devices(&ctx.lux, &inst.devices).await
            {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(
                        "[sidecarClient] inst {inst_id} 绑定展开失败（设备未注册？）: {e}"
                    );
                    continue;
                }
            };
            // v3 才需要解析凭证（v2 不下发 app_config——不必为旧客户端读 lux secrets）。
            let resolved_app = if peer_ver == "3" {
                Some(resolve_app_config_for_push(&inst.app_config, &ctx.lux).await)
            } else {
                None
            };
            let cfg = build_config_push(&inst_id, &peer_ver, &expanded, resolved_app.as_ref());
            let expanded = std::sync::Arc::new(expanded);
            if writer
                .send(Bytes::from(encode_payload(T_CONFIG_PUSH, &cfg)))
                .await
                .is_err()
            {
                continue;
            }
            tracing::info!(
                "[sidecarClient] inst {inst_id} external connected (proto v{peer_ver}), config pushed"
            );

            let mut last_hb = Instant::now();
            // 心跳超时检测 ticker(非采集,不接 jitter——jitter 会改变超时判定语义)。
            let mut ticker = tokio::time::interval(hb_check);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // observed stat 周期 flush ticker（与采集 ticker 解耦；错峰 + 条件落盘，
            // 见 stat_tracker::flush_ticker / flush_if_due）。
            let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(&inst_id);
            tracker.extra_insert(
                "gate_rejected",
                serde_json::json!(gate_rejected.load(std::sync::atomic::Ordering::Relaxed)),
            );
            tracker.flush().await;

            loop {
                tokio::select! {
                    _ = token.cancelled() => break 'outer,
                    m = cmd_rx.recv() => {
                        let Some(m) = m else { break 'outer };
                        if m.cmd == "write" {
                            let cmd_id = next_cmd_id;
                            next_cmd_id += 1;
                            let payload = serde_json::json!({
                                "cmd_id": cmd_id,
                                "devid": m.devid,
                                "cmd": m.cmd,
                                "data": m.data,
                            });
                            if writer.send(Bytes::from(encode_payload(T_COMMAND, &payload))).await.is_err() {
                                tracing::warn!("[sidecarClient] inst {inst_id} command send failed, disconnect");
                                break;
                            }
                        } else {
                            tracing::info!("[sidecarClient] inst {inst_id} unknown cmd: {}", m.cmd);
                        }
                    }
                    res = reader.next() => {
                        match res {
                            Some(Ok(bytes)) => match decode(&bytes) {
                                Ok(Recv::DataUp(d)) => {
                                    last_hb = Instant::now();
                                    // M-3 校验闸：devid ∈ 绑定展开 ∧ tag ∈ 模型点表
                                    //（非空模型时；空 tags 模型=迁移空点表兼容放行）。
                                    // 未建模 sample 拒收（warn+计数），合法保留。
                                    let Some(dev_snap) = expanded.iter().find(|e| e.devid == d.devid)
                                    else {
                                        gate_rejected
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        tracing::warn!(
                                            "[sidecarClient] inst {inst_id} DataUp 设备 {} 未绑定，整帧拒收",
                                            d.devid
                                        );
                                        // D-M4（审查修复）：闸拒 tag 登记发现队列供 adopt。
                                        let mut rej = BTreeMap::new();
                                        for samp in &d.samples {
                                            rej.insert(
                                                samp.tag.clone(),
                                                json_to_sample_value(&samp.value),
                                            );
                                        }
                                        crate::model::discovery::note_rejected(
                                            &ctx.lux,
                                            &ctx.hwid,
                                            &d.devid,
                                            rej,
                                        )
                                        .await;
                                        continue;
                                    };
                                    let mut kept: Vec<&DataUpSample> = Vec::new();
                                    let mut gate_rej: BTreeMap<String, SampleValue> =
                                        BTreeMap::new();
                                    for samp in &d.samples {
                                        if dev_snap.model.tags.is_empty()
                                            || dev_snap
                                                .model
                                                .tags
                                                .iter()
                                                .any(|t| t.name == samp.tag)
                                        {
                                            kept.push(samp);
                                        } else {
                                            gate_rejected.fetch_add(
                                                1,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                            tracing::warn!(
                                                "[sidecarClient] inst {inst_id} DataUp 标签 {}.{} 未在模型点表，拒收",
                                                d.devid, samp.tag
                                            );
                                            gate_rej.insert(
                                                samp.tag.clone(),
                                                json_to_sample_value(&samp.value),
                                            );
                                        }
                                    }
                                    // D-M4（审查修复）：被拒 tag 进发现队列（数据不放行）。
                                    if !gate_rej.is_empty() {
                                        crate::model::discovery::note_rejected(
                                            &ctx.lux,
                                            &ctx.hwid,
                                            &d.devid,
                                            gate_rej,
                                        )
                                        .await;
                                    }
                                    // 按 ts 合并同 devid 同时刻的多 tag 到一个 Sample
                                    //（Sample.values 语义=一设备一时刻多变量）。避免逐 tag 拆分成
                                    // 多 Sample 导致 N 次 pipeline/rtdata.write，且破坏下游整 Sample 聚合。
                                    let mut by_ts: BTreeMap<i64, BTreeMap<String, SampleValue>> = BTreeMap::new();
                                    for samp in kept {
                                        by_ts
                                            .entry(samp.ts)
                                            .or_default()
                                            .insert(samp.tag.clone(), json_to_sample_value(&samp.value));
                                    }
                                    for (ts, values) in by_ts {
                                        let sample = Sample {
                                            hwid: ctx.hwid.clone(),
                                            devid: d.devid.clone(),
                                            values,
                                            timestamp: ts,
                                        };
                                        if tx.send(sample).await.is_err() {
                                            break 'outer;
                                        }
                                    }
                                    // observed tick_ok：bridge 观察到外部 DataUp 并已推 hub。
                                    // duration=0（observed 无采集耗时概念，外部自测耗时不上报）。
                                    tracker.tick_ok(0);
                                    if tracker.should_flush() {
                                        tracker.flush().await;
                                    }
                                }
                                Ok(Recv::CommandAck(a)) => {
                                    if !a.ok {
                                        tracing::warn!(
                                            "[sidecarClient] inst {inst_id} cmd {} ack err: {:?}",
                                            a.cmd_id, a.err
                                        );
                                    }
                                }
                                Ok(Recv::Heartbeat) => {
                                    last_hb = Instant::now();
                                    tracker.extra_insert(
                                        "last_heartbeat_ts",
                                        serde_json::json!(chrono::Local::now().timestamp()),
                                    );
                                }
                                Ok(Recv::Hello(_)) => {
                                    tracing::warn!("[sidecarClient] inst {inst_id} duplicate Hello, ignore");
                                }
                                Err(e) => tracing::warn!("[sidecarClient] inst {inst_id} bad frame: {e}"),
                            },
                            Some(Err(e)) => {
                                tracker.tick_fail(&format!("read err: {e}"));
                                tracing::warn!("[sidecarClient] inst {inst_id} read err: {e}");
                                break;
                            }
                            None => {
                                tracker.set_status("degraded");
                                tracker.flush().await;
                                tracing::info!("[sidecarClient] inst {inst_id} connection closed");
                                break;
                            }
                        }
                    }
                    _ = flush_ticker.tick() => {
                        tracker.flush_if_due().await;
                    }
                    _ = ticker.tick() => {
                        if last_hb.elapsed() > hb_timeout {
                            tracker.set_status("degraded");
                            tracker.flush().await;
                            tracing::warn!("[sidecarClient] inst {inst_id} heartbeat timeout, disconnect");
                            break;
                        }
                    }
                }
            }
            // 连接断（非 token/非 hub 关）：回 'outer 等新连接（外部重连会重新分发）。
        }

        tracker.set_status("stopped");
        tracker.flush().await;

        ctx.bridge_registry.unregister(&inst_id);
        tracing::info!("[sidecarClient] inst {inst_id} exited");
    }
}

// ========== acceptor ==========

/// bridge acceptor：监听单端口，accept 后读 Hello、校验 token、分发到实例 task。
///
/// 挂在 `shutdown` child token 上；`cfg.enable=false` 时 lib.rs 不调用此函数。
pub async fn acceptor(
    listener: TcpListener,
    hello_timeout: Duration,
    max_connections: usize,
    lux: lux::EmbeddedClient,
    jwt_secret: Arc<str>,
    registry: BridgeRegistry,
    shutdown: CancellationToken,
) {
    if let Ok(a) = listener.local_addr() {
        tracing::info!(
            "[sidecarClient] bridge listening on {a} (max {max_connections} concurrent conns)"
        );
    }
    // 并发上限：防连接风暴/DoS（端口暴露到局域网时尤其重要）。permit 在连接 task 结束时释放。
    let sem = Arc::new(Semaphore::new(max_connections));

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            res = listener.accept() => match res {
                Ok((stream, addr)) => {
                    let sem = Arc::clone(&sem);
                    let lux = lux.clone();
                    let registry = registry.clone();
                    let secret = Arc::clone(&jwt_secret);
                    let shutdown2 = shutdown.clone();
                    tokio::spawn(async move {
                        // 拿 permit（满则等待）；shutdown 时放弃，避免 acceptor 退出后挂死。
                        let _permit = tokio::select! {
                            _ = shutdown2.cancelled() => return,
                            p = sem.acquire_owned() => match p {
                                Ok(p) => p,
                                Err(_) => return,
                            },
                        };
                        handle_connect(stream, addr, lux, secret, registry, hello_timeout).await;
                    });
                }
                Err(e) => tracing::warn!("[sidecarClient] accept failed: {e}"),
            },
        }
    }
    tracing::info!("[sidecarClient] bridge acceptor exited");
}

/// 处理单个外部连接：读 Hello → 校验 → 分发（或拒绝）。
async fn handle_connect(
    mut stream: TcpStream,
    addr: std::net::SocketAddr,
    lux: lux::EmbeddedClient,
    jwt_secret: Arc<str>,
    registry: BridgeRegistry,
    hello_timeout: Duration,
) {
    let first = match read_one_frame(&mut stream, MAX_FRAME, hello_timeout).await {
        Some(b) => b,
        None => {
            tracing::warn!("[sidecarClient] {addr} hello timeout/err, close");
            return;
        }
    };
    let hello = match decode(&first) {
        Ok(Recv::Hello(h)) => h,
        _ => {
            tracing::warn!("[sidecarClient] {addr} first frame not Hello, close");
            return;
        }
    };
    if !SUPPORTED_VERSIONS.contains(&hello.ver.as_str()) {
        // 版本协商：集合匹配（v2/v3 并存）。不支持的版本带升级提示拒连。
        let msg = format!(
            "不支持的协议版本 v{}；本宿主支持 {:?}（当前 v{CURRENT_VERSION}）。\
             v2 载荷：devices[].tags[] 模型点表；v3 追加 app_config 与 devices[].bind",
            hello.ver, SUPPORTED_VERSIONS
        );
        tracing::warn!("[sidecarClient] {addr} inst {} {msg}", hello.inst_id);
        let p = encode_payload(T_HELLO_ACK, &serde_json::json!({"ok": false, "msg": msg}));
        let _ = write_one_frame(&mut stream, &p).await;
        return;
    }

    let ok = ext_token::verify(&lux, &jwt_secret, &hello.inst_id, &hello.token)
        .await
        .unwrap_or(false);
    if !ok {
        tracing::warn!(
            "[sidecarClient] {addr} inst {} token verify failed",
            hello.inst_id
        );
        let p = encode_payload(
            T_HELLO_ACK,
            &serde_json::json!({"ok": false, "msg": "token invalid"}),
        );
        let _ = write_one_frame(&mut stream, &p).await;
        return;
    }

    let conn = BridgeConn {
        stream,
        ver: hello.ver,
    };
    match registry.try_dispatch(&hello.inst_id, conn) {
        Ok(()) => tracing::info!(
            "[sidecarClient] {addr} inst {} connected, dispatched",
            hello.inst_id
        ),
        Err((mut c, err)) => {
            tracing::warn!(
                "[sidecarClient] {addr} inst {} dispatch failed: {err:?}",
                hello.inst_id
            );
            let p = encode_payload(
                T_HELLO_ACK,
                &serde_json::json!({"ok": false, "msg": format!("instance not ready: {err:?}")}),
            );
            let _ = write_one_frame(&mut c.stream, &p).await;
        }
    }
}

inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "sidecarClient",
        create: || Box::new(SidecarClient),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_heartbeat_empty_body() {
        assert!(matches!(decode(&[T_HEARTBEAT]), Ok(Recv::Heartbeat)));
    }

    #[test]
    fn decode_dataup_multi_samples() {
        let body = r#"{"devid":"d1","samples":[{"tag":"t1","value":1.5,"ts":100},{"tag":"t2","value":true,"ts":100}]}"#;
        let mut payload = vec![T_DATA_UP];
        payload.extend_from_slice(body.as_bytes());
        match decode(&payload).unwrap() {
            Recv::DataUp(d) => {
                assert_eq!(d.devid, "d1");
                assert_eq!(d.samples.len(), 2);
                assert_eq!(d.samples[0].tag, "t1");
            }
            _ => panic!("expected DataUp"),
        }
    }

    #[test]
    fn decode_rejects_unknown_type_and_empty() {
        assert!(decode(&[200, b'{', b'}']).is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn encode_payload_type_prefix() {
        let p = encode_payload(T_HEARTBEAT, &serde_json::json!({}));
        assert_eq!(p[0], T_HEARTBEAT);
    }

    #[test]
    fn json_to_sample_value_maps_types() {
        assert!(matches!(
            json_to_sample_value(&serde_json::json!(true)),
            SampleValue::Bool(true)
        ));
        assert!(matches!(
            json_to_sample_value(&serde_json::json!(42)),
            SampleValue::Int(42)
        ));
        assert!(matches!(
            json_to_sample_value(&serde_json::json!(1.5)),
            SampleValue::Float(_)
        ));
        assert!(matches!(
            json_to_sample_value(&serde_json::json!("hi")),
            SampleValue::Str(_)
        ));
    }

    #[test]
    fn parse_hb_timeout_override_or_default() {
        assert_eq!(
            parse_hb_timeout(
                &serde_json::json!({"heartbeat_timeout_secs": 30}),
                Duration::from_secs(15)
            ),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_hb_timeout(&serde_json::json!({}), Duration::from_secs(15)),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn hello_ver_missing_falls_back_to_v2() {
        // 缺 ver → 按 v2 兼容（协议文档 §4「ver 缺失按 v2 处理」；不发 ver 的存量
        // 客户端不被拒）。与"必填"不同——这是显式裁决，改动须同步协议文档。
        let h: Hello = serde_json::from_str(r#"{"inst_id":"x","token":"t"}"#).unwrap();
        assert_eq!(h.ver, DEFAULT_VER);
        assert_eq!(h.ver, "2");
    }

    // ===== 版本协商（集合匹配）=====

    /// v2/v3 并存；v1 与未知版本不在集合内（拒连）。
    #[test]
    fn supported_versions_include_v2_and_v3_only() {
        assert!(
            SUPPORTED_VERSIONS.contains(&"2"),
            "v2 必须保留（存量客户端）"
        );
        assert!(SUPPORTED_VERSIONS.contains(&"3"), "v3 是当前版本");
        assert!(!SUPPORTED_VERSIONS.contains(&"1"), "v1 已停用");
        assert!(!SUPPORTED_VERSIONS.contains(&"4"), "未实现的版本不应放行");
        assert!(
            SUPPORTED_VERSIONS.contains(&CURRENT_VERSION),
            "当前版本须在支持集内"
        );
    }

    // ===== ConfigPush 载荷（纯函数）=====

    /// 造一个最小展开产物（1 设备 1 tag，带 bind）。
    fn expanded_one() -> Vec<crate::model::expand::ExpandedDevice> {
        use crate::model::types::{DeviceEntry, DeviceModel, TagTemplate};
        vec![crate::model::expand::ExpandedDevice {
            devid: "devA".into(),
            bind: [("group".to_string(), "g1".to_string())].into(),
            entry: DeviceEntry {
                devid: "devA".into(),
                model: "m1".into(),
                params: [("base".to_string(), "100".to_string())].into(),
                ..Default::default()
            },
            model: DeviceModel {
                model_id: "m1".into(),
                tags: vec![TagTemplate {
                    name: "temp".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:{base}:f32".into()),
                    unit: Some("C".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        }]
    }

    /// v2 载荷与 M-3 形态逐字节一致：无 app_config、无 bind、v=2。
    #[test]
    fn config_push_v2_shape_unchanged() {
        let ex = expanded_one();
        let cfg = build_config_push("inst1", "2", &ex, Some(&serde_json::json!({"a": 1})));
        assert_eq!(cfg["v"], 2);
        assert_eq!(cfg["inst_id"], "inst1");
        assert!(cfg.get("app_config").is_none(), "v2 不下发 app_config");
        let dev = &cfg["devices"][0];
        assert_eq!(dev["devid"], "devA");
        assert_eq!(dev["model"], "m1");
        assert!(dev.get("bind").is_none(), "v2 不下发 bind");
        // tags 形态不变（addr 已解析）
        assert_eq!(dev["tags"][0]["name"], "temp");
        assert_eq!(dev["tags"][0]["addr"], "holding:100:f32");
        assert_eq!(dev["tags"][0]["unit"], "C");
        assert_eq!(dev["tags"][0]["rw"], "r");
    }

    /// v3 载荷：v2 全部字段不变 + app_config + devices[].bind。
    #[test]
    fn config_push_v3_adds_app_config_and_bind() {
        let ex = expanded_one();
        let app = serde_json::json!({"prog_id": "Matrikon.OPC.Simulation.1", "password": "pw"});
        let cfg = build_config_push("inst1", "3", &ex, Some(&app));
        assert_eq!(cfg["v"], 3);
        assert_eq!(cfg["app_config"]["prog_id"], "Matrikon.OPC.Simulation.1");
        assert_eq!(cfg["app_config"]["password"], "pw");
        // v2 字段一字不改
        assert_eq!(cfg["devices"][0]["devid"], "devA");
        assert_eq!(cfg["devices"][0]["tags"][0]["addr"], "holding:100:f32");
        // v3 新增 bind
        assert_eq!(cfg["devices"][0]["bind"]["group"], "g1");
    }

    /// v3 空 bind 也发 `{}`（形态稳定，外部按存在性判断能力）。
    #[test]
    fn config_push_v3_empty_bind_still_present() {
        let mut ex = expanded_one();
        ex[0].bind.clear();
        let cfg = build_config_push("i", "3", &ex, Some(&serde_json::json!({})));
        assert_eq!(cfg["devices"][0]["bind"], serde_json::json!({}));
    }

    /// app_config 裁剪：宿主内部键排除，其余原样（含未知协议键）。
    #[test]
    fn push_app_config_excludes_host_keys_only() {
        let app = serde_json::json!({
            "secrets": ["secret:k1"],
            "heartbeat_timeout_secs": 30,
            "prog_id": "X",
            "opc_host": "1.2.3.4",
            "nested": {"a": 1},
        });
        let out = push_app_config(&app);
        assert!(out.get("secrets").is_none(), "secrets 是宿主内部键");
        assert!(
            out.get("heartbeat_timeout_secs").is_none(),
            "心跳已由 HelloAck 下发"
        );
        assert_eq!(out["prog_id"], "X");
        assert_eq!(out["opc_host"], "1.2.3.4");
        assert_eq!(out["nested"]["a"], 1, "嵌套对象原样透传");
    }

    /// 非对象 app_config（null/标量）→ `{}`（外部解析零分支）。
    #[test]
    fn push_app_config_non_object_yields_empty() {
        for v in [serde_json::Value::Null, serde_json::json!(42)] {
            assert_eq!(push_app_config(&v), serde_json::json!({}));
        }
    }

    /// 纯软 tag（模型 addr=None）→ addr 回退 tag 名（与 to_device_spec 同口径）。
    #[test]
    fn push_devices_soft_tag_addr_falls_back_to_name() {
        use crate::model::types::{DeviceEntry, DeviceModel, TagTemplate};
        let ex = vec![crate::model::expand::ExpandedDevice {
            devid: "d".into(),
            bind: Default::default(),
            entry: DeviceEntry {
                devid: "d".into(),
                model: "m".into(),
                ..Default::default()
            },
            model: DeviceModel {
                model_id: "m".into(),
                tags: vec![TagTemplate {
                    name: "soft".into(),
                    data_type: "float".into(),
                    addr: None,
                    ..Default::default()
                }],
                ..Default::default()
            },
        }];
        let cfg = build_config_push("i", "2", &ex, None);
        assert_eq!(cfg["devices"][0]["tags"][0]["addr"], "soft");
    }
}
