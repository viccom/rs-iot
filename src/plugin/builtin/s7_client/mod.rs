//! `s7Client`：西门子 S7 南向采集插件（tokio-s7 0.1，纯 Rust，ISO-on-TCP :102）。
//!
//! 周期连接 S7-200/200Smart/300/400/1200/1500/Sinumerik828D PLC，按 `devices` 批量读取
//! 变量（单请求超 `max_items_per_request` 项自动分片，多 PDU 顺序读），组装 `Sample`
//! 推入 hub；同时消费下行命令（`cmd="write"`）写回 PLC。
//!
//! 配置（`InstConfig::app_config` JSON）：
//! ```json
//! {
//!   "endpoint": "192.168.0.100",
//!   "plc_type": "s1200",
//!   "rack": 0, "slot": 1,
//!   "period": 2,
//!   "timeout_ms": 5000,
//!   "auto_reconnect": true,
//!   "connection_type": "pg",
//!   "max_pdu_size": 960,
//!   "max_items_per_request": 20,
//!   "verify_write": true
//! }
//! ```
//! - `endpoint`：PLC 主机（IPv4 或方括号 IPv6）。**可带端口**（`192.168.0.100:102`、
//!   `[::1]:102`，前端 schema 提示的 `host:port` 形态即此），带端口时优先于 `port`
//!   键（S-18：旧实现把整串当 host 交给库，库再拼 `host:port` → `ip:102:102` 必连不上）
//! - `port`：端口（缺省 102，S7 标准 ISO-on-TCP；非标端口 PLC 真实存在，亦便于
//!   测试连进程内 mock server）。`endpoint` 自带端口时不生效
//! - `plc_type`：`s200`/`s200smart`/`s300`/`s400`/`s1200`/`s1500`/`sinumerik828d`
//!   （缺省 `s1200`，大小写不敏感）
//! - `rack`/`slot`：可选；按 `plc_type` 缺省（`with_plc_type` 自动填）。只给其中一个
//!   时另一个取 0 并 warn 一次（S-20：旧实现要求两者同时给，单独给 rack 被静默忽略）
//! - `period`：采集周期秒（缺省 2，最小 1）
//! - `timeout_ms`/`auto_reconnect`：可选（缺省 5000 / true）
//! - `connection_type`：`pg`/`pc`/`op`（缺省 `pg`）——**tokio-s7 0.1.7 不消费该字段**
//!   （只存不发，TSAP 仅由 `plc_type` + rack/slot 算出），显式配了非默认值时启动
//!   warn 一条说明它被忽略（S-9），不假装支持
//! - `max_pdu_size`：Setup Communication 里**提出**的 PDU 上限（缺省 960，范围
//!   240-4096），同时是协商值的容忍上界——对端回更大的 PDU 时按本值收紧分片（S-3）
//! - `max_items_per_request`：单请求项数上限（缺省 20，clamp 1-20）。实际每请求项数
//!   = `min(本值, max(1, (协商 PDU − 20) / 12))`——**库不做自动分批**（超 PDU 直接
//!   报错并触发 S-2 热重连），插件按协商 PDU 分片多请求（S-3）
//! - `verify_write`：写后读回校验（缺省 true，S-1）——tokio-s7 丢弃写应答的
//!   item 级 `return_code`（写失败也返回 Ok），本开关按同地址读回比对做临时防线
//!
//! 变量映射（复用 `devices`，与 opcua/modbus 对齐）：
//! - `DeviceSpec.value` = 设备 ID（`rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = S7 地址串（如 `DB1.DBD0` / `M10.0` / `I0.0` / `V100`）
//! - `ChildSpec.label` = values map 的 key（空则用地址串）
//! - `ChildSpec.type` = **kind**（强制显式，见下表）
//!
//! # kind 与地址语法（重要）
//!
//! tokio-s7 的地址解析器按"位置 + 数字过滤"工作：类型前缀字符 `X/B/W/D/DBX/DBB/DBW/DBD`
//! **纯装饰、被忽略**，`extract_number` 只保留 ASCII 数字（`<REG>item.rs:211-222`）。
//! **变量位宽由 `kind` 决定，不是地址前缀**；位寻址靠点段数（`DB1.DBX0.0` 3 段 /
//! `M10.5` 2 段）。库对坏形态是静默取错，故插件在**配置期严格校验**（见
//! [`address`] 模块），只接受：
//!
//! - `DB<n>.DBX<b>.<bit>`（仅 `bool`）、`DB<n>.DBB<b>` / `DB<n>.DBW<b>` / `DB<n>.DBD<b>`
//!   （非位 kind，宽度由 kind 决定——`DB1.DBW0` + `i32` = 从字地址起读 4 字节）
//! - `I/Q/M/V` 区：`<A>[B|W|D]<b>`（非位）与 `<A><b>.<bit>`（`bool`）；`V` 经库固定映射到 DB1
//!
//! 不支持（配置期明确报错，不留运行期怪错）：`%DB1.DBX0.0`（TIA 前缀，提示去掉）、
//! `DB1.DBX0.5` + 数值 kind（库会读 `DB1.DBW0`）、`DB1.0`（缺尺寸字母）、
//! `T`/`C` 区（BCD 语义）、`P`/`PIW`/`PQW`（外设区）、`AI`/`AQ`/`SM`（S7-200 区）。
//!
//! | kind | 位宽 | 示例地址 | 说明 |
//! |---|---|---|---|
//! | `bool` | 1 bit | `M10.5` / `DB1.DBX0.0` | 位寻址，必须配位地址 |
//! | `byte`/`i8` | 1 字节 | `DB1.DBB0` / `MB10` | **有符号** i8（0xFF → -1） |
//! | `ubyte`/`u8` | 1 字节 | `DB1.DBB0` / `MB10` | 无符号 u8（0-255，PLC BYTE 的正确映射） |
//! | `i16`/`u16` | 2 字节 | `DB1.DBW0` | 大端 |
//! | `i32`/`u32` | 4 字节 | `DB1.DBD0` | 大端 |
//! | `i64`/`u64` | 8 字节 | `DB1.DBD0` | 大端（`u64` 读成 `SampleValue::UInt`，不符号翻转） |
//! | `f32` | 4 字节 | `DB1.DBD0` | IEEE754 大端 |
//! | `f64` | 8 字节 | `DB1.DBD0` | IEEE754 大端 |
//! | `string[N]` | N+2 字节 | `DB1.DBB0` | S7 STRING（2B 头：max+actual；ISO-8859-1）。**必须写 `[N]`**（与 PLC 声明一致），缺 `[N]` 的点位启动期拒绝（S-8：库按 254 全长读，240B PDU 必失败） |
//! | `wstring[N]` | 2N+4 字节 | `DB1.DBB0` | S7 WSTRING（UTF-16BE，4B 头）。同上，**必须写 `[N]`** |
//! | `raw:N`/`bytes:N` | N 字节 | `DB1.DBB0` | 读 N 字节编码成十六进制串（`"DEADBEEF"`），失败/长度不符 → Null（S-14/S-21） |
//!
//! string/wstring/raw 走单点读路径（不进批量——大对象会顶爆协商 PDU 的响应空间）。
//!
//! 写命令 `data` JSON（两种契约等价，`address` 优先）：
//! - HMI：`{"tag":"点位显示名","value":42.5}`——按 `devid` + 启动期点位索引反查地址；
//! - 原生：`{"address":"DB1.DBD0","value":42.5,"kind":"f32"}`。
//!
//! `kind` 缺省时取**点位索引里该地址/tag 声明的 kind**，索引查不到则 Err（S-6：旧实现
//! 缺省推断 I64/F64，`DB1.DBD0` 写 4 字节点位会静默覆盖 8 字节）。string 写的 `max_len`
//! 只能取点位声明的 `[N]`（命令给的更大 → Err，见 S-7）。
//!
//! # v1 边界
//!
//! - 读写均只支持上表类型（数值/位/string/wstring/raw）。
//! - 小端 `_le` 变体不暴露（S7 原生大端；库支持 `*_le` 方法，非标设备后置）。
//! - counter/timer（`C`/`T` 区）不支持（值为 BCD 编码，库按 ×4 编址）。
//! - SZL 诊断（`szl_read`/`get_cpu_info`/`get_plc_status`）不暴露——属运维诊断非采集，
//!   后续按需加 REST 端点而非插件配置。

mod address;

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, InstConfig, Sample, SampleValue};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// 库的 S7Client 与本插件 struct S7Client 同名，用别名区分（S7Conn 表示到 PLC 的连接）。
use tokio_s7::client::{S7Client as S7Conn, S7Config};
use tokio_s7::error::S7Error;
use tokio_s7::item::{DataItem, RequestItem};
use tokio_s7::types::{PlcType, S7ReturnCode};

/// S7 采集插件（注册名 `s7Client`）。
#[derive(Debug, Default)]
pub struct S7Client;

/// 从 `app_config` 解析的插件配置。
#[derive(Debug)]
struct S7Cfg {
    /// PLC 主机（已拆掉 endpoint 自带的端口；IPv6 带方括号——库按 `{host}:{port}` 解析成 SocketAddr）。
    endpoint: String,
    /// PLC 端口（endpoint 自带端口优先；否则 `port` 键；再否则 102）。
    port: u16,
    /// PLC 型号（决定 rack/slot 默认 + PDU）。
    plc_type: PlcType,
    /// 用户显式指定的 rack/slot（缺省 None → 走 plc_type 默认）。
    rack_slot: Option<(u8, u8)>,
    /// 采集周期秒。
    period: u64,
    /// 单次操作超时毫秒。
    timeout_ms: u64,
    /// 是否启用库内置自动重连。
    auto_reconnect: bool,
    /// COTP 连接资源类型（PG=0x01 / PC=0x02 / OP=0x03，缺省 PG）。
    /// **库不消费此字段**（S-9），见 [`warn_ignored_connection_type`]。
    connection_type: u8,
    /// `connection_type` 是否被显式配置（S-9：显式非默认值 → 启动 warn 一条）。
    connection_type_explicit: bool,
    /// 单 PDU 读请求最大项数上限（S7 协议上限 20，缺省 20；库不自动分批，插件按此分片）。
    /// 实际值还要按协商 PDU 收紧，见 [`max_items_per_request`]。
    max_items_per_request: usize,
    /// Setup Communication 提出的 PDU 上限（S-3；真实值以协商回来的 `pdu_length` 为准）。
    max_pdu_size: u16,
    /// 写后读回校验（S-1；tokio-s7 丢弃写应答 item 级 return_code 的临时防线）。
    verify_write: bool,
}

/// S7 标准端口（ISO-on-TCP，RFC 1006）。
const S7_PORT: u16 = 102;

/// 从 `app_config` JSON 解析插件配置。
// 表单默认值常量（审计 C1；前端 pluginSchemas.ts s7Client 镜像契约）。
const DEFAULT_PLC_TYPE: &str = "s1200";
const DEFAULT_PERIOD_SECS: u64 = 2;
const DEFAULT_TIMEOUT_MS: u64 = 5000;

