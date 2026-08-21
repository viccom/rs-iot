//! serial IO backend（spec §8）：tokio-serial，懒 open + err 置 None 重 open。

use super::super::config::{FlowControl, Parity};
use super::IoBackend;
use tokio_serial::SerialPortBuilderExt;

/// serial IO backend：独占一条 `tokio_serial::SerialStream`，懒 open + err 置 None。
///
/// 对标 `TcpIo`（spec §8 不变式：连接生命周期由 IO task 管）。serial 无"对端关闭"，
/// read/write err（设备拔插/线缆）→ 置 None，下次 request 重 open（reopen 失败报错）。
/// read 用 idle 累积成帧（同 tcp，串口流式）；recv_idle 读被动帧。
pub(crate) struct SerialIo {
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
    pub(crate) fn new(
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
    ///
    /// open 底层是阻塞系统调用（Windows CreateFileW，驱动异常时可无限期不返回），
    /// 直接在 async 上下文调用会冻结 IO task——select/abort 均无法抢占系统调用级
    /// 阻塞，stopinst 后 COM 口被占直到杀进程（2026-08-20 现场实测，见设计文档
    /// 遗留清单）。故把 builder move 进 spawn_blocking、外包 5s timeout：
    /// open 超时 → Err 置 None，下轮 request 重试，IO task 不再停摆。
    ///
    /// 语义注记：超时 ≠ COM 句柄立即释放——blocking 线程无法强杀，若驱动层真卡死，
    /// 句柄由该线程自然结束时释放（主流程不再被卡是质变：进程级冻结 → 单线程
    /// 挂起）；串口流本身的读写是 mio 集成的 SerialStream，不受此影响。
    async fn ensure_connected(&mut self) -> mlua::Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let builder = tokio_serial::new(&self.port, self.baud_rate)
            .data_bits(map_data_bits(self.data_bits))
            .parity(map_parity(self.parity))
            .stop_bits(map_stop_bits(self.stop_bits))
            .flow_control(map_flow(self.flow_control));
        // open_native_async = mio_serial 阻塞 open（系统调用）+ reactor 注册（依赖
        // Handle::current，spawn_blocking 线程内可用），产出 SerialStream（Send，
        // 跨线程 move 回 IO task 安全）。
        let opened = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || builder.open_native_async()),
        )
        .await;
        match opened {
            Ok(Ok(Ok(s))) => {
                self.stream = Some(s);
                Ok(())
            }
            Ok(Ok(Err(e))) => Err(mlua::Error::RuntimeError(format!(
                "serial open {}: {e}",
                self.port
            ))),
            // blocking 线程 panic（join 失败）按 open 失败处理
            Ok(Err(e)) => Err(mlua::Error::RuntimeError(format!(
                "serial open {}: blocking task {e}",
                self.port
            ))),
            Err(_) => Err(mlua::Error::RuntimeError(format!(
                "serial open {}: 超时（5s）——IO task 不冻结，句柄由 blocking 线程结束时释放",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::lua_core::io_engine::{spawn_io_task, IoEngine};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// 测试标配参数（9600 8N1 + 50ms idle 成帧窗）。
    fn test_io(port: String) -> SerialIo {
        SerialIo::new(port, 9600, 8, Parity::None, 1, FlowControl::None, 50)
    }

    /// SerialIo 包成 IO task（on_data 丢弃；token 可控）——真机②③用（断言 IO task 不被
    /// open 冻结，需走 spawn_io_task 全链路）。
    fn spawn_serial_task(
        port: &str,
        token: CancellationToken,
    ) -> (IoEngine, tokio::task::JoinHandle<()>) {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        spawn_io_task(
            Box::new(test_io(port.to_string())),
            tx,
            "serial-test".into(),
            crate::io_trace::TraceRegistry::new(),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            token,
        )
    }

    /// open 不存在口：立即 ENOENT（非门控也安全，跑得快），但必须落在 5s 预算内返回
    /// Err——回归锚：timeout 包装不得吞掉快速失败路径（Task 7）。
    #[tokio::test]
    async fn open_missing_port_errors_within_budget() {
        let mut io = test_io("COM99_not_exist".into());
        let t0 = Instant::now();
        let r = io.request(b"x").await;
        assert!(r.is_err(), "open 不存在口应 Err，got {r:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "不存在口应在 5s 预算内返回（elapsed {:?}）",
            t0.elapsed()
        );
    }

    /// 解析 `RSIOT_SERIAL_E2E=A,B`（com0com 虚拟串口对，A=SerialIo 侧，B=对端）。
    fn e2e_pair() -> (String, String) {
        let v = std::env::var("RSIOT_SERIAL_E2E")
            .expect("需 RSIOT_SERIAL_E2E=A,B（如 COM3,COM4；com0com 虚拟串口对）");
        let (a, b) = v
            .split_once(',')
            .expect("RSIOT_SERIAL_E2E 需 'A,B' 形态（如 COM3,COM4）");
        (a.trim().to_string(), b.trim().to_string())
    }

    /// 真机①正常对端回归：COM4 侧开口回显 + 0xAA → SerialIo request 往返；drop 后
    /// COM 句柄应释放（第三方重开同口成功佐证——计划 Task 7 验证点）。
    ///
    /// 跑法（本机 com0com 对 COM3/COM4）：
    /// ```text
    /// RSIOT_SERIAL_E2E=COM3,COM4 \
    ///   cargo test -p rs-iot --features lua,lua-http io_engine::serial -- --ignored
    /// ```
    #[ignore = "需 com0com 虚拟串口对 + RSIOT_SERIAL_E2E=A,B 环境变量，见测试注释"]
    #[tokio::test]
    async fn serial_e2e_pair_roundtrip_and_handle_release() {
        let (pa, pb) = e2e_pair();
        // 对端 B：收什么回什么 + 尾巴 0xAA（同 tcp_roundtrip 形态）
        let mut peer = tokio_serial::new(&pb, 9600)
            .open_native_async()
            .unwrap_or_else(|e| panic!("对端 {pb} open 失败（env 口配错？）：{e}"));
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 64];
            loop {
                match peer.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut r = buf[..n].to_vec();
                        r.push(0xAA);
                        let _ = peer.write_all(&r).await;
                    }
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut io = test_io(pa.clone());
        let resp = io.request(b"\x01\x02").await;
        assert_eq!(resp.unwrap(), vec![0x01, 0x02, 0xAA]);

        // 句柄释放佐证：drop SerialIo → 第三方重开同口应成功（stopinst 后 COM 口
        // 不被残留句柄占住）。
        drop(io);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let reopen = tokio_serial::new(&pa, 9600).open_native_async();
        assert!(
            reopen.is_ok(),
            "drop 后 {pa} 句柄应释放（重开失败：{:?}）",
            reopen.err()
        );
    }

    /// 真机②不存在口：open → 5s 内返回 Err 且 IO task 存活（token 可 stop）——防回归
    /// 到 open 冻结 IO task（不存在口通常立即 ENOENT，立即 Err 也在断言范围内）。
    #[ignore = "需 RSIOT_SERIAL_E2E=A,B 环境变量，见 serial_e2e_pair_roundtrip_and_handle_release 注释"]
    #[tokio::test]
    async fn serial_e2e_missing_port_errors_and_io_task_alive() {
        let token = CancellationToken::new();
        let (engine, join) = spawn_serial_task("COM99_not_exist", token.clone());
        let t0 = Instant::now();
        // 脚本侧预算放宽到 30s（clamp 上限）——若 open 冻结 IO task，request 会吃满
        // 预算返回 "IO request timeout"（elapsed ≈30s）；正常路径立即/5s 内返回。
        let r = engine.request(b"x", 30_000).await;
        assert!(r.is_err(), "open 不存在口应 Err，got {r:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "应在 5s 内返回 Err（elapsed {:?}）",
            t0.elapsed()
        );
        // IO task 存活佐证：token cancel → 限时退出
        token.cancel();
        let exited = tokio::time::timeout(Duration::from_secs(3), join).await;
        assert!(exited.is_ok(), "IO task 应存活且可被 token stop");
    }

    /// 真机③被占口：第三方先独占 COM 口 → SerialIo open → **10s 内返回**（Err 或 Ok
    /// 均可）即可——真正的驱动级卡死难以确定性复现，本项断言「不回归到永久阻塞」：
    /// Windows 被占口通常立即 PermissionDenied；若触发驱动异常走 5s open 超时路径，
    /// elapsed ≈5s 同样满足。open 失败路径不应泄漏句柄：释放占用后第三方重开成功。
    #[ignore = "需 com0com 虚拟串口对 + RSIOT_SERIAL_E2E=A,B 环境变量，见 serial_e2e_pair_roundtrip_and_handle_release 注释"]
    #[tokio::test]
    async fn serial_e2e_occupied_port_no_wedge_and_no_handle_leak() {
        let (pa, _pb) = e2e_pair();
        let holder = tokio_serial::new(&pa, 9600)
            .open_native_async()
            .unwrap_or_else(|e| panic!("预占 {pa} open 失败（env 口配错？）：{e}"));

        let token = CancellationToken::new();
        let (engine, join) = spawn_serial_task(&pa, token.clone());
        let t0 = Instant::now();
        let _r = engine.request(b"x", 30_000).await; // 方向不断言（Err/Ok 均可），await 返回即未挂死
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "被占口 open 应 10s 内返回（Err/Ok 均可，防回归到永久阻塞；elapsed {:?}）",
            t0.elapsed()
        );
        token.cancel();
        let exited = tokio::time::timeout(Duration::from_secs(3), join).await;
        assert!(exited.is_ok(), "IO task 应可被 token stop");

        // 句柄未泄漏佐证：释放第三方占用 → SerialIo 的 open 失败路径（stream=None）
        // 不持句柄，第三方重开应成功。
        drop(holder);
        tokio::time::sleep(Duration::from_millis(300)).await; // 给可能残留的 blocking 线程收尾
        let reopen = tokio_serial::new(&pa, 9600).open_native_async();
        assert!(
            reopen.is_ok(),
            "释放占用后 {pa} 应可重开（open 失败路径泄漏句柄？{:?}）",
            reopen.err()
        );
    }
}
