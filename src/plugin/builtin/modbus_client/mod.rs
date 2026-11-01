//! `modbusClient`：Modbus 南向采集插件（tokio-modbus 0.17），支持 **TCP / RTU（serial /
//! over-tcp / over-udp）** 四种传输，统一产出 `Context`，采集/写逻辑同构。
//!
//! 周期连接外部 Modbus 从站，按 `devices` 配置批量读，组装 `Sample` 推入 hub。
//! 同时消费下行命令（`cmd="write"`）写回从站。满足现场三类需求：
//!
//! 1. **布尔**：独立 coil/discrete（FC01/FC02，`coil:0:bool`）或 holding/input 寄存器位
//!    解析（`holding:0:bit:3`，多个布尔打包进一个寄存器）。
//! 2. **混合类型批量读 + 偏移转换**（一次 FC03/FC04 读连续区间，各点按 offset 解码）。
//! 3. **大块读取部分使用**（`gap_threshold`/`max_regs` 控制区间合并）。
//!
//! 配置（`InstConfig::app_config` JSON，扁平 `kind` 决定传输；`kind` 缺省即 tcp，向后兼容）：
//!
//! **TCP**（默认）：
//! ```json
//! { "kind":"tcp", "endpoint":"192.168.1.5:502", "slave_id":1, "period":3,
//!   "gap_threshold":16, "max_regs":100, "reconnect_secs":2 }
//! ```
//! **RTU over serial**（串口直连 RS-485/RS-232；`serial_port` 必填，串口参数缺省 9600/8N1/NONE）：
//! ```json
//! { "kind":"rtu_serial", "serial_port":"COM3", "baud_rate":9600,
//!   "data_bits":8, "parity":"none", "stop_bits":1, "flow_control":"none",
//!   "slave_id":1, "period":3 }
//! ```
//! **RTU over TCP**（串口服务器/网关透传，RTU 帧+CRC 走 TCP，无 MBAP 头）：
//! ```json
//! { "kind":"rtu_tcp", "endpoint":"192.168.1.5:502", "slave_id":1, "period":3 }
//! ```
//! **RTU over UDP**（部分 DTU 用 UDP datagram 透传 RTU 帧；内部 connected socket，
//! datagram 边界天然保帧，无流式粘包）：
//! ```json
//! { "kind":"rtu_udp", "endpoint":"10.0.0.7:502", "slave_id":1, "period":3 }
//! ```
//!
//! 串口参数（仅 rtu_serial）：`baud_rate`(默认 9600) / `data_bits`(5/6/7/8，默认 8) /
//! `parity`(none/even/odd，默认 none) / `stop_bits`(1/2，默认 1) /
//! `flow_control`(none/software/hardware，默认 none)。
//!
//! 变量映射（复用 `devices`，与 opcua/datasim 对齐）：
//! - `DeviceSpec.value` = 设备 ID（`rtdata:<devid>` 的 key）
//! - `ChildSpec.value` = 地址串（如 `holding:0:f32:ABCD`）
//! - `ChildSpec.label` = values map key（显示名；空则用地址串）
//!
//! 地址串语法 `<func>:<addr>:<type>[:<byte_order>][:<bit>]`：
//! - `func`：`coil`(FC01) / `discrete`(FC02，只读) / `holding`(FC03) / `input`(FC04，只读)
//! - `addr`：0-based 地址（u16；与 wire 一致，**不做 40001 自动转换**——各厂商减 1/减 0
//!   不一致。PLC tag 换算：holding=tag-40001，input=tag-30001，coil=tag-1，discrete=tag-10001）
//! - `type`：
//!   - `coil`/`discrete`：仅 `bool`（直接布尔，1 bit/addr，FC01/FC02 返回 `Vec<bool>`）
//!   - `holding`/`input`：`bit`（寄存器位，需位号）/ `i16`/`u16`/`i32`/`u32`/`f32`/`f64`
//! - `byte_order`（holding/input 的 i32/u32/f32/f64 可选，默认 `ABCD`）：`ABCD`(大端)/
//!   `CDAB`(字交换)/`BADC`(字内字节交换)/`DCBA`(小端)。`i16`/`u16` 单寄存器无字节序，带则报错。
//! - `bit`（holding/input 的 `bit` 类型必填）：0-15
//!
//! 示例：`coil:0:bool`、`discrete:5:bool`、`holding:0:f32:ABCD`、`holding:10:bit:3`、`input:5:u16`。
//!
//! **字节序**：以 32 位 4 字节 `[A,B,C,D]`（`A=reg0 高字节`）为基准，置换后统一
//! `from_be_bytes`。4 种记法均为对合置换（`perm4[perm4[i]]==i`），故 `encode` 是 `decode`
//! 的逆，单测以 round-trip 为金标准。
//!
//! **批量合并**：`gap_threshold` 约束相邻两点**最大单空洞**（非未用总数）；`max_regs`
//! 约束单区间最大单元数（coil/discrete 为 bit 数，holding/input 为寄存器数；协议上限
//! 寄存器类 125、coil/discrete 2000，真实设备常更低，遇 IllegalDataAddress 自动对半降级）。
//!
//! **写命令**（`data` JSON，两种契约等价，`address` 优先；成功/失败经 `finish_write`
//! 回报 REST 写入口）：
//! - 原生：`{"address":"holding:0:f32:ABCD","value":12.34}`
//! - HMI：`{"tag":"点位显示名","value":12.34}`（按 `msg.devid` + tag 查启动期索引，
//!   全表唯一 tag 可跨 devid 兜底）
//! - coil → FC05（单）/ FC0F（多）
//! - holding u16/i16/bit → FC06；i32/u32/f32/f64 → FC16；bit 走 RMW（读-改-写，单 task 内
//!   原子但非事务）
//! - discrete/input 只读，写拒绝
//! - 写值按点位类型 checked 校验：越界/非有限浮点/无法识别的布尔串一律拒写并回报原因
//!   （不做 `as` 回绕、不做 0 兜底）
//! - `data.slave_id` 不得覆盖设备归属（devid 已映射到 slave A 时只允许 A）
//!
//! **读降级**：region 读失败（异常/超时/transport）为其所有点位投递 `SampleValue::Null`
//! （坏质量标记）；`IllegalDataAddress` 额外把该 region 按点位对半拆，下一 tick 用细分
//! 区间（单点位后停止拆分）。
//!
//! **传输边界**：TCP / RTU(serial / over-tcp / over-udp) 四种均产出统一 `Context`，
//! 采集/写逻辑同构；ASCII 暂不支持（tokio-modbus 无，未来可借 `rmodbus` codec 后置）。
//! 实例级 slave_id（多 RTU 从站网关需每 slave 建一实例，共享 endpoint）。RTU over UDP 用
//! connected socket（`connect(peer)` 后 `send`/`recv` 无需 addr），datagram 边界天然消除
//! 流式粘包/分帧歧义。

mod address;
mod codec;
mod config;
mod merge;
mod transport;

use crate::host::HostContext;
use crate::plugin::sample_sender::SampleSender;
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, DeviceSpec, InstConfig, Sample, SampleValue};
use address::{addr_fc, addr_func_name, parse_point, Func, PointSpec, Type};
use async_trait::async_trait;
use codec::{decode_point, encode_value, Buf, WritePayload};
use config::{parse_cfg, PollStrategy, Transport};
use merge::{merge_regions, Region};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_modbus::client::{Context, Reader, Writer};
use tokio_modbus::prelude::SlaveContext;
use tokio_modbus::{ExceptionCode, Slave};
use tokio_util::sync::CancellationToken;
use transport::connect;

#[cfg(test)]
use crate::types::ChildSpec;
#[cfg(test)]
use address::Order;
#[cfg(test)]
use codec::{decode_f32, decode_u32, decode_u64, encode_u32, encode_u64, extract_bit};
#[cfg(test)]
use config::{FlowKind, ParityKind};
#[cfg(test)]
use tokio::net::UdpSocket;
#[cfg(test)]
use transport::UdpIo;

/// Modbus 采集插件（无状态工厂标记，每个实例 task 内持有自己的 `Context`）。
/// 支持 TCP / RTU（serial / over-tcp / over-udp）四种传输，统一产出 `Context`，采集/写逻辑同构。
#[derive(Debug, Default)]
pub struct ModbusClient;

/// JSON → SampleValue（写命令 value 字段）。与 opcua_client 同实现。
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

/// 一个 slave 的采集计划：devid + unit id + 预合并的 region 列表（owned）。
///
/// 预解析产物（`plan_devices`），采集循环每 tick 直接遍历 `regions`，不再重复 `merge_regions`。
/// `slave_id` 由 `DeviceSpec.slave_id` 提供，缺省回退实例级 `cfg.slave_id`（向后兼容）。
struct SlavePlan {
    /// 设备 ID（Sample.devid / rtdata:<devid> 的 key）。
    devid: String,
    /// Modbus unit id（读前 `set_slave`）。
    slave_id: u8,
    /// 该 slave 的合并读区间（owned；地址解析失败的点位已丢弃）。
    regions: Vec<Region>,
}

/// 预解析 `devices` → `Vec<SlavePlan>`（纯函数，便于单测）。
///
/// - 每个有点子的 device → 一个 `SlavePlan`；无点位设备跳过。
/// - 点位地址解析失败 → `warn` 丢弃该点位，保留其余。
/// - `slave_id`：`DeviceSpec.slave_id` 优先，缺省回退 `fallback_slave_id`（= `cfg.slave_id`）。
fn plan_devices(
    devices: &[DeviceSpec],
    gap_threshold: u16,
    max_regs: u16,
    fallback_slave_id: u8,
) -> Vec<SlavePlan> {
    devices
        .iter()
        .filter_map(|dev| {
            let pairs: Vec<(String, PointSpec)> = dev
                .children
                .iter()
                .filter_map(|c| {
                    parse_point(&c.value)
                        .map(|p| {
                            let field = if c.label.is_empty() {
                                c.value.clone()
                            } else {
                                c.label.clone()
                            };
                            (field, p)
                        })
                        .map_err(|e| {
                            tracing::warn!(
                                "[modbusClient] dev {} bad address '{}': {}",
                                dev.value,
                                c.value,
                                e
                            );
                            e
                        })
                        .ok()
                })
                .collect();
            if pairs.is_empty() {
                return None;
            }
            let regions = merge_regions(&pairs, gap_threshold, max_regs);
            Some(SlavePlan {
                devid: dev.value.clone(),
                slave_id: dev.slave_id.unwrap_or(fallback_slave_id),
                regions,
            })
        })
        .collect()
}

