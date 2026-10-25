//! SampleSender：Source 数据出口的归属登记包装（D-OWN-1）。
//!
//! 不动 Sample/pipeline/lux（ADR D1 保全）——Source trait 的 tx 参数从裸
//! `mpsc::Sender<Sample>` 换为本类型；`Deref` 兼容既有用法，**自身 `send`/`try_send`
//! 方法遮蔽 Deref 的**（Rust 方法查找优先自身）：转发前把 `sample.devid` 归属
//! 登记到 lux `dev2inst`（T1 dev_registry）。进程内 `seen` 去重（同 devid 仅首次
//! HSET，防每 tick 旁路写）；登记失败仅 warn 不阻断数据面（归属是管理面数据，
//! 最终一致——启动预登记兜底，见 T3）。

use crate::types::Sample;
use lux::EmbeddedClient;
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Source 出口包装（`InstanceManager::start_source` 唯一构造点）。
///
/// `Clone` 全共享（inner/lux/seen 同一登记态）——同实例多任务（lua VM 回调、
/// collect 循环）克隆后共享 `seen` 去重表。`lux=None`（[`SampleSender::untracked`]）
/// 纯转发不登记：luaNorth 黑洞 stub 通道与测试用。
///
/// 登记时序：**先登记后转发**——样本即使因背压被丢弃，devid 已产出即归属成立
/// （丢样不否定归属）。`send`（热路径）seen 命中直接转发零额外开销；seen miss
/// 才 await `dev_registry::note`（进程内 lux HGET，微秒级，不值得 spawn 引入乱序）。
/// `try_send` 是同步函数且 EmbeddedClient 无同步面（全 async API）——seen miss 时
/// 经 `Handle::try_current()` spawn detached 登记任务（登记不依赖发送成败顺序，
/// 晚到无碍最终一致）。
///
/// 未实现 `Debug`（EmbeddedClient 无 Debug，同 `HostContext` 口径 expect 放行）。
#[expect(missing_debug_implementations)]
#[derive(Clone)]
pub struct SampleSender {
    /// 被包装的 hub 发送端。
    inner: mpsc::Sender<Sample>,
    /// 登记目标 lux；`None` = 纯转发（测试 / north 黑洞通道）。
    lux: Option<EmbeddedClient>,
    /// 宿主实例 ID（dev2inst 的归属方）。
    inst_id: String,
    /// 进程内已登记 devid 集（防每 tick 旁路写；Clone 共享）。
    seen: Arc<Mutex<HashSet<String>>>,
}

