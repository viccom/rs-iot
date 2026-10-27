//! Modbus 插件配置：传输模式 / 串口参数 / `parse_cfg`（解析 `app_config`）。

/// 传输模式（扁平 `kind` 映射）。RTU 三种均经 `rtu::attach_slave` 产 `Context`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transport {
    /// Modbus TCP（MBAP 头）。
    Tcp,
    /// RTU over serial（RS-485/RS-232 串口直连）。
    RtuSerial,
    /// RTU over TCP（串口服务器/网关透传，RTU 帧+CRC 走 TCP，无 MBAP 头）。
    RtuTcp,
    /// RTU over UDP（部分 DTU 用 UDP datagram 透传 RTU 帧）。
    RtuUdp,
}

/// 串口校验位（映射 `serialport::Parity`，工业默认 None）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ParityKind {
    #[default]
    None,
    Even,
    Odd,
}

/// 串口流控（映射 `serialport::FlowControl`，工业默认 None）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum FlowKind {
    #[default]
    None,
    Software,
    Hardware,
}

/// 多 slave 轮询策略（半双工 RS485 物理串行，区别在 (slave,region) 遍历顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum PollStrategy {
    /// 轮转：每轮对每个 slave 各发 1 条报文（各取下一 region），多轮采完。
    /// 所有 slave 齐头并进、刷新均匀；单 slave 故障天然隔离。**默认**。
    #[default]
    RoundRobin,
    /// 逐设备：一个 slave 所有 region 采完再换下一个。单设备多点时间一致。
    Sequential,
}

/// 从 `app_config` 解析的插件配置。`transport` 决定必填字段（rtu_serial 需 serial_port，余需 endpoint）。
#[derive(Debug)]
pub(super) struct ModbusCfg {
    pub(super) transport: Transport,
    /// tcp/rtu_tcp/rtu_udp 的 `ip:port`（rtu_serial 时为空）。
    pub(super) endpoint: String,
    /// rtu_serial 串口名（COM3 / /dev/ttyUSB0）。
    pub(super) serial_port: Option<String>,
    /// rtu_serial 波特率（默认 9600）。
    pub(super) baud_rate: u32,
    /// rtu_serial 数据位（5/6/7/8，默认 8）。
    pub(super) data_bits: u8,
    /// rtu_serial 校验位（默认 None）。
    pub(super) parity: ParityKind,
    /// rtu_serial 停止位（1/2，默认 1）。
    pub(super) stop_bits: u8,
    /// rtu_serial 流控（默认 None）。
    pub(super) flow_control: FlowKind,
    pub(super) slave_id: u8,
    pub(super) period: u64,
    /// 相邻两点最大单空洞（超过则分裂成两次读）。
    pub(super) gap_threshold: u16,
    /// 单次请求最大单元数（coil/discrete 为 bit 数，holding/input 为寄存器数；协议上限 125）。
    pub(super) max_regs: u16,
    pub(super) reconnect_secs: u64,
    /// 多 slave 轮询策略（默认 RoundRobin）。采集循环消费。
    pub(super) poll_strategy: PollStrategy,
    /// 单次读请求超时（秒）。slave 无响应时跳过该请求（故障隔离），默认 2。
    pub(super) request_timeout_secs: u64,
}

// ---------- 表单默认值（单一事实源；前端 pluginSchemas.ts modbusClient 镜像
// 此组值，改动须双侧同步——镜像契约由 config.rs defaults 锁定测试 +
// frontend schema-values.test.ts 双向锚定，审计 C1） ----------
pub(super) const DEFAULT_KIND: &str = "tcp";
pub(super) const DEFAULT_BAUD_RATE: u64 = 9600;
pub(super) const DEFAULT_DATA_BITS: u64 = 8;
pub(super) const DEFAULT_STOP_BITS: u64 = 1;
pub(super) const DEFAULT_SLAVE_ID: u64 = 1;
pub(super) const DEFAULT_PERIOD_SECS: u64 = 3;
pub(super) const DEFAULT_GAP_THRESHOLD: u64 = 16;
pub(super) const DEFAULT_MAX_REGS: u64 = 100;
pub(super) const DEFAULT_RECONNECT_SECS: u64 = 2;
pub(super) const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 2;
// match None 分支的默认（parity/flow_control/poll_strategy 无 unwrap_or
// 字面量，语义即缺省）：parity=none / flow=none / poll=round_robin。