/// 构造一个 tick 的轮询序列 `(plan_idx, region_idx)`，按 `strategy` 排列。
///
/// - `RoundRobin`：外层按 region 下标——先所有 slave 的第 0 region，再第 1 region…
///   （所有 slave 齐头并进、刷新均匀；period 内轮不完时每 slave 都采到一部分）
/// - `Sequential`：外层按 slave——slave0 所有 region、slave1 所有 region…
///   （单 slave 多点时间一致，但 slave 间有延迟）
///
/// 半双工 RS485 物理串行，两种都是顺序遍历，差别仅在排列。
fn build_poll_order(plans: &[SlavePlan], strategy: PollStrategy) -> Vec<(usize, usize)> {
    let max_regions = plans.iter().map(|p| p.regions.len()).max().unwrap_or(0);
    let mut order = Vec::new();
    match strategy {
        PollStrategy::RoundRobin => {
            for r in 0..max_regions {
                for (pi, plan) in plans.iter().enumerate() {
                    if r < plan.regions.len() {
                        order.push((pi, r));
                    }
                }
            }
        }
        PollStrategy::Sequential => {
            for (pi, plan) in plans.iter().enumerate() {
                for r in 0..plan.regions.len() {
                    order.push((pi, r));
                }
            }
        }
    }
    order
}

/// 写目标索引（F-1，2026-11-01 协议语义审查）：点位显示名（tag）→ 已解析的绝对地址规约。
///
/// HMI 只发 `{"tag":点位显示名,"value":…}`（`hmi-studio/src/lib/command.ts`），
/// 而向从站写必须知道绝对地址。索引在启动期由 `plan_devices` 的产物构建——
/// 即 `inst.devices` 经绑定展开后的绝对地址（含 `slave_id` 解析结果）。
#[derive(Debug, Default)]
struct WriteIndex {
    /// devid → (tag → 点位规约)：本设备命中优先（防同名 tag 跨设备歧义）。
    by_devid: HashMap<String, HashMap<String, PointSpec>>,
    /// tag → [(devid, 点位规约)]：供"全表唯一 tag"跨 devid 兜底与歧义判定。
    global: HashMap<String, Vec<(String, PointSpec)>>,
}

impl WriteIndex {
    /// 从采集计划建索引：各 region 的 `(field, spec)` 即（Sample key = label, 绝对地址）。
    fn from_plans(plans: &[SlavePlan]) -> Self {
        let mut idx = Self::default();
        for plan in plans {
            for reg in &plan.regions {
                for (field, spec) in &reg.points {
                    idx.by_devid
                        .entry(plan.devid.clone())
                        .or_default()
                        .insert(field.clone(), spec.clone());
                    idx.global
                        .entry(field.clone())
                        .or_default()
                        .push((plan.devid.clone(), spec.clone()));
                }
            }
        }
        idx
    }

    /// 按 devid + tag 解析目标；本设备未命中时，仅当 tag 全表唯一才兜底（有歧义就拒）。
    fn resolve(&self, devid: &str, tag: &str) -> Result<(String, PointSpec), String> {
        if let Some(spec) = self.by_devid.get(devid).and_then(|m| m.get(tag)) {
            return Ok((devid.to_string(), spec.clone()));
        }
        match self.global.get(tag).map(Vec::as_slice) {
            Some([(d, spec)]) => Ok((d.clone(), spec.clone())),
            Some(hits) => Err(format!(
                "写命令 tag '{tag}' 在设备 {devid} 不存在，且全表有 {} 处同名点位（{:?}），无法确定目标",
                hits.len(),
                hits.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>()
            )),
            None => Err(format!("写命令 tag '{tag}' 在设备 {devid} 与全表均无此点位")),
        }
    }
}

/// 解析写命令目标（F-1）：原生 `address` 优先（旧契约行为不变），缺失时用 `tag` 查索引。
///
/// 返回 `(目标 devid, 点位规约)`——跨 devid 兜底时目标 devid 随之切换（决定 slave 归属）。
fn resolve_write_target(
    req: &serde_json::Value,
    devid: &str,
    index: &WriteIndex,
) -> Result<(String, PointSpec), String> {
    if let Some(addr) = req.get("address").and_then(|v| v.as_str()) {
        let p = parse_point(addr).map_err(|e| format!("写命令 address '{addr}' 非法: {e}"))?;
        return Ok((devid.to_string(), p));
    }
    let tag = req.get("tag").and_then(|v| v.as_str()).ok_or_else(|| {
        "写命令缺少 address/tag（HMI 发 tag=点位显示名，原生客户端发 address）".to_string()
    })?;
    index.resolve(devid, tag)
}

/// 解析写命令的目标从站（MB-5，2026-11-01 协议语义审查）。
///
/// - devid 已映射到 slave A：`data.slave_id` 只允许等于 A（显式同值视为冗余），否则 Err
///   ——此前 `data.slave_id` 优先于 devid 映射，可向总线上任意从站写；
/// - devid 未映射（设备无点位，如纯软设备）：允许显式单播地址 1..=247；
/// - 两者都无 → Err。
fn resolve_write_slave(
    devid: &str,
    req: &serde_json::Value,
    devid_to_slave: &HashMap<String, u8>,
) -> Result<u8, String> {
    let explicit = match req.get("slave_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let n = v
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| (1..=247).contains(n))
                .ok_or_else(|| format!("写命令 slave_id={v} 非法（Modbus 单播从站地址 1-247）"))?;
            Some(n)
        }
    };
    match (devid_to_slave.get(devid).copied(), explicit) {
        (Some(mapped), Some(req_slave)) if mapped != req_slave => Err(format!(
            "写命令 slave_id={req_slave} 与设备 {devid} 归属的 slave {mapped} 冲突（拒写，不得向任意从站写）"
        )),
        (Some(mapped), _) => Ok(mapped),
        (None, Some(s)) => Ok(s),
        (None, None) => Err(format!(
            "写命令无法确定 slave_id（devid={devid} 未在 devices 配置，data 也无 slave_id）"
        )),
    }
}

/// F-3（2026-11-01 协议语义审查）：region 读失败时为其所有点位投递
/// `SampleValue::Null`（坏质量标记）——不再静默跳过，让下游把旧值当成本拍新鲜值。
/// 落盘侧（rtdata/历史）跳过 Null，不覆盖上次有效值。
fn mark_region_null(
    acc: &mut BTreeMap<String, BTreeMap<String, SampleValue>>,
    devid: &str,
    reg: &Region,
) {
    let entry = acc.entry(devid.to_string()).or_default();
    for (field, _) in &reg.points {
        entry.insert(field.clone(), SampleValue::Null);
    }
}

/// MB-7（2026-11-01 协议语义审查）：把 region 按点位对半拆成两个子区间（纯函数）。
///
/// 按**点位**切分而非地址中点：多寄存器点位（i32/u32/f32/f64）跨切点会读不完整，
/// 点位切分保证每个子区间内的点位都被完整覆盖（区间内空隙作废，只少读几个地址）。
/// 点位数 < 2 时无法再拆（已到单点位 = 保底 1 个单元）→ `None`，即拆分终止。
fn split_region(reg: &Region) -> Option<(Region, Region)> {
    if reg.points.len() < 2 {
        return None;
    }
    let mid = reg.points.len() / 2;
    let make = |pts: &[(String, PointSpec)]| -> Option<Region> {
        let start = pts.iter().map(|(_, p)| p.addr).min()?;
        let end = pts
            .iter()
            .map(|(_, p)| p.addr.saturating_add(p.width() - 1))
            .max()?;
        Some(Region {
            func: reg.func,
            start,
            end,
            points: pts.to_vec(),
        })
    };
    Some((make(&reg.points[..mid])?, make(&reg.points[mid..])?))
}

/// 应用一个 tick 收集到的拆分请求（MB-7）：把对应 region 替换为两个子区间。
///
/// 同一 plan 命中多个 region 时按 region 下标**降序**处理（一处替换为两处会挤后
/// 其后的下标）。返回是否发生变化（变了要重建轮询顺序）；不可再拆的请求记一条
/// warn 后保持原样（异常计数继续累加）。
fn apply_region_splits(plans: &mut [SlavePlan], reqs: Vec<(usize, usize)>, inst_id: &str) -> bool {
    let mut reqs = reqs;
    reqs.sort_unstable();
    reqs.dedup();
    let mut changed = false;
    for (pi, ri) in reqs.into_iter().rev() {
        let Some(plan) = plans.get_mut(pi) else {
            continue;
        };
        let Some(reg) = plan.regions.get(ri).cloned() else {
            continue;
        };
        match split_region(&reg) {
            Some((a, b)) => {
                tracing::warn!(
                    "[modbusClient] inst {inst_id} slave {} read {}+{} IllegalDataAddress：拆分为 {}+{} / {}+{}（下一 tick 生效）",
                    plan.slave_id,
                    reg.start,
                    reg.len(),
                    a.start,
                    a.len(),
                    b.start,
                    b.len()
                );
                plan.regions.splice(ri..=ri, [a, b]);
                changed = true;
            }
            None => tracing::warn!(
                "[modbusClient] inst {inst_id} slave {} read {}+{} IllegalDataAddress 且已无法再拆（单点位），继续按异常计数",
                plan.slave_id,
                reg.start,
                reg.len()
            ),
        }
    }
    changed
}

/// tick 耗时超过 period 的告警限流窗口（F-7，2026-11-01 协议语义审查）。
const OVERRUN_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// tick 超时告警状态（F-7）。
///
/// `MissedTickBehavior::Skip` 会静默丢拍：一轮轮询慢于 period 时采集频率悄悄低于
/// 配置值而无人知晓。这里按 [`OVERRUN_WARN_INTERVAL`] 限流 warn（防每拍刷屏），
/// 计数进 tracker extra（`tick_overrun_count`）供运维观察。
#[derive(Debug, Default)]
struct OverrunWarn {
    /// 累计超时次数（进 extra，不受限流影响）。
    count: u64,
    /// 上次告警时刻（限流窗口起点）。
    last: Option<std::time::Instant>,
}

