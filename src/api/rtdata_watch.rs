//! RtdataWatchService：订阅 lux key 事件维护 per-devid 单调版本表（订阅升级
//! Phase 2，D8/P7/P9）——`/api/v1/rtdatas/subscribe` ksub 读源的数据底座。
//!
//! 语义（P7/P9；消费方 `routes_rtdata_subscribe` 的 tick 节奏与 `diff_scope`
//! 冻结不动，本服务只换"谁脏了"的信号源）：
//! - **版本表即设备注册表**：设备首个 key 事件（或启动 `KEYS rtdata:*` 铺底）
//!   即入表；`mark` 每调用 +1，tick 间多次事件天然 coalesce（连接只看落后与否）。
//! - **DEL 也是 key 事件**：版本 bump 后消费方 HGETALL 得空 map → `diff_scope`
//!   自然出 removed（等价原轮询语义，P9）；版本表不删项。
//! - **epoch 重同步**：订阅 Lagged（丢事件）→ epoch+1，持有旧 epoch 的连接
//!   `dirty_since` 得 `Dirty::Full`，全量重读后自行重建游标（P7）。
//!
//! 本模块无条件编译（服务常驻构造、零副作用）；poll 模式下编排层不 spawn
//! 消费循环，lux 无订阅者零开销承诺保持（P8）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// 脏集查询结果（P7）。
#[derive(Debug)]
pub enum Dirty {
    /// epoch 未变：自上次查询以来版本落后（或新出现）的设备列表（游标已推进）。
    Delta(Vec<String>),
    /// epoch 已变（Lagged 重同步）：调用方须全量重读并重建 `last_versions` /
    /// `last_epoch` 游标——本查询**不动游标**。
    Full,
}

/// key 事件消费与版本表状态（内部 `Mutex` 只做短临界区，锁内无 await）。
#[derive(Debug, Default)]
pub struct RtdataWatchService {
    /// per-devid 单调版本号（每事件 +1）；表即设备注册表（P9）。
    versions: Mutex<HashMap<String, u64>>,
    /// 全局重同步代际：Lagged → +1（P7）。
    epoch: AtomicU64,
}

impl RtdataWatchService {
    /// 新服务：epoch=0、空版本表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 事件消费入口：devid 版本 +1（首见入表 = 设备注册，P9）。
    ///
    /// DEL 事件同样走这里（不看 payload）——版本 bump 后消费方 HGETALL 得空
    /// map，`diff_scope` 自然出 removed；版本表不删项。
    pub fn mark(&self, devid: &str) {
        let mut v = self.versions.lock().expect("rtdata_watch versions 锁中毒");
        *v.entry(devid.to_string()).or_insert(0) += 1;
    }

    /// Lagged 重同步信号：全局 epoch +1。持有旧 epoch 的连接据此转全量重读（P7）。
    pub fn bump_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// 当前全局 epoch（新服务从 0 起步，未发生重同步恒 0）。
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// 设备当前版本（None = 未入注册表）。
    pub fn version(&self, devid: &str) -> Option<u64> {
        self.versions
            .lock()
            .expect("rtdata_watch versions 锁中毒")
            .get(devid)
            .copied()
    }

    /// 版本表设备数（设备注册表口径，P9）。
    pub fn device_count(&self) -> usize {
        self.versions
            .lock()
            .expect("rtdata_watch versions 锁中毒")
            .len()
    }

    /// 脏集增量查询（P7）：epoch 不等 → `Dirty::Full`（**不动游标**——调用方
    /// 全量重读后自行重建）；相等 → 收集版本表中"新出现或版本 > last"的设备，
    /// 并把其版本写回 `last_versions`（游标副作用：无新事件则下次查询为空集）。
    pub fn dirty_since(&self, last_versions: &mut HashMap<String, u64>, last_epoch: u64) -> Dirty {
        if self.epoch() != last_epoch {
            return Dirty::Full;
        }
        let versions = self.versions.lock().expect("rtdata_watch versions 锁中毒");
        let mut dirty = Vec::new();
        for (devid, ver) in versions.iter() {
            if last_versions.get(devid).is_some_and(|last| *last >= *ver) {
                continue; // 版本已追平，不脏
            }
            dirty.push(devid.clone());
            last_versions.insert(devid.clone(), *ver);
        }
        Dirty::Delta(dirty)
    }

    /// 启动铺底（P9）：`KEYS rtdata:*` 中缺失的设备入表版本 0——幂等（已有版本
    /// 的设备不动，铺底不占事件版本号）；失败仅告警，事件流仍会补注册。
    pub async fn backfill_keys(&self, client: &lux::EmbeddedClient) {
        let keys = match client.execute_value("KEYS", &["rtdata:*"]).await {
            Ok(lux::EmbeddedValue::Array(keys)) => keys,
            Ok(other) => {
                warn!("[rtdataWatch] KEYS 铺底非数组返回: {other:?}");
                return;
            }
            Err(e) => {
                warn!("[rtdataWatch] KEYS 铺底失败: {e}");
                return;
            }
        };
        let mut v = self.versions.lock().expect("rtdata_watch versions 锁中毒");
        for k in keys {
            let name = match k {
                lux::EmbeddedValue::Bulk(b) => String::from_utf8_lossy(&b).to_string(),
                lux::EmbeddedValue::Simple(s) => s,
                _ => continue,
            };
            if let Some(devid) = name.strip_prefix("rtdata:") {
                v.entry(devid.to_string()).or_insert(0);
            }
        }
    }

