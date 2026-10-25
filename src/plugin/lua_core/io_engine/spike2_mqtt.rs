//! spike-2（设计 §9 风险 R2 / Q10，阶段 B Task 2）：MQTT 透传 backend 与
//! io_engine 独占模型的适配实验——EventLoop 独立 pump task + 内部通道分路。
//!
//! 适配模型（T11 生产 backend 的形态依据）：
//! - pump task 独占 `eventloop.poll()`（不进 select!，规避 poll 取消安全未知问题）；
//! - 出包按「当前是否有在飞 request」经 [`route_incoming`] 分路：
//!   有在飞 → resp oneshot（首帧即响应，reply topic 模板）；无在飞 → 被动帧通道（on_data）；
//! - `request(frame)` = 注册在飞 slot → publish req_topic → 等首帧（超时清 slot）；
//! - `recv_idle()` = 被动帧通道小窗口 recv。
//!
//! 本模块为 spike 实验代码（`#[cfg(test)]`，不进 lib build），真网络双向收发归 T11 e2e。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{AsyncClient, Event, MqttOptions, QoS};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// pump 分路结果：Incoming Publish 按「当前是否有在飞 request」二分，其余忽略。
#[derive(Debug, PartialEq, Eq)]
enum SpikeRouted {
    /// 有在飞 request：Publish 视为 resp_topic 首帧 → resp oneshot。
    Response(Vec<u8>),
    /// 无在飞 request：Publish 视为透传被动帧 → on_data 通道。
    Passive(Vec<u8>),
    /// 非 Publish 包（ConnAck/Ping/SubAck 等）或 Outgoing 事件。
    Ignore,
}

/// spike-2 核心：pump task 收到的 event 如何分路（纯函数，T11 生产 backend 复用此形态）。
fn route_incoming(has_inflight: bool, event: &Event) -> SpikeRouted {
    match event {
        Event::Incoming(rumqttc::Packet::Publish(p)) => {
            let frame = p.payload.to_vec();
            if has_inflight {
                SpikeRouted::Response(frame)
            } else {
                SpikeRouted::Passive(frame)
            }
        }
        _ => SpikeRouted::Ignore,
    }
}

/// 在飞 request 回执（pump 分路送达：Response → Ok 帧，poll 错误 → Err）。
type SpikeReply = Result<Vec<u8>, String>;

/// spike-2 原型：EventLoop 独立 pump task + 内部通道分路的最小 MQTT backend 形态。
///
/// 验证与 [`super::IoBackend`] 独占模型的对接（`request`/`recv_idle` 二分签名对齐）；
/// 生产 MqttIo（T11）按此模型实现并接 `spawn_io_task`。
struct SpikeMqttIo {
    client: AsyncClient,
    passive_rx: mpsc::Receiver<Vec<u8>>,
    /// pump 上报的 poll 错误流（重连期间宿主观测/计数用）。
    err_rx: mpsc::Receiver<String>,
    inflight: Arc<Mutex<Option<oneshot::Sender<SpikeReply>>>>,
    pump: JoinHandle<()>,
}

impl SpikeMqttIo {
    /// 连 127.0.0.1:1（必失败端口，无需真实 broker——验证的是模型不是网络）。
    fn new_dead_broker() -> Self {
        let mut opts = MqttOptions::new("spike2", "127.0.0.1", 1);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, eventloop) = AsyncClient::new(opts, 16);
        Self::new(client, eventloop)
    }

    fn new(client: AsyncClient, mut eventloop: rumqttc::EventLoop) -> Self {
        let (passive_tx, passive_rx) = mpsc::channel::<Vec<u8>>(16);
        let (err_tx, err_rx) = mpsc::channel::<String>(16);
        let inflight: Arc<Mutex<Option<oneshot::Sender<SpikeReply>>>> = Arc::new(Mutex::new(None));
        let slot = inflight.clone();
        let pump = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(ev) => match route_incoming(slot.lock().unwrap().is_some(), &ev) {
                        SpikeRouted::Response(frame) => {
                            if let Some(tx) = slot.lock().unwrap().take() {
                                let _ = tx.send(Ok(frame));
                            }
                        }
                        SpikeRouted::Passive(frame) => {
                            // 满通道丢帧（同 spawn_io_task 的 P3 try_send 语义）
                            let _ = passive_tx.try_send(frame);
                        }
                        SpikeRouted::Ignore => {}
                    },
                    Err(e) => {
                        // 错误送达两条路：在飞 request 立即 Err；err 通道供宿主观测。
                        // poll 报错后继续循环 poll（rumqttc 重连语义：clean 后下次 poll 重连）。
                        if let Some(tx) = slot.lock().unwrap().take() {
                            let _ = tx.send(Err(format!("mqtt poll: {e}")));
                        }
                        let _ = err_tx.try_send(e.to_string());
                        tokio::time::sleep(Duration::from_millis(100)).await; // 重连退避
                    }
                }
            }
        });
        Self {
            client,
            passive_rx,
            err_rx,
            inflight,
            pump,
        }
    }

    /// request = publish req_topic + 等首帧（reply topic 模板，Q10）。
    async fn request(&self, frame: &[u8], timeout_ms: u64) -> Result<Vec<u8>, String> {
        let (otx, orx) = oneshot::channel();
        // 先注册 slot 再 publish（规避「响应在注册前到达」竞态）
        *self.inflight.lock().unwrap() = Some(otx);
        // 实测：未连接/重连中 publish 立即排队返回 Ok，连接错误只能经 pump poll 路径送达 orx
        self.client
            .publish("spike2/req", QoS::AtLeastOnce, false, frame)
            .await
            .map_err(|e| format!("publish: {e}"))?;
        match tokio::time::timeout(Duration::from_millis(timeout_ms), orx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("pump dropped".into()),
            Err(_) => {
                // 超时弃等，清 slot（防晚到帧误投下一个 request——「超时弃订」语义）
                self.inflight.lock().unwrap().take();
                Err(format!("request timeout ({timeout_ms}ms)"))
            }
        }
    }

    /// recv_idle = 被动帧通道小窗口 recv（无帧回 None，同 TcpIo idle 成帧窗语义）。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        match tokio::time::timeout(Duration::from_millis(50), self.passive_rx.recv()).await {
            Ok(Some(frame)) => Some(frame),
            Ok(None) | Err(_) => None,
        }
    }
}

