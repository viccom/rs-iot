//! 连接：按 transport 选 TCP / RTU（serial / over-tcp / over-udp），统一产 `Context`。

use super::config::{FlowKind, ModbusCfg, ParityKind, Transport};
use crate::io_trace::{TraceIo, TraceRegistry};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UdpSocket};
use tokio_modbus::client::{rtu, tcp, Context};
use tokio_modbus::prelude::SlaveContext;
use tokio_modbus::Slave;
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, StopBits};

/// 建立到 Modbus 从站的连接（按 transport 选 TCP / RTU(serial/over-tcp/over-udp)）。
/// 四种传输统一返回 `Context`，下游采集/写逻辑同构。每次新建 Context 都 `set_slave`（drop 后丢失）。
///
/// 所有传输的底层 IO 都包一层 `TraceIo`（trace 关闭时 `dump` 快速路径几乎零成本），
/// 便于报文 trace 拦截 tokio-modbus 吞掉的收发字节。`inst_id` + `trace_registry`
/// 由调用方（`Source::run` 作用域）传入。
pub(super) async fn connect(
    cfg: &ModbusCfg,
    inst_id: &str,
    trace_registry: TraceRegistry,
) -> Result<Context, String> {
    let mut ctx = match cfg.transport {
        Transport::Tcp => {
            let addr = parse_addr(&cfg.endpoint)?;
            // TCP:手动 connect + tcp::attach_slave（接受泛型 transport，可插 TraceIo）。
            // 原先用的 `tcp::connect` 库内建连接，不经我们手，无法包 TraceIo，故改写。
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| format!("connect tcp {}: {e}", cfg.endpoint))?;
            tcp::attach_slave(
                TraceIo::new(stream, inst_id.into(), trace_registry.clone()),
                Slave(cfg.slave_id),
            )
        }
        Transport::RtuTcp => {
            let addr = parse_addr(&cfg.endpoint)?;
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| format!("connect rtu-tcp {}: {e}", cfg.endpoint))?;
            rtu::attach_slave(
                TraceIo::new(stream, inst_id.into(), trace_registry.clone()),
                Slave(cfg.slave_id),
            )
        }
        Transport::RtuUdp => {
            let addr = parse_addr(&cfg.endpoint)?;
            let sock = UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|e| format!("bind rtu-udp: {e}"))?;
            sock.connect(addr)
                .await
                .map_err(|e| format!("connect rtu-udp {}: {e}", cfg.endpoint))?;
            rtu::attach_slave(
                TraceIo::new(UdpIo::new(sock), inst_id.into(), trace_registry.clone()),
                Slave(cfg.slave_id),
            )
        }
        Transport::RtuSerial => {
            let port = cfg
                .serial_port
                .as_ref()
                .ok_or_else(|| "rtu_serial 缺 serial_port".to_string())?;
            let builder = tokio_serial::new(port, cfg.baud_rate)
                .data_bits(map_data_bits(cfg.data_bits))
                .parity(map_parity(cfg.parity))
                .stop_bits(map_stop_bits(cfg.stop_bits))
                .flow_control(map_flow(cfg.flow_control));
            let stream = builder
                .open_native_async()
                .map_err(|e| format!("open serial {port}: {e}"))?;
            rtu::attach_slave(
                TraceIo::new(stream, inst_id.into(), trace_registry.clone()),
                Slave(cfg.slave_id),
            )
        }
    };
    // 统一 set_slave：RTU 在 attach_slave 已带 slave，再设无害；TCP 必须设。
    ctx.set_slave(Slave(cfg.slave_id));
    Ok(ctx)
}

/// 解析 `ip:port` → `SocketAddr`（预解析避免 DNS 边界，与原 TCP 实现一致）。
fn parse_addr(endpoint: &str) -> Result<SocketAddr, String> {
    endpoint
        .parse()
        .map_err(|e| format!("invalid endpoint '{endpoint}': {e}"))
}

/// `data_bits`(u8) → serialport `DataBits`（parse_cfg 已校验 5/6/7/8）。
fn map_data_bits(b: u8) -> DataBits {
    match b {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    }
}

