//! MQTT 透传 IO backend（设计 §5.4/Q10，spike-2 方案 a 转正，阶段 B T11）。
//!
//! spike-2（`spike2_mqtt.rs`，#[cfg(test)] 存档）验证的模型：
//! - EventLoop 独立 pump task 独占 `eventloop.poll()`（不进 select!，规避 poll
//!   取消安全未知问题）；
//! - 出包按「当前是否有在飞 request」经 [`route_incoming`] 分路：有在飞 → resp
//!   oneshot（resp_topic 首帧即响应）；无在飞 → 被动帧通道（on_data）；
//! - `request(frame)` = 注册在飞 slot → publish req_topic → 等首帧（超时清 slot
//!   防晚到帧误投下一个 request）；
//! - `recv_idle()` = 被动帧通道小窗口 recv。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{AsyncClient, MqttOptions, QoS};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::IoBackend;

/// client_id 序号（多实例唯一）：`mqtt-io-<pid>-<seq>`。
static CLIENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 解析 endpoint `"host:port"`。失败返回 `(原样字符串, 0)`——port 0 连接必失败并经
/// pump poll 冒出到 request Err（`new` 无 Result，对齐其他 backend 的懒失败语义）。
fn parse_endpoint(endpoint: &str) -> (String, u16) {
    match endpoint.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h.to_string(), port),
            Err(_) => (endpoint.to_string(), 0),
        },
        None => (endpoint.to_string(), 0),
    }
}

/// pump 分路结果：Incoming Publish 按「当前是否有在飞 request」二分，其余忽略。
#[derive(Debug, PartialEq, Eq)]
enum Routed {
    /// 有在飞 request：Publish 视为 resp_topic 首帧 → resp oneshot。
    Response(Vec<u8>),
    /// 无在飞 request：Publish 视为透传被动帧 → on_data 通道。
    Passive(Vec<u8>),
    /// 非 Publish 包（ConnAck/Ping/SubAck 等）或 Outgoing 事件。
    Ignore,
}

/// pump task 收到的 event 如何分路（spike-2 route_incoming 的生产转正形态）。
fn route_incoming(has_inflight: bool, event: &rumqttc::Event) -> Routed {
    match event {
        rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) => {
            let frame = p.payload.to_vec();
            if has_inflight {
                Routed::Response(frame)
            } else {
                Routed::Passive(frame)
            }
        }
        _ => Routed::Ignore,
    }
}

/// 订阅集合组装：resp_topic + passive_topics 去重（resp 优先，透传被动帧供 on_data）。
fn subscription_set(resp: &str, passive: &[String]) -> Vec<String> {
    let mut out = vec![resp.to_string()];
    for t in passive {
        if !out.iter().any(|x| x == t) {
            out.push(t.clone());
        }
    }
    out
}

/// 在飞 request 回执 sender（pump 分路送达：Response → Ok 帧；poll 错误 → Err）。
type InflightReply = oneshot::Sender<mlua::Result<Vec<u8>>>;

/// 在飞 slot 的共享句柄（request 注册 / pump 分路读取与取走）。
type InflightSlot = Arc<Mutex<Option<InflightReply>>>;

/// MQTT 透传 IO backend（spike-2 原型 SpikeMqttIo 的生产转正）。
///
/// 与 spike 的差异（转正补齐）：pump 在每次 ConnAck 统一（重）订阅 resp + passive
/// 集合（首连与断连重连同路径——clean session 下不依赖 broker 恢复会话）；request
/// 等首帧带 30s 兜底超时（spike 内部超时改为上层 IoEngine timeout 先行，兜底仅防
/// 静默链路挂死 IO task——分路依据 has_inflight，slot 常驻会把后续被动帧全部误投
/// Response）。断连不置显式未连接态：pump poll 继续即重连（spike-2 结论 3）。
pub(crate) struct MqttIo {
    client: AsyncClient,
    passive_rx: mpsc::Receiver<Vec<u8>>,
    inflight: InflightSlot,
    req_topic: String,
    /// 被动帧 recv_idle 窗口。
    idle: Duration,
    pump: JoinHandle<()>,
}