impl OverrunWarn {
    /// 记录一次超时；返回本次是否应打 warn（首拍或距上次 ≥ 窗口）。
    fn record(&mut self, now: std::time::Instant) -> bool {
        self.count = self.count.saturating_add(1);
        let due = self
            .last
            .is_none_or(|t| now.duration_since(t) >= OVERRUN_WARN_INTERVAL);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// 一次 region 读结果（区分三类失败，驱动故障隔离决策）。
///
/// `Timeout` 在调用方由 `tokio::time::timeout` 包装产生（`read_region` 本身不超时）。
enum ReadOutcome {
    /// 成功，含解码缓冲。
    Ok(Buf),
    /// 协议异常（连接正常，如 IllegalDataAddress）——跳过该 region，不拆 session。
    Exception(ExceptionCode),
    /// transport 错（连接断）——拆 session 重连，中断本轮。
    Transport(tokio_modbus::Error),
    /// per-request 超时（slave 无响应）——跳过该 region，不拆 session（故障隔离）。
    Timeout,
}

/// 读一个 region（按 func 分发），返回 `ReadOutcome`。调用方负责 `set_slave` + 超时包装。
async fn read_region(sess: &mut Context, reg: &Region) -> ReadOutcome {
    let len = reg.len();
    let res = match reg.func {
        Func::Coil => sess
            .read_coils(reg.start, len)
            .await
            .map(|r| r.map(Buf::Bits)),
        Func::Discrete => sess
            .read_discrete_inputs(reg.start, len)
            .await
            .map(|r| r.map(Buf::Bits)),
        Func::Holding => sess
            .read_holding_registers(reg.start, len)
            .await
            .map(|r| r.map(Buf::Regs)),
        Func::Input => sess
            .read_input_registers(reg.start, len)
            .await
            .map(|r| r.map(Buf::Regs)),
    };
    match res {
        Ok(Ok(buf)) => ReadOutcome::Ok(buf),
        Ok(Err(exc)) => ReadOutcome::Exception(exc),
        Err(e) => ReadOutcome::Transport(e),
    }
}

/// `select!` 事件。把事件类型与 `Context` 的 `&mut` 借用解耦：modbus 的 read/write 取
/// `&mut self`（不像 opcua 的 `Arc<Session>` 不可变借用），若在 `select!` 分支内直接
/// `&mut session`，两分支会编译期 borrow 冲突。故 select 只产事件，`&mut session` 在
/// `match evt` 单分支内完成。
enum Evt {
    Tick,
    Cmd(Option<CtlMessage>),
}

#[async_trait]
impl Source for ModbusClient {
    fn name(&self) -> &'static str {
        "modbusClient"
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
                tracing::warn!("[modbusClient] inst {inst_id} bad config: {e}");
                return;
            }
        };
        // M-2 模型驱动装配：绑定展开（模板 addr × devreg params → 绝对地址，
        // slave_id ← bind），后续 plan/轮询逻辑零改动。
        let devices = match crate::model::expand::expand_to_specs(&ctx.lux, &inst.devices).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("[modbusClient] inst {inst_id} 绑定展开失败（设备未注册？）: {e}");
                return;
            }
        };
        // 预解析 devices → SlavePlan（每 slave 独立 regions + slave_id）+ 轮询顺序。
        // plans/poll_order 可变：MB-7 的 IllegalDataAddress 降级会在运行期把一个 region
        // 拆成两个并重建轮询顺序。
        let mut plans = plan_devices(&devices, cfg.gap_threshold, cfg.max_regs, cfg.slave_id);
        let mut poll_order = build_poll_order(&plans, cfg.poll_strategy);
        let req_timeout = Duration::from_secs(cfg.request_timeout_secs);
        // MB-8（2026-11-01 协议语义审查）：RTU 类传输的 region 间帧间静默（上游库无 t3.5）。
        // 0 或 TCP 不插 sleep（TCP 无帧间静默语义）。
        let request_gap = match cfg.transport {
            Transport::Tcp => None,
            Transport::RtuSerial | Transport::RtuTcp | Transport::RtuUdp => {
                (cfg.request_gap_ms > 0).then(|| Duration::from_millis(cfg.request_gap_ms))
            }
        };
        // C-3（2026-11-01 审查）：per-plan 连续超时计数。RTU 帧无事务 ID——读超时后
        // 迟到的响应会串到下个同功能码请求（tick N 的 region A 响应被当作
        // tick N+1 的 region B 数据解码发布，**静默错值**）。Timeout 分支此前
        // 永不拆 session（故障隔离取舍），现在同一 plan 连续 2 次超时即拆 session
        // 重连（保单 slave 隔离：计数按 plan 独立，任何成功/exception 响应归零
        // ——exception 也是 slave 在应答的活性证明）。
        const TIMEOUT_STREAK_DROP: u32 = 2;
        let mut timeout_streak: Vec<u32> = vec![0; plans.len()];
        // handle_write 反查 devid → slave_id（写命令按目标设备切 slave）。
        let devid_to_slave: HashMap<String, u8> = plans
            .iter()
            .map(|p| (p.devid.clone(), p.slave_id))
            .collect();
        // F-1：写目标索引（devid → tag → 绝对地址），供 HMI 的 {"tag","value"} 写命令反查。
        let write_index = WriteIndex::from_plans(&plans);
        // F-2：声明本实例支持写回执——REST 写入口据此在窗口内等待 handle_write 的业务
        // 结果（成功 200 / 明确失败 422 / 无回报 504）。放在装配成功之后：配置/绑定失败
        // 直接返回的实例不声明能力（避免写入口白等）。
        ctx.ctl_registry.declare_write_ack(inst_id.as_str());

        let period = Duration::from_secs(cfg.period);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase =
            std::time::Duration::from_millis(crate::ticker::phase_ms(inst_id.as_str(), period));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        // exception（modbus 协议层错误码）独立计数——与 transport 失败区分（后者拆 session）。
        let mut exceptions: u64 = 0;
        // F-7：tick 超 period 的告警限流状态。
        let mut overrun = OverrunWarn::default();
        let mut flush_ticker = crate::plugin::stat_tracker::flush_ticker(inst_id.as_str());
        // 启动即落一次初始 stat（ticker 首拍带相位延迟，见 stat_tracker::flush_ticker）。
        tracker.flush().await;

        let mut session: Option<Context> = None;

        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                _ = flush_ticker.tick() => {
                    tracker.flush_if_due().await;
                    continue;
                }
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    // F-2：每条命令处理后回报业务结果（成功/失败原因），REST 写入口据此
                    // 返回 200/422；无回执通道（request_id=None）时 finish_write 是 no-op。
                    let result = if m.cmd == "write" {
                        handle_write(
                            &mut session,
                            &m.data,
                            &m.devid,
                            &write_index,
                            &devid_to_slave,
                            req_timeout,
                        )
                        .await
                    } else {
                        tracing::info!("[modbusClient] inst {inst_id} unknown cmd: {}", m.cmd);
                        Err(format!("实例不支持命令 '{}'（仅 write）", m.cmd))
                    };
                    if let Err(e) = &result {
                        tracing::warn!(
                            "[modbusClient] inst {inst_id} cmd {} devid {} 失败: {e}",
                            m.cmd,
                            m.devid
                        );
                    }
                    ctx.ctl_registry
                        .finish_write(m.request_id.as_deref(), result);
                }
                Evt::Cmd(None) => break,
                Evt::Tick => {
                    if session.is_none() {
                        match connect(&cfg, inst_id.as_str(), ctx.trace_registry.clone()).await {
                            Ok(c) => {
                                tracing::info!(
                                    "[modbusClient] inst {inst_id} connected {} slave {}",
                                    cfg.endpoint,
                                    cfg.slave_id
                                );
                                session = Some(c);
                            }
                            Err(e) => {
                                tracing::warn!("[modbusClient] inst {inst_id} connect failed: {e}");
                                // C-6（2026-11-01 审查）：退避 sleep 与 token 竞争——
                                // 此前裸 sleep 最长把停机延迟 reconnect_secs（可配 86400）。
                                tokio::select! {
                                    _ = token.cancelled() => break,
                                    _ = tokio::time::sleep(Duration::from_secs(cfg.reconnect_secs)) => {}
                                }
                                continue;
                            }
                        }
                    }
                    let sess = match &mut session {
                        Some(s) => s,
                        None => continue,
                    };
                    // 单 tick 内所有 devid 共用一个 timestamp（点间一致，对标 opcua/s7 批量读语义）。
                    let now = chrono::Local::now().timestamp();
                    // tick 计时（tick_ok 上报耗时；一轮轮询=从首个 region read 到 acc 发送完）。
                    let tick_started = std::time::Instant::now();
                    // 按 devid 累积解码值（round_robin 下同 devid 的 region 散落序列各处，故聚合）。
                    let mut acc: BTreeMap<String, BTreeMap<String, SampleValue>> = BTreeMap::new();
                    let mut transport_failed = false;
                    // 已超时 slave：跳过其剩余 region（spec §3.2 Sequential「超时 break 到下一
                    // slave」；RoundRobin 同样受益——死从站后续 region 也大概率超时，跳过省时，
                    // 避免死从站 K region 付 K×timeout 拖垮整轮）。换 slave（pi 变）自然解除。
                    let mut timed_out_slave: Option<usize> = None;
                    // MB-7：本 tick 命中 IllegalDataAddress 且还能拆的 region（tick 末统一应用，
                    // 避免遍历 poll_order 期间改 plans 造成借用冲突）。
                    let mut split_requests: Vec<(usize, usize)> = Vec::new();
                    for (step, (pi, ri)) in poll_order.iter().enumerate() {
                        if timed_out_slave.is_some_and(|s| s == *pi) {
                            // F-3：该 slave 已超时，本 region 不再发请求——点位同样没有可信值，
                            // 一并标 Null（否则这些点静默保留旧值，正是 F-3 的问题）。
                            mark_region_null(
                                &mut acc,
                                plans[*pi].devid.as_str(),
                                &plans[*pi].regions[*ri],
                            );
                            continue;
                        }
                        // MB-8：相邻 region 请求之间插入帧间静默（仅 RTU 类 + 显式配置 >0）。
                        if step > 0 {
                            if let Some(gap) = request_gap {
                                tokio::select! {
                                    _ = token.cancelled() => break,
                                    _ = tokio::time::sleep(gap) => {}
                                }
                            }
                        }
                        let plan = &plans[*pi];
                        let reg = &plan.regions[*ri];
                        sess.set_slave(Slave(plan.slave_id));
                        let outcome =
                            match tokio::time::timeout(req_timeout, read_region(sess, reg)).await {
                                Ok(o) => o,
                                Err(_) => ReadOutcome::Timeout,
                            };
                        match outcome {
                            ReadOutcome::Ok(buf) => {
                                timeout_streak[*pi] = 0;
                                for (field, p) in &reg.points {
                                    if let Some(v) = decode_point(p, &buf, reg.start) {
                                        acc.entry(plan.devid.clone())
                                            .or_default()
                                            .insert((*field).to_string(), v);
                                    }
                                }
                            }
                            ReadOutcome::Exception(exc) => {
                                // exception = slave 在应答（活性证明），超时计数归零。
                                timeout_streak[*pi] = 0;
                                exceptions += 1;
                                tracker
                                    .extra_insert("exception_count", serde_json::json!(exceptions));
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} exception: {exc:?}",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len()
                                );
                                // F-3：失败 region 的点位投递 Null（坏质量），不静默跳过。
                                mark_region_null(&mut acc, &plan.devid, reg);
                                // MB-7：IllegalDataAddress = 请求区间超出从站实际点表，对半拆
                                // 后退让；其他异常码不是地址问题，拆了也没用（保持计数+warn）。
                                if exc == ExceptionCode::IllegalDataAddress {
                                    split_requests.push((*pi, *ri));
                                }
                            }
                            ReadOutcome::Timeout => {
                                timed_out_slave = Some(*pi);
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} timeout ({}s)，跳过该 slave 剩余 region",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len(),
                                    cfg.request_timeout_secs
                                );
                                // F-3：超时 region 的点位同样标 Null。
                                mark_region_null(&mut acc, &plan.devid, reg);
                                // C-3：连续 2 次超时拆 session——迟到响应串扰下一请求
                                // 的风险只在「超时后继续复用同一 session」时存在。
                                timeout_streak[*pi] = timeout_streak[*pi].saturating_add(1);
                                if timeout_streak[*pi] >= TIMEOUT_STREAK_DROP {
                                    transport_failed = true;
                                    tracing::warn!(
                                        "[modbusClient] inst {inst_id} slave {} 连续 {} 次超时，拆 session 重连（防 RTU 迟到响应串扰）",
                                        plan.slave_id,
                                        timeout_streak[*pi]
                                    );
                                }
                            }
                            ReadOutcome::Transport(e) => {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} failed: {e:?}",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len()
                                );
                                // F-3：失败 region 的点位投递 Null（断连时整轮中断，
                                // 未尝试的 region 不标——下 tick 重连后重采）。
                                mark_region_null(&mut acc, &plan.devid, reg);
                                transport_failed = true;
                                break;
                            }
                        }
                    }
                    // MB-7：应用拆分并重建轮询顺序——下一 tick 用细分区间，终止于单点位。
                    if !split_requests.is_empty()
                        && apply_region_splits(&mut plans, split_requests, inst_id.as_str())
                    {
                        poll_order = build_poll_order(&plans, cfg.poll_strategy);
                    }
                    if transport_failed {
                        // transport 失败可能因断连：drop Context，下 tick 重连。
                        // 超时/exception 不拆（故障隔离：单 slave 无响应不影响其他 slave）。
                        session = None;
                    }
                    for (devid, values) in acc {
                        if values.is_empty() {
                            continue;
                        }
                        let sample = Sample {
                            hwid: ctx.hwid.clone(),
                            devid,
                            values,
                            timestamp: now,
                        };
                        if tx.send(sample).await.is_err() {
                            break;
                        }
                    }
                    // tick 语义=「一轮轮询」：transport_failed→fail（拆 session，下 tick 重连），
                    // 否则 ok（含 exception/timeout——故障隔离不拆总线，仍计入 exception_count）。
                    if transport_failed {
                        tracker.tick_fail("transport failed");
                    } else {
                        let elapsed = tick_started.elapsed();
                        tracker.tick_ok(elapsed.as_millis() as u64);
                        // F-7：一轮轮询超过 period（MissedTickBehavior::Skip 已静默丢拍）——
                        // warn 限流 60s 一条，计数进 extra 供运维观察采集频率是否达标。
                        if elapsed > period {
                            if overrun.record(std::time::Instant::now()) {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} tick 耗时 {}ms > period {}ms（MissedTickBehavior::Skip 已丢拍，采集频率低于配置），累计 {} 次",
                                    elapsed.as_millis(),
                                    period.as_millis(),
                                    overrun.count
                                );
                            }
                            tracker.extra_insert(
                                "tick_overrun_count",
                                serde_json::json!(overrun.count),
                            );
                        }
                    }
                    if tracker.should_flush() {
                        tracker.flush().await;
                    }
                }
            }
        }
        // Context drop 即关连接（tokio-modbus 无显式 disconnect API）。
        tracker.set_status("stopped");
        tracker.flush().await;
        tracing::info!("[modbusClient] inst {inst_id} exited");
    }
}