fn parse_cfg(app: &serde_json::Value) -> Result<S7Cfg, String> {
    let endpoint_raw = app
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "app_config.endpoint missing".to_string())?
        .to_string();
    // S-20：端口用 checked 转换（旧实现 `as u16` 把 70000 截断成 4464，连错端口）。
    let port_cfg = match app.get("port") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| "port must be a non-negative integer".to_string())?;
            let p = u16::try_from(n).map_err(|_| format!("port {n} 超出 1-65535"))?;
            if p == 0 {
                return Err("port 0 不是合法的 TCP 端口（缺省 102）".to_string());
            }
            Some(p)
        }
    };
    // S-18：endpoint 里的 `ip:102` 必须拆开——库把 host 与 port 再拼一次会得到
    // `ip:102:102`，parse::<SocketAddr>() 失败 → 永远连不上。
    let (endpoint, port) = parse_endpoint(&endpoint_raw, port_cfg.unwrap_or(S7_PORT))?;
    // endpoint 自带端口与 `port` 键冲突时以 endpoint 为准（前端提示 `host:port`，那里
    // 的端口才是用户看着填的那个），但要出声——静默忽略显式配置最难查。
    if let Some(p) = port_cfg {
        if port != p {
            tracing::warn!(
                "[s7Client] endpoint '{endpoint_raw}' 自带端口 {port}，忽略 port={p}（endpoint 里的端口优先）"
            );
        }
    }
    let plc_type = parse_plc_type(
        app.get("plc_type")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_PLC_TYPE),
    )?;
    // S-20：rack/slot 单独给时另一个取 0（旧实现要求同时给，只给 rack 被静默忽略，
    // 实际用上的仍是 plc_type 默认值——配置与生效不一致比连不上更难查）。
    // 选"取 0"而非"报错"：显式 rack 得以生效，槽位不对在 connect 阶段立刻可见。
    let rack = parse_opt_u8(app.get("rack"), "rack")?;
    let slot = parse_opt_u8(app.get("slot"), "slot")?;
    let rack_slot = match (rack, slot) {
        (Some(r), Some(s)) => Some((r, s)),
        (Some(r), None) => Some((r, 0)),
        (None, Some(s)) => Some((0, s)),
        (None, None) => None,
    };
    let period = app
        .get("period")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_PERIOD_SECS)
        .max(1);
    let timeout_ms = app
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let auto_reconnect = app
        .get("auto_reconnect")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let ct_raw = app
        .get("connection_type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .map(str::to_lowercase);
    let connection_type = match ct_raw.as_deref() {
        None | Some("") | Some("pg") => 0x01,
        Some("pc") => 0x02,
        Some("op") => 0x03,
        Some(other) => return Err(format!("unsupported connection_type: {other} (pg/pc/op)")),
    };
    // S-9：库只定义 connection_type 不用它（client.rs:34/89-92，TSAP 仅由 plc_type +
    // rack/slot 算出），显式配了非默认值只 warn，不假装支持。
    let connection_type_explicit = ct_raw.as_deref().is_some_and(|s| !s.is_empty());
    let max_items_per_request = app
        .get("max_items_per_request")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(1, 20) as usize;
    // S-3/S-20：PDU 配置边界。S7 Setup Communication 的 PDU 字段下限 240（S7-300 老
    // CPU 的协商值），上限取 4096（S7-1500 最大档；库默认 960）。越界拒绝而不是截断——
    // 反向的截断会让"配了 8192"的现场以为拿到了大 PDU。
    let max_pdu_size = match app.get("max_pdu_size") {
        None | Some(serde_json::Value::Null) => 960,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| "max_pdu_size must be an integer".to_string())?;
            if !(240..=4096).contains(&n) {
                return Err(format!(
                    "max_pdu_size {n} 超出 240-4096（S7 协商 PDU 范围）"
                ));
            }
            n as u16
        }
    };
    let verify_write = app
        .get("verify_write")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    Ok(S7Cfg {
        endpoint,
        port,
        plc_type,
        rack_slot,
        period,
        timeout_ms,
        auto_reconnect,
        connection_type,
        connection_type_explicit,
        max_items_per_request,
        max_pdu_size,
        verify_write,
    })
}

/// 可选 `rack`/`slot` 键 → `u8`（checked；非整数或越界 → Err）。
fn parse_opt_u8(v: Option<&serde_json::Value>, key: &str) -> Result<Option<u8>, String> {
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| format!("{key} must be a non-negative integer"))?;
            let n = u8::try_from(n).map_err(|_| format!("{key} {n} 超出 0-255"))?;
            Ok(Some(n))
        }
    }
}

/// 解析 `endpoint` 为 `(host, port)`（S-18，2026-11-01 协议语义审查）。
///
/// 库的 `run_actor` 用 `format!("{host}:{port}").parse::<SocketAddr>()`
/// （`<REG>tokio-s7-0.1.7/src/client.rs:1137-1152`）——前端 schema hint 提示的
/// `host:port` 形态若整串当 host 传下去，库会拼成 `host:port:port` 解析失败，
/// 表现为"配置看着没错但永远连不上"。
///
/// 规则：
/// - `[::1]:102` / `[::1]`：方括号内是 host（IPv6），其后可选 `:port`；
/// - 含冒号且末段是数字：拆成 host + port（`192.168.0.100:102`）；
/// - 否则整串当 host，端口用 `default_port`；
/// - 裸 IPv6（`::1`，无方括号）无法拆且库也解析不了 → Err 要求加方括号。
fn parse_endpoint(raw: &str, default_port: u16) -> Result<(String, u16), String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("endpoint 为空".to_string());
    }
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| format!("endpoint '{raw}' 的 IPv6 方括号未闭合（形如 [::1]:102）"))?;
        let host = format!("[{}]", &rest[..end]);
        let tail = &rest[end + 1..];
        if tail.is_empty() {
            return Ok((host, default_port));
        }
        let port = tail.strip_prefix(':').ok_or_else(|| {
            format!("endpoint '{raw}' 的方括号后只能是 ':端口'（形如 [::1]:102）")
        })?;
        return Ok((host, parse_port(port, raw)?));
    }
    if let Some((host, port)) = s.rsplit_once(':') {
        if port.is_empty() {
            return Err(format!(
                "endpoint '{raw}' 的端口为空（形如 host:102；不要主机后带冒号）"
            ));
        }
        if port.bytes().all(|b| b.is_ascii_digit()) {
            if host.contains(':') {
                return Err(format!(
                    "endpoint '{raw}' 是裸 IPv6 字面量且无法拆分端口：请写成 '[::1]:102' 形式"
                ));
            }
            if host.is_empty() {
                return Err(format!("endpoint '{raw}' 缺少主机部分（应为 host:port）"));
            }
            return Ok((host.to_string(), parse_port(port, raw)?));
        }
        if s.matches(':').count() > 1 {
            return Err(format!(
                "endpoint '{raw}' 是裸 IPv6 字面量：请写成 '[::1]:102' 形式（库按 SocketAddr 解析，需方括号）"
            ));
        }
    }
    Ok((s.to_string(), default_port))
}

/// `endpoint` 里的端口段 → u16（checked，0 拒绝）。
fn parse_port(s: &str, raw: &str) -> Result<u16, String> {
    let n: u16 = s
        .parse()
        .map_err(|_| format!("endpoint '{raw}' 的端口 '{s}' 超出 1-65535"))?;
    if n == 0 {
        return Err(format!("endpoint '{raw}' 的端口不能是 0"));
    }
    Ok(n)
}

/// `plc_type` 字符串 → `PlcType`（大小写不敏感）。
fn parse_plc_type(s: &str) -> Result<PlcType, String> {
    match s.trim().to_lowercase().as_str() {
        "s200" => Ok(PlcType::S200),
        "s200smart" => Ok(PlcType::S200Smart),
        "s300" => Ok(PlcType::S300),
        "s400" => Ok(PlcType::S400),
        "s1200" => Ok(PlcType::S1200),
        "s1500" => Ok(PlcType::S1500),
        "sinumerik828d" => Ok(PlcType::Sinumerik828D),
        other => Err(format!("unsupported plc_type: {other}")),
    }
}

/// S-9（2026-11-01 协议语义审查）：`connection_type` 是 tokio-s7 的死配置。
///
/// 库只定义/默认/赋值该字段（`<REG>tokio-s7-0.1.7/src/client.rs:34,54,89-92`），
/// 真正发出去的 COTP TSAP 只由 `plc_type` + rack/slot 算出（`client.rs:1178-1183`）。
/// 显式配了非默认值（PG）时启动 warn 一条说明它被忽略——不假装支持，也不静默吞掉
/// 用户的配置意图（等上游/fork 补丁落地后再接）。
fn warn_ignored_connection_type(cfg: &S7Cfg, inst_id: &str) {
    if cfg.connection_type_explicit && cfg.connection_type != 0x01 {
        tracing::warn!(
            "[s7Client] inst {inst_id} 配置 connection_type=0x{:02X} 被 tokio-s7 0.1.7 忽略（库只用 plc_type+rack/slot 算 TSAP）；该项待上游/fork 补丁生效后才有效",
            cfg.connection_type
        );
    }
}

/// 变量位宽类型（kind）。地址前缀不可靠，位宽必须由此显式指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 位。
    Bool,
    /// 有符号字节（i8）。**注意是 i8**：PLC 的 BYTE（0-255）应配 `ubyte`（S-10）。
    Byte,
    /// 无符号字节（u8，0-255）——S7 BYTE 的正确映射（S-10）。
    UByte,
    /// 有符号 16 位。
    I16,
    /// 无符号 16 位。
    U16,
    /// 有符号 32 位。
    I32,
    /// 无符号 32 位。
    U32,
    /// 有符号 64 位。
    I64,
    /// 无符号 64 位。
    U64,
    /// 32 位浮点（IEEE754 大端）。
    F32,
    /// 64 位浮点（IEEE754 大端）。
    F64,
    /// S7 STRING（2B 头 + ISO-8859-1；单点读写路径，不进批量）。
    String,
    /// S7 WSTRING（4B 头 + UTF-16BE；单点读写路径，不进批量）。
    WString,
    /// 原始字节串（N 字节 → 十六进制字符串；单点读路径）。
    /// 用于 DT/DTL/S5TIME/BCD 等库无 typed API 的类型（S-14）。
    Raw(u16),
}

/// string kind 的声明最大字符数缺省值（S7 常见 `String[254]`）。
const DEFAULT_STRING_MAX_LEN: u16 = 254;

/// `raw:N` 单次读取的最大字节数（协商 PDU 上限 4096 − 请求/响应头开销的保守上限）。
/// 超过此时插件侧直接拒绝——库的读响应受 PDU 限制，超了每拍必失败。
const MAX_RAW_LEN: u16 = 1024;

/// 每设备点位分组（采集路径不同）：数值/位批量读 + string/wstring/raw 单点读。
struct DevPoints {
    /// 设备 ID（`rtdata:<devid>` 的 key）。
    devid: String,
    /// 批量读点位：`(field, addr, kind)`。
    batch: Vec<(String, String, Kind)>,
    /// 单点读点位：`(field, addr, kind, arg)`——string/wstring 的 arg 是声明字符数，
    /// raw 的 arg 是字节数。
    singles: Vec<(String, String, Kind, u16)>,
}

/// `parse_kind` 的完整结果：**是否显式声明了 `[N]`** 决定点位能否采集。
///
/// S-8：string/wstring 缺 `[N]` 时库按 254 全长读（`max_len + 2` 字节），在 240B
/// PDU 或小 DB 上每拍必失败——必须由点表声明长度，故这里要能区分"声明了"与
/// "用了缺省值"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedKind {
    /// 变量类型。
    kind: Kind,
    /// string/wstring 的声明字符数（raw = 字节数）；非长度类 kind 无意义。
    max_len: u16,
    /// 是否显式写了 `[N]`（或 `raw:N`/`bytes:N` 的显式 N）。
    len_declared: bool,
}