/// `ParityKind` → serialport `Parity`。
fn map_parity(p: ParityKind) -> Parity {
    match p {
        ParityKind::None => Parity::None,
        ParityKind::Even => Parity::Even,
        ParityKind::Odd => Parity::Odd,
    }
}

/// `stop_bits`(u8) → serialport `StopBits`（parse_cfg 已校验 1/2）。
fn map_stop_bits(b: u8) -> StopBits {
    match b {
        2 => StopBits::Two,
        _ => StopBits::One,
    }
}

/// `FlowKind` → serialport `FlowControl`。
fn map_flow(f: FlowKind) -> FlowControl {
    match f {
        FlowKind::None => FlowControl::None,
        FlowKind::Software => FlowControl::Software,
        FlowKind::Hardware => FlowControl::Hardware,
    }
}

/// UDP RTU 传输适配器：把 connected `UdpSocket` 包成 `AsyncRead + AsyncWrite`，供 `rtu::attach_slave`。
///
/// RTU client "写一帧→读一帧" 的交互正好对应 UDP "攒帧→发一个 datagram→收一个 datagram"，
/// 1:1 请求-响应，datagram 边界天然保帧，无流式粘包/分帧歧义。
///
/// - `poll_write`：攒字节到 `write_buf`（不实际发，等 flush）——对应 RTU client 编码一帧的多次 write；
/// - `poll_flush`：`write_buf` 整体作为一个 datagram `poll_send`——一个 RTU 帧 = 一个 UDP 包；
/// - `poll_read`：先消费 `read_buf` 剩余（上次 recv 未读完），否则 `poll_recv` 收一个整 datagram 入队再按容量吐出。
///
/// 内部用 `UdpSocket::poll_send`/`poll_recv`（cancel-safe），`UdpIo` 字段均为非自引用故 `Unpin`。
pub(super) struct UdpIo {
    /// 已 connect(peer) 的 UDP socket。
    sock: UdpSocket,
    /// 待 flush 发送的积压字节（攒一帧）。
    write_buf: Vec<u8>,
    /// 上次 recv 到但未消费完的字节（UDP 一次 recv = 整 datagram，poll_read 分批吐出）。
    read_buf: VecDeque<u8>,
}

impl UdpIo {
    /// 包一个已 `connect(peer)` 的 `UdpSocket`。
    pub(super) fn new(sock: UdpSocket) -> Self {
        Self {
            sock,
            write_buf: Vec::new(),
            read_buf: VecDeque::new(),
        }
    }
}

impl AsyncRead for UdpIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // 先消费 read_buf 剩余（上次 recv 的 datagram 未读完部分）。
        if !this.read_buf.is_empty() {
            drain_into(this, buf);
            return Poll::Ready(Ok(()));
        }
        // read_buf 空：recv 一个新 datagram（以太网 MTU 1500 足够单 RTU 帧 ≤256B）。
        const MAX_DGRAM: usize = 1500;
        let mut tmp = [0u8; MAX_DGRAM];
        let mut rb = ReadBuf::new(&mut tmp);
        match this.sock.poll_recv(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                this.read_buf.extend(rb.filled().iter().copied());
                drain_into(this, buf);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for UdpIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.write_buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        while !this.write_buf.is_empty() {
            // poll_send 理论整包发（UDP），但 API 返回字节数，循环 drain 保险。
            match this.sock.poll_send(cx, this.write_buf.as_slice()) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::other("udp poll_send wrote 0 bytes")));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_buf.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        // UDP 无连接态可关；清空 write_buf（丢弃未发数据，失联重连语义）。
        self.get_mut().write_buf.clear();
        Poll::Ready(Ok(()))
    }
}

/// 从 `read_buf` 队首按 `buf` 剩余容量吐字节（UdpIo::poll_read 复用）。
fn drain_into(this: &mut UdpIo, buf: &mut ReadBuf<'_>) {
    let n = buf.remaining().min(this.read_buf.len());
    // VecDeque 非连续，drain 收集成切片喂 `put_slice`（ReadBuf 无逐字节 put_u8）。
    let chunk: Vec<u8> = this.read_buf.drain(..n).collect();
    buf.put_slice(&chunk);
}
