//! `mqttPub`：北向 MQTT 发布 Sink。
//!
//! 对标 Lite_IOT `bMqtt`（paho 客户端 + broker）。rs-iot 用原生 Rust 的 `rumqttc`
//! 连接**外部公网/本地 broker**：
//! - 发布：每条 Sample → `PUBLISH <hwid>/<devid>/datas <json>`
//! - 订阅：`<hwid>/command` → 解析 `CtlMessage` → 经 `CtlRegistry` 路由到实例

use crate::host::HostContext;
use crate::plugin::traits::Sink;
use crate::types::{CtlMessage, InstConfig, Sample};
use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 北向 MQTT 发布 Sink（`mqttPub`）；STEP 5 将重写为 pull 模式的常驻 `MqttPublisher`。
#[derive(Debug, Default)]
pub struct MqttPub;

struct BrokerCfg {
    host: String,
    port: u16,
    client_id: String,
    username: Option<String>,
    password: Option<String>,
}

impl BrokerCfg {
    fn parse(hwid: &str, v: &serde_json::Value) -> Self {
        Self {
            host: v
                .get("host")
                .and_then(|x| x.as_str())
                .unwrap_or("localhost")
                .to_string(),
            port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(1883) as u16,
            client_id: v
                .get("clientId")
                .and_then(|x| x.as_str())
                .unwrap_or(hwid)
                .to_string(),
            username: v.get("username").and_then(|x| x.as_str()).map(String::from),
            password: v.get("password").and_then(|x| x.as_str()).map(String::from),
        }
    }
}

#[async_trait]
impl Sink for MqttPub {
    fn name(&self) -> &'static str {
        "mqttPub"
    }

    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        mut rx: mpsc::Receiver<Sample>,
        mut cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    ) {
        let b = BrokerCfg::parse(&ctx.hwid, &inst.app_config);
        let mut opts = MqttOptions::new(&b.client_id, &b.host, b.port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        if let (Some(u), Some(p)) = (b.username, b.password) {
            opts.set_credentials(u, p);
        }
        let (client, mut connection) = AsyncClient::new(opts, 10);

        // 订阅命令下行：<hwid>/command
        let cmd_topic = format!("{}/command", ctx.hwid);
        if let Err(e) = client.subscribe(&cmd_topic, QoS::AtMostOnce).await {
            tracing::warn!("[mqttPub] subscribe {} failed: {e}", cmd_topic);
        } else {
            tracing::info!("[mqttPub] subscribed {}", cmd_topic);
        }

        tracing::info!(
            "[mqttPub] inst {} -> broker {}:{} (clientId={})",
            inst.inst_id,
            b.host,
            b.port,
            b.client_id
        );

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[mqttPub] inst {} stopping", inst.inst_id);
                    break;
                }
                s = rx.recv() => {
                    if let Some(sample) = s {
                        let topic = format!("{}/{}/datas", sample.hwid, sample.devid);
                        match serde_json::to_vec(&sample) {
                            Ok(payload) => {
                                if let Err(e) = client.publish(topic.clone(), QoS::AtMostOnce, false, payload).await {
                                    tracing::warn!("[mqttPub] publish {} failed: {e}", topic);
                                }
                            }
                            Err(e) => tracing::warn!("[mqttPub] encode sample failed: {e}"),
                        }
                    }
                }
                m = cmd_rx.recv() => {
                    if let Some(m) = m {
                        tracing::info!("[mqttPub] inst {} got cmd: {}", inst.inst_id, m.cmd);
                    }
                }
                ev = connection.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Ok(cmd) = serde_json::from_slice::<CtlMessage>(&p.payload) {
                                let inst = cmd.instance.clone();
                                ctx.ctl_registry.send(&inst, cmd);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[mqttPub] connection error: {e}");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }
    }
}

inventory::submit! {
    crate::plugin::registry::SinkEntry {
        name: "mqttPub",
        create: || Box::new(MqttPub),
    }
}