/// kind 字符串 → `(Kind, max_len)`（大小写不敏感；string 支持 `string` / `string[64]`
/// 语法，`[N]` 为 PLC 声明的最大字符数，须与声明一致——读写都依赖它定位头/长度）。
///
/// 仅测试保留（既有断言锁 `string` → 254 的缺省语义）；生产路径一律走
/// [`parse_kind_full`]——它多带一个"是否显式声明 `[N]`"标志（S-8 必需）。
#[cfg(test)]
fn parse_kind(s: &str) -> Result<(Kind, u16), String> {
    let p = parse_kind_full(s)?;
    Ok((p.kind, p.max_len))
}

/// kind 字符串 → [`ParsedKind`]（S-8/S-14 需要"是否显式声明长度"）。
fn parse_kind_full(s: &str) -> Result<ParsedKind, String> {
    let t = s.trim().to_lowercase();
    // raw:N / bytes:N：S-14 原始字节串（N 显式给，不允许缺省）。
    if let Some(rest) = t.strip_prefix("raw:").or_else(|| t.strip_prefix("bytes:")) {
        let n = rest
            .parse::<u16>()
            .map_err(|_| format!("bad raw length in '{s}'（形如 raw:16 / bytes:4）"))?;
        if n == 0 || n > MAX_RAW_LEN {
            return Err(format!(
                "raw 长度 {n} 超出 1-{MAX_RAW_LEN}（形如 raw:16 / bytes:4）"
            ));
        }
        return Ok(ParsedKind {
            kind: Kind::Raw(n),
            max_len: n,
            len_declared: true,
        });
    }
    // string[N] / wstring[N]：拆出方括号里的声明长度。
    let (base, max_len, len_declared) = match (t.find('['), t.ends_with(']')) {
        (Some(i), true) => {
            let n = t[i + 1..t.len() - 1]
                .parse::<u16>()
                .map_err(|_| format!("bad kind length in '{s}'"))?;
            // B-4（2026-11-01 审查）：钳到 S7 规范上限 254——库的 read/write_string
            // 以 `max_len + 2`（u16）算总长，65535 会回绕成 1 → 写路径 `buf[1]`
            // 越界 panic（release 无 overflow-checks 必崩）。
            (t[..i].to_string(), n.clamp(1, DEFAULT_STRING_MAX_LEN), true)
        }
        _ => (t, DEFAULT_STRING_MAX_LEN, false),
    };
    let kind = match base.as_str() {
        "bool" | "bit" => Kind::Bool,
        "byte" | "i8" => Kind::Byte,
        "ubyte" | "u8" | "uint8" => Kind::UByte,
        "i16" | "int16" => Kind::I16,
        "u16" | "uint16" => Kind::U16,
        "i32" | "int32" => Kind::I32,
        "u32" | "uint32" => Kind::U32,
        "i64" | "int64" => Kind::I64,
        "u64" | "uint64" => Kind::U64,
        "f32" | "float" | "float32" => Kind::F32,
        "f64" | "double" | "float64" => Kind::F64,
        "string" => Kind::String,
        "wstring" => Kind::WString,
        other => return Err(format!("unsupported kind: {other}")),
    };
    // 长度声明只对 string/wstring 有意义（`i16[4]` 之类的写法是打错，明确拒绝）。
    if len_declared && !matches!(kind, Kind::String | Kind::WString) {
        return Err(format!(
            "只有 string/wstring 支持 [N] 长度声明，kind '{s}' 不支持"
        ));
    }
    Ok(ParsedKind {
        kind,
        max_len,
        len_declared,
    })
}

/// 按协商 PDU 算单请求最大项数（S-3，2026-11-01 协议语义审查）。
///
/// 库在发包前校验 `bytes.len() - 7 > pdu_limit` 即报
/// `Read request exceeds PDU limit`（`<REG>tokio-s7-0.1.7/src/client.rs:1322-1324`），
/// 而插件的旧实现固定 20 项/请求：协商出 240B PDU 的 CPU（S7-300 常见）上 20 项
/// 恰好超限，每拍报错 → 拆连接 → 触发 S-2 的热重连风暴。
///
/// 请求体开销：TPKT(4) + COTP(3) + S7 头(10) + 参数(2) = 19B，库按 `len - 7` 计
/// 即 12B；每项 12B。这里再留 8B 余量（响应侧也要装下 N 项数据），取
/// `(pdu - 20) / 12` 的保守值，至少 1 项。
fn max_items_per_request(pdu_len: u16, cfg_cap: usize) -> usize {
    let by_pdu = ((pdu_len as usize).saturating_sub(20)) / 12;
    by_pdu.max(1).min(cfg_cap.max(1))
}

/// 生效的每请求项数（S-3）：协商 PDU 与 `max_pdu_size` 配置上界取小后套公式。
///
/// 取小是为了防对端回一个比我们提议值更大的 PDU（`max_pdu_size` 既是提议值也是
/// 容忍上界）——插件承诺的"配置可再收紧"必须在不信任 PLC 应答时也成立。
fn effective_items_per_request(pdu_len: u16, cfg: &S7Cfg) -> usize {
    max_items_per_request(pdu_len.min(cfg.max_pdu_size), cfg.max_items_per_request)
}

/// kind → 批量 read 的 `RequestItem`（bool 走 `parse_bit`，其余 `parse_byte(count)`）。
/// string/wstring/raw 走单点读写路径（`read_string` 等），不进批量——返回错误防误用。
fn build_request(addr: &str, kind: Kind) -> Result<RequestItem, S7Error> {
    match kind {
        Kind::Bool => RequestItem::parse_bit(addr),
        Kind::Byte | Kind::UByte => RequestItem::parse_byte(addr, 1),
        Kind::I16 | Kind::U16 => RequestItem::parse_byte(addr, 2),
        Kind::I32 | Kind::U32 | Kind::F32 => RequestItem::parse_byte(addr, 4),
        Kind::I64 | Kind::U64 | Kind::F64 => RequestItem::parse_byte(addr, 8),
        Kind::String | Kind::WString | Kind::Raw(_) => Err(S7Error::Error(
            "string/wstring/raw not in batch path".into(),
        )),
    }
}

/// kind 所需的最小字节数（bool/byte/ubyte=1，i16/u16=2，i32/u32/f32=4，
/// i64/u64/f64=8，raw=N）。string/wstring 不经 `decode_item`（单点 API 直接返回 String）。
fn kind_byte_count(kind: Kind) -> usize {
    match kind {
        Kind::Bool | Kind::Byte | Kind::UByte => 1,
        Kind::I16 | Kind::U16 => 2,
        Kind::I32 | Kind::U32 | Kind::F32 => 4,
        Kind::I64 | Kind::U64 | Kind::F64 => 8,
        Kind::Raw(n) => n as usize,
        Kind::String | Kind::WString => 0,
    }
}

/// `DataItem` → `SampleValue`（检查 `return_code`；按 kind 大端解码 `data`）。
///
/// 单项失败（`return_code != Success` / 数据长度不足）返回 `None`，由调用方
/// 限流 warn + 投递 `SampleValue::Null`（F-3/S-15）——批量 read 不因单点失败报
/// `Err`，部分失败体现在每项的 `return_code`（对标 modbus exception）。
fn decode_item(kind: Kind, item: &DataItem) -> Option<SampleValue> {
    if item.return_code != S7ReturnCode::Success {
        return None;
    }
    decode_bytes(kind, &item.data).ok()
}

/// 按 kind 大端解码一段字节（读路径与写后校验共用）。
///
/// 失败原因（长度不足/类型不支持）由调用方决定日志与计数——纯函数便于单测。
fn decode_bytes(kind: Kind, d: &[u8]) -> Result<SampleValue, String> {
    let need = kind_byte_count(kind);
    if d.len() < need {
        return Err(format!(
            "数据太短：{kind:?} 需 {need} 字节，实际 {}",
            d.len()
        ));
    }
    Ok(match kind {
        Kind::Bool => SampleValue::Bool(d[0] != 0),
        Kind::Byte => SampleValue::Int(i8::from_be_bytes([d[0]]) as i64),
        Kind::UByte => SampleValue::Int(u8::from_be_bytes([d[0]]) as i64),
        Kind::I16 => SampleValue::Int(i16::from_be_bytes([d[0], d[1]]) as i64),
        Kind::U16 => SampleValue::Int(u16::from_be_bytes([d[0], d[1]]) as i64),
        Kind::I32 => SampleValue::Int(i32::from_be_bytes([d[0], d[1], d[2], d[3]]) as i64),
        Kind::U32 => SampleValue::Int(u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as i64),
        Kind::I64 => SampleValue::Int(i64::from_be_bytes([
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        ])),
        // S-19：u64 走独立变体，超 2^63 不符号翻转（F-5 框架侧已支持）。
        Kind::U64 => SampleValue::UInt(u64::from_be_bytes([
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        ])),
        Kind::F32 => SampleValue::Float(f32::from_be_bytes([d[0], d[1], d[2], d[3]]) as f64),
        Kind::F64 => SampleValue::Float(f64::from_be_bytes([
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        ])),
        // S-14：原始字节 → 大写十六进制串（"DEADBEEF"）。
        Kind::Raw(n) => SampleValue::Str(hex_encode(&d[..n as usize])),
        // string/wstring 走单点 API，不经此函数。
        Kind::String | Kind::WString => {
            return Err("string/wstring 不在字节解码路径".to_string());
        }
    })
}

/// 字节串 → 大写十六进制（`raw:N` 的对外表示）。
fn hex_encode(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02X}");
    }
    s
}

/// 会话期错误的连接致命性判定（B-2，2026-11-01 审查）。
///
/// 库的 `is_connection_error()` 只认 ConnectionRefused/Timeout/Closed/NotConnected
/// 四变体——但会话期读错误实际产出是 `IoErr`（TCP RST）与 `Error("No data in read
/// response" / "Actor disconnected")`（FIN/actor 退出），四变体一个都不会出现，
/// 原判定恒 false → `conn_lost` 永不置位、插件重连是死代码（auto_reconnect=false
/// 时一次断线即永久停摆）。此处补 `IoErr`；不可判别的 `Error(_)` 字符串类
/// （actor 已死）由 [`CONSEC_FAIL_RECONNECT`] 连续失败计数兜底。
fn conn_fatal(e: &S7Error) -> bool {
    e.is_connection_error() || matches!(e, S7Error::IoErr(_))
}

/// 连续读失败 N 拍 → 视作连接失效拆连接重连（覆盖 `Error(_)` 形态的 actor 死亡）。
const CONSEC_FAIL_RECONNECT: u32 = 3;

/// 会话期单次 I/O 上限（B-1）：库的 `with_timeout` 只包 connect，会话期
/// `read`/`write` 的 `framed` 收发无任何超时——半开连接（拔网线，无 RST/FIN）
/// 会永久挂死 select 的 tick 分支，token/cmd 全停。插件侧统一包本上限，
/// 超时视作连接失效（`conn_lost`）拆连接。
fn io_timeout(cfg: &S7Cfg) -> Duration {
    Duration::from_millis(cfg.timeout_ms.max(1))
}

