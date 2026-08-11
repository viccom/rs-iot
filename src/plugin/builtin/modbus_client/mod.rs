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
//! 约束单区间最大单元数（coil/discrete 为 bit 数，holding/input 为寄存器数；协议上限 125，
//! 真实设备常更低，遇 IllegalDataAddress 往下调）。
//!
//! **写命令**（`data` JSON `{"address":"holding:0:f32:ABCD","value":12.34}`）：
//! - coil → FC05（单）/ FC0F（多）
//! - holding u16/i16/bit → FC06；i32/u32/f32/f64 → FC16；bit 走 RMW（读-改-写，单 task 内
//!   原子但非事务，部分失败仅 warn）
//! - discrete/input 只读，写拒绝
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
use crate::plugin::traits::Source;
use crate::types::{CtlMessage, DeviceSpec, InstConfig, Sample, SampleValue};
use address::{addr_fc, addr_func_name, parse_point, Func, PointSpec, Type};
use async_trait::async_trait;
use codec::{decode_point, encode_value, Buf, WritePayload};
use config::{parse_cfg, PollStrategy};
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
use config::{FlowKind, ParityKind, Transport};
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
        tx: mpsc::Sender<Sample>,
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
        // 预解析 devices → SlavePlan（每 slave 独立 regions + slave_id）+ 轮询顺序。
        let plans = plan_devices(&inst.devices, cfg.gap_threshold, cfg.max_regs, cfg.slave_id);
        let poll_order = build_poll_order(&plans, cfg.poll_strategy);
        let req_timeout = Duration::from_secs(cfg.request_timeout_secs);
        // handle_write 反查 devid → slave_id（写命令按目标设备切 slave）。
        let devid_to_slave: HashMap<String, u8> = plans
            .iter()
            .map(|p| (p.devid.clone(), p.slave_id))
            .collect();

        let period = Duration::from_secs(cfg.period);
        // jitter 相位：FNV-1a(inst_id) % period，多实例首次 tick 散布，削整点脉冲。
        let phase = std::time::Duration::from_millis(crate::plugin::ticker::phase_ms(
            inst_id.as_str(),
            period,
        ));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + phase, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut tracker = crate::plugin::stat_tracker::StatTracker::new(
            inst.inst_id.to_string(),
            ctx.lux.clone(),
        );
        // exception（modbus 协议层错误码）独立计数——与 transport 失败区分（后者拆 session）。
        let mut exceptions: u64 = 0;
        let mut flush_ticker = tokio::time::interval(std::time::Duration::from_secs(
            crate::plugin::stat_tracker::FLUSH_INTERVAL_SECS,
        ));

        let mut session: Option<Context> = None;

        loop {
            let evt = tokio::select! {
                _ = token.cancelled() => break,
                msg = cmd_rx.recv() => Evt::Cmd(msg),
                _ = flush_ticker.tick() => {
                    tracker.flush().await;
                    continue;
                }
                _ = ticker.tick() => Evt::Tick,
            };
            match evt {
                Evt::Cmd(Some(m)) => {
                    if m.cmd == "write" {
                        handle_write(&mut session, &m.data, &m.devid, &devid_to_slave).await;
                    } else {
                        tracing::info!("[modbusClient] inst {inst_id} unknown cmd: {}", m.cmd);
                    }
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
                                tokio::time::sleep(Duration::from_secs(cfg.reconnect_secs)).await;
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
                    for (pi, ri) in &poll_order {
                        if timed_out_slave.is_some_and(|s| s == *pi) {
                            continue;
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
                                for (field, p) in &reg.points {
                                    if let Some(v) = decode_point(p, &buf, reg.start) {
                                        acc.entry(plan.devid.clone())
                                            .or_default()
                                            .insert((*field).to_string(), v);
                                    }
                                }
                            }
                            ReadOutcome::Exception(exc) => {
                                exceptions += 1;
                                tracker
                                    .extra_insert("exception_count", serde_json::json!(exceptions));
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} exception: {exc:?}",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len()
                                );
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
                            }
                            ReadOutcome::Transport(e) => {
                                tracing::warn!(
                                    "[modbusClient] inst {inst_id} slave {} read {}+{} failed: {e:?}",
                                    plan.slave_id,
                                    reg.start,
                                    reg.len()
                                );
                                transport_failed = true;
                                break;
                            }
                        }
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
                        tracker.tick_ok(tick_started.elapsed().as_millis() as u64);
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

/// 处理 write 命令：`data` = JSON `{"address":"holding:0:f32:ABCD","value":12.34}`。
///
/// coil → FC05/0F；holding u16/i16/bit → FC06，i32/u32/f32/f64 → FC16，bit 走 RMW；
/// discrete/input 只读拒绝。写失败仅 warn，不拆 session（读 tick 兜底发现断连）。
async fn handle_write(
    session: &mut Option<Context>,
    data: &str,
    devid: &str,
    devid_to_slave: &HashMap<String, u8>,
) {
    let req: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[modbusClient] write bad json: {e}");
            return;
        }
    };
    let addr = match req.get("address").and_then(|v| v.as_str()) {
        Some(a) => a,
        None => {
            tracing::warn!("[modbusClient] write missing address");
            return;
        }
    };
    let p = match parse_point(addr) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[modbusClient] write bad address '{addr}': {e}");
            return;
        }
    };
    if p.func.is_readonly() {
        tracing::warn!(
            "[modbusClient] write rejected: {}(FC0{}) 只读",
            addr_func_name(p.func),
            addr_fc(p.func)
        );
        return;
    }
    let value = match req.get("value") {
        Some(v) => json_to_sample_value(v),
        None => {
            tracing::warn!("[modbusClient] write missing value");
            return;
        }
    };
    let sess = match session {
        Some(s) => s,
        None => {
            tracing::warn!("[modbusClient] write ignored: not connected");
            return;
        }
    };
    // 按 devid 反查 slave_id（data 可显式带 slave_id 覆盖）。
    let slave_id = req
        .get("slave_id")
        .and_then(|v| v.as_u64())
        .and_then(|v| u8::try_from(v).ok())
        .or_else(|| devid_to_slave.get(devid).copied());
    let slave_id = match slave_id {
        Some(s) => s,
        None => {
            tracing::warn!(
                "[modbusClient] write 无法确定 slave_id（devid={devid} 未在 devices 配置，data 也无 slave_id）"
            );
            return;
        }
    };
    sess.set_slave(Slave(slave_id));
    // 仅 holding bit 写需 RMW（coil 直接写单 coil，无需读当前值）。
    let current_reg = if p.func == Func::Holding && p.typ == Type::Bit {
        match sess.read_holding_registers(p.addr, 1).await {
            Ok(Ok(buf)) => buf.first().copied(),
            Ok(Err(exc)) => {
                tracing::warn!("[modbusClient] bit write RMW read exception: {exc:?}");
                return;
            }
            Err(e) => {
                tracing::warn!("[modbusClient] bit write RMW read failed: {e:?}");
                return;
            }
        }
    } else {
        None
    };
    let payload = match encode_value(&value, &p, current_reg) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[modbusClient] write encode failed: {e}");
            return;
        }
    };
    let result = match payload {
        WritePayload::Coils(c) => {
            if c.len() == 1 {
                sess.write_single_coil(p.addr, c[0]).await
            } else {
                sess.write_multiple_coils(p.addr, &c).await
            }
        }
        WritePayload::Regs(r) => {
            if r.len() == 1 {
                sess.write_single_register(p.addr, r[0]).await
            } else {
                sess.write_multiple_registers(p.addr, &r).await
            }
        }
    };
    match result {
        Ok(Ok(())) => {}
        Ok(Err(exc)) => tracing::warn!("[modbusClient] write exception: {exc:?}"),
        Err(e) => tracing::warn!("[modbusClient] write failed: {e:?}"),
    }
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
        assert_eq!(cfg2.max_regs, 125, "max_regs clamp 125");
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
                },
                ChildSpec {
                    label: "bad".into(),
                    value: "holding:x:u16".into(),
                    r#type: "".into(),
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
}
