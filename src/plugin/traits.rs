//! 插件 trait 契约。Lite_IOT 的 `Plugin.Execute(appConfig, devices, dataQueue,
//! syncDataQueue, wg, stopChan)` 在此按职责拆为正交的 `Source` / `Sink` /
//! `Processor`，用 Rust async + `CancellationToken` 表达生命周期。

use crate::host::HostContext;
use crate::types::{CtlMessage, InstConfig, Sample};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 南向：产出数据。`datasim` 等模拟/采集插件实现此 trait。
///
/// `run` 在独立 tokio task 中执行，通过 `tx` 推送 `Sample`，从 `cmd_rx`
/// 接收下行命令，`token` 触发退出。
#[async_trait]
pub trait Source: Send + Sync {
    /// 插件名（注册表 key，对应 `InstConfig::app_code`）。
    fn name(&self) -> &'static str;

    /// 运行采集循环：产出经 `tx` 推入 hub，命令从 `cmd_rx` 消费，`token` 取消时退出。
    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        tx: mpsc::Sender<Sample>,
        cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    );
}

/// 北向：消费数据。`mqttPub` 等发布插件实现此 trait。
#[async_trait]
pub trait Sink: Send + Sync {
    /// 插件名（注册表 key）。
    fn name(&self) -> &'static str;

    /// 运行发布循环：从 `rx` 消费样本发布，命令从 `cmd_rx` 消费，`token` 取消时退出。
    async fn run(
        &self,
        ctx: HostContext,
        inst: InstConfig,
        rx: mpsc::Receiver<Sample>,
        cmd_rx: mpsc::Receiver<CtlMessage>,
        token: CancellationToken,
    );
}

/// L2 数据转换。默认 `PassthroughProcessor`；`feature=transform` 时可用 `RhaiProcessor`。
#[async_trait]
pub trait Processor: Send + Sync {
    /// 返回 None 表示丢弃该样本。
    async fn process(&self, sample: Sample) -> Option<Sample>;
}