/// 建立到 PLC 的 S7 连接（`with_plc_type` 自动填 rack/slot 默认；用户显式配则覆盖）。
///
/// 返回 `(conn, 协商 PDU 长度)`——S-3：`pdu_length` 是 pub 字段，`connect()` 里
/// 由 Setup Communication 应答赋值（`<REG>tokio-s7-0.1.7/src/client.rs:150,194-204`），
/// 单请求项数必须按它算（[`max_items_per_request`]）。
async fn connect(cfg: &S7Cfg) -> Result<(S7Conn, u16), String> {
    let mut s7 = S7Config::new(&cfg.endpoint)
        .with_plc_type(cfg.plc_type)
        .with_port(cfg.port)
        .with_timeout(cfg.timeout_ms)
        .with_auto_reconnect(cfg.auto_reconnect)
        .with_connection_type(cfg.connection_type);
    // S-3/S-20：Setup Communication 里提出的 PDU 上限（库无 builder，字段是 pub）。
    s7.max_pdu_size = cfg.max_pdu_size;
    if let Some((rack, slot)) = cfg.rack_slot {
        s7 = s7.with_rack_slot(rack, slot);
    }
    let mut client = S7Conn::new(s7);
    client
        .connect()
        .await
        .map_err(|e| format!("connect {}:{}: {e}", cfg.endpoint, cfg.port))?;
    let pdu_len = client.pdu_length;
    Ok((client, pdu_len))
}

/// 拆连接超时（单次 disconnect 的上限）。
///
/// 库 actor 在连接丢失后会 `sleep(2s)` 再重连（`<REG>tokio-s7-0.1.7/src/client.rs:1228`），
/// 期间的 Disconnect 命令只能等它连上并进入 `serve_commands` 才被消费；这里等 1s
/// 等不到就交回调用方，下拍再试（见 [`shutdown_conn`] 的调用点）。
const CONN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// 优雅拆连接（S-2，2026-11-01 协议语义审查）。
///
/// **库缺陷**：`S7Client` 被 drop 时 `cmd_tx` 随之关闭 → actor 的
/// `serve_commands` 在 `while let Some(cmd) = cmd_rx.recv()` 处拿到 `None` 后
/// 返回 `Ok(1)`（`<REG>tokio-s7-0.1.7/src/client.rs:1301`）——而 `run_actor` 只对
/// `Ok(0)`（正常 Disconnect）break，其余一律**无间隔**重连（同文件 1221-1230）：
/// 探针实测 drop 后 0.6s 建连 2044 次（≈3400/s），足以打爆 PLC 的连接资源。
///
/// 因此插件的原则是：**任何丢弃 client 之前先 `disconnect()`**，让 actor 走
/// `Ok(0)` 正常退出（`client.rs:1288-1300` 的 Disconnect 分支）。
///
/// 返回 `true` = 拆干净（actor 已走或已确定退出，可以安全 drop/重建）；
/// `false` = disconnect 超时（actor 还在退避重连里），调用方**必须保留 client**
/// （保住 cmd_tx 才能让 actor 停在 2s 退避而非热循环），下拍重试。
async fn shutdown_conn(c: &mut S7Conn, inst_id: &str) -> bool {
    match tokio::time::timeout(CONN_SHUTDOWN_TIMEOUT, c.disconnect()).await {
        Ok(Ok(())) => {
            tracing::info!("[s7Client] inst {inst_id} disconnected (actor exited)");
            true
        }
        Ok(Err(e)) => {
            // `disconnect()` 无论成败都会把 cmd_tx 置 None（client.rs:181），
            // 通道已关；正常路径下 actor 收到 Disconnect 后即退出，这里的 Err 多是
            // "Not connected"/"Actor disconnected"（actor 早就没了），不会热重连。
            tracing::warn!(
                "[s7Client] inst {inst_id} disconnect 返回错误（actor 多半已退出）: {e}"
            );
            true
        }
        Err(_) => {
            tracing::warn!(
                "[s7Client] inst {inst_id} disconnect 超时（{CONN_SHUTDOWN_TIMEOUT:?}，库 actor 应在退避重连中）；保留连接下拍重试，避免 drop 触发库的无间隔热重连"
            );
            false
        }
    }
}

/// JSON → `SampleValue`（写命令的 value 字段）。
fn json_to_sample_value(v: &serde_json::Value) -> SampleValue {
    match v {
        serde_json::Value::Bool(b) => SampleValue::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(SampleValue::Int)
            .or_else(|| n.as_f64().map(SampleValue::Float))
            .unwrap_or(SampleValue::Int(0)),
        serde_json::Value::String(s) => SampleValue::Str(s.clone()),
        other => SampleValue::Str(other.to_string()),
    }
}

/// 按 `SampleValue` 推断 kind（写命令未显式指定时）。
///
/// **S-6（2026-11-01 协议语义审查）后不再使用**：`Int → I64` / `Float → F64` 的推断
/// 让 `{"address":"DB1.DBD0","value":42}` 静默写 8 字节（覆盖 DBB4-7）。写入宽度
/// 只能来自"点位声明的 kind"或命令显式给的 kind，两者都没有就拒绝——见
/// [`PreparedWrite::build`]。函数保留为**测试基线**，说明旧行为错在哪。
#[cfg(test)]
fn infer_kind(v: &SampleValue) -> Kind {
    match v {
        SampleValue::Bool(_) => Kind::Bool,
        SampleValue::Int(_) => Kind::I64,
        SampleValue::Float(_) => Kind::F64,
        SampleValue::UInt(_) => Kind::U64,
        SampleValue::Str(_) => Kind::Byte,
        SampleValue::Null => Kind::Bool,
    }
}

// ---------------------------------------------------------------------------
// 写值强类型转换（S-11）
// ---------------------------------------------------------------------------

/// 已按目标 kind 做过范围/有限性校验的写值（S-11：不再有 `as` 截断）。
#[derive(Debug, Clone, PartialEq)]
enum WVal {
    /// bool。
    Bool(bool),
    /// i8。
    I8(i8),
    /// u8。
    U8(u8),
    /// i16。
    I16(i16),
    /// u16。
    U16(u16),
    /// i32。
    I32(i32),
    /// u32。
    U32(u32),
    /// i64。
    I64(i64),
    /// u64。
    U64(u64),
    /// f32。
    F32(f32),
    /// f64。
    F64(f64),
    /// 字符串。
    Str(String),
}

impl WVal {
    /// 写成 PLC 之后**期望读回的值**（S-1 写后校验用）。
    fn expect(&self) -> SampleValue {
        match self {
            WVal::Bool(b) => SampleValue::Bool(*b),
            WVal::I8(v) => SampleValue::Int(*v as i64),
            WVal::U8(v) => SampleValue::Int(*v as i64),
            WVal::I16(v) => SampleValue::Int(*v as i64),
            WVal::U16(v) => SampleValue::Int(*v as i64),
            WVal::I32(v) => SampleValue::Int(*v as i64),
            WVal::U32(v) => SampleValue::Int(*v as i64),
            WVal::I64(v) => SampleValue::Int(*v),
            WVal::U64(v) => SampleValue::UInt(*v),
            WVal::F32(v) => SampleValue::Float(*v as f64),
            WVal::F64(v) => SampleValue::Float(*v),
            WVal::Str(s) => SampleValue::Str(s.clone()),
        }
    }
}

/// 写值 → 目标 kind 的强类型形态，**全程 checked**（S-11）。
///
/// 旧实现（`as i16` / `parse().unwrap_or(0)`）把 `70000` 写成 `4464`、把 `"abc"`
/// 写成 0——控制不生效且不报错。这里任何越界/非数字/非有限值都返回 `Err(原因)`，
/// 由写入口回 422。
fn convert_write(kind: Kind, v: &SampleValue) -> Result<WVal, String> {
    if matches!(v, SampleValue::Null) {
        return Err("写值不能是 Null（坏质量标记不可写）".to_string());
    }
    Ok(match kind {
        Kind::Bool => WVal::Bool(to_bool(v)?),
        Kind::Byte => {
            WVal::I8(to_i64(v)?.try_into().map_err(|_| {
                format!("写值 {v:?} 超出 i8 范围 -128..127（kind=byte 是有符号字节）")
            })?)
        }
        Kind::UByte => WVal::U8(
            to_u64(v)?
                .try_into()
                .map_err(|_| format!("写值 {v:?} 超出 u8 范围 0..255"))?,
        ),
        Kind::I16 => WVal::I16(
            to_i64(v)?
                .try_into()
                .map_err(|_| format!("写值 {v:?} 超出 i16 范围 -32768..32767"))?,
        ),
        Kind::U16 => WVal::U16(
            to_u64(v)?
                .try_into()
                .map_err(|_| format!("写值 {v:?} 超出 u16 范围 0..65535"))?,
        ),
        Kind::I32 => WVal::I32(
            to_i64(v)?
                .try_into()
                .map_err(|_| format!("写值 {v:?} 超出 i32 范围"))?,
        ),
        Kind::U32 => WVal::U32(
            to_u64(v)?
                .try_into()
                .map_err(|_| format!("写值 {v:?} 超出 u32 范围"))?,
        ),
        Kind::I64 => WVal::I64(to_i64(v)?),
        Kind::U64 => WVal::U64(to_u64(v)?),
        Kind::F32 => {
            let f = to_f64(v)?;
            // f64 → f32 溢出（如 1e300）会变 inf，静默写 inf 是错值。
            if f.is_finite() && f.abs() > f32::MAX as f64 {
                return Err(format!("写值 {f} 超出 f32 范围"));
            }
            WVal::F32(f as f32)
        }
        Kind::F64 => WVal::F64(to_f64(v)?),
        Kind::String | Kind::WString => match v {
            SampleValue::Str(s) => WVal::Str(s.clone()),
            other => {
                return Err(format!(
                    "string 写入要求字符串值，收到 {other:?}（PLC STRING 不做隐式数字转换）"
                ))
            }
        },
        Kind::Raw(n) => {
            return Err(format!(
                "raw:{n} 只读（库无按字节串写入的语义），请改用具体数值 kind"
            ))
        }
    })
}

/// 写值 → `f64`（checked：非有限/非数字串 → Err）。
fn to_f64(v: &SampleValue) -> Result<f64, String> {
    let f = match v {
        SampleValue::Float(f) => *f,
        SampleValue::Int(i) => *i as f64,
        SampleValue::UInt(u) => *u as f64,
        SampleValue::Bool(b) => u8::from(*b) as f64,
        SampleValue::Str(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("写值 '{s}' 不是数字"))?,
        SampleValue::Null => return Err("写值不能是 Null".to_string()),
    };
    if !f.is_finite() {
        return Err(format!("写值 {f} 不是有限浮点数"));
    }
    Ok(f)
}

/// 写值 → `i64`（checked：非整数/越界 → Err）。
fn to_i64(v: &SampleValue) -> Result<i64, String> {
    match v {
        SampleValue::Int(i) => Ok(*i),
        SampleValue::UInt(u) => i64::try_from(*u)
            .map_err(|_| format!("写值 {u} 超出 i64 范围（无符号数不适合有符号点位）")),
        SampleValue::Bool(b) => Ok(i64::from(*b)),
        SampleValue::Float(f) => float_to_i64(*f),
        SampleValue::Str(s) => {
            let f = s
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("写值 '{s}' 不是数字"))?;
            float_to_i64(f)
        }
        SampleValue::Null => Err("写值不能是 Null".to_string()),
    }
}

/// f64 → i64（要求有限且整数值，不再 `as` 截断小数）。
fn float_to_i64(f: f64) -> Result<i64, String> {
    if !f.is_finite() {
        return Err(format!("写值 {f} 不是有限浮点数"));
    }
    if f.fract() != 0.0 {
        return Err(format!("写值 {f} 不是整数（整型点位不接受小数）"));
    }
    if f < i64::MIN as f64 || f > i64::MAX as f64 {
        return Err(format!("写值 {f} 超出 i64 范围"));
    }
    Ok(f as i64)
}