impl SampleSender {
    /// 生产构造：绑定登记目标 lux 与宿主 inst_id（`InstanceManager::start_source`）。
    pub fn new(inner: mpsc::Sender<Sample>, lux: EmbeddedClient, inst_id: String) -> Self {
        Self {
            inner,
            lux: Some(lux),
            inst_id,
            seen: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 纯转发构造（不登记）：luaNorth 黑洞 stub 通道与测试用（生产 Source 路径恒
    /// 走 [`SampleSender::new`]）。
    pub fn untracked(inner: mpsc::Sender<Sample>) -> Self {
        Self {
            inner,
            lux: None,
            inst_id: String::new(),
            seen: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 遮蔽 `mpsc::Sender::send`：先归属登记再转发（返回类型与 mpsc 对齐）。
    pub async fn send(&self, sample: Sample) -> Result<(), mpsc::error::SendError<Sample>> {
        self.note_send(&sample.devid).await;
        self.inner.send(sample).await
    }

    /// 遮蔽 `mpsc::Sender::try_send`：登记走 detached task（同步面无 lux API），
    /// 发送语义与 mpsc 完全一致。
    pub fn try_send(&self, sample: Sample) -> Result<(), mpsc::error::TrySendError<Sample>> {
        self.note_spawn(&sample.devid);
        self.inner.try_send(sample)
    }

    /// 异步登记（`send` 路径）：seen 查重 → miss 才 await note；失败仅 warn
    /// （seen 保留——防对坏 lux 每 tick 重试旁路写；归属最终一致由 T3 启动预登记兜底）。
    async fn note_send(&self, devid: &str) {
        let Some(lux) = self.lux.as_ref() else {
            return;
        };
        if !self.mark_seen(devid) {
            return;
        }
        if let Err(e) = crate::db::dev_registry::note(lux, devid, &self.inst_id).await {
            tracing::warn!(
                "[sample-sender] dev2inst 登记 {devid} <- {} 失败（不阻断数据面）: {e}",
                self.inst_id
            );
        }
    }

    /// detached 登记（`try_send` 路径）：spawn 异步 note；无 runtime（同步线程调用）
    /// 时回滚 seen 放行后续补登记。
    fn note_spawn(&self, devid: &str) {
        let Some(lux) = self.lux.clone() else {
            return;
        };
        if !self.mark_seen(devid) {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let devid = devid.to_string();
                let inst_id = self.inst_id.clone();
                handle.spawn(async move {
                    if let Err(e) = crate::db::dev_registry::note(&lux, &devid, &inst_id).await {
                        tracing::warn!(
                            "[sample-sender] dev2inst 登记 {devid} <- {inst_id} 失败（不阻断数据面）: {e}"
                        );
                    }
                });
            }
            Err(_) => {
                // 回滚 seen：本次未登记成，后续异步上下文的 send/try_send 仍可补。
                if let Ok(mut seen) = self.seen.lock() {
                    seen.remove(devid);
                }
                tracing::warn!("[sample-sender] {devid} 登记跳过：try_send 路径无 tokio runtime");
            }
        }
    }

    /// seen 查重标记：返回 `true` = 首见需登记；已见/锁中毒返 `false`（保守跳过，
    /// 数据面不受影响）。
    fn mark_seen(&self, devid: &str) -> bool {
        self.seen
            .lock()
            .map(|mut seen| seen.insert(devid.to_string()))
            .unwrap_or(false)
    }
}

/// Deref 兼容裸 Sender 的其余用法（`tx.reserve()` 等）——`send`/`try_send` 被自身
/// 方法遮蔽（Rust 方法查找：自身方法优先于 Deref 目标）。
impl Deref for SampleSender {
    type Target = mpsc::Sender<Sample>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时 data_dir（tag 区分用例，防并行测试互扰；照 db/dev_registry 模式）。
    async fn test_client(tag: &str) -> EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("sndsend-{tag}-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    /// 最小 Sample（单 tag 占位值）。
    fn sample(devid: &str) -> Sample {
        Sample {
            hwid: crate::types::Hwid::new("it"),
            devid: devid.into(),
            values: [("v".to_string(), crate::types::SampleValue::Int(1))]
                .into_iter()
                .collect(),
            timestamp: 0,
        }
    }

    /// 轮询等待 dev2inst 出现归属（detached 登记任务异步完成；上限 2s 防永挂）。
    async fn wait_owners(lux: &EmbeddedClient, devid: &str) -> Vec<String> {
        for _ in 0..200 {
            let owners = crate::db::dev_registry::owners(lux, devid).await.unwrap();
            if !owners.is_empty() {
                return owners;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        crate::db::dev_registry::owners(lux, devid).await.unwrap()
    }

    /// send：转发前登记 dev2inst；channel 收到原 Sample。
    #[tokio::test]
    async fn send_registers_dev2inst() {
        let lux = test_client("send-reg").await;
        let (tx, mut rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::new(tx, lux.clone(), "inst-a".into());

        ss.send(sample("d1")).await.expect("转发");

        assert_eq!(
            crate::db::dev_registry::owners(&lux, "d1").await.unwrap(),
            vec!["inst-a"],
            "send 应先登记归属"
        );
        let got = rx.recv().await.expect("channel 收到样本");
        assert_eq!(got.devid, "d1");
        assert_eq!(got.timestamp, 0, "原样转发不改 Sample");
    }

    /// send seen 去重：同 devid 二次 send 不重复登记；新 devid 照常登记。
    #[tokio::test]
    async fn send_seen_dedupes() {
        let lux = test_client("send-dedup").await;
        let (tx, _rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::new(tx, lux.clone(), "inst-a".into());

        ss.send(sample("d1")).await.unwrap();
        ss.send(sample("d1")).await.unwrap();
        assert_eq!(
            crate::db::dev_registry::owners(&lux, "d1").await.unwrap(),
            vec!["inst-a"],
            "同 devid 二次 send 应去重（单宿主）"
        );

        ss.send(sample("d2")).await.unwrap();
        assert_eq!(
            crate::db::dev_registry::owners(&lux, "d2").await.unwrap(),
            vec!["inst-a"],
            "新 devid 应登记"
        );
    }

    /// Clone 共享 seen：克隆分持两处 send 同 devid 仍单条归属。
    #[tokio::test]
    async fn clone_shares_seen() {
        let lux = test_client("send-clone").await;
        let (tx, _rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::new(tx, lux.clone(), "inst-a".into());
        let ss2 = ss.clone();

        ss.send(sample("d1")).await.unwrap();
        ss2.send(sample("d1")).await.unwrap();
        assert_eq!(
            crate::db::dev_registry::owners(&lux, "d1").await.unwrap(),
            vec!["inst-a"],
            "克隆共享登记态，不得双写"
        );
    }

    /// untracked（lux=None）：纯转发，dev2inst 无键。
    #[tokio::test]
    async fn untracked_forwards_without_note() {
        let lux = test_client("send-untracked").await;
        let (tx, mut rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::untracked(tx);

        ss.send(sample("d1")).await.expect("纯转发");
        assert_eq!(rx.recv().await.unwrap().devid, "d1", "样本照常转发");
        assert!(
            crate::db::dev_registry::owners(&lux, "d1")
                .await
                .unwrap()
                .is_empty(),
            "untracked 不登记 dev2inst"
        );
        assert!(
            crate::db::dev_registry::devices_of(&lux, "")
                .await
                .unwrap()
                .is_empty(),
            "整表无归属记录"
        );
    }

    /// try_send：同步面 spawn detached 登记，最终归属落表 + 转发。
    #[tokio::test]
    async fn try_send_registers_dev2inst() {
        let lux = test_client("try-reg").await;
        let (tx, mut rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::new(tx, lux.clone(), "inst-a".into());

        ss.try_send(sample("d1")).expect("转发");
        assert_eq!(rx.try_recv().unwrap().devid, "d1", "同步转发即刻可见");

        let owners = wait_owners(&lux, "d1").await;
        assert_eq!(owners, vec!["inst-a"], "detached 登记任务应完成归属");
    }

    /// try_send seen 去重：同 devid 二次 try_send 单宿主；新 devid 登记。
    #[tokio::test]
    async fn try_send_seen_dedupes() {
        let lux = test_client("try-dedup").await;
        let (tx, _rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::new(tx, lux.clone(), "inst-a".into());

        ss.try_send(sample("d1")).unwrap();
        ss.try_send(sample("d1")).unwrap();
        assert_eq!(
            wait_owners(&lux, "d1").await,
            vec!["inst-a"],
            "try_send 同 devid 去重"
        );

        ss.try_send(sample("d2")).unwrap();
        assert_eq!(
            wait_owners(&lux, "d2").await,
            vec!["inst-a"],
            "try_send 新 devid 登记"
        );
    }

    /// try_send untracked：纯转发零登记。
    #[tokio::test]
    async fn try_send_untracked_forwards_without_note() {
        let lux = test_client("try-untracked").await;
        let (tx, mut rx) = mpsc::channel::<Sample>(8);
        let ss = SampleSender::untracked(tx);

        ss.try_send(sample("d1")).expect("纯转发");
        assert_eq!(rx.try_recv().unwrap().devid, "d1");
        assert!(
            crate::db::dev_registry::owners(&lux, "d1")
                .await
                .unwrap()
                .is_empty(),
            "untracked try_send 不登记"
        );
    }
}
