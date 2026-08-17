//! `mqttPub`：北向 MQTT 发布器（pull 模式，STEP 5 起）。
//!
//! 常驻独立 task：连接外部 broker，订阅 `<hwid>/command` 接收下行命令，
//! 周期（`publish_interval_secs`）从 lux 读取所有 `rtdata:<devid>` 并组装
//! `{hwid, devid, values, timestamp}` 后 `PUBLISH <hwid>/<devid>/datas`。
//!
//! 与 pipeline 解耦：pipeline 只写 lux rtdata，北向发布完全由本发布器周期抽取。

use crate::config::MqttConfig;
use crate::host::HostContext;
use crate::types::CtlMessage;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 北向 MQTT 发布器：周期从 lux 抽取 rtdata 发布（pull 模式，不再 impl Sink）。
#[derive(Debug)]
pub struct MqttPub {
    /// broker 配置（host/port/credentials/publish_interval_secs）。
    cfg: MqttConfig,
}

impl MqttPub {
    /// 构造。
    pub fn new(cfg: MqttConfig) -> Self {
        Self { cfg }
    }

    /// 常驻运行：连 broker、订阅命令、周期 publish rtdata，直到 `token` 取消。
    pub async fn run(self, ctx: HostContext, token: CancellationToken) {
        let hwid = ctx.hwid.clone();
        let mut opts = MqttOptions::new(hwid.to_string(), &self.cfg.host, self.cfg.port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        if let (Some(u), Some(p)) = (&self.cfg.username, &self.cfg.password) {
            opts.set_credentials(u, p);
        }
        let (client, mut connection) = AsyncClient::new(opts, 10);

        let cmd_topic = format!("{hwid}/command");
        if let Err(e) = client.subscribe(&cmd_topic, QoS::AtMostOnce).await {
            tracing::warn!("[mqtt] subscribe {cmd_topic} failed: {e}");
        } else {
            tracing::info!("[mqtt] subscribed {cmd_topic}");
        }
        tracing::info!(
            "[mqtt] publisher -> {}:{} (pull every {}s)",
            self.cfg.host,
            self.cfg.port,
            self.cfg.publish_interval_secs
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(self.cfg.publish_interval_secs));
        // 跳过首个立即 tick（启动瞬间无需发布）
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[mqtt] publisher stopping");
                    break;
                }
                ev = connection.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if let Ok(cmd) = serde_json::from_slice::<CtlMessage>(&p.payload) {
                                let inst = cmd.instance.to_string();
                                ctx.ctl_registry.send(&inst, cmd);
                            }
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            ctx.metrics.mqtt_connected(true);
                            tracing::info!("[mqtt] connected to broker");
                        }
                        Ok(_) => {}
                        Err(e) => {
                            ctx.metrics.mqtt_connected(false);
                            tracing::warn!("[mqtt] connection error: {e}");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                _ = ticker.tick() => {
                    publish_rtdata(&ctx, &hwid, &client, &ctx.metrics).await;
                }
            }
        }
    }
}