impl Drop for SpikeMqttIo {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::{ConnAck, ConnectReturnCode, Packet, Publish};

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
            SpikeRouted::Response(b"\x01resp".to_vec())
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
            SpikeRouted::Passive(b"\x02passive".to_vec())
        );
    }

    /// 分路：非 Publish 包（ConnAck/Ping）与 Outgoing 事件 → 忽略。
    #[test]
    fn route_ignores_non_publish_and_outgoing() {
        let connack = Event::Incoming(Packet::ConnAck(ConnAck::new(
            ConnectReturnCode::Success,
            false,
        )));
        assert_eq!(route_incoming(true, &connack), SpikeRouted::Ignore);
        let ping = Event::Incoming(Packet::PingResp);
        assert_eq!(route_incoming(false, &ping), SpikeRouted::Ignore);
        let outgoing = Event::Outgoing(rumqttc::Outgoing::Publish(7));
        assert_eq!(route_incoming(true, &outgoing), SpikeRouted::Ignore);
    }

    /// spike-2 主实验：EventLoop 独立 pump + 通道分路在「broker 不可达」下的行为。
    ///
    /// 验证模型不是网络（真网络双向收发归 T11 e2e）：
    /// 1. 未连接时 publish 立即排队返回（不阻塞）——失败感知只能走 poll 路径；
    /// 2. 连接错误从 poll 冒出 → pump 不 panic、错误经通道送达等待中的 request（Err 而非挂死）；
    /// 3. poll 报错后 EventLoop 可继续 poll（rumqttc 重连语义），pump 存活；
    /// 4. 无被动帧时 recv_idle 回 None。
    #[tokio::test]
    async fn spike_pump_error_reaches_request_and_pump_survives() {
        let mut io = SpikeMqttIo::new_dead_broker();

        // (1) publish 未连接立即排队返回 Ok——不阻塞、不报连接错误（关键实测行为）。
        let probe = tokio::time::timeout(
            Duration::from_secs(1),
            io.client
                .publish("spike2/req", QoS::AtLeastOnce, false, b"probe"),
        )
        .await
        .expect("publish 未连接也应立即返回（排队），不应阻塞");
        assert!(probe.is_ok(), "排队 publish 应 Ok，got: {probe:?}");

        // (2) request 收到 Err（poll 错误送达 oneshot 或超时兜底），绝不永久挂起。
        // 计时断言：错误送达路径真实工作时应远快于内部 3s 超时返回（loopback 拒绝毫秒级，
        // 最多错过首轮错误再等一轮 100ms 退避 ≈ <1s；走满 3s 说明只有超时兜底在工作）。
        let t0 = tokio::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), io.request(b"\x01", 3000))
            .await
            .expect("request 不应挂死");
        let elapsed = t0.elapsed();
        assert!(r.is_err(), "broker 不可达时 request 应 Err，got: {r:?}");
        assert!(
            elapsed < Duration::from_millis(2500),
            "request 应经 pump 错误送达快速失败（{elapsed:?}），而非 3s 超时兜底"
        );
        // 错误确实经内部通道冒出（连接失败由 pump 感知并上报，宿主可观测）。
        let first_err = tokio::time::timeout(Duration::from_secs(3), io.err_rx.recv())
            .await
            .expect("err 通道应有连接错误")
            .expect("err 通道不应关闭");
        assert!(!first_err.is_empty());

        // (3) pump 存活且持续重连 poll（≥2 轮错误证明 poll 报错后可继续 poll）。
        let mut errors = 1usize; // 上面已取一条
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while errors < 2 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), io.err_rx.recv()).await {
                Ok(Some(_)) => errors += 1,
                Ok(None) => break, // 通道关闭（pump 死）→ 下面 is_finished 断言兜底
                Err(_) => {}       // 本窗口无新错误，继续等到 deadline
            }
        }
        assert!(
            !io.pump.is_finished(),
            "poll 错误后 pump 应存活继续重连，不应 panic/退出"
        );
        assert!(
            errors >= 2,
            "poll 报错后应可继续 poll（重连语义），错误数 {errors} < 2"
        );

        // (4) 无被动帧时 recv_idle → None（不 panic、不挂起）。
        assert!(io.recv_idle().await.is_none());
    }
}