impl MqttIo {
    /// 构造即起 pump task（连接由 pump 的 poll 驱动，懒连接同 TcpIo 语义）。
    ///
    /// topic 模板最小实现：固定字符串直用（`{devid}` 等占位符列后续，设计 §5.4）。
    /// `idle_ms` 为被动帧 recv_idle 窗口（下限 50ms 防 select! 忙循环）。
    pub(crate) fn new(
        endpoint: String,
        req_topic: String,
        resp_topic: String,
        passive_topics: Vec<String>,
        idle_ms: u64,
    ) -> Self {
        let (host, port) = parse_endpoint(&endpoint);
        // client_id 多实例唯一（进程内序号 + pid 跨实例）；透传 broker 通常内网匿名，
        // username/password 留 cfg 后续加（设计 §5.4 定案）。
        let client_id = format!(
            "mqtt-io-{}-{}",
            std::process::id(),
            CLIENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut opts = MqttOptions::new(client_id, host, port);
        opts.set_clean_session(true);
        opts.set_keep_alive(Duration::from_secs(30));
        let (client, mut eventloop) = AsyncClient::new(opts, 16);

        let subs = subscription_set(&resp_topic, &passive_topics);
        let (passive_tx, passive_rx) = mpsc::channel::<Vec<u8>>(16);
        let inflight: InflightSlot = Arc::new(Mutex::new(None));
        let slot = inflight.clone();
        let sub_client = client.clone();
        let pump = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(ev) => {
                        // ConnAck：统一（重）订阅（首连 + 断连重连同路径）。
                        if matches!(ev, rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) {
                            for t in &subs {
                                if let Err(e) =
                                    sub_client.subscribe(t.clone(), QoS::AtLeastOnce).await
                                {
                                    tracing::warn!("[MqttIo] 订阅 {t} 失败：{e}");
                                }
                            }
                        }
                        // 分路依据先取快照：match scrutinee 里的临时 MutexGuard 会存活
                        // 到整个 match 语句结束，臂内二次 lock 同一把非重入 std Mutex 即
                        // 自死锁（T11 roundtrip 实测挂死；spike-2 同款写法但其测试从未
                        // 走真 Response 投递故未暴露）——let 语句先行取值，guard 即取即释。
                        let has_inflight = slot.lock().unwrap().is_some();
                        match route_incoming(has_inflight, &ev) {
                            Routed::Response(frame) => {
                                if let Some(tx) = slot.lock().unwrap().take() {
                                    let _ = tx.send(Ok(frame));
                                }
                            }
                            Routed::Passive(frame) => {
                                // 满通道丢帧（同 spawn_io_task 的 P3 try_send 语义）
                                let _ = passive_tx.try_send(frame);
                            }
                            Routed::Ignore => {}
                        }
                    }
                    Err(e) => {
                        // 错误送达在飞 request（立即 Err）；poll 报错后继续循环 poll
                        // （rumqttc 重连语义，spike-2 结论：Err 后 EventLoop 可继续）。
                        if let Some(tx) = slot.lock().unwrap().take() {
                            let _ =
                                tx.send(Err(mlua::Error::RuntimeError(format!("mqtt poll: {e}"))));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await; // 重连退避
                    }
                }
            }
        });
        Self {
            client,
            passive_rx,
            inflight,
            req_topic,
            idle: Duration::from_millis(idle_ms.max(50)),
            pump,
        }
    }
}

#[async_trait::async_trait]
impl IoBackend for MqttIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        let (otx, orx) = oneshot::channel();
        // 先注册 slot 再 publish（规避「响应在注册前到达」竞态，spike-2 实测）。
        *self.inflight.lock().unwrap() = Some(otx);
        // publish 排队语义：未连接/重连中返回 Ok，连接错误经 pump poll 冒出
        // （spike-2 结论 1）。
        if let Err(e) = self
            .client
            .publish(&self.req_topic, QoS::AtLeastOnce, false, frame)
            .await
        {
            self.inflight.lock().unwrap().take();
            return Err(mlua::Error::RuntimeError(format!("mqtt publish: {e}")));
        }
        // 等首帧：pump 分路送达 / poll 错误送达 / 30s 兜底。业务超时由上层
        // IoEngine::request 的 timeout_ms 先行（默认 200ms，clamp 上限 30s）；
        // 兜底仅防「连接健康但设备永不回帧」挂死 IO task（分路依据 has_inflight，
        // slot 常驻会把后续被动帧全部误投 Response）。超时清 slot 防晚到帧误投
        // 下一个 request（QoS1 断连补发可能迟到——设备侧需幂等，spike-2 结论 4）。
        match tokio::time::timeout(Duration::from_secs(30), orx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(mlua::Error::RuntimeError("mqtt pump dropped".into())),
            Err(_) => {
                self.inflight.lock().unwrap().take();
                Err(mlua::Error::RuntimeError(
                    "mqtt: 等响应超时（30s 兜底，链路静默）".into(),
                ))
            }
        }
    }

    /// 被动帧 = 无在飞 request 时收到的 Publish（on_data）。小窗口 recv：无帧回
    /// None（同 TcpIo idle 成帧窗语义；窗口下限 50ms 防 select! 忙循环）。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        match tokio::time::timeout(self.idle, self.passive_rx.recv()).await {
            Ok(Some(frame)) => Some(frame),
            Err(_) => None, // idle 窗口无帧 → 重 select（正常节拍）
            // 复审（阶段 B 深度审查）：通道关闭（pump panic/退出，tx 全 drop）若也
            // 返 None，外层 select! 会立即重进 recv_idle → recv 立即再返 None →
            // 忙循环烧 CPU。挂起等到 idle 窗口满再返 None，节拍与无帧一致。
            Ok(None) => {
                tokio::time::sleep(self.idle).await;
                None
            }
        }
    }
}