/// 从 lux 读取所有 `rtdata:<devid>`，组装并 `PUBLISH`。
///
/// lux rtdata 结构：key=`rtdata:<devid>`，hash field=`<devid>.<tag>`（或 `<tag>`），
/// value=`[ts, localTime, value]` JSON。组装为 `{hwid, devid, values:{field: value}, timestamp}`。
async fn publish_rtdata(
    ctx: &HostContext,
    hwid: &crate::types::Hwid,
    client: &AsyncClient,
    metrics: &crate::process_metrics::ProcessMetrics,
) {
    let keys = match ctx.lux.execute_value("KEYS", &["rtdata:*"]).await {
        Ok(lux::EmbeddedValue::Array(a)) => a,
        _ => return,
    };
    for k in keys {
        let key = match &k {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => continue,
        };
        let devid = key.strip_prefix("rtdata:").unwrap_or(&key);
        let arr = match ctx.lux.execute_value("HGETALL", &[&key]).await {
            Ok(lux::EmbeddedValue::Array(a)) => a,
            _ => continue,
        };
        let (values, latest_ts) = parse_rtdata_hash(&arr);
        let payload = serde_json::json!({
            "hwid": hwid,
            "devid": devid,
            "values": serde_json::Value::Object(values),
            "timestamp": latest_ts,
        });
        let topic = format!("{hwid}/{devid}/datas");
        match serde_json::to_vec(&payload) {
            Ok(bytes) => {
                match client
                    .publish(topic.clone(), QoS::AtMostOnce, false, bytes)
                    .await
                {
                    Ok(()) => metrics.mqtt_publish(),
                    Err(e) => {
                        metrics.mqtt_publish_fail();
                        tracing::warn!("[mqtt] publish {topic} failed: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("[mqtt] encode payload failed: {e}"),
        }
    }
}

/// 解析 `HGETALL rtdata:<devid>` 返回值（field → `[ts, localTime, value]` JSON）
/// 为 `values` map（field → value）+ 最新 ts。
///
/// 非 `[ts, localTime, value]` 格式的 cell 原样保留为字符串（兼容旧数据）。
fn parse_rtdata_hash(
    arr: &[lux::EmbeddedValue],
) -> (serde_json::Map<String, serde_json::Value>, i64) {
    let mut values = serde_json::Map::new();
    let mut latest_ts: i64 = 0;
    let mut i = 0;
    while i + 1 < arr.len() {
        let field = match &arr[i] {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => {
                i += 2;
                continue;
            }
        };
        let cell_raw = match &arr[i + 1] {
            lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(b).to_string(),
            lux::EmbeddedValue::Simple(s) => s.clone(),
            _ => {
                i += 2;
                continue;
            }
        };
        // cell = [ts, localTime, value]
        if let Ok(cell) = serde_json::from_str::<Vec<serde_json::Value>>(&cell_raw) {
            if cell.len() >= 3 {
                if let Some(ts) = cell[0].as_i64() {
                    latest_ts = latest_ts.max(ts);
                }
                values.insert(field, cell[2].clone());
            }
        } else {
            values.insert(field, serde_json::Value::String(cell_raw));
        }
        i += 2;
    }
    (values, latest_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> lux::EmbeddedValue {
        lux::EmbeddedValue::Bulk(s.as_bytes().to_vec().into())
    }

    #[test]
    fn parse_rtdata_hash_extracts_values_and_max_ts() {
        let arr = vec![
            bulk("devA.temp"),
            bulk(r#"[1700000000, "2023-11-14 20:13:20", "89.9"]"#),
            bulk("devA.humidity"),
            bulk(r#"[1700000100, "2023-11-14 20:15:00", "45.2"]"#),
        ];
        let (values, ts) = parse_rtdata_hash(&arr);
        assert_eq!(ts, 1700000100, "latest ts should win");
        assert_eq!(values.get("devA.temp").unwrap(), "89.9");
        assert_eq!(values.get("devA.humidity").unwrap(), "45.2");
    }

    #[test]
    fn parse_rtdata_hash_keeps_unparseable_cell_as_string() {
        let arr = vec![bulk("raw"), bulk("not-json")];
        let (values, ts) = parse_rtdata_hash(&arr);
        assert_eq!(ts, 0);
        assert_eq!(values.get("raw").unwrap(), "not-json");
    }

    #[test]
    fn parse_rtdata_hash_skips_short_cells() {
        let arr = vec![
            bulk("a"),
            bulk("[1]"),
            bulk("b"),
            bulk(r#"[300, "t", "2"]"#),
        ];
        let (values, ts) = parse_rtdata_hash(&arr);
        assert_eq!(ts, 300);
        assert!(!values.contains_key("a"), "len<3 cell must be skipped");
        assert_eq!(values.get("b").unwrap(), "2");
    }
}