/// 写值 → `u64`（checked：负数/越界/非整数 → Err）。
fn to_u64(v: &SampleValue) -> Result<u64, String> {
    match v {
        SampleValue::Int(i) => {
            u64::try_from(*i).map_err(|_| format!("写值 {i} 为负或越界，不能写入无符号点位"))
        }
        SampleValue::UInt(u) => Ok(*u),
        SampleValue::Bool(b) => Ok(u64::from(*b)),
        SampleValue::Float(f) => {
            if !f.is_finite() {
                return Err(format!("写值 {f} 不是有限浮点数"));
            }
            if f.fract() != 0.0 {
                return Err(format!("写值 {f} 不是整数（整型点位不接受小数）"));
            }
            if *f < 0.0 || *f >= u64::MAX as f64 {
                return Err(format!("写值 {f} 超出 u64 范围"));
            }
            Ok(*f as u64)
        }
        SampleValue::Str(s) => {
            let f = s
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("写值 '{s}' 不是数字"))?;
            let _ = to_u64(&SampleValue::Float(f))?;
            Ok(f as u64)
        }
        SampleValue::Null => Err("写值不能是 Null".to_string()),
    }
}

/// 写值 → `bool`（S-11：字符串支持 true/false/1/0/on/off，其余数字拒绝）。
fn to_bool(v: &SampleValue) -> Result<bool, String> {
    match v {
        SampleValue::Bool(b) => Ok(*b),
        SampleValue::Int(i) => match i {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(format!("写值 {other} 不能写入 bool 点位（只接受 0/1）")),
        },
        SampleValue::UInt(u) => match u {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(format!("写值 {other} 不能写入 bool 点位（只接受 0/1）")),
        },
        SampleValue::Float(f) => {
            if *f == 0.0 {
                Ok(false)
            } else if *f == 1.0 {
                Ok(true)
            } else {
                Err(format!("写值 {f} 不能写入 bool 点位（只接受 0/1）"))
            }
        }
        SampleValue::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Ok(true),
            "false" | "0" | "off" => Ok(false),
            other => Err(format!(
                "写值 '{other}' 不能写入 bool 点位（接受 true/false/1/0/on/off）"
            )),
        },
        SampleValue::Null => Err("写值不能是 Null".to_string()),
    }
}

// ---------------------------------------------------------------------------
// 单点读 + 写后校验（S-21 / S-1）
// ---------------------------------------------------------------------------

/// 单点读：string/wstring/raw。
///
/// 返回库的 [`S7Error`] 而不是字符串——调用方要按 [`conn_fatal`] 区分"该点坏值"
/// 与"连接失效"（后者拆连接重连），字符串化会丢掉这个判别能力。
///
/// S-21：库的 `read_bytes` 家族不检查 item 级 `return_code`，失败时返回空 `data`
/// （`<REG>tokio-s7-0.1.7/src/client.rs:270-278`）——这里对 raw 显式校验长度，
/// string/wstring 由库的长度检查 + 本函数的错误传播兜底。
async fn read_single(c: &S7Conn, addr: &str, kind: Kind, arg: u16) -> Result<SampleValue, S7Error> {
    match kind {
        // 原样传播库错误（不包装成 `Error(_)`）：调用方要靠 [`conn_fatal`] 区分
        // "该点坏值"与"连接失效"，包装会丢掉 `IoErr` 等变体导致断线后不再重连。
        Kind::String => c.read_string(addr, arg).await.map(SampleValue::Str),
        Kind::WString => c.read_wstring(addr, arg).await.map(SampleValue::Str),
        Kind::Raw(n) => {
            let b = c.read_bytes(addr, n).await?;
            // S-21：库不校验 item return_code，失败时 data 为空——长度不符即本拍无值。
            if b.len() != n as usize {
                return Err(S7Error::Error(format!(
                    "raw 读长度不符：期望 {n} 字节，实际 {}（item 级错误被库吞掉）",
                    b.len()
                )));
            }
            Ok(SampleValue::Str(hex_encode(&b)))
        }
        other => Err(S7Error::Error(format!("{other:?} 不是单点读类型"))),
    }
}