/// 处理 write 命令并回报业务成败（F-2，2026-11-01 协议语义审查）。
///
/// `data` 兼容两种契约（`address` 优先，旧行为不变）：
/// - 原生：`{"address":"holding:0:f32:ABCD","value":12.34}`；
/// - HMI：`{"tag":"点位显示名","value":12.34}`——按 `inst.devid` + tag 查启动期索引，
///   全表唯一 tag 可跨 devid 兜底（F-1）。
///
/// coil → FC05/0F；holding u16/i16/bit → FC06，i32/u32/f32/f64 → FC16，bit 走 RMW；
/// discrete/input 只读拒绝。
///
/// 返回 `Result<(), String>`：成功 `Ok(())`；任何业务失败（JSON/地址/tag 未命中/只读/
/// 缺 value/取值越界/slave 归属冲突/未连接/从站异常/超时）→ `Err(原因)`，由 run() 经
/// `finish_write` 回给 REST 写入口（422）。技术细节只在此处返回、由 run() 统一记一条
/// warn，避免双层重复日志。
async fn handle_write(
    session: &mut Option<Context>,
    data: &str,
    devid: &str,
    index: &WriteIndex,
    devid_to_slave: &HashMap<String, u8>,
    req_timeout: Duration,
) -> Result<(), String> {
    let req: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("写命令 data 非 JSON: {e}"))?;
    let (target_devid, p) = resolve_write_target(&req, devid, index)?;
    if p.func.is_readonly() {
        return Err(format!(
            "写命令目标 {}(FC0{}) 只读，不可写",
            addr_func_name(p.func),
            addr_fc(p.func)
        ));
    }
    let value = match req.get("value") {
        Some(v) => json_to_sample_value(v),
        None => return Err("写命令缺少 value".to_string()),
    };
    // MB-5：slave 归属校验放在任何 I/O 之前（越权写必须连请求都不发）。
    let slave_id = resolve_write_slave(&target_devid, &req, devid_to_slave)?;
    let sess = session
        .as_mut()
        .ok_or_else(|| "写命令丢弃：实例未连接从站（下个采集 tick 重连）".to_string())?;
    sess.set_slave(Slave(slave_id));
    // C-1（2026-11-01 审查）：写路径（RMW 读 + 写）统一包 req_timeout——读路径早有
    // 超时，写路径此前裸 await：slave 收包不回（TCP 半开）会把 handle_write 连同
    // 整个 select 永久挂死（token/cmd/flush 全停，stop 3s 超时后 task 泄漏）。
    // 超时即拆 session：RTU 下在途响应若迟到，也不能留给下一请求串扰。
    let mut drop_sess = false;
    // 仅 holding bit 写需 RMW（coil 直接写单 coil，无需读当前值）。
    let mut rmw_err: Option<String> = None;
    let current_reg = if p.func == Func::Holding && p.typ == Type::Bit {
        match tokio::time::timeout(req_timeout, sess.read_holding_registers(p.addr, 1)).await {
            Ok(Ok(Ok(buf))) => buf.first().copied(),
            Ok(Ok(Err(exc))) => {
                rmw_err = Some(format!("bit 写 RMW 读从站异常 {exc:?}"));
                None
            }
            Ok(Err(e)) => {
                rmw_err = Some(format!("bit 写 RMW 读 transport 失败: {e:?}"));
                None
            }
            Err(_) => {
                rmw_err = Some(format!(
                    "bit 写 RMW 读超时（{req_timeout:?}），已拆 session"
                ));
                drop_sess = true;
                None
            }
        }
    } else {
        None
    };
    if let Some(e) = rmw_err {
        if drop_sess {
            *session = None;
        }
        return Err(e);
    }
    let payload =
        encode_value(&value, &p, current_reg).map_err(|e| format!("写值编码失败: {e}"))?;
    let result = match payload {
        WritePayload::Coils(c) if c.len() == 1 => {
            tokio::time::timeout(req_timeout, sess.write_single_coil(p.addr, c[0])).await
        }
        WritePayload::Coils(c) => {
            tokio::time::timeout(req_timeout, sess.write_multiple_coils(p.addr, &c)).await
        }
        WritePayload::Regs(r) if r.len() == 1 => {
            tokio::time::timeout(req_timeout, sess.write_single_register(p.addr, r[0])).await
        }
        WritePayload::Regs(r) => {
            tokio::time::timeout(req_timeout, sess.write_multiple_registers(p.addr, &r)).await
        }
    };
    let res = match result {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(exc))) => Err(format!("从站拒绝写（异常 {exc:?}）")),
        Ok(Err(e)) => Err(format!("写请求 transport 失败: {e:?}")),
        Err(_) => {
            drop_sess = true;
            Err(format!("写请求超时（{req_timeout:?}），已拆 session"))
        }
    };
    if drop_sess {
        *session = None;
    }
    res
}

