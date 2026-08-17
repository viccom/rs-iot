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
//! 详见 `docs/external-plugin-protocol.md`。

use crate::db::ext_token;
use crate::host::{BridgeRegistry, HostContext};
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

/// 协议版本（`Hello.ver`，缺省视为该版本）。
const PROTO_VER: &str = "1";
/// 单帧最大字节数（codec 上限，容纳大配置 push）。
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// task 侧心跳超时缺省（`app_config.heartbeat_timeout_secs` 可覆盖）。
const HB_TIMEOUT_DEFAULT_SECS: u64 = 15;

fn default_ver() -> String {
    PROTO_VER.into()
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
        tx: mpsc::Sender<Sample>,
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

        // 连接 slot：acceptor 分发来的 TcpStream 经此进入 task。容量 1（外部一次一个连接；
        // 重连时若 task 未取走上一个，dispatch 报 Full，acceptor 回 ack 让外部稍后重试）。
        let (slot_tx, mut slot_rx) = mpsc::channel::<TcpStream>(1);
        ctx.bridge_registry.register(&inst_id, slot_tx);

        // observed StatTracker（M4）：外部进程不写 lux，bridge 观察派生指标——
        // 收 DataUp → tick_ok（duration=0，observed 无耗时概念）、连接/心跳 → extra、
        // 断联 → degraded。语义是「rs-iot 观察到的外部活跃度」，非外部自报。
        let mut tracker =
            crate::plugin::stat_tracker::StatTracker::new(inst_id.clone(), ctx.lux.clone());
        let mut reconnects: u64 = 0;

        let mut next_cmd_id: u64 = 1;

        'outer: loop {
            let stream = tokio::select! {
                _ = token.cancelled() => break 'outer,
                s = slot_rx.recv() => match s {
                    Some(s) => s,
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
            let cfg = serde_json::json!({
                "inst_id": &inst_id,
                "devices": &inst.devices,
            });
            if writer
                .send(Bytes::from(encode_payload(T_CONFIG_PUSH, &cfg)))
                .await
                .is_err()
            {
                continue;
            }
            tracing::info!("[sidecarClient] inst {inst_id} external connected, config pushed");

            let mut last_hb = Instant::now();
            // 心跳超时检测 ticker(非采集,不接 jitter——jitter 会改变超时判定语义)。
            let mut ticker = tokio::time::interval(hb_check);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // observed stat 周期 flush ticker（与采集 ticker 解耦，5s 落一次 lux stat:<inst>）。
            let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
                crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
            ));

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
                                    // 按 ts 合并同 devid 同时刻的多 tag 到一个 Sample
                                    //（Sample.values 语义=一设备一时刻多变量）。避免逐 tag 拆分成
                                    // 多 Sample 导致 N 次 pipeline/rtdata.write，且破坏下游整 Sample 聚合。
                                    let mut by_ts: BTreeMap<i64, BTreeMap<String, SampleValue>> = BTreeMap::new();
                                    for samp in &d.samples {
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
                        tracker.flush().await;
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
    if hello.ver != PROTO_VER {
        let msg = format!("version mismatch: bridge={PROTO_VER} ext={}", hello.ver);
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

    match registry.try_dispatch(&hello.inst_id, stream) {
        Ok(()) => tracing::info!(
            "[sidecarClient] {addr} inst {} connected, dispatched",
            hello.inst_id
        ),
        Err((mut s, err)) => {
            tracing::warn!(
                "[sidecarClient] {addr} inst {} dispatch failed: {err:?}",
                hello.inst_id
            );
            let p = encode_payload(
                T_HELLO_ACK,
                &serde_json::json!({"ok": false, "msg": format!("instance not ready: {err:?}")}),
            );
            let _ = write_one_frame(&mut s, &p).await;
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
    fn hello_ver_defaults_to_proto() {
        let h: Hello = serde_json::from_str(r#"{"inst_id":"x","token":"t"}"#).unwrap();
        assert_eq!(h.ver, PROTO_VER);
    }
}
