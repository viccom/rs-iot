//! Lua 插件配置：传输 / 周期 / 沙箱参数 / `parse_cfg`（解析 `app_config`）。
//!
//! `LuaCfg` struct 级 `#[allow(dead_code)]`：`serial_port`（serial 后端 Plan 6）、
//! `timeout_ms`（Lua 侧 `ctx:request` 显式传）、`recv_mode`（MVP 总 `Timeout`）三字段
//! lib build 仍未读，struct 级 allow 抑制字段级 dead_code；其余项（`parse_cfg`/
//! `Transport`/`RecvMode`）已被 `run` 消费，不再 dead。

// ---------- luaClient 表单默认值（审计 C1；前端 pluginSchemas.ts 镜像契约，改动须双侧同步） ----------
pub(crate) const DEFAULT_PERIOD_SECS: u64 = 3;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 200;
pub(crate) const DEFAULT_RECV_IDLE_MS: u64 = 50;
pub(crate) const DEFAULT_RECONNECT_SECS: u64 = 5;

/// 传输模式（Plan 1 仅实现 tcp；serial/udp 框架预留，Task 7 接入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// TCP 连接（`ip:port`）。
    Tcp,
    /// 串口直连（RS-485/RS-232）。
    Serial,
    /// UDP datagram（部分 DTU）。
    Udp,
    /// MQTT 透传（Q10/spike-2 方案 a，T11）：request 走 req_topic 发布 + resp_topic
    /// 首帧即响应；passive_topics 无在飞 request 时的推送 → on_data。
    Mqtt,
}

impl Transport {
    /// lua `ctx:transport()` / 日志用的字符串形式（与 `app_config.kind` 一致）。
    ///
    /// 脚本据此选帧格式（serial → PDU+CRC16、tcp → MBAP 头、udp → 裸 RTU/自定义）。
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Transport::Tcp => "tcp",
            Transport::Serial => "serial",
            Transport::Udp => "udp",
            Transport::Mqtt => "mqtt",
        }
    }
}

/// 接收成帧模式（spec §8）。Plan 1 MVP 仅用 `Timeout`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecvMode {
    /// 读到静默 idle_ms 返回累积字节。
    Timeout,
}

/// 串口校验（对标 modbus_client `ParityKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parity {
    /// 无校验。
    None,
    /// 偶校验。
    Even,
    /// 奇校验。
    Odd,
}

/// 串口流控（对标 modbus_client `FlowKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlowControl {
    /// 无流控。
    None,
    /// 软件流控（XON/XOFF）。
    Software,
    /// 硬件流控（RTS/CTS）。
    Hardware,
}

/// 从 `app_config` 解析的插件配置。
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct LuaCfg {
    pub(crate) transport: Transport,
    /// tcp/udp/mqtt 的地址（serial 时为空；mqtt = broker `host:port`）。
    pub(crate) endpoint: String,
    /// mqtt 透传 req topic（`ctx:request` 发布目的地，T11）。
    pub(crate) req_topic: String,
    /// mqtt 透传 resp topic（在飞 request 期间首帧即响应，T11）。
    pub(crate) resp_topic: String,
    /// mqtt 透传被动帧 topics（无在飞 request 时收到 → on_data，可空，T11）。
    pub(crate) passive_topics: Vec<String>,
    /// serial 串口名。
    pub(crate) serial_port: Option<String>,
    /// serial 波特率（默认 9600）。
    pub(crate) baud_rate: u32,
    /// serial 数据位（5/6/7/8，默认 8）。
    pub(crate) data_bits: u8,
    /// serial 校验（默认 None）。
    pub(crate) parity: Parity,
    /// serial 停止位（1/2，默认 1）。
    pub(crate) stop_bits: u8,
    /// serial 流控（默认 None）。
    pub(crate) flow_control: FlowControl,
    pub(crate) period: u64,
    /// ctx:request 默认超时（毫秒）。
    pub(super) timeout_ms: u64,
    pub(super) recv_mode: RecvMode,
    pub(crate) recv_idle_ms: u64,
    pub(super) reconnect_secs: u64,
    /// VM 内存限额（MB）。
    pub(super) mem_limit_mb: usize,
    /// 指令 hook 阈值（spec §9.2）。
    pub(crate) hook_instr_limit: u32,
    /// 连续 collect 失败 N 次标记 degraded（spec §9.6）。
    pub(crate) fail_threshold: u32,
    /// 单回调时长预算（毫秒，Q3/§4.5-1：collect/on_write/on_data 每次调用包 timeout，
    /// 超时跳过本轮 + tick_fail 计数，连续超限走 fail_threshold → degraded）。
    pub(crate) callback_timeout_ms: u64,
    /// 脚本目录名（→ scripts/<script>/main.lua）。
    pub(crate) script: String,
}