/// 写后读回比较（S-1）：布尔/整数精确比较、浮点相对误差 ≤ 1e-6、字符串精确。
fn value_matches(kind: Kind, want: &SampleValue, got: &SampleValue) -> bool {
    match kind {
        Kind::Bool => matches!((want, got), (SampleValue::Bool(a), SampleValue::Bool(b)) if a == b),
        Kind::String | Kind::WString | Kind::Raw(_) => {
            matches!((want, got), (SampleValue::Str(a), SampleValue::Str(b)) if a == b)
        }
        Kind::F32 | Kind::F64 => match (as_f64(want), as_f64(got)) {
            // 相对误差 1e-6（下限 1 避免 0 附近除零式误判）
            (Some(a), Some(b)) => (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0),
            _ => false,
        },
        _ => match (as_i128(want), as_i128(got)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
    }
}

/// 数值视图（浮点比较用）。
fn as_f64(v: &SampleValue) -> Option<f64> {
    match v {
        SampleValue::Float(f) => Some(*f),
        SampleValue::Int(i) => Some(*i as f64),
        SampleValue::UInt(u) => Some(*u as f64),
        SampleValue::Bool(b) => Some(u8::from(*b) as f64),
        _ => None,
    }
}

/// 整数视图（精确比较用；i128 装得下 u64 全量）。
fn as_i128(v: &SampleValue) -> Option<i128> {
    match v {
        SampleValue::Int(i) => Some(*i as i128),
        SampleValue::UInt(u) => Some(*u as i128),
        SampleValue::Bool(b) => Some(*b as i128),
        SampleValue::Float(f) if f.is_finite() && f.fract() == 0.0 => Some(*f as i128),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 日志限流（S-15 / F-7）
// ---------------------------------------------------------------------------

/// 单点永久失败告警窗口（S-15：坏点位不再每拍刷日志）。
const POINT_BAD_WARN_WINDOW: Duration = Duration::from_secs(60);

/// tick 超 period 告警限流窗口（F-7）。
const OVERRUN_WARN_WINDOW: Duration = Duration::from_secs(60);

/// 按 key 限流的日志闸门（同 opcua 插件的 `RateLimit`，插件内自持不跨模块）。
#[derive(Debug)]
struct WarnLimiter {
    /// 允许打日志的最小间隔。
    window: Duration,
    /// key → 上次放行时刻。
    last: std::collections::HashMap<String, std::time::Instant>,
    /// 上次清理时刻（防 key 无限增长）。
    last_prune: std::time::Instant,
}

impl WarnLimiter {
    /// 新建限流闸门。
    fn new(window: Duration) -> Self {
        Self {
            window,
            last: std::collections::HashMap::new(),
            last_prune: std::time::Instant::now(),
        }
    }

    /// `true` = 本次允许打日志；`false` = 窗口内重复，抑制。
    fn allow(&mut self, key: &str) -> bool {
        let now = std::time::Instant::now();
        if now.duration_since(self.last_prune) >= self.window.max(Duration::from_secs(1)) * 10 {
            let w = self.window;
            self.last.retain(|_, t| now.duration_since(*t) < w);
            self.last_prune = now;
        }
        match self.last.get(key) {
            Some(t) if now.duration_since(*t) < self.window => false,
            _ => {
                self.last.insert(key.to_string(), now);
                true
            }
        }
    }
}

/// tick 超 period 的告警状态（F-7，2026-11-01 协议语义审查）。
///
/// `MissedTickBehavior::Skip` 会静默丢拍：一轮采集慢于 period 时实际采集频率低于
/// 配置值而无人知晓。这里限流 warn + 计数进 tracker extra（`tick_overrun_count`）。
#[derive(Debug, Default)]
struct OverrunWarn {
    /// 累计超时次数。
    count: u64,
    /// 上次告警时刻。
    last: Option<std::time::Instant>,
}

impl OverrunWarn {
    /// 记录一次超时；返回本次是否应打 warn。
    fn record(&mut self, now: std::time::Instant) -> bool {
        self.count = self.count.saturating_add(1);
        let due = self
            .last
            .is_none_or(|t| now.duration_since(t) >= OVERRUN_WARN_WINDOW);
        if due {
            self.last = Some(now);
        }
        due
    }
}

// ---------------------------------------------------------------------------
// 写目标索引与写命令预处理（F-1 / S-6 / S-7）
// ---------------------------------------------------------------------------

/// 写目标点位规约（装配期从点表建索引时生成）。
#[derive(Debug, Clone, PartialEq)]
struct PointSpec {
    /// 点位显示名（= `Sample.values` 的 key = `ChildSpec.label`）。
    field: String,
    /// 已解析绝对地址。
    addr: String,
    /// 声明的 kind（写入宽度的**唯一**可信来源，S-6）。
    kind: Kind,
    /// string/wstring 的声明字符数 / raw 的字节数（其余 kind 无意义）。
    arg: u16,
    /// 是否显式声明了长度（S-7/S-8）。
    len_declared: bool,
}

/// 写目标索引（F-1）：`devid → 显示名/地址 → 点位规约`。
///
/// HMI 只发 `{"tag": 点位显示名, "value": …}`（`hmi-studio/src/lib/command.ts:54`），
/// 旧插件只认 `address` → 写命令被整条丢弃且 REST 返回 200。索引按 `inst.devices`
/// 建，支持 `tag` 反查绝对地址与 kind；`address` 契约保留（原生客户端/脚本）。
#[derive(Debug, Default)]
struct WriteIndex {
    /// devid → 显示名 → 规约（本设备命中优先）。
    by_devid: std::collections::HashMap<String, std::collections::HashMap<String, PointSpec>>,
    /// 显示名 → [(devid, 规约)]（跨 devid 唯一 tag 兜底 + 歧义判定）。
    global: std::collections::HashMap<String, Vec<(String, PointSpec)>>,
    /// devid → 绝对地址 → 规约（写命令只给 address 时取声明 kind / 长度）。
    addr_by_devid: std::collections::HashMap<String, std::collections::HashMap<String, PointSpec>>,
    /// 绝对地址 → [(devid, 规约)]。
    addr_global: std::collections::HashMap<String, Vec<(String, PointSpec)>>,
}

impl WriteIndex {
    /// 登记一个点位（装配期调用一次/点位）。
    fn insert(&mut self, devid: &str, spec: PointSpec) {
        self.by_devid
            .entry(devid.to_string())
            .or_default()
            .insert(spec.field.clone(), spec.clone());
        self.global
            .entry(spec.field.clone())
            .or_default()
            .push((devid.to_string(), spec.clone()));
        self.addr_by_devid
            .entry(devid.to_string())
            .or_default()
            .insert(spec.addr.clone(), spec.clone());
        self.addr_global
            .entry(spec.addr.clone())
            .or_default()
            .push((devid.to_string(), spec));
    }

    /// 按显示名解析：本设备优先，未命中时仅当全表唯一才兜底（有歧义就拒）。
    fn by_tag(&self, devid: &str, tag: &str) -> Result<PointSpec, String> {
        if let Some(spec) = self.by_devid.get(devid).and_then(|m| m.get(tag)) {
            return Ok(spec.clone());
        }
        match self.global.get(tag).map(Vec::as_slice) {
            Some([(_, spec)]) => Ok(spec.clone()),
            Some(hits) => Err(format!(
                "写命令 tag '{tag}' 在设备 {devid} 不存在，且全表有 {} 处同名点位（{:?}），无法确定目标",
                hits.len(),
                hits.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>()
            )),
            None => Err(format!(
                "写命令 tag '{tag}' 在设备 {devid} 与全表均无此点位（检查前端按钮的 tag 是否与点表 label 一致）"
            )),
        }
    }

    /// 按绝对地址解析（只用于取声明 kind/长度；未登记返回 None，不是错误）。
    ///
    /// 多设备同址（多台 PLC 同布局）时要求 kind 与长度一致才可推断，否则视为未登记。
    fn by_addr(&self, devid: &str, addr: &str) -> Option<PointSpec> {
        if let Some(spec) = self.addr_by_devid.get(devid).and_then(|m| m.get(addr)) {
            return Some(spec.clone());
        }
        match self.addr_global.get(addr).map(Vec::as_slice) {
            Some([(_, spec)]) => Some(spec.clone()),
            Some(hits) => {
                let first = &hits[0].1;
                hits.iter()
                    .all(|(_, s)| s.kind == first.kind && s.arg == first.arg)
                    .then(|| first.clone())
            }
            None => None,
        }
    }
}

/// 写命令预处理结果（解析 + 校验完成，可以直接发请求）。
#[derive(Debug)]
struct PreparedWrite {
    /// 目标绝对地址。
    addr: String,
    /// 写入 kind（来自点表声明或命令显式给，S-6）。
    kind: Kind,
    /// string/wstring 的写入长度 / raw 字节数。
    arg: u16,
    /// 已 checked 的写值。
    value: WVal,
}

impl PreparedWrite {
    /// 解析写命令（F-1/F-2/S-6/S-7/S-11/S-12）。
    ///
    /// `data` 两种契约：
    /// - HMI：`{"tag":"点位显示名","value":…}`；
    /// - 原生：`{"address":"DB1.DBD0","value":…,"kind":"f32"}`（`address` 优先）。
    fn build(data: &str, devid: &str, index: &WriteIndex) -> Result<Self, String> {
        let req: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("写命令 data 非 JSON: {e}"))?;
        let (addr, spec) = if let Some(a) = req.get("address").and_then(|v| v.as_str()) {
            let a = a.trim().to_string();
            let spec = index.by_addr(devid, &a);
            (a, spec)
        } else if let Some(tag) = req.get("tag").and_then(|v| v.as_str()) {
            let spec = index.by_tag(devid, tag)?;
            let a = spec.addr.clone();
            (a, Some(spec))
        } else {
            return Err(
                "写命令缺少 address/tag（HMI 发 tag=点位显示名，原生客户端发 address）".to_string(),
            );
        };
        let value_sv = match req.get("value") {
            Some(v) => json_to_sample_value(v),
            None => return Err("写命令缺少 value".to_string()),
        };
        let explicit = match req.get("kind").and_then(|v| v.as_str()) {
            Some(k) => {
                Some(parse_kind_full(k).map_err(|e| format!("写命令 kind '{k}' 非法: {e}"))?)
            }
            None => None,
        };
        // S-6：写入宽度只认点表声明或命令显式 kind；两者都没有 → Err（不再默认 I64/F64
        // 写 8 字节覆盖相邻字节）。显式 kind 与点表声明不符同样拒绝：那多半是配置/前端
        // 传错，而按任何一方写都会覆盖别的点位。
        let kind = match (explicit.map(|p| p.kind), spec.as_ref().map(|s| s.kind)) {
            (Some(e), Some(d)) if e != d => {
                return Err(format!(
                    "写命令 kind {e:?} 与点位 '{}'({}) 声明的 {d:?} 不符：写入宽度以点表为准，请改 kind 或改点表",
                    spec.as_ref().map(|s| s.field.as_str()).unwrap_or(&addr),
                    addr
                ));
            }
            (Some(e), _) => e,
            (None, Some(d)) => d,
            (None, None) => {
                return Err(format!(
                    "写命令目标 '{addr}' 不在点表中且未给 kind：无法确定写入宽度（旧实现会默认写 8 字节）。请给 kind（如 \"i16\"/\"f32\"）或在点表登记该点位"
                ));
            }
        };
        address::validate_address(&addr, kind).map_err(|e| format!("写命令地址非法: {e}"))?;
        let arg = resolve_write_len(&req, kind, spec.as_ref(), explicit)?;
        let value = convert_write(kind, &value_sv)?;
        Ok(Self {
            addr,
            kind,
            arg,
            value,
        })
    }
}

/// 解析 string/wstring 的写入长度（S-7，2026-11-01 协议语义审查）。
///
/// 库的 `write_string(addr, max_len, s)` 会写 `max_len + 2` 字节
/// （`<REG>tokio-s7-0.1.7/src/client.rs:590-599`）——旧实现命令缺省 max_len 就取
/// 254，`string[10]` 点位一写覆盖后 244 字节。规则：
/// - 基准长度只能是点位声明的 `[N]`（或命令显式 kind 里的 `[N]`）；
/// - 命令 `max_len` 大于基准 → Err；小于等于 → 按命令值写；
/// - 没有任何声明长度却给了 `max_len` → Err（提示点表补 `[N]`）——命令自报的长度
///   无法证明与 PLC 侧的 STRING 声明一致。
fn resolve_write_len(
    req: &serde_json::Value,
    kind: Kind,
    spec: Option<&PointSpec>,
    explicit: Option<ParsedKind>,
) -> Result<u16, String> {
    match kind {
        Kind::String | Kind::WString => {
            let declared = spec
                .filter(|s| matches!(s.kind, Kind::String | Kind::WString) && s.len_declared)
                .map(|s| s.arg);
            // 命令显式 kind 里的 [N] 也是一份"声明"：比点表声明更长同样会覆盖相邻字节。
            if let (Some(d), Some(e)) = (declared, explicit.filter(|p| p.len_declared)) {
                if e.max_len > d {
                    return Err(format!(
                        "写命令 kind 声明长度 [{}] 超过点表声明的 [{d}]：会覆盖相邻字节",
                        e.max_len
                    ));
                }
            }
            let declared =
                declared.or_else(|| explicit.filter(|p| p.len_declared).map(|p| p.max_len));
            let cmd_len = match req.get("max_len") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => {
                    let n = v
                        .as_u64()
                        .ok_or_else(|| "写命令 max_len 必须是正整数".to_string())?;
                    let n =
                        u16::try_from(n).map_err(|_| format!("写命令 max_len {n} 超出 1-254"))?;
                    if n == 0 {
                        return Err("写命令 max_len 不能为 0".to_string());
                    }
                    if n > DEFAULT_STRING_MAX_LEN {
                        return Err(format!(
                            "写命令 max_len {n} 超出 S7 STRING 上限 {DEFAULT_STRING_MAX_LEN}"
                        ));
                    }
                    Some(n)
                }
            };
            match (declared, cmd_len) {
                (Some(d), Some(c)) if c > d => Err(format!(
                    "写命令 max_len {c} 超过点位声明的 [{d}]：会覆盖相邻字节（库按 max_len+2 字节写）"
                )),
                (Some(_), Some(c)) => Ok(c),
                (Some(d), None) => Ok(d),
                (None, Some(_)) => Err(
                    "写命令给了 max_len 但点位未声明长度：无法校验与 PLC 声明一致，请在点表 kind 写 string[N]"
                        .to_string(),
                ),
                (None, None) => Err(
                    "写 string 缺少长度：请在点表 kind 写 string[N]（或命令 kind 给 string[N]）".to_string(),
                ),
            }
        }
        Kind::Raw(n) => Ok(n),
        _ => Ok(0),
    }
}

/// 投递一台设备的一拍样本。`values` 里**允许**出现 `SampleValue::Null`（坏质量点）：
/// 下游 rtdata / history 会跳过它们，同设备其它有效点照常入库（F-3 口径）。
///
/// 返回 `false` = hub 已关闭（调用方应退出采集循环）。
async fn send_device_sample(
    tx: &SampleSender,
    hwid: &crate::types::Hwid,
    devid: &str,
    values: BTreeMap<String, SampleValue>,
) -> bool {
    if values.is_empty() {
        return true;
    }
    let sample = Sample {
        hwid: hwid.clone(),
        devid: devid.to_string(),
        values,
        timestamp: chrono::Local::now().timestamp(),
    };
    tx.send(sample).await.is_ok()
}

#[async_trait]
impl Source for S7Client {
    fn name(&self) -> &'static str {
        "s7Client"
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
                tracing::warn!("[s7Client] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // M-2 模型驱动装配：绑定展开（模板 addr × devreg params → 绝对 S7 地址）。
        let expanded = match crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("[s7Client] inst {inst_id} 绑定展开失败（设备未注册？）: {e}");
                return;
            }
        };
        // 预解析所有设备的点位分组（采集循环内不再重复 parse）：
        // 数值/位 → 批量读（按协商 PDU 分片）；string/wstring/raw → 单点读。
        //
        // 装配期一次性拒掉坏点位（S-12/S-13/S-8）：地址形态非法、位地址配数值 kind、
        // string 缺 `[N]` 声明——都是"库会在运行期静默读错/每拍必失败"的形态，宁可
        // 启动期 warn 跳过并少一个点，也不带病进采集循环。坏点位 warn 只在启动期
        // 打（一次），运行期不刷屏。
        let mut devices: Vec<DevPoints> = Vec::with_capacity(expanded.len());
        // F-1：写目标索引（devid → 显示名/地址 → 绝对地址 + kind），供 HMI 的
        // `{"tag","value"}` 写命令反查。
        let mut write_index = WriteIndex::default();
        for dev in &expanded {
            let mut batch = Vec::new();
            let mut singles = Vec::new();
            for c in &dev.children {
                let field = if c.label.is_empty() {
                    c.value.clone()
                } else {
                    c.label.clone()
                };
                let parsed = match parse_kind_full(&c.r#type) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            "[s7Client] inst {inst_id} dev {} 点位 '{}' kind '{}' 非法，跳过: {e}",
                            dev.value,
                            field,
                            c.r#type
                        );
                        continue;
                    }
                };
                if let Err(e) = address::validate_address(&c.value, parsed.kind) {
                    tracing::warn!(
                        "[s7Client] inst {inst_id} dev {} 点位 '{}' 地址 '{}' 非法，跳过: {e}",
                        dev.value,
                        field,
                        c.value
                    );
                    continue;
                }
                // S-8：string/wstring 必须声明 `[N]`——库按 `max_len + 2` 全长读，
                // 缺省 254 在 240B PDU / 小 DB 上每拍必失败（旧实现让整轮读失败重连）。
                // 启动期拒绝该点并 warn 一次，不留给运行期。
                if matches!(parsed.kind, Kind::String | Kind::WString) && !parsed.len_declared {
                    tracing::warn!(
                        "[s7Client] inst {inst_id} dev {} 点位 '{}' kind '{}' 未声明长度，跳过：请在点表写 {}[N]（与 PLC 声明一致，如 string[20]）",
                        dev.value,
                        field,
                        c.r#type,
                        c.r#type
                    );
                    continue;
                }
                match parsed.kind {
                    Kind::String | Kind::WString | Kind::Raw(_) => {
                        singles.push((field.clone(), c.value.clone(), parsed.kind, parsed.max_len))
                    }
                    _ => batch.push((field.clone(), c.value.clone(), parsed.kind)),
                }
                write_index.insert(
                    &dev.value,
                    PointSpec {
                        field,
                        addr: c.value.clone(),
                        kind: parsed.kind,
                        arg: parsed.max_len,
                        len_declared: parsed.len_declared,
                    },
                );
            }
            devices.push(DevPoints {
                devid: dev.value.clone(),
                batch,
                singles,
            });
        }
        // S-9：`connection_type` 是 tokio-s7 的死配置，显式配了非默认值只 warn。
        warn_ignored_connection_type(&cfg, inst_id.as_str());
        // F-2：声明本实例支持写回执——REST 写入口据此在窗口内等待 handle_write 的
        // 业务结果（成功 200 / 明确失败 422 / 无回报 504）。放在装配成功之后：配置/
        // 绑定失败直接 return 的实例不声明能力（避免写入口白等）。
        ctx.ctl_registry.declare_write_ack(inst_id.as_str());

        let period = Duration::from_secs(cfg.period);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase =
            std::time::Duration::from_millis(crate::ticker::phase_ms(inst_id.as_str(), period));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // StatTracker（M4 实例指标）+ flush ticker（周期落 lux stat:<inst>）。
        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        let mut reconnects: u64 = 0;
        // B-1/B-2：会话期 I/O 上限 + 连续失败计数（见两 helper 注释）。
        let io_timeout = io_timeout(&cfg);
        let mut consec_read_fails: u32 = 0;
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst_id.as_str());
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        let mut client: Option<S7Conn> = None;
        // S-2：连接待拆标记。连接失效后**必须先 disconnect 再放下**——直接把
        // `S7Client` drop 掉会让库 actor 进入无间隔热重连（见 [`shutdown_conn`]）。
        // 拆不干净（disconnect 超时：actor 正在 2s 退避重连中）时保留 client，
        // 下拍重试；此间不发 I/O（旧连接已不可用）。
        let mut shutdown_pending = false;
        // S-3：当前连接的每请求项数（连接成功后按协商 PDU 重算，见 [`max_items_per_request`]）。
        let mut items_per_request = cfg.max_items_per_request;
        // S-15：单点坏值告警限流 + 累计计数（并入 tracker extra）。
        let mut bad_warn = WarnLimiter::new(POINT_BAD_WARN_WINDOW);
        let mut point_bad: u64 = 0;
        // S-17：读响应项数与请求不符的累计次数。
        let mut item_mismatch: u64 = 0;
        // S-1：写后校验失败的累计次数。
        let mut verify_fails: u64 = 0;
        // F-7：tick 超 period 告警（MissedTickBehavior::Skip 静默丢拍）。
        let mut overrun = OverrunWarn::default();

        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[s7Client] inst {inst_id} stopping");
                    break;
                }
                msg = cmd_rx.recv() => {
                    // 通道关闭（stop 先 unregister 再 cancel）即退出——`None` 空转会让
                    // select 立即重新 Poll 进热循环（与 modbus `Evt::Cmd(None) => break` 对齐）。
                    let Some(m) = msg else { break };
                    // F-1/F-2：HMI `{tag,value}` 与旧 `{address,value}` 统一解析；成败都
                    // 经回执回流（不再只 warn + REST 200）。
                    let verify_before = verify_fails;
                    let result = if m.cmd == "write" {
                        handle_write(
                            &client,
                            &m.data,
                            &m.devid,
                            &write_index,
                            &cfg,
                            io_timeout,
                            &mut verify_fails,
                        )
                        .await
                    } else {
                        tracing::info!("[s7Client] inst {inst_id} unknown cmd: {}", m.cmd);
                        Err(format!("实例不支持命令 '{}'（仅 write）", m.cmd))
                    };
                    if let Err(e) = &result {
                        tracing::warn!(
                            "[s7Client] inst {inst_id} cmd {} devid {} 失败: {e}",
                            m.cmd,
                            m.devid
                        );
                    }
                    if verify_fails != verify_before {
                        tracker.extra_insert(
                            "write_verify_fail_count",
                            serde_json::json!(verify_fails),
                        );
                    }
                    ctx.ctl_registry
                        .finish_write(m.request_id.as_deref(), result);
                }
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                }
                _ = ticker.tick() => {
                    let tick_started = std::time::Instant::now();
                    // S-2：上拍残留的"待拆连接"先拆干净再谈重连。拆不干净（库 actor
                    // 正在 2s 退避重连，Disconnect 还没被消费）时保留 client——保住
                    // cmd_tx 才能阻止 actor 空转热重连，本拍跳过 I/O。
                    if shutdown_pending {
                        match client.as_mut() {
                            Some(c) => {
                                if shutdown_conn(c, inst_id.as_str()).await {
                                    client = None;
                                    shutdown_pending = false;
                                } else {
                                    tracker.tick_fail("disconnect pending (actor reconnecting)");
                                    if tracker.should_flush() {
                                        tracker.flush().await;
                                    }
                                    continue;
                                }
                            }
                            None => shutdown_pending = false,
                        }
                    }
                    if client.is_none() {
                        match connect(&cfg).await {
                            Ok((c, pdu_len)) => {
                                reconnects += 1;
                                tracker.extra_insert(
                                    "reconnect_count",
                                    serde_json::json!(reconnects),
                                );
                                // S-3：单请求项数由**协商 PDU** 决定。库发包前校验
                                // `len-7 > pdu_limit` 就报错（client.rs:1322-1324），
                                // 固定 20 项在 240B PDU 上每拍超限 → 拆连接 → S-2 风暴。
                                let effective_pdu = pdu_len.min(cfg.max_pdu_size);
                                items_per_request = effective_items_per_request(pdu_len, &cfg);
                                tracker.extra_insert(
                                    "negotiated_pdu",
                                    serde_json::json!(pdu_len),
                                );
                                tracker.extra_insert(
                                    "items_per_request",
                                    serde_json::json!(items_per_request),
                                );
                                tracing::info!(
                                    "[s7Client] inst {inst_id} connected {} (协商 PDU {pdu_len}B，生效 {effective_pdu}B → 每请求 {items_per_request} 项)",
                                    cfg.endpoint
                                );
                                client = Some(c);
                            }
                            Err(e) => {
                                tracker.tick_fail(&format!("connect failed: {e}"));
                                tracing::warn!("[s7Client] inst {inst_id} connect failed: {e}");
                                if tracker.should_flush() {
                                    tracker.flush().await;
                                }
                                continue;
                            }
                        }
                    }
                    let c = match &client {
                        Some(c) => c,
                        None => continue,
                    };
                    let mut tick_ok = true;
                    // 连接丢失标志（借用结束后统一拆连接——c 借用期间不能改 client）。
                    let mut conn_lost = false;
                    for dev in &devices {
                        if dev.batch.is_empty() && dev.singles.is_empty() {
                            continue;
                        }
                        let mut values = BTreeMap::new();

                        // ① string/wstring/raw 单点读（per-item 容错；连接错误中断）。
                        for (field, addr, kind, arg) in &dev.singles {
                            // B-1：单点读包 io_timeout（库会话期无超时）。
                            let r =
                                tokio::time::timeout(io_timeout, read_single(c, addr, *kind, *arg))
                                    .await;
                            match r {
                                Ok(Ok(v)) => {
                                    values.insert(field.clone(), v);
                                }
                                Ok(Err(e)) => {
                                    if conn_fatal(&e) {
                                        conn_lost = true;
                                        tracing::warn!(
                                            "[s7Client] inst {inst_id} 单点 '{addr}' 读丢失连接: {e}"
                                        );
                                        break;
                                    }
                                    // S-15/F-3：单点失败限流 warn（60s/点）+ 计数 +
                                    // 投递 Null（不再静默跳过让旧值冒充新鲜数据）。
                                    point_bad = point_bad.saturating_add(1);
                                    if bad_warn.allow(&format!("{}.{}", dev.devid, field)) {
                                        tracing::warn!(
                                            "[s7Client] inst {inst_id} 点位 '{field}'({addr}) 读失败: {e}；该点投递 Null（坏值累计 {point_bad}）"
                                        );
                                    }
                                    values.insert(field.clone(), SampleValue::Null);
                                }
                                Err(_) => {
                                    // B-1：无响应（半开）——视作连接失效。
                                    conn_lost = true;
                                    tracing::warn!(
                                        "[s7Client] inst {inst_id} 单点 '{addr}' 读超时 ({}ms)，拆连接",
                                        cfg.timeout_ms
                                    );
                                    break;
                                }
                            }
                        }
                        if conn_lost {
                            // F-3：本设备还没读到的批量点一并标 Null（断链时不能留旧值
                            // 冒充新鲜数据），已读到的点照常投递。
                            for p in &dev.batch {
                                values.entry(p.0.clone()).or_insert(SampleValue::Null);
                            }
                            let _ = send_device_sample(&tx, &ctx.hwid, &dev.devid, values).await;
                            break;
                        }

                        // ② 数值/位批量读：按 items_per_request（协商 PDU 派生）分片多
                        // PDU（库不自动分批，超限直接报错 → 触发 S-2 热重连）。
                        let mut items: Vec<RequestItem> = Vec::with_capacity(dev.batch.len());
                        let mut ok_pts: Vec<&(String, String, Kind)> =
                            Vec::with_capacity(dev.batch.len());
                        for p in &dev.batch {
                            match build_request(&p.1, p.2) {
                                Ok(it) => {
                                    items.push(it);
                                    ok_pts.push(p);
                                }
                                Err(e) => {
                                    // 装配期已过 address 校验，这里只剩库解析器更严的
                                    // 边界形态（防御）：标 Null + 限流 warn，不中断整轮。
                                    point_bad = point_bad.saturating_add(1);
                                    if bad_warn.allow(&format!("{}.{}", dev.devid, p.0)) {
                                        tracing::warn!(
                                            "[s7Client] inst {inst_id} 点位 '{}' 地址 '{}' 无法构造请求: {e}；投递 Null",
                                            p.0,
                                            p.1
                                        );
                                    }
                                    values.insert(p.0.clone(), SampleValue::Null);
                                }
                            }
                        }
                        let mut read_failed = false;
                        for (chunk_items, chunk_pts) in items
                            .chunks(items_per_request)
                            .zip(ok_pts.chunks(items_per_request))
                        {
                            // B-1：批量读包 io_timeout（库会话期无超时，半开连接会
                            // 永久挂死 tick 分支）。
                            match tokio::time::timeout(io_timeout, c.read(chunk_items.to_vec()))
                                .await
                            {
                                Ok(Ok(data_items)) => {
                                    // S-17：库会丢弃非 Data 项（client.rs:1337-1349），
                                    // 项数不符时 `zip` 会把后续点整体错位——整批标 Null
                                    // 并告警，绝不错位取值。
                                    if data_items.len() != chunk_pts.len() {
                                        item_mismatch = item_mismatch.saturating_add(1);
                                        tracing::warn!(
                                            "[s7Client] inst {inst_id} 读响应项数 {} 与请求 {} 不符（库丢弃非 Data 项），本批全部标 Null（累计 {item_mismatch}）",
                                            data_items.len(),
                                            chunk_pts.len()
                                        );
                                        for p in chunk_pts {
                                            values.insert(p.0.clone(), SampleValue::Null);
                                        }
                                        continue;
                                    }
                                    for (p, di) in chunk_pts.iter().zip(data_items.iter()) {
                                        match decode_item(p.2, di) {
                                            Some(v) => {
                                                values.insert(p.0.clone(), v);
                                            }
                                            None => {
                                                // S-15/F-3：单项坏值限流 warn + 计数 +
                                                // 投递 Null（例：PLC 回 InvalidAddress）。
                                                point_bad = point_bad.saturating_add(1);
                                                if bad_warn
                                                    .allow(&format!("{}.{}", dev.devid, p.0))
                                                {
                                                    tracing::warn!(
                                                        "[s7Client] inst {inst_id} 点位 '{}'({}) 解码失败 return_code={:?} data_len={}；投递 Null（坏值累计 {point_bad}）",
                                                        p.0,
                                                        p.1,
                                                        di.return_code,
                                                        di.data.len()
                                                    );
                                                }
                                                values.insert(p.0.clone(), SampleValue::Null);
                                            }
                                        }
                                    }
                                }
                                Ok(Err(e)) => {
                                    read_failed = true;
                                    tracker.tick_fail(&format!("read failed: {e}"));
                                    tracing::warn!(
                                        "[s7Client] inst {inst_id} read failed: {e}"
                                    );
                                    // 连接类错误：拆连接重连（库的 auto_reconnect 也
                                    // 可能在后台恢复，此处兜底）。B-2：is_connection_error
                                    // 会话期恒 false（见 conn_fatal 注释），IoErr 一并计入。
                                    if conn_fatal(&e) {
                                        conn_lost = true;
                                    }
                                    break;
                                }
                                Err(_) => {
                                    read_failed = true;
                                    conn_lost = true;
                                    tracker.tick_fail("read timeout");
                                    tracing::warn!(
                                        "[s7Client] inst {inst_id} read timeout ({}ms)，拆连接",
                                        cfg.timeout_ms
                                    );
                                    break;
                                }
                            }
                        }
                        if read_failed {
                            tick_ok = false;
                            // F-3：本设备未读到值的批量点按坏质量投递（读取中断时不能
                            // 留旧值冒充新鲜数据；rtdata/history 会跳过 Null）。
                            for p in &dev.batch {
                                values.entry(p.0.clone()).or_insert(SampleValue::Null);
                            }
                            let _ = send_device_sample(&tx, &ctx.hwid, &dev.devid, values).await;
                            break;
                        }

                        if !send_device_sample(&tx, &ctx.hwid, &dev.devid, values).await {
                            break;
                        }
                    }
                    // c 借用已结束，此处统一拆连接（单点/批量读路径共享）。
                    if conn_lost {
                        tick_ok = false;
                        tracker.tick_fail("connection lost");
                        // S-2：先 disconnect 停库 actor 再放下。拆不干净（actor 在退避
                        // 重连中）时保留 client，下拍继续尝试——绝不直接 drop。
                        if let Some(c) = client.as_mut() {
                            if shutdown_conn(c, inst_id.as_str()).await {
                                client = None;
                            } else {
                                shutdown_pending = true;
                            }
                        }
                    }
                    if tick_ok {
                        tracker.tick_ok(tick_started.elapsed().as_millis() as u64);
                        consec_read_fails = 0;
                    } else {
                        // B-2 兜底：错误形态不可判别（`Error(_)` 字符串类，如 actor
                        // 已死后的 "Actor disconnected"）时靠连续失败计数拆连接——
                        // 否则 auto_reconnect=false 下一次断线即永久停摆。
                        consec_read_fails = consec_read_fails.saturating_add(1);
                        if consec_read_fails >= CONSEC_FAIL_RECONNECT
                            && client.is_some()
                            && !shutdown_pending
                        {
                            tracing::warn!(
                                "[s7Client] inst {inst_id} 连续 {consec_read_fails} 拍读失败（错误形态不可判别，视作连接失效重连）"
                            );
                            if let Some(c) = client.as_mut() {
                                if shutdown_conn(c, inst_id.as_str()).await {
                                    client = None;
                                } else {
                                    shutdown_pending = true;
                                }
                            }
                        }
                    }
                    // S-15/S-17 计数并入 stat extra（只在有累计时写，避免每拍覆盖）。
                    if point_bad > 0 {
                        tracker.extra_insert("point_bad_count", serde_json::json!(point_bad));
                    }
                    if item_mismatch > 0 {
                        tracker.extra_insert(
                            "read_item_mismatch_count",
                            serde_json::json!(item_mismatch),
                        );
                    }
                    // F-7：一轮采集慢于 period → Skip 静默丢拍，这里限流 warn + 计数。
                    let elapsed = tick_started.elapsed();
                    if elapsed > period {
                        let warn_now = overrun.record(std::time::Instant::now());
                        tracker.extra_insert(
                            "tick_overrun_count",
                            serde_json::json!(overrun.count),
                        );
                        if warn_now {
                            tracing::warn!(
                                "[s7Client] inst {inst_id} 本拍耗时 {elapsed:?} 超过采集周期 {period:?}（MissedTickBehavior::Skip 会丢拍；累计 {} 次）",
                                overrun.count
                            );
                        }
                    }
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }

        if let Some(mut c) = client {
            // B-3：disconnect 包 1s 上限——库 actor 在重连循环中时 Disconnect 要等
            // 重连成功才被处理，PLC 不可达即无限期挂起（stop 已 3s 超时返回，task 泄漏）。
            // S-2：这也是"退而求其次"的拆法——停机路径上超时后仍会 drop（进程/实例
            // 生命周期即将结束，残留 actor 随 runtime 一并收回）。
            let _ = tokio::time::timeout(CONN_SHUTDOWN_TIMEOUT, c.disconnect()).await;
        }
        tracing::info!("[s7Client] inst {inst_id} exited");

        // 统一退出处理（cancelled 触发；stat 落 stopped）。
        tracker.set_status("stopped");
        tracker.flush().await;
    }
}