    /// spawn 事件消费循环：先 `KEYS rtdata:*` 铺底（存量设备注册），再
    /// `ksubscribe("rtdata:*")` 消费 key 事件（同步建订阅、RAII 退订——drop 即
    /// kunsub；不能走 RESP `cmd("KSUB")`，lux 显式拦截）。退出路径：token 取消
    /// 或订阅 Closed → 退订结束；Lagged → `bump_epoch` + 重铺 KEYS 后继续。
    ///
    /// 返回 JoinHandle 供测试 join；进程编排层可不 join（token 级联取消即退）。
    pub fn spawn(
        self: &Arc<Self>,
        client: lux::EmbeddedClient,
        token: CancellationToken,
    ) -> JoinHandle<()> {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            svc.backfill_keys(&client).await;
            let mut sub = client.ksubscribe("rtdata:*");
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    r = sub.recv() => match r {
                        Ok(msg) => {
                            if matches!(msg.kind, lux::EmbeddedMessageKind::KeyEvent) {
                                if let Some(devid) = msg.channel.strip_prefix("rtdata:") {
                                    // payload=b"del" 也 mark（P9）：版本 bump 后
                                    // 消费方 HGETALL 空 → diff_scope 自然出 removed。
                                    svc.mark(devid);
                                }
                            }
                        }
                        Err(lux::LuxError::SubscriptionLagged(skipped)) => {
                            warn!(
                                "[rtdataWatch] 订阅滞后丢 {skipped} 事件，epoch+1 全量重同步并重铺 KEYS"
                            );
                            svc.bump_epoch();
                            svc.backfill_keys(&client).await;
                        }
                        Err(lux::LuxError::SubscriptionClosed) => {
                            warn!("[rtdataWatch] 订阅已关闭，watcher 退出");
                            break;
                        }
                        Err(e) => {
                            warn!("[rtdataWatch] 订阅异常，watcher 退出: {e}");
                            break;
                        }
                    }
                }
            }
            // 退出路径显式退订（RAII drop → kunsub），先于 client 离场。
            drop(sub);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 版本表语义:mark 每调用 +1;未 mark 设备无记录;版本表即设备注册表。
    #[test]
    fn watch_version_bump_and_coalesce() {
        let svc = RtdataWatchService::new();
        assert_eq!(svc.version("a"), None, "未 mark 的设备无版本记录");
        svc.mark("a");
        assert_eq!(svc.version("a"), Some(1), "首个事件入版本表(P9)");
        svc.mark("a");
        assert_eq!(
            svc.version("a"),
            Some(2),
            "锚定:mark 每调用 +1(Delta 只看落后与否,tick 间多次事件天然 coalesce)"
        );
        assert_eq!(svc.device_count(), 1, "版本表即设备注册表(P9)");
        assert_eq!(svc.version("b"), None);
    }

    /// dirty_since 增量语义:落后才报脏;查询推进 last_versions(下次无新事件
    /// 则无脏);新设备(last 无记录)报脏。
    #[test]
    fn watch_dirty_delta() {
        let svc = RtdataWatchService::new();
        svc.mark("a");
        svc.mark("a"); // 同设备两次事件,仍只报一个脏设备(集合语义)
        let mut last = HashMap::new();

        match svc.dirty_since(&mut last, svc.epoch()) {
            Dirty::Delta(v) => assert_eq!(v, vec!["a".to_string()]),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }

        // last 已被 dirty_since 推进 → 无新事件时再查无脏
        match svc.dirty_since(&mut last, svc.epoch()) {
            Dirty::Delta(v) => assert!(v.is_empty(), "无新脏设备,却报了 {v:?}"),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }

        // 新设备(last 无记录且版本表有)→ Delta 含之
        svc.mark("b");
        match svc.dirty_since(&mut last, svc.epoch()) {
            Dirty::Delta(v) => assert!(v.contains(&"b".to_string()), "新设备应报脏: {v:?}"),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }
    }

    /// epoch 语义:bump 后递增;epoch 变更 → Dirty::Full(即使版本已追平,
    /// 调用方全量重读——Lagged 重同步路径,P7)。
    #[test]
    fn watch_epoch_full() {
        let svc = RtdataWatchService::new();
        svc.mark("a");
        let e0 = svc.epoch();
        svc.bump_epoch();
        assert!(svc.epoch() > e0, "bump_epoch 后全局 epoch 递增");

        let mut last = HashMap::new();
        last.insert("a".to_string(), svc.version("a").unwrap());
        let d = svc.dirty_since(&mut last, e0);
        assert!(matches!(d, Dirty::Full), "epoch 变更强制 Full");
    }

    /// 新服务 epoch 从 0 起步(未发生 Lagged/重同步时恒 0)。
    #[test]
    fn watch_epoch_zero_when_unchanged() {
        let svc = RtdataWatchService::new();
        assert_eq!(svc.epoch(), 0, "起步 epoch=0");
    }
}