/// 解析 `app_config`。tcp 默认；period=0 表示纯被动（仅 on_data，Plan 1 不用，保留）。
pub(crate) fn parse_cfg(app: &serde_json::Value) -> Result<LuaCfg, String> {
    let script = app
        .get("script")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.script missing（脚本目录名）".to_string())?
        .to_string();
    if !script
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || script.is_empty()
    {
        return Err(format!(
            "非法 script 名 '{script}'（仅 [A-Za-z0-9_-]，防路径穿越）"
        ));
    }

    let kind = app.get("kind").and_then(|v| v.as_str()).unwrap_or("tcp");
    let transport = match kind {
        "tcp" => Transport::Tcp,
        "serial" => Transport::Serial,
        "udp" => Transport::Udp,
        "mqtt" => Transport::Mqtt,
        other => return Err(format!("unsupported kind '{other}'（tcp/serial/udp/mqtt）")),
    };
    let (endpoint, serial_port) = match transport {
        Transport::Serial => {
            let sp = app
                .get("serial_port")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "serial 需要 serial_port".to_string())?
                .to_string();
            (String::new(), Some(sp))
        }
        _ => {
            let ep = app
                .get("endpoint")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "app_config.endpoint missing".to_string())?
                .to_string();
            (ep, None)
        }
    };
    // mqtt 透传 topics（T11）：req/resp 必填（request 闭环两端）；passive 可选（空 =
    // 纯 request/resp 场景，on_data 不触发）。topic 模板占位符（`{devid}` 等）列后续
    // （设计 §5.4），当前为固定字符串直用。
    let (req_topic, resp_topic, passive_topics) = if transport == Transport::Mqtt {
        let req = app
            .get("req_topic")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "mqtt 需要 req_topic（request 发布目的地）".to_string())?
            .to_string();
        let resp = app
            .get("resp_topic")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "mqtt 需要 resp_topic（在飞 request 首帧即响应）".to_string())?
            .to_string();
        let passive = app
            .get("passive_topics")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (req, resp, passive)
    } else {
        (String::new(), String::new(), Vec::new())
    };

    let baud_rate = app
        .get("baud_rate")
        .and_then(|v| v.as_u64())
        .unwrap_or(9600) as u32;
    let data_bits = app.get("data_bits").and_then(|v| v.as_u64()).unwrap_or(8) as u8;
    let stop_bits = app.get("stop_bits").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
    let parity = match app.get("parity").and_then(|v| v.as_str()).unwrap_or("none") {
        "none" => Parity::None,
        "even" => Parity::Even,
        "odd" => Parity::Odd,
        other => return Err(format!("unsupported parity '{other}'（none/even/odd）")),
    };
    let flow_control = match app
        .get("flow_control")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
    {
        "none" => FlowControl::None,
        "software" => FlowControl::Software,
        "hardware" => FlowControl::Hardware,
        other => {
            return Err(format!(
                "unsupported flow_control '{other}'（none/software/hardware）"
            ))
        }
    };

    // 表单默认值常量（审计 C1）：前端 pluginSchemas.ts luaClient 镜像此组值
    //（改动须双侧同步；defaults 锁定测试 + schema-values.test.ts 锚定）。
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_PERIOD_SECS);
    let timeout_ms = app
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .max(10);
    let recv_idle_ms = app
        .get("recv_idle_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RECV_IDLE_MS);
    let reconnect_secs = app
        .get("reconnect_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RECONNECT_SECS)
        .max(1);
    let mem_limit_mb = app
        .get("mem_limit_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(super::budget::DEFAULT_MEM_LIMIT_MB as u64) as usize;
    let hook_instr_limit =
        app.get("hook_instr_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(super::budget::DEFAULT_HOOK_INSTR_LIMIT as u64) as u32;
    let fail_threshold = app
        .get("fail_threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(super::budget::DEFAULT_FAIL_THRESHOLD as u64) as u32;
    // Q3/§4.5-1：缺省 10s；max(1) 钳下限（0 会把每个回调打成必超时）。
    let callback_timeout_ms = app
        .get("callback_timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(super::budget::DEFAULT_CALLBACK_TIMEOUT_MS)
        .max(1);

    Ok(LuaCfg {
        transport,
        endpoint,
        req_topic,
        resp_topic,
        passive_topics,
        serial_port,
        baud_rate,
        data_bits,
        parity,
        stop_bits,
        flow_control,
        period,
        timeout_ms,
        recv_mode: RecvMode::Timeout,
        recv_idle_ms,
        reconnect_secs,
        mem_limit_mb,
        hook_instr_limit,
        fail_threshold,
        callback_timeout_ms,
        script,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tcp_defaults() {
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.transport, Transport::Tcp);
        assert_eq!(cfg.period, 3);
        assert_eq!(cfg.timeout_ms, 200);
        assert_eq!(cfg.mem_limit_mb, 32);
    }

    #[test]
    fn parse_rejects_bad_script_name() {
        let r = parse_cfg(&json!({"script":"../etc","endpoint":"x"}));
        assert!(r.is_err());
        let r = parse_cfg(&json!({"script":"","endpoint":"x"}));
        assert!(r.is_err());
    }

    #[test]
    fn parse_missing_script_errors() {
        assert!(parse_cfg(&json!({"endpoint":"x"})).is_err());
    }

    #[test]
    fn parse_serial_needs_port() {
        assert!(parse_cfg(&json!({"script":"a","kind":"serial"})).is_err());
        let cfg = parse_cfg(&json!({"script":"a","kind":"serial","serial_port":"COM3"})).unwrap();
        assert_eq!(cfg.transport, Transport::Serial);
        assert_eq!(cfg.serial_port.as_deref(), Some("COM3"));
    }

    #[test]
    fn parse_fail_threshold_default_and_override() {
        // 缺省 = 10（spec §9.6 连续失败 N 次标 degraded）。
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.fail_threshold, 10);
        // 显式覆盖。
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"x","fail_threshold":3})).unwrap();
        assert_eq!(cfg.fail_threshold, 3);
    }

    #[test]
    fn parse_serial_params() {
        let cfg = parse_cfg(&json!({
            "script":"a","kind":"serial","serial_port":"COM3",
            "baud_rate":19200,"data_bits":7,"parity":"even","stop_bits":2,"flow_control":"hardware"
        }))
        .unwrap();
        assert_eq!(cfg.transport, Transport::Serial);
        assert_eq!(cfg.serial_port.as_deref(), Some("COM3"));
        assert_eq!(cfg.baud_rate, 19200);
        assert_eq!(cfg.data_bits, 7);
        assert_eq!(cfg.parity, Parity::Even);
        assert_eq!(cfg.stop_bits, 2);
        assert_eq!(cfg.flow_control, FlowControl::Hardware);
    }

    #[test]
    fn parse_serial_defaults() {
        // 缺省：9600/8N1/NONE（对标 modbus_client）。
        let cfg = parse_cfg(&json!({"script":"a","kind":"serial","serial_port":"COM3"})).unwrap();
        assert_eq!(cfg.baud_rate, 9600);
        assert_eq!(cfg.data_bits, 8);
        assert_eq!(cfg.parity, Parity::None);
        assert_eq!(cfg.stop_bits, 1);
        assert_eq!(cfg.flow_control, FlowControl::None);
    }

    /// T11：kind=mqtt 透传传输——req_topic/resp_topic 必填，passive_topics 可选，
    /// endpoint（broker addr）必填，transport() 返 "mqtt"。
    #[test]
    fn parse_mqtt_kind_topics() {
        // 缺 req_topic/resp_topic → Err（两者均为必填）。
        assert!(
            parse_cfg(&json!({"script":"a","kind":"mqtt","endpoint":"127.0.0.1:1883"})).is_err()
        );
        assert!(parse_cfg(&json!({
            "script":"a","kind":"mqtt","endpoint":"127.0.0.1:1883","req_topic":"dev/req"
        }))
        .is_err());
        assert!(parse_cfg(&json!({
            "script":"a","kind":"mqtt","endpoint":"127.0.0.1:1883","resp_topic":"dev/resp"
        }))
        .is_err());
        // 缺 endpoint（broker addr）→ Err（tcp/udp 同款必填）。
        assert!(parse_cfg(&json!({
            "script":"a","kind":"mqtt","req_topic":"dev/req","resp_topic":"dev/resp"
        }))
        .is_err());

        // 全量：transport=mqtt + topics 落位 + passive_topics 解析。
        let cfg = parse_cfg(&json!({
            "script":"a","kind":"mqtt","endpoint":"127.0.0.1:1883",
            "req_topic":"dev/req","resp_topic":"dev/resp",
            "passive_topics":["dev/push","dev/alarm"]
        }))
        .unwrap();
        assert_eq!(cfg.transport, Transport::Mqtt);
        assert_eq!(cfg.transport.as_str(), "mqtt");
        assert_eq!(cfg.endpoint, "127.0.0.1:1883");
        assert_eq!(cfg.req_topic, "dev/req");
        assert_eq!(cfg.resp_topic, "dev/resp");
        assert_eq!(
            cfg.passive_topics,
            vec!["dev/push".to_string(), "dev/alarm".to_string()]
        );

        // passive_topics 可缺省（空 vec——仅 request/resp 闭环场景）。
        let cfg = parse_cfg(&json!({
            "script":"a","kind":"mqtt","endpoint":"127.0.0.1:1883",
            "req_topic":"dev/req","resp_topic":"dev/resp"
        }))
        .unwrap();
        assert!(cfg.passive_topics.is_empty());
    }

    #[test]
    fn parse_bad_parity_rejected() {
        assert!(parse_cfg(
            &json!({"script":"a","kind":"serial","serial_port":"COM3","parity":"x"})
        )
        .is_err());
    }

    /// T5：callback_timeout_ms 缺省 10s（Q3/§4.5-1 单回调时长预算），可配，越界钳制 >0。
    #[test]
    fn parse_callback_timeout_default_override_and_clamp() {
        // 缺省 = 10_000ms。
        let cfg = parse_cfg(&json!({"script":"devA","endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.callback_timeout_ms, 10_000);
        // 显式覆盖。
        let cfg =
            parse_cfg(&json!({"script":"devA","endpoint":"x","callback_timeout_ms":200})).unwrap();
        assert_eq!(cfg.callback_timeout_ms, 200);
        // 0 钳到 1（防"预算为零=每回调必超时"的误配置把实例打死）。
        let cfg =
            parse_cfg(&json!({"script":"devA","endpoint":"x","callback_timeout_ms":0})).unwrap();
        assert_eq!(cfg.callback_timeout_ms, 1);
    }
}