impl Drop for MqttIo {
    fn drop(&mut self) {
        // IO task 退出（backend drop）时停 pump（spike-2 同款）。
        self.pump.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::{ConnAck, ConnectReturnCode, Event, MqttOptions, Packet, Publish, QoS};
    use std::time::Duration;

    /// 起进程内 rumqttd（明文 TCP v4，127.0.0.1 随机端口；配置同 T10 north e2e 的
    /// spawn_inproc_broker——tests/ 无法复用 src 内 cfg(test) helper，进程内闭环
    /// 归属单元级故在此复制）。返回端口。
    fn spawn_inproc_broker(tag: &str) -> u16 {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0 选空闲端口");
            l.local_addr().expect("local_addr").port()
        };
        let config = rumqttd::Config {
            router: rumqttd::RouterConfig {
                max_connections: 64,
                max_outgoing_packet_count: 200,
                max_segment_size: 10 * 1024 * 1024,
                max_segment_count: 10,
                custom_segment: None,
                initialized_filters: None,
                shared_subscriptions_strategy: Default::default(),
            },
            v4: Some(
                [(
                    "test".to_string(),
                    rumqttd::ServerSettings {
                        name: format!("mqtt-io-test-{tag}"),
                        listen: format!("127.0.0.1:{port}").parse().expect("SocketAddr"),
                        tls: None,
                        next_connection_delay_ms: 1,
                        connections: rumqttd::ConnectionSettings {
                            connection_timeout_ms: 60_000,
                            max_payload_size: 1024 * 1024,
                            max_inflight_count: 100,
                            auth: None,
                            external_auth: None,
                            dynamic_filters: false,
                        },
                    },
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };
        let mut broker = rumqttd::Broker::new(config);
        std::thread::Builder::new()
            .name(format!("rumqttd-mqtt-io-{tag}"))
            .spawn(move || {
                if let Err(e) = broker.start() {
                    eprintln!("[mqtt-io-test] rumqttd start 失败：{e:?}");
                }
            })
            .expect("spawn rumqttd 线程");
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return port;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("rumqttd 5s 未就绪（port {port}）");
    }

    /// 测试侧「设备」：订阅 REQ_TOPIC 收帧回 echo+0xAA；返回 client 句柄（推被动帧用）。
    async fn spawn_device(port: u16) -> rumqttc::AsyncClient {
        let mut opts = MqttOptions::new("mqtt-io-test-device", "127.0.0.1", port);
        opts.set_clean_session(true);
        let (client, mut eventloop) = rumqttc::AsyncClient::new(opts, 16);
        client
            .subscribe(REQ_TOPIC, QoS::AtLeastOnce)
            .await
            .expect("device subscribe");
        let poll_client = client.clone();
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let mut r = p.payload.to_vec();
                        r.push(0xAA);
                        let _ = poll_client
                            .publish(RESP_TOPIC, QoS::AtLeastOnce, false, r)
                            .await;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                    _ => {}
                }
            }
        });
        client
    }

    const REQ_TOPIC: &str = "mqtt-io-test/req";
    const RESP_TOPIC: &str = "mqtt-io-test/resp";
    const PASSIVE_TOPIC: &str = "mqtt-io-test/passive";

    /// 分路：有在飞 request 时 Publish → Response（resp 首帧）。
    #[test]
    fn route_publish_to_response_when_inflight() {
        let ev = Event::Incoming(Packet::Publish(Publish::new(
            "dev/resp",
            QoS::AtLeastOnce,
            b"\x01resp",
        )));
        assert_eq!(
            route_incoming(true, &ev),
            Routed::Response(b"\x01resp".to_vec())
        );
    }

    /// 分路：无在飞 request 时 Publish → Passive（on_data 被动帧）。
    #[test]
    fn route_publish_to_passive_when_no_inflight() {
        let ev = Event::Incoming(Packet::Publish(Publish::new(
            "dev/transparent",
            QoS::AtLeastOnce,
            b"\x02passive",
        )));
        assert_eq!(
            route_incoming(false, &ev),
            Routed::Passive(b"\x02passive".to_vec())
        );
    }

    /// 分路：非 Publish 包（ConnAck/Ping）与 Outgoing 事件 → 忽略。
    #[test]
    fn route_ignores_non_publish_and_outgoing() {
        let connack = Event::Incoming(Packet::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )));
        assert_eq!(route_incoming(true, &connack), Routed::Ignore);
        let ping = Event::Incoming(Packet::PingResp);
        assert_eq!(route_incoming(false, &ping), Routed::Ignore);
        let outgoing = Event::Outgoing(rumqttc::Outgoing::Publish(7));
        assert_eq!(route_incoming(true, &outgoing), Routed::Ignore);
    }

    /// 订阅集合：resp 优先 + passive 追加 + 重复去重（resp 与 passive 撞名只订一次）。
    #[test]
    fn subscription_set_resp_first_passive_appended_dedup() {
        let set = subscription_set(
            "t/resp",
            &["t/a".to_string(), "t/resp".to_string(), "t/b".to_string()],
        );
        assert_eq!(
            set,
            vec!["t/resp".to_string(), "t/a".to_string(), "t/b".to_string()]
        );
        // 无 passive：仅 resp。
        assert_eq!(subscription_set("t/resp", &[]), vec!["t/resp".to_string()]);
    }

    /// MqttIo 闭环（对标 mod.rs 的 tcp_roundtrip/udp_roundtrip）：进程内 broker +
    /// 测试侧设备（订 req 回 echo+0xAA）→ request 闭环出数 + 被动帧 recv_idle。
    #[tokio::test]
    async fn mqtt_roundtrip_over_inproc_broker() {
        let port = spawn_inproc_broker("roundtrip");
        let device = spawn_device(port).await;

        let mut io = MqttIo::new(
            format!("127.0.0.1:{port}"),
            REQ_TOPIC.to_string(),
            RESP_TOPIC.to_string(),
            vec![PASSIVE_TOPIC.to_string()],
            50,
        );
        let resp = io.request(b"\x01\x02").await.expect("mqtt request 应闭环");
        assert_eq!(resp, vec![0x01, 0x02, 0xAA]);

        // 被动帧：无在飞 request 时设备推 PASSIVE_TOPIC → recv_idle 收到。
        device
            .publish(PASSIVE_TOPIC, QoS::AtLeastOnce, false, b"\x02passive")
            .await
            .expect("publish passive");
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match io.recv_idle().await {
                    Some(f) => break f,
                    None => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("被动帧应在 recv_idle 到达");
        assert_eq!(frame, b"\x02passive");
    }

    /// broker 不可达：request 应快速 Err（连接错误经 pump poll 冒出，而非挂死）。
    #[tokio::test]
    async fn mqtt_dead_broker_errors_fast() {
        let mut io = MqttIo::new(
            "127.0.0.1:1".to_string(), // 必失败端口（spike-2 同款）
            REQ_TOPIC.to_string(),
            RESP_TOPIC.to_string(),
            Vec::new(),
            50,
        );
        let t0 = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), io.request(b"\x01"))
            .await
            .expect("request 不应挂死");
        assert!(r.is_err(), "broker 不可达时 request 应 Err，got {r:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "应经 pump 错误送达快速失败（{:?}）",
            t0.elapsed()
        );
        // pump 存活（断连后持续重连 poll，不 panic）。
        assert!(!io.pump.is_finished(), "pump 应存活");
        // 无被动帧时 recv_idle → None（不挂起）。
        assert!(io.recv_idle().await.is_none());
    }
}