/// 处理 write 命令并**回报业务成败**（F-2，2026-11-01 协议语义审查）。
///
/// `data` 兼容两种契约（`address` 优先，旧行为不变）：
/// - HMI：`{"tag":"点位显示名","value":42.5}`——按 `devid` + 启动期索引反查地址/kind（F-1）；
/// - 原生：`{"address":"DB1.DBD0","value":42.5,"kind":"f32"}`。
///
/// 返回值语义：`Ok(())` = PLC 侧确认写入（`verify_write` 时还要求读回一致）；
/// `Err(原因)` = 任何业务失败（JSON/地址/tag/kind/长度/取值越界/未连接/超时/写后校验
/// 不一致），由 run() 经 `finish_write` 回给 REST 写入口（422）。
async fn handle_write(
    client: &Option<S7Conn>,
    data: &str,
    devid: &str,
    index: &WriteIndex,
    cfg: &S7Cfg,
    io_timeout: Duration,
    verify_fails: &mut u64,
) -> Result<(), String> {
    // 先解析校验（配置/命令错误即使未连接也要如实回 422），再检查连接。
    let prepared = PreparedWrite::build(data, devid, index)?;
    let c = client.as_ref().ok_or_else(|| {
        format!(
            "写命令目标 {} 无法下发：实例未连接 PLC（下个采集 tick 重连）",
            prepared.addr
        )
    })?;
    // B-1：写路径同款 io_timeout（库会话期写无超时，半开连接会把 handle_write
    // 连同整个 select 挂死）。超时后连接由下个读 tick 的失败计数接管拆连接。
    match tokio::time::timeout(io_timeout, write_typed(c, &prepared)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("写 {} 失败: {e}", prepared.addr)),
        Err(_) => {
            return Err(format!(
                "写 {} 超时（{io_timeout:?}，PLC 无响应）",
                prepared.addr
            ))
        }
    }
    // S-1：tokio-s7 丢弃写应答的 item 级 return_code（`<REG>tokio-s7-0.1.7/src/client.rs:1374-1382`
    // 只做头级 check_response 就 Ok(())），写失败一律当成功。`verify_write` 是 fork
    // 补丁落地前的临时防线：按同地址读回比对，不一致即报错（回 422 + 计数）。
    // **上游修复后应删除本分支**（配置项与读回逻辑一并删）。
    if cfg.verify_write {
        match tokio::time::timeout(io_timeout, verify_written(c, &prepared)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                *verify_fails = verify_fails.saturating_add(1);
                return Err(format!(
                    "写后校验失败: {}（累计 {verify_fails} 次；PLC 未接受写入或地址/宽度不匹配）",
                    e
                ));
            }
            Err(_) => {
                *verify_fails = verify_fails.saturating_add(1);
                return Err(format!(
                    "写后校验超时（{io_timeout:?}）：写入已发出但读回无响应（累计 {verify_fails} 次）"
                ));
            }
        }
    }
    Ok(())
}

