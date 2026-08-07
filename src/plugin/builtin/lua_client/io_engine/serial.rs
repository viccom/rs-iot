//! serial IO backend（spec §8）：tokio-serial，懒 open + err 置 None 重 open。

use super::super::config::{FlowControl, Parity};
use super::IoBackend;
use tokio_serial::SerialPortBuilderExt;

/// serial IO backend：独占一条 `tokio_serial::SerialStream`，懒 open + err 置 None。
///
/// 对标 `TcpIo`（spec §8 不变式：连接生命周期由 IO task 管）。serial 无"对端关闭"，
/// read/write err（设备拔插/线缆）→ 置 None，下次 request 重 open（reopen 失败报错）。
/// read 用 idle 累积成帧（同 tcp，串口流式）；recv_idle 读被动帧。
pub(in crate::plugin::builtin::lua_client) struct SerialIo {
    /// 当前串口流（None = 未 open/err，下次 request 前 ensure_connected 重 open）。
    stream: Option<tokio_serial::SerialStream>,
    port: String,
    baud_rate: u32,
    data_bits: u8,
    parity: Parity,
    stop_bits: u8,
    flow_control: FlowControl,
    idle: std::time::Duration,
}

impl SerialIo {
    /// 构造（不立即 open，首次 request 才 open）。
    pub(in crate::plugin::builtin::lua_client) fn new(
        port: String,
        baud_rate: u32,
        data_bits: u8,
        parity: Parity,
        stop_bits: u8,
        flow_control: FlowControl,
        recv_idle_ms: u64,
    ) -> Self {
        Self {
            stream: None,
            port,
            baud_rate,
            data_bits,
            parity,
            stop_bits,
            flow_control,
            idle: std::time::Duration::from_millis(recv_idle_ms),
        }
    }

    /// 确保串口已 open；None 则 open（失败返回 Err）。
    async fn ensure_connected(&mut self) -> mlua::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let builder = tokio_serial::new(&self.port, self.baud_rate)
            .data_bits(map_data_bits(self.data_bits))
            .parity(map_parity(self.parity))
            .stop_bits(map_stop_bits(self.stop_bits))
            .flow_control(map_flow(self.flow_control));
        match builder.open_native_async() {
            Ok(s) => {
                self.stream = Some(s);
                Ok(())
            }
            Err(e) => Err(mlua::Error::RuntimeError(format!(
                "serial open {}: {e}",
                self.port
            ))),
        }
    }
}

/// `data_bits`(u8) → tokio_serial `DataBits`。
fn map_data_bits(b: u8) -> tokio_serial::DataBits {
    match b {
        5 => tokio_serial::DataBits::Five,
        6 => tokio_serial::DataBits::Six,
        7 => tokio_serial::DataBits::Seven,
        _ => tokio_serial::DataBits::Eight,
    }
}

/// `Parity` → tokio_serial `Parity`。
fn map_parity(p: Parity) -> tokio_serial::Parity {
    match p {
        Parity::None => tokio_serial::Parity::None,
        Parity::Even => tokio_serial::Parity::Even,
        Parity::Odd => tokio_serial::Parity::Odd,
    }
}

/// `stop_bits`(u8) → tokio_serial `StopBits`。
fn map_stop_bits(b: u8) -> tokio_serial::StopBits {
    match b {
        2 => tokio_serial::StopBits::Two,
        _ => tokio_serial::StopBits::One,
    }
}

/// `FlowControl` → tokio_serial `FlowControl`。
fn map_flow(f: FlowControl) -> tokio_serial::FlowControl {
    match f {
        FlowControl::None => tokio_serial::FlowControl::None,
        FlowControl::Software => tokio_serial::FlowControl::Software,
        FlowControl::Hardware => tokio_serial::FlowControl::Hardware,
    }
}

#[async_trait::async_trait]
impl IoBackend for SerialIo {
    async fn request(&mut self, frame: &[u8]) -> mlua::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.ensure_connected().await?;
        if let Err(e) = self
            .stream
            .as_mut()
            .expect("ensure_connected 后必有 stream")
            .write_all(frame)
            .await
        {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("serial write: {e}")));
        }
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let mut io_err: Option<std::io::Error> = None;
        loop {
            let stream = self.stream.as_mut().expect("read 期间 stream 必在");
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(e)) => {
                    io_err = Some(e);
                    break;
                }
                Err(_) => break,
            }
        }
        if let Some(e) = io_err {
            self.stream = None;
            return Err(mlua::Error::RuntimeError(format!("serial read: {e}")));
        }
        if buf.is_empty() {
            return Err(mlua::Error::RuntimeError("serial: 无响应".into()));
        }
        Ok(buf)
    }

    /// 无在飞 request 时主动收一帧（idle 累积），同 TcpIo::recv_idle。
    async fn recv_idle(&mut self) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        self.ensure_connected().await.ok()?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            let stream = self.stream.as_mut()?;
            match tokio::time::timeout(self.idle, stream.read(&mut tmp)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                Ok(Err(_)) => {
                    self.stream = None;
                    break;
                }
                Err(_) => break,
            }
        }
        if buf.is_empty() {
            None
        } else {
            Some(buf)
        }
    }
}
