//! Lua 插件配置：传输 / 周期 / 沙箱参数 / `parse_cfg`（解析 `app_config`）。
//!
//! `LuaCfg` struct 级 `#[allow(dead_code)]`：`serial_port`（serial 后端 Plan 6）、
//! `timeout_ms`（Lua 侧 `ctx:request` 显式传）、`recv_mode`（MVP 总 `Timeout`）三字段
//! lib build 仍未读，struct 级 allow 抑制字段级 dead_code；其余项（`parse_cfg`/
//! `Transport`/`RecvMode`）已被 `run` 消费，不再 dead。

/// 传输模式（Plan 1 仅实现 tcp；serial/udp 框架预留，Task 7 接入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transport {
    /// TCP 连接（`ip:port`）。
    Tcp,
    /// 串口直连（RS-485/RS-232）。
    Serial,
    /// UDP datagram（部分 DTU）。
    Udp,
}

/// 接收成帧模式（spec §8）。Plan 1 MVP 仅用 `Timeout`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecvMode {
    /// 读到静默 idle_ms 返回累积字节。
    Timeout,
}

/// 从 `app_config` 解析的插件配置。
#[allow(dead_code)]
#[derive(Debug)]
pub(super) struct LuaCfg {
    pub(super) transport: Transport,
    /// tcp/udp 的 `ip:port`（serial 时为空）。
    pub(super) endpoint: String,
    /// serial 串口名。
    pub(super) serial_port: Option<String>,
    pub(super) period: u64,
    /// ctx:request 默认超时（毫秒）。
    pub(super) timeout_ms: u64,
    pub(super) recv_mode: RecvMode,
    pub(super) recv_idle_ms: u64,
    pub(super) reconnect_secs: u64,
    /// VM 内存限额（MB）。
    pub(super) mem_limit_mb: usize,
    /// 指令 hook 阈值（spec §9.2）。
    pub(super) hook_instr_limit: u32,
    /// 脚本目录名（→ scripts/<script>/main.lua）。
    pub(super) script: String,
}

/// 解析 `app_config`。tcp 默认；period=0 表示纯被动（仅 on_data，Plan 1 不用，保留）。
pub(super) fn parse_cfg(app: &serde_json::Value) -> Result<LuaCfg, String> {
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
        other => return Err(format!("unsupported kind '{other}'（tcp/serial/udp）")),
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

    let period = app.get("period").and_then(|v| v.as_u64()).unwrap_or(3);
    let timeout_ms = app
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(200)
        .max(10);
    let recv_idle_ms = app
        .get("recv_idle_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(50);
    let reconnect_secs = app
        .get("reconnect_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .max(1);
    let mem_limit_mb = app
        .get("mem_limit_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(32) as usize;
    let hook_instr_limit = app
        .get("hook_instr_limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(1_000_000) as u32;

    Ok(LuaCfg {
        transport,
        endpoint,
        serial_port,
        period,
        timeout_ms,
        recv_mode: RecvMode::Timeout,
        recv_idle_ms,
        reconnect_secs,
        mem_limit_mb,
        hook_instr_limit,
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
}