// 静态注册到 inventory（编译期，编进单一二进制）
inventory::submit! {
    crate::plugin::registry::SourceEntry {
        name: "modbusClient",
        create: || Box::new(ModbusClient),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_point_accepts_standard_forms() {
        assert_eq!(
            parse_point("holding:0:f32:ABCD").unwrap(),
            PointSpec {
                func: Func::Holding,
                addr: 0,
                typ: Type::F32,
                order: Order::Abcd,
                bit: None,
            }
        );
        assert_eq!(
            parse_point("holding:10:bit:3").unwrap(),
            PointSpec {
                func: Func::Holding,
                addr: 10,
                typ: Type::Bit,
                order: Order::Abcd,
                bit: Some(3),
            }
        );
        assert_eq!(parse_point("input:5:u16").unwrap().typ, Type::U16);
        assert_eq!(parse_point("holding:0:f32").unwrap().order, Order::Abcd);
        assert_eq!(
            parse_point("holding:0:f32:cdab").unwrap().order,
            Order::Cdab
        );
        assert_eq!(parse_point("holding:0:f32:le").unwrap().order, Order::Dcba);
        // coil/discrete bool
        assert_eq!(parse_point("coil:0:bool").unwrap().func, Func::Coil);
        assert_eq!(parse_point("discrete:5:bool").unwrap().func, Func::Discrete);
        assert_eq!(parse_point("fc01:0:bool").unwrap().func, Func::Coil);
    }

    #[test]
    fn parse_point_rejects_invalid() {
        assert!(parse_point("holding:0:bit").is_err(), "bit 缺位号");
        assert!(parse_point("holding:0:bit:16").is_err(), "bit 越界");
        assert!(
            parse_point("holding:0:u16:ABCD").is_err(),
            "u16 不支持字节序"
        );
        assert!(
            parse_point("holding:0:i16:ABCD").is_err(),
            "i16 不支持字节序"
        );
        assert!(parse_point("holding:0:f32:XYZW").is_err(), "非法 order");
        assert!(parse_point("holding:0:badtype").is_err(), "非法 type");
        assert!(parse_point("holding:x:u16").is_err(), "非法 addr");
        assert!(parse_point("holding:0").is_err(), "缺 type");
        // func-type 组合校验
        assert!(parse_point("coil:0:i16").is_err(), "coil 不接受 i16");
        assert!(
            parse_point("coil:0:bool:ABCD").is_err(),
            "coil 不支持字节序段"
        );
        assert!(
            parse_point("holding:0:bool").is_err(),
            "holding 不接受 bool（用 bit）"
        );
        assert!(parse_point("input:0:bool").is_err(), "input 不接受 bool");
    }

    #[test]
    fn width_matches_type() {
        assert_eq!(parse_point("coil:0:bool").unwrap().width(), 1);
        assert_eq!(parse_point("holding:0:bit:0").unwrap().width(), 1);
        assert_eq!(parse_point("holding:0:u16").unwrap().width(), 1);
        assert_eq!(parse_point("holding:0:f32").unwrap().width(), 2);
        assert_eq!(parse_point("holding:0:f64").unwrap().width(), 4);
    }

    #[test]
    fn parse_cfg_tcp_defaults_and_clamps() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.transport, Transport::Tcp);
        assert_eq!(cfg.endpoint, "127.0.0.1:502");
        assert_eq!(cfg.serial_port, None);
        assert_eq!(cfg.slave_id, 1);
        assert_eq!(cfg.period, 3);
        assert_eq!(cfg.gap_threshold, 16);
        assert_eq!(cfg.max_regs, 100);
        assert_eq!(cfg.reconnect_secs, 2);

        let cfg2 =
            parse_cfg(&serde_json::json!({"endpoint":"x","period":0,"max_regs":9999})).unwrap();
        assert_eq!(cfg2.period, 1, "period 0 clamp 1");
        // MB-6（2026-11-01 协议语义审查，**有意行为变更**）：max_regs 上界由 125
        // 放宽到 2000——coil/discrete 走 FC01/02，协议上限 2000；寄存器类的 125
        // 上限改在 merge_regions 按功能钳（见 merge::tests::merge_regions_caps_max_units_per_func）。
        assert_eq!(cfg2.max_regs, 2000, "max_regs clamp 2000（原 125）");
    }

    #[test]
    fn parse_cfg_rtu_serial_defaults_and_params() {
        let cfg =
            parse_cfg(&serde_json::json!({"kind":"rtu_serial","serial_port":"COM3"})).unwrap();
        assert_eq!(cfg.transport, Transport::RtuSerial);
        assert_eq!(cfg.serial_port.as_deref(), Some("COM3"));
        assert_eq!(cfg.endpoint, "", "rtu_serial 无 endpoint");
        // 工业默认 9600/8N1/NONE。
        assert_eq!(cfg.baud_rate, 9600);
        assert_eq!(cfg.data_bits, 8);
        assert_eq!(cfg.parity, ParityKind::None);
        assert_eq!(cfg.stop_bits, 1);
        assert_eq!(cfg.flow_control, FlowKind::None);

        let cfg3 = parse_cfg(&serde_json::json!({
            "kind":"rtu_serial","serial_port":"/dev/ttyUSB0","baud_rate":115200,
            "data_bits":7,"parity":"even","stop_bits":2,"flow_control":"hardware"
        }))
        .unwrap();
        assert_eq!(cfg3.baud_rate, 115200);
        assert_eq!(cfg3.data_bits, 7);
        assert_eq!(cfg3.parity, ParityKind::Even);
        assert_eq!(cfg3.stop_bits, 2);
        assert_eq!(cfg3.flow_control, FlowKind::Hardware);
    }

    #[test]
    fn parse_cfg_rtu_tcp_and_rtu_udp() {
        let cfg =
            parse_cfg(&serde_json::json!({"kind":"rtu_tcp","endpoint":"192.168.1.5:502"})).unwrap();
        assert_eq!(cfg.transport, Transport::RtuTcp);
        assert_eq!(cfg.endpoint, "192.168.1.5:502");

        let cfg = parse_cfg(
            &serde_json::json!({"kind":"rtu_udp","endpoint":"10.0.0.7:502","slave_id":2}),
        )
        .unwrap();
        assert_eq!(cfg.transport, Transport::RtuUdp);
        assert_eq!(cfg.slave_id, 2);
    }

    #[test]
    fn parse_cfg_rejects_invalid_kind_and_params() {
        assert!(
            parse_cfg(&serde_json::json!({})).is_err(),
            "tcp 缺 endpoint"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_tcp"})).is_err(),
            "rtu_tcp 缺 endpoint"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_serial"})).is_err(),
            "rtu_serial 缺 serial_port"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"ascii","endpoint":"x"})).is_err(),
            "非法 kind（ASCII 后置）"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","data_bits":9}))
                .is_err(),
            "非法 data_bits"
        );
        assert!(
            parse_cfg(&serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","stop_bits":3}))
                .is_err(),
            "非法 stop_bits"
        );
        assert!(
            parse_cfg(
                &serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","parity":"mark"})
            )
            .is_err(),
            "非法 parity"
        );
        assert!(
            parse_cfg(
                &serde_json::json!({"kind":"rtu_serial","serial_port":"COM3","flow_control":"auto"})
            )
            .is_err(),
            "非法 flow_control"
        );
    }

    #[test]
    fn parse_cfg_poll_strategy_defaults_round_robin() {
        let cfg = parse_cfg(&serde_json::json!({"endpoint":"127.0.0.1:502"})).unwrap();
        assert_eq!(cfg.poll_strategy, config::PollStrategy::RoundRobin);
        assert_eq!(cfg.request_timeout_secs, 2, "per-request 超时默认 2s");
    }

    #[test]
    fn parse_cfg_poll_strategy_sequential() {
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"127.0.0.1:502","poll_strategy":"sequential"
        }))
        .unwrap();
        assert_eq!(cfg.poll_strategy, config::PollStrategy::Sequential);
    }

    #[test]
    fn parse_cfg_request_timeout_clamped() {
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"127.0.0.1:502","request_timeout_secs":0
        }))
        .unwrap();
        assert_eq!(cfg.request_timeout_secs, 1, "0 clamp 1");
    }

    /// C-4（2026-11-01 审查）：非数字字符串写值必须 Err 拒写——此前
    /// `unwrap_or(0)` 会静默向 PLC 写 0（工业写危险）。
    #[test]
    fn encode_value_rejects_non_numeric_string() {
        let p = parse_point("holding:0:f32").unwrap();
        let err =
            encode_value(&SampleValue::Str("1O".into()), &p, None).expect_err("非数字串必须拒写");
        assert!(err.contains("非数字"), "错误信息应说明原因：{err}");
        // 数字串照常接受（兼容下行 JSON 值为字符串数字的既有用法）
        assert!(encode_value(&SampleValue::Str("1.5".into()), &p, None).is_ok());
        let pi = parse_point("holding:4:i32").unwrap();
        assert!(encode_value(&SampleValue::Str("42".into()), &pi, None).is_ok());
        assert!(encode_value(&SampleValue::Str("4x".into()), &pi, None).is_err());
        // bool 点的 "true" 串不受影响（不走数值路径）
        let pb = parse_point("coil:0:bool").unwrap();
        assert!(encode_value(&SampleValue::Str("true".into()), &pb, None).is_ok());
    }

    /// C-5：数值键越界（u8/u16/u32 域外）显式 Err，不再 `as` 静默截断
    /// （300 号 slave_id 截成 44 会打到错误从站）。
    #[test]
    fn parse_cfg_rejects_out_of_range_numbers() {
        for (key, bad) in [
            ("slave_id", 300u64),
            ("gap_threshold", 65537),
            ("baud_rate", u64::from(u32::MAX) + 1),
            ("data_bits", 261), // 261 as u8 = 5，会绕过 5..=8 白名单——必须 Err
            ("stop_bits", 257), // 257 as u8 = 1，同理
        ] {
            let cfg = parse_cfg(&serde_json::json!({"endpoint":"127.0.0.1:502", key: bad}));
            assert!(cfg.is_err(), "{key}={bad} 必须整体 Err（不得静默截断）");
        }
        // 边界内合法值照常
        assert!(
            parse_cfg(&serde_json::json!({
                "endpoint":"127.0.0.1:502","slave_id":247,"gap_threshold":65535,"data_bits":8,"stop_bits":2
            }))
            .is_ok()
        );
    }

    /// C-6：reconnect_secs 钳到 [1,3600]（小时级退避等同静默瘫痪）。
    #[test]
    fn parse_cfg_reconnect_secs_clamped() {
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"127.0.0.1:502","reconnect_secs":86400
        }))
        .unwrap();
        assert_eq!(cfg.reconnect_secs, 3600, "86400 钳 3600");
        let cfg = parse_cfg(&serde_json::json!({
            "endpoint":"127.0.0.1:502","reconnect_secs":0
        }))
        .unwrap();
        assert_eq!(cfg.reconnect_secs, 1);
    }

    #[test]
    fn parse_cfg_rejects_bad_poll_strategy() {
        assert!(
            parse_cfg(&serde_json::json!({
                "endpoint":"127.0.0.1:502","poll_strategy":"random"
            }))
            .is_err(),
            "非法 poll_strategy"
        );
    }

    #[tokio::test]
    async fn udpio_keeps_datagram_boundary() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 一对 loopback UDP：UdpIo(client, connected) → server。
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server_addr).await.unwrap();
        let mut io = UdpIo::new(client);

        // 两帧分别写 + flush：每帧应是独立 datagram，不粘包。
        let frame1: &[u8] = b"\x01\x03\x00\x00\x00\x01";
        let frame2: &[u8] = b"\x01\x03\x02\x12\x34";
        io.write_all(frame1).await.unwrap();
        io.flush().await.unwrap();
        io.write_all(frame2).await.unwrap();
        io.flush().await.unwrap();

        let mut buf1 = [0u8; 64];
        let (n1, client_addr) = server.recv_from(&mut buf1).await.unwrap();
        assert_eq!(&buf1[..n1], frame1, "第一帧独立 datagram");
        let mut buf2 = [0u8; 64];
        let (n2, _) = server.recv_from(&mut buf2).await.unwrap();
        assert_eq!(&buf2[..n2], frame2, "第二帧独立 datagram，未与第一帧粘包");

        // server 回一个 datagram，UdpIo 应读回完整字节。
        let resp: &[u8] = b"\x01\x03\x02\xAB\xCD";
        server.send_to(resp, client_addr).await.unwrap();
        let mut rbuf = [0u8; 64];
        let rn = io.read(&mut rbuf).await.unwrap();
        assert_eq!(&rbuf[..rn], resp, "回包完整读取");
    }

    #[tokio::test]
    async fn udpio_read_chunked_from_one_datagram() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 验证 read_buf：一个 datagram 被多次小容量 read 分块消费，顺序保持。
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server_addr).await.unwrap();
        let mut io = UdpIo::new(client);

        // 先握手让 server 拿到 client_addr。
        io.write_all(b"hi").await.unwrap();
        io.flush().await.unwrap();
        let mut tmp = [0u8; 16];
        let (_, client_addr) = server.recv_from(&mut tmp).await.unwrap();

        // server 发 8 字节 datagram；UdpIo poll_read 整包入 read_buf，再分批吐出。
        server.send_to(b"01234567", client_addr).await.unwrap();
        let mut a = [0u8; 2];
        assert_eq!(io.read(&mut a).await.unwrap(), 2);
        assert_eq!(&a, b"01");
        assert_eq!(io.read(&mut a).await.unwrap(), 2);
        assert_eq!(&a, b"23");
        let mut b4 = [0u8; 4];
        assert_eq!(io.read(&mut b4).await.unwrap(), 4);
        assert_eq!(&b4, b"4567");
    }

    #[test]
    fn extract_bit_works() {
        let reg = 0b0000_0000_0000_1010u16; // bit1, bit3
        assert!(!extract_bit(reg, 0));
        assert!(extract_bit(reg, 1));
        assert!(!extract_bit(reg, 2));
        assert!(extract_bit(reg, 3));
    }

    #[test]
    fn decode_u32_roundtrip_all_orders() {
        let orders = [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba];
        for o in orders {
            for v in [
                0x00000000u32,
                0x41480000,
                0xFFFFFFFF,
                0x12345678,
                0x80000001,
            ] {
                let regs = encode_u32(v, o);
                assert_eq!(decode_u32(regs, o), v, "order {o:?} v {v:#010x} round-trip");
            }
        }
    }

    #[test]
    fn decode_f32_known_value_abcd() {
        // f32 12.5 = 0x41480000，ABCD → regs [0x4148, 0x0000]
        assert_eq!(
            encode_u32(12.5_f32.to_bits(), Order::Abcd),
            [0x4148, 0x0000]
        );
        assert!((decode_f32([0x4148, 0x0000], Order::Abcd) - 12.5_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn decode_i32_negative() {
        let v: i32 = -1;
        for o in [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba] {
            let regs = encode_u32(v as u32, o);
            assert_eq!(decode_u32(regs, o) as i32, -1, "i32 -1 order {o:?}");
        }
    }

    #[test]
    fn decode_u64_roundtrip_all_orders() {
        let orders = [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba];
        for o in orders {
            for v in [0u64, 0x4024000000000000, u64::MAX, 0x123456789ABCDEF0] {
                let regs = encode_u64(v, o);
                assert_eq!(decode_u64(regs, o), v, "f64 order {o:?} v {v:#018x}");
            }
        }
    }

    #[test]
    fn decode_point_from_regs_buf() {
        let p = parse_point("holding:0:f32:ABCD").unwrap();
        let buf = Buf::Regs(vec![0x4148, 0x0000]);
        assert!(
            matches!(decode_point(&p, &buf, 0), Some(SampleValue::Float(f)) if (f-12.5).abs() < 1e-6)
        );
        // bit@1:bit3 从 reg1=0x000A 取 bit3
        let pb = parse_point("holding:1:bit:3").unwrap();
        assert_eq!(
            decode_point(&pb, &Buf::Regs(vec![0x0000, 0x000A]), 0),
            Some(SampleValue::Bool(true))
        );
        // 缓冲区不足 → None
        assert_eq!(decode_point(&p, &Buf::Regs(vec![0x4148]), 0), None);
    }

    #[test]
    fn decode_point_from_bits_buf() {
        // coil/discrete 从 Bits buf 按 offset 取
        let p = parse_point("coil:2:bool").unwrap();
        assert_eq!(
            decode_point(&p, &Buf::Bits(vec![false, false, true, false]), 0),
            Some(SampleValue::Bool(true))
        );
        let pd = parse_point("discrete:0:bool").unwrap();
        assert_eq!(
            decode_point(&pd, &Buf::Bits(vec![true, false]), 0),
            Some(SampleValue::Bool(true))
        );
    }

    #[test]
    fn encode_value_coil_bool() {
        let p = parse_point("coil:0:bool").unwrap();
        assert_eq!(
            encode_value(&SampleValue::Bool(true), &p, None).unwrap(),
            WritePayload::Coils(vec![true])
        );
        assert_eq!(
            encode_value(&SampleValue::Int(1), &p, None).unwrap(),
            WritePayload::Coils(vec![true])
        );
        assert_eq!(
            encode_value(&SampleValue::Int(0), &p, None).unwrap(),
            WritePayload::Coils(vec![false])
        );
    }

    #[test]
    fn encode_value_holding_bit_rmw() {
        let p = parse_point("holding:0:bit:1").unwrap();
        // 当前 reg=0x0001（bit0=1），写 bit1=true → 0x0003
        let r = encode_value(&SampleValue::Bool(true), &p, Some(0x0001)).unwrap();
        assert_eq!(r, WritePayload::Regs(vec![0x0003]));
        // 写 bit1=false → 0x0001（bit1 清，bit0 不变）
        let r = encode_value(&SampleValue::Bool(false), &p, Some(0x0003)).unwrap();
        assert_eq!(r, WritePayload::Regs(vec![0x0001]));
        // bit 写缺 current_reg → Err
        assert!(encode_value(&SampleValue::Bool(true), &p, None).is_err());
    }

    #[test]
    fn encode_value_holding_f32_abcd() {
        let p = parse_point("holding:0:f32:ABCD").unwrap();
        let r = encode_value(&SampleValue::Float(12.5), &p, None).unwrap();
        assert_eq!(r, WritePayload::Regs(vec![0x4148, 0x0000]));
    }

    #[test]
    fn encode_value_rejects_readonly_and_bad_combo() {
        let pd = parse_point("discrete:0:bool").unwrap();
        assert!(
            encode_value(&SampleValue::Bool(true), &pd, None).is_err(),
            "discrete 只读"
        );
        let pi = parse_point("input:0:u16").unwrap();
        assert!(
            encode_value(&SampleValue::Int(1), &pi, None).is_err(),
            "input 只读"
        );
    }

    #[test]
    fn merge_regions_empty() {
        assert!(merge_regions(&[], 16, 100).is_empty());
    }

    #[test]
    fn merge_regions_single() {
        let pairs = vec![("a".into(), parse_point("holding:0:f32").unwrap())];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, 0);
        assert_eq!(r[0].len(), 2);
    }

    #[test]
    fn merge_regions_adjacent_and_gap() {
        // 3×f32（0-5）+ 4×u16（6-9）连续 → 一个区间 0-9（len=10）
        let pairs = vec![
            ("a".into(), parse_point("holding:0:f32").unwrap()),
            ("b".into(), parse_point("holding:2:f32").unwrap()),
            ("c".into(), parse_point("holding:4:f32").unwrap()),
            ("d".into(), parse_point("holding:6:u16").unwrap()),
            ("e".into(), parse_point("holding:7:u16").unwrap()),
            ("f".into(), parse_point("holding:8:u16").unwrap()),
            ("g".into(), parse_point("holding:9:u16").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1, "连续应合并为 1 个区间");
        assert_eq!(r[0].len(), 10);
    }

    #[test]
    fn merge_regions_gap_split() {
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:100:u16").unwrap()),
        ];
        assert_eq!(merge_regions(&pairs, 16, 100).len(), 2);
        let r = merge_regions(&pairs, 100, 125);
        assert_eq!(r.len(), 1, "gap=100 应合并");
        assert_eq!(r[0].len(), 101);
    }

    #[test]
    fn merge_regions_max_regs_split() {
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:5:u16").unwrap()),
        ];
        assert_eq!(merge_regions(&pairs, 16, 10).len(), 1, "0-6 len=6 <10 合并");
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:50:u16").unwrap()),
        ];
        assert_eq!(
            merge_regions(&pairs, 100, 10).len(),
            2,
            "合并后 len=51>10 分裂"
        );
    }

    #[test]
    fn merge_regions_bit_and_f32_coexist() {
        let pairs = vec![
            ("f".into(), parse_point("holding:4:f32:ABCD").unwrap()),
            ("b".into(), parse_point("holding:5:bit:2").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].points.len(), 2);
    }

    #[test]
    fn merge_regions_multi_bit_fold() {
        let pairs = vec![
            ("b0".into(), parse_point("holding:5:bit:0").unwrap()),
            ("b1".into(), parse_point("holding:5:bit:1").unwrap()),
            ("b2".into(), parse_point("holding:5:bit:2").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 1, "3 bit 折叠为 1 寄存器");
        assert_eq!(r[0].points.len(), 3);
    }

    #[test]
    fn merge_regions_four_funcs_separate() {
        // 4 种 func 各自独立地址空间，必分 4 组
        let pairs = vec![
            ("c".into(), parse_point("coil:0:bool").unwrap()),
            ("d".into(), parse_point("discrete:0:bool").unwrap()),
            ("h".into(), parse_point("holding:0:u16").unwrap()),
            ("i".into(), parse_point("input:0:u16").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 4, "4 func 各一组");
        assert!(r.iter().any(|r| r.func == Func::Coil));
        assert!(r.iter().any(|r| r.func == Func::Discrete));
        assert!(r.iter().any(|r| r.func == Func::Holding));
        assert!(r.iter().any(|r| r.func == Func::Input));
    }

    #[test]
    fn merge_regions_coils_batch() {
        // 需求①变体：10 个连续 coil → 1 个区间（一次 read_coils(0,10)）
        let pairs: Vec<(String, PointSpec)> = (0..10)
            .map(|i| {
                (
                    format!("c{i}"),
                    parse_point(&format!("coil:{i}:bool")).unwrap(),
                )
            })
            .collect();
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].func, Func::Coil);
        assert_eq!(r[0].len(), 10);
        assert_eq!(r[0].points.len(), 10);
    }

    #[test]
    fn merge_regions_unsorted_input() {
        let pairs = vec![
            ("b".into(), parse_point("holding:5:u16").unwrap()),
            ("a".into(), parse_point("holding:0:u16").unwrap()),
        ];
        let r = merge_regions(&pairs, 16, 100);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, 0);
        assert_eq!(r[0].end, 5);
    }

    #[test]
    fn json_to_sample_value_handles_types() {
        assert_eq!(
            json_to_sample_value(&serde_json::json!(true)),
            SampleValue::Bool(true)
        );
        assert_eq!(
            json_to_sample_value(&serde_json::json!(42)),
            SampleValue::Int(42)
        );
        assert!(matches!(
            json_to_sample_value(&serde_json::json!(1.5)),
            SampleValue::Float(_)
        ));
        assert_eq!(
            json_to_sample_value(&serde_json::json!("x")),
            SampleValue::Str("x".into())
        );
    }

    #[test]
    fn func_is_readonly() {
        assert!(Func::Discrete.is_readonly());
        assert!(Func::Input.is_readonly());
        assert!(!Func::Coil.is_readonly());
        assert!(!Func::Holding.is_readonly());
    }

    #[test]
    fn plan_devices_assigns_device_slave_id_with_fallback() {
        let devices = vec![
            DeviceSpec {
                label: "".into(),
                value: "m1".into(),
                children: vec![ChildSpec {
                    label: "t".into(),
                    value: "holding:0:u16".into(),
                    r#type: "".into(),
                    props: None,
                }],
                slave_id: Some(1),
            },
            DeviceSpec {
                label: "".into(),
                value: "m2".into(),
                children: vec![ChildSpec {
                    label: "t".into(),
                    value: "holding:0:u16".into(),
                    r#type: "".into(),
                    props: None,
                }],
                slave_id: None,
            },
        ];
        let plans = plan_devices(&devices, 16, 100, 9);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].devid, "m1");
        assert_eq!(plans[0].slave_id, 1, "device 级 slave_id 优先");
        assert_eq!(plans[1].slave_id, 9, "缺省回退 fallback");
        assert_eq!(plans[0].regions.len(), 1, "merge 预计算");
    }

    #[test]
    fn plan_devices_skips_bad_address_but_keeps_device() {
        let devices = vec![DeviceSpec {
            label: "".into(),
            value: "m1".into(),
            children: vec![
                ChildSpec {
                    label: "good".into(),
                    value: "holding:0:u16".into(),
                    r#type: "".into(),
                    props: None,
                },
                ChildSpec {
                    label: "bad".into(),
                    value: "holding:x:u16".into(),
                    r#type: "".into(),
                    props: None,
                },
            ],
            slave_id: Some(1),
        }];
        let plans = plan_devices(&devices, 16, 100, 1);
        assert_eq!(
            plans[0]
                .regions
                .iter()
                .map(|r| r.points.len())
                .sum::<usize>(),
            1,
            "非法地址丢弃，合法保留"
        );
    }

    #[test]
    fn plan_devices_empty_children_skipped() {
        let devices = vec![DeviceSpec {
            label: "".into(),
            value: "m1".into(),
            children: vec![],
            slave_id: Some(1),
        }];
        let plans = plan_devices(&devices, 16, 100, 1);
        assert!(plans.is_empty(), "无点位设备不入 plan");
    }

    #[test]
    fn build_poll_order_round_robin_interleaves() {
        // 2 slave 各 1 region → round_robin: [(0,0),(1,0)]（先全 slave 第 0 region）
        let mk_region = || {
            merge_regions(
                &[("x".into(), parse_point("holding:0:u16").unwrap())],
                16,
                100,
            )[0]
            .clone()
        };
        let plans = vec![
            SlavePlan {
                devid: "a".into(),
                slave_id: 1,
                regions: vec![mk_region()],
            },
            SlavePlan {
                devid: "b".into(),
                slave_id: 2,
                regions: vec![mk_region()],
            },
        ];
        assert_eq!(
            build_poll_order(&plans, PollStrategy::RoundRobin),
            vec![(0, 0), (1, 0)]
        );
    }

    #[test]
    fn build_poll_order_round_robin_multi_region() {
        // 2 slave × 2 region → round_robin: (0,0)(1,0)(0,1)(1,1)
        let mk_region = |start: u16| {
            merge_regions(
                &[(
                    format!("t{start}"),
                    parse_point(&format!("holding:{start}:u16")).unwrap(),
                )],
                16,
                100,
            )[0]
            .clone()
        };
        let plans = vec![
            SlavePlan {
                devid: "a".into(),
                slave_id: 1,
                regions: vec![mk_region(0), mk_region(10)],
            },
            SlavePlan {
                devid: "b".into(),
                slave_id: 2,
                regions: vec![mk_region(0), mk_region(10)],
            },
        ];
        assert_eq!(
            build_poll_order(&plans, PollStrategy::RoundRobin),
            vec![(0, 0), (1, 0), (0, 1), (1, 1)]
        );
    }

    #[test]
    fn build_poll_order_sequential_drains_each_slave() {
        let mk_region = |start: u16| {
            merge_regions(
                &[(
                    format!("t{start}"),
                    parse_point(&format!("holding:{start}:u16")).unwrap(),
                )],
                16,
                100,
            )[0]
            .clone()
        };
        let plans = vec![
            SlavePlan {
                devid: "a".into(),
                slave_id: 1,
                regions: vec![mk_region(0), mk_region(10)],
            },
            SlavePlan {
                devid: "b".into(),
                slave_id: 2,
                regions: vec![mk_region(0)],
            },
        ];
        // 逐 slave 打透：(0,0)(0,1)(1,0)
        assert_eq!(
            build_poll_order(&plans, PollStrategy::Sequential),
            vec![(0, 0), (0, 1), (1, 0)]
        );
    }

    #[test]
    fn build_poll_order_empty_when_no_plans() {
        let plans: Vec<SlavePlan> = vec![];
        assert!(build_poll_order(&plans, PollStrategy::RoundRobin).is_empty());
    }

    // ---------- 以下为 2026-11-01 协议语义审查修复的测试 ----------

    /// 构造测试设备（devid + [(label, 地址串)] + 可选 slave_id）。
    fn mk_device(devid: &str, slave: Option<u8>, tags: &[(&str, &str)]) -> DeviceSpec {
        DeviceSpec {
            label: devid.into(),
            value: devid.into(),
            children: tags
                .iter()
                .map(|(label, addr)| ChildSpec {
                    label: (*label).into(),
                    value: (*addr).into(),
                    r#type: String::new(),
                    props: None,
                })
                .collect(),
            slave_id: slave,
        }
    }

    /// F-1（2026-11-01 协议语义审查）：HMI 只发 `{"tag":点位显示名,"value":…}`。
    /// 写目标按 devid + tag 反查绝对地址，全表唯一 tag 可跨 devid 兜底；
    /// 原生 `address` 字段保留且优先（旧契约行为不变）。
    #[test]
    fn write_index_resolves_tag_per_devid_and_global_unique() {
        let devices = vec![
            mk_device(
                "devA",
                Some(1),
                &[("temp", "holding:0:u16"), ("speed", "holding:1:u16")],
            ),
            mk_device("devB", Some(2), &[("level", "holding:2:f32")]),
        ];
        let plans = plan_devices(&devices, 16, 100, 1);
        let index = WriteIndex::from_plans(&plans);

        // devid 命中（同名 tag 优先本设备）
        let (d, p) = resolve_write_target(
            &serde_json::json!({"tag": "temp", "value": 1}),
            "devA",
            &index,
        )
        .unwrap();
        assert_eq!((d.as_str(), p.addr), ("devA", 0));

        // msg.devid 不匹配但 tag 全表唯一 → 跨 devid 兜底
        let (d, p) =
            resolve_write_target(&serde_json::json!({"tag": "level"}), "other", &index).unwrap();
        assert_eq!((d.as_str(), p.addr), ("devB", 2));

        // tag 不存在 → Err
        assert!(
            resolve_write_target(&serde_json::json!({"tag": "nope"}), "devA", &index).is_err(),
            "未知 tag 必须 Err"
        );

        // 原生 address 优先于 tag（旧契约不变）
        let (d, p) = resolve_write_target(
            &serde_json::json!({"address": "holding:9:u16", "tag": "temp"}),
            "devA",
            &index,
        )
        .unwrap();
        assert_eq!((d.as_str(), p.addr), ("devA", 9));
        // 只给 address 照常工作
        assert!(resolve_write_target(
            &serde_json::json!({"address": "holding:9:u16"}),
            "devA",
            &index
        )
        .is_ok());
        // 两者都缺 → Err
        assert!(resolve_write_target(&serde_json::json!({"value": 1}), "devA", &index).is_err());
    }

    /// F-1 续：跨设备重名 tag 不得兜底（有歧义就拒，不猜目标）。
    #[test]
    fn write_index_rejects_ambiguous_cross_device_tag() {
        let devices = vec![
            mk_device("devA", Some(1), &[("temp", "holding:0:u16")]),
            mk_device("devB", Some(2), &[("temp", "holding:5:u16")]),
        ];
        let plans = plan_devices(&devices, 16, 100, 1);
        let index = WriteIndex::from_plans(&plans);
        // 本设备命中照常
        let (d, p) =
            resolve_write_target(&serde_json::json!({"tag": "temp"}), "devB", &index).unwrap();
        assert_eq!((d.as_str(), p.addr), ("devB", 5));
        // 第三个 devid + 重名 tag → 歧义拒写
        let err =
            resolve_write_target(&serde_json::json!({"tag": "temp"}), "devX", &index).unwrap_err();
        assert!(err.contains("temp"), "{err}");
    }

    /// MB-5（2026-11-01 协议语义审查）：`data.slave_id` 不得覆盖设备归属——
    /// devid 已映射到 slave A 时只允许等于 A（此前可向任意从站写）。
    #[test]
    fn resolve_write_slave_guards_device_ownership() {
        let mapped: HashMap<String, u8> = [("devA".to_string(), 3u8)].into_iter().collect();
        // 与设备映射一致：允许（显式同值不改变目标）
        assert_eq!(
            resolve_write_slave("devA", &serde_json::json!({"slave_id": 3}), &mapped).unwrap(),
            3
        );
        // 与设备映射冲突：Err
        let err =
            resolve_write_slave("devA", &serde_json::json!({"slave_id": 9}), &mapped).unwrap_err();
        assert!(
            err.contains("slave_id") && err.contains('9'),
            "冲突错误要点名请求值与设备归属：{err}"
        );
        // 无显式：用设备映射
        assert_eq!(
            resolve_write_slave("devA", &serde_json::json!({}), &mapped).unwrap(),
            3
        );
        // devid 未映射（设备无点位）：允许显式单播地址
        assert_eq!(
            resolve_write_slave("devB", &serde_json::json!({"slave_id": 7}), &mapped).unwrap(),
            7
        );
        // 未映射且无显式 → Err
        assert!(resolve_write_slave("devB", &serde_json::json!({}), &mapped).is_err());
        // 显式越界/广播/保留 → Err
        for bad in [0u64, 248, 300] {
            assert!(
                resolve_write_slave("devB", &serde_json::json!({"slave_id": bad}), &mapped)
                    .is_err(),
                "slave_id={bad} 必须 Err"
            );
        }
    }

    /// F-2（2026-11-01 协议语义审查）：`handle_write` 返回 `Result<(), String>`——
    /// 业务失败必须回流 REST 写入口（422），不再只是 warn 后 200。
    /// 本例覆盖全部纯校验失败分支（不触网：session=None）。
    #[tokio::test]
    async fn handle_write_reports_business_errors() {
        let devices = vec![
            mk_device("devA", Some(3), &[("temp", "holding:0:u16")]),
            mk_device("devR", Some(4), &[("di", "discrete:0:bool")]),
        ];
        let plans = plan_devices(&devices, 16, 100, 1);
        let index = WriteIndex::from_plans(&plans);
        let mapped: HashMap<String, u8> = [("devA".to_string(), 3u8)].into_iter().collect();
        let mut none: Option<Context> = None;
        let t = Duration::from_millis(10);

        // data 不是 JSON
        assert!(
            handle_write(&mut none, "not-json", "devA", &index, &mapped, t)
                .await
                .is_err()
        );
        // 缺 address/tag
        let e = handle_write(&mut none, r#"{"value":1}"#, "devA", &index, &mapped, t)
            .await
            .unwrap_err();
        assert!(e.contains("address") || e.contains("tag"), "{e}");
        // tag 未命中
        assert!(handle_write(
            &mut none,
            r#"{"tag":"nope","value":1}"#,
            "devA",
            &index,
            &mapped,
            t
        )
        .await
        .is_err());
        // 只读功能码
        let e = handle_write(
            &mut none,
            r#"{"tag":"di","value":true}"#,
            "devR",
            &index,
            &mapped,
            t,
        )
        .await
        .unwrap_err();
        assert!(e.contains("只读"), "{e}");
        // 缺 value
        let e = handle_write(&mut none, r#"{"tag":"temp"}"#, "devA", &index, &mapped, t)
            .await
            .unwrap_err();
        assert!(e.contains("value"), "{e}");
        // slave_id 越权（MB-5 经 handle_write 生效）
        let e = handle_write(
            &mut none,
            r#"{"tag":"temp","value":1,"slave_id":9}"#,
            "devA",
            &index,
            &mapped,
            t,
        )
        .await
        .unwrap_err();
        assert!(e.contains("slave_id"), "{e}");
        // 合法写但因未连接失败（错误必须带原因，不是静默 200）
        let e = handle_write(
            &mut none,
            r#"{"tag":"temp","value":1}"#,
            "devA",
            &index,
            &mapped,
            t,
        )
        .await
        .unwrap_err();
        assert!(e.contains("连接"), "{e}");
    }

    /// F-3：region 读失败必须为其所有点位投递 `SampleValue::Null`（坏质量标记），
    /// 不再静默跳过。
    #[test]
    fn mark_region_null_marks_every_point() {
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:1:f32").unwrap()),
        ];
        let regions = merge_regions(&pairs, 16, 100);
        let mut acc: BTreeMap<String, BTreeMap<String, SampleValue>> = BTreeMap::new();
        acc.entry("d1".into())
            .or_default()
            .insert("a".into(), SampleValue::Int(7));
        mark_region_null(&mut acc, "d1", &regions[0]);
        let vals = &acc["d1"];
        assert_eq!(vals["a"], SampleValue::Null, "本拍失败点覆盖为 Null");
        assert_eq!(vals["b"], SampleValue::Null);
    }

    /// MB-7（2026-11-01 协议语义审查）：IllegalDataAddress 时 region 对半拆分，
    /// 逐层拆到单点位（保底 1 个单元）后终止。
    #[test]
    fn split_region_halves_points_until_single() {
        let pairs: Vec<(String, PointSpec)> = (0..8)
            .map(|i| {
                (
                    format!("c{i}"),
                    parse_point(&format!("coil:{i}:bool")).unwrap(),
                )
            })
            .collect();
        let regions = merge_regions(&pairs, 16, 100);
        assert_eq!(regions.len(), 1, "8 个连续 coil 合 1 区");
        let (a, b) = split_region(&regions[0]).expect("8 点可拆");
        assert_eq!((a.start, a.end, a.points.len()), (0, 3, 4));
        assert_eq!((b.start, b.end, b.points.len()), (4, 7, 4));
        // 第二层：a → 2+2
        let (a1, a2) = split_region(&a).expect("4 点可拆");
        assert_eq!((a1.points.len(), a2.points.len()), (2, 2));
        // 第三层：a1 → 1+1
        let (a11, a12) = split_region(&a1).expect("2 点可拆");
        assert_eq!((a11.points.len(), a12.points.len()), (1, 1));
        assert_eq!(a11.start, 0);
        assert_eq!(a11.end, 0);
        // 终止：单点位无法再拆（保底 1 个单元）
        assert!(split_region(&a11).is_none(), "单点位拆分终止");
        assert!(split_region(&a12).is_none());
    }

    /// MB-7 续：多寄存器点位（f32）不得被拆点切断——按点位对半切，子区间各自完整。
    #[test]
    fn split_region_keeps_multiregister_points_whole() {
        let pairs = vec![
            ("a".into(), parse_point("holding:0:u16").unwrap()),
            ("b".into(), parse_point("holding:1:f32").unwrap()),
            ("c".into(), parse_point("holding:3:u16").unwrap()),
        ];
        let regions = merge_regions(&pairs, 16, 100);
        assert_eq!(regions[0].len(), 4);
        let (l, r) = split_region(&regions[0]).unwrap();
        assert_eq!(l.points.len(), 1, "左侧只含 a");
        assert_eq!((l.start, l.end), (0, 0));
        assert_eq!(r.points.len(), 2, "右侧含 b(f32 占 1-2) + c");
        assert_eq!((r.start, r.end), (1, 3));
        assert!(l.end < r.start, "两子区间不重叠");
    }

    /// MB-7 续：把拆分请求应用到计划表（替换 1 个为 2 个子区间；不可拆时不变）。
    #[test]
    fn apply_region_splits_replaces_region() {
        let devices = vec![mk_device(
            "devA",
            Some(1),
            &[
                ("a", "holding:0:u16"),
                ("b", "holding:1:u16"),
                ("c", "holding:2:u16"),
            ],
        )];
        let mut plans = plan_devices(&devices, 16, 100, 1);
        assert_eq!(plans[0].regions.len(), 1);
        assert!(
            apply_region_splits(&mut plans, vec![(0, 0)], "test"),
            "3 点区间可拆"
        );
        assert_eq!(plans[0].regions.len(), 2, "1 个区间替换为 2 个");
        assert_eq!(
            plans[0]
                .regions
                .iter()
                .map(|r| r.points.len())
                .sum::<usize>(),
            3,
            "点位不丢"
        );
        // 只剩单点位的区间：拆分请求为 no-op（终止），返回 false
        let devices = vec![mk_device("devB", Some(2), &[("only", "holding:0:u16")])];
        let mut single = plan_devices(&devices, 16, 100, 1);
        assert!(!apply_region_splits(&mut single, vec![(0, 0)], "test"));
        assert_eq!(single[0].regions.len(), 1, "不可拆则保持原区间");
    }

    /// F-7：tick 实际耗时 > period 的告警限流（每 60s 一条），计数不受限流影响。
    #[test]
    fn overrun_warn_is_rate_limited() {
        let base = std::time::Instant::now();
        let mut w = OverrunWarn::default();
        assert!(w.record(base), "首次超时告警");
        assert!(
            !w.record(base + Duration::from_secs(59)),
            "60s 窗口内不重复告警"
        );
        assert!(
            w.record(base + Duration::from_secs(60)),
            "满 60s 再告警一次"
        );
        assert_eq!(w.count, 3, "计数不受限流影响（进 tracker extra）");
    }
}