/// 写后读回校验（S-1 的插件侧临时防线）：按同一地址/kind 读回并与写入值比较。
///
/// 比较口径：整数/布尔精确、浮点相对误差 1e-6、字符串/raw 精确（[`value_matches`]）。
/// bit 写按位比较——库的 `read_bool` 发的是 BIT 寻址（传输尺寸 1 bit），PLC 只回
/// 目标位，因此不会把相邻位混进来。
async fn verify_written(c: &S7Conn, p: &PreparedWrite) -> Result<(), String> {
    let want = p.value.expect();
    let got = match p.kind {
        Kind::String | Kind::WString | Kind::Raw(_) => read_single(c, &p.addr, p.kind, p.arg)
            .await
            .map_err(|e| format!("读回失败: {e}"))?,
        Kind::Bool => SampleValue::Bool(
            c.read_bool(&p.addr)
                .await
                .map_err(|e| format!("读回失败: {e}"))?,
        ),
        _ => {
            // 用 read_bytes + 长度校验（S-21：库 typed 读在 item 失败时会 panic）。
            let n = kind_byte_count(p.kind) as u16;
            let bytes = c
                .read_bytes(&p.addr, n)
                .await
                .map_err(|e| format!("读回失败: {e}"))?;
            decode_bytes(p.kind, &bytes).map_err(|e| format!("读回解码失败: {e}"))?
        }
    };
    if value_matches(p.kind, &want, &got) {
        Ok(())
    } else {
        Err(format!("写 {} 期望 {:?}，读回 {:?}", p.addr, want, got))
    }
}

/// 按已强类型化的写值调库 typed write_* 方法（大端编码由库处理）。
///
/// S-11 之后这里**不再有 `as` 转换**：`WVal` 与 kind 一一对应（`convert_write` 已做
/// 范围/有限性校验），写错宽度或越界在到达这里之前就被拒绝。
async fn write_typed(c: &S7Conn, p: &PreparedWrite) -> Result<(), S7Error> {
    let addr = p.addr.as_str();
    match &p.value {
        WVal::Bool(b) => c.write_bool(addr, *b).await,
        // byte 是有符号 i8：按位取 u8（-1 → 0xFF），读回仍按 i8 解释。
        WVal::I8(v) => c.write_byte(addr, *v as u8).await,
        WVal::U8(v) => c.write_byte(addr, *v).await,
        WVal::I16(v) => c.write_int16(addr, *v).await,
        WVal::U16(v) => c.write_uint16(addr, *v).await,
        WVal::I32(v) => c.write_int32(addr, *v).await,
        WVal::U32(v) => c.write_uint32(addr, *v).await,
        WVal::I64(v) => c.write_int64(addr, *v).await,
        WVal::U64(v) => c.write_uint64(addr, *v).await,
        WVal::F32(v) => c.write_float32(addr, *v).await,
        WVal::F64(v) => c.write_float64(addr, *v).await,
        WVal::Str(s) => match p.kind {
            Kind::WString => c.write_wstring(addr, p.arg, s).await,
            _ => c.write_string(addr, p.arg, s).await,
        },
    }
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "s7Client",
        create: || Box::new(S7Client),
    }
}

#[cfg(test)]
mod tests;