/// 解析 `app_config`。`kind` 扁平：tcp（默认，向后兼容）/ rtu_serial / rtu_tcp / rtu_udp。
/// rtu_serial 必须有 `serial_port`，其余必须有 `endpoint`。串口参数缺省 9600/8N1/NONE。
pub(super) fn parse_cfg(app: &serde_json::Value) -> Result<ModbusCfg, String> {
    let kind = app
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_KIND);
    let transport = match kind {
        "tcp" => Transport::Tcp,
        "rtu_serial" => Transport::RtuSerial,
        "rtu_tcp" => Transport::RtuTcp,
        "rtu_udp" => Transport::RtuUdp,
        other => {
            return Err(format!(
                "unsupported kind '{other}': 仅支持 tcp / rtu_serial / rtu_tcp / rtu_udp（ASCII 见文档后置）"
            ));
        }
    };
    // rtu_serial 必须有 serial_port；其余必须有 endpoint。
    let (endpoint, serial_port) = match transport {
        Transport::RtuSerial => {
            let sp = app
                .get("serial_port")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "rtu_serial 需要 serial_port（如 COM3 / /dev/ttyUSB0）".to_string())?
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
    let baud_rate = app
        .get("baud_rate")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_BAUD_RATE) as u32;
    let data_bits = app
        .get("data_bits")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_DATA_BITS) as u8;
    if !matches!(data_bits, 5..=8) {
        return Err(format!("unsupported data_bits {data_bits}（5/6/7/8）"));
    }
    let parity = match app.get("parity").and_then(|v| v.as_str()) {
        Some("even") => ParityKind::Even,
        Some("odd") => ParityKind::Odd,
        Some("none") | None => ParityKind::None,
        Some(other) => return Err(format!("unsupported parity '{other}'（none/even/odd）")),
    };
    let stop_bits = app
        .get("stop_bits")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_STOP_BITS) as u8;
    if !matches!(stop_bits, 1 | 2) {
        return Err(format!("unsupported stop_bits {stop_bits}（1/2）"));
    }
    let flow_control = match app.get("flow_control").and_then(|v| v.as_str()) {
        Some("software") => FlowKind::Software,
        Some("hardware") => FlowKind::Hardware,
        Some("none") | None => FlowKind::None,
        Some(other) => {
            return Err(format!(
                "unsupported flow_control '{other}'（none/software/hardware）"
            ))
        }
    };
    let slave_id = app
        .get("slave_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_SLAVE_ID) as u8;
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_PERIOD_SECS)
        .max(1);
    let gap_threshold = app
        .get("gap_threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_GAP_THRESHOLD) as u16;
    let max_regs = app
        .get("max_regs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_REGS)
        .clamp(1, 125) as u16;
    let reconnect_secs = app
        .get("reconnect_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_RECONNECT_SECS)
        .max(1);
    let poll_strategy = match app.get("poll_strategy").and_then(|v| v.as_str()) {
        Some("round_robin") | None => PollStrategy::RoundRobin,
        Some("sequential") => PollStrategy::Sequential,
        Some(other) => {
            return Err(format!(
                "unsupported poll_strategy '{other}'（round_robin / sequential）"
            ))
        }
    };
    let request_timeout_secs = app
        .get("request_timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS)
        .max(1);
    Ok(ModbusCfg {
        transport,
        endpoint,
        serial_port,
        baud_rate,
        data_bits,
        parity,
        stop_bits,
        flow_control,
        slave_id,
        period,
        gap_threshold,
        max_regs,
        reconnect_secs,
        poll_strategy,
        request_timeout_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表单默认值锁定（审计 C1）：空 app_config（合法最小形态 kind=tcp +
    /// endpoint）→ 全字段等于 DEFAULT_* 常量——后端侧防静默改，前端
    /// pluginSchemas.ts 以此组值为镜像契约（schema-values.test.ts 锚定）。
    #[test]
    fn defaults_match_frontend_mirror_contract() {
        let cfg = parse_cfg(&serde_json::json!({"kind": "tcp", "endpoint": "127.0.0.1:502"}))
            .expect("最小合法配置");
        assert_eq!(cfg.period, DEFAULT_PERIOD_SECS);
        assert_eq!(cfg.reconnect_secs, DEFAULT_RECONNECT_SECS);
        assert_eq!(cfg.request_timeout_secs, DEFAULT_REQUEST_TIMEOUT_SECS);
        assert_eq!(cfg.gap_threshold, DEFAULT_GAP_THRESHOLD as u16);
        assert_eq!(cfg.max_regs, DEFAULT_MAX_REGS as u16);
        // 串口参数（tcp 形态下也解析，供 rtu 切换）
        assert_eq!(cfg.baud_rate, DEFAULT_BAUD_RATE as u32);
        assert_eq!(cfg.slave_id, DEFAULT_SLAVE_ID as u8);
        // match None 分支默认
        assert!(matches!(cfg.parity, super::ParityKind::None));
        assert!(matches!(cfg.flow_control, super::FlowKind::None));
        assert!(matches!(cfg.poll_strategy, super::PollStrategy::RoundRobin));
    }
}
