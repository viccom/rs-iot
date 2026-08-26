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
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

/// 脏集查询结果（P7）。
#[derive(Debug)]
pub enum Dirty {
    /// epoch 未变：自上次查询以来版本落后（或新出现）的设备列表（游标已推进）。
    Delta(Vec<String>),
    /// epoch 已变（Lagged 重同步）：调用方须全量重读并重建 `last_versions` /
    /// `last_epoch` 游标——本查询**不动游标**。
    Full,
}

/// ksub 连接游标（P7）：版本表基线 + epoch + marks 三元组。经 [`RtdataWatchService::
/// cursor`] 建立与重建（`dirty_since` 消费推进）。
#[derive(Debug, Clone)]
pub struct WatchCursor {
    /// 版本表基线（per-devid 已见版本）。
    versions: HashMap<String, u64>,
    /// 建立时的全局 epoch。
    epoch: u64,
    /// 建立时的 marks 计数（O(1) 早退判据）。
    marks: u64,
}

impl WatchCursor {
    /// 建立时的全局 epoch（Resync 返回值/日志用）。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// key 事件消费与版本表状态（内部 `Mutex` 只做短临界区，锁内无 await）。
#[derive(Debug, Default)]
pub struct RtdataWatchService {
    /// per-devid 单调版本号（每事件 +1）；表即设备注册表（P9）。
    versions: Mutex<HashMap<String, u64>>,
    /// 全局重同步代际：Lagged → +1（P7）。
    epoch: AtomicU64,
    /// 版本表内容代号：每次 mark 与 backfill 新增设备 +1（**与表变更同一
    /// 锁临界区内递增**——marks 与表状态对齐是 `dirty_since` O(1) 早退
    /// 正确性的前提：marks 相等 ⇔ 表自游标建立后无任何变更）。
    marks: AtomicU64,
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
        // 与表变更同临界区递增（见字段注释的对齐不变量）。
        self.marks.fetch_add(1, Ordering::Relaxed);
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

    /// 版本表快照（P7 游标基线）：返回拷贝，与表后续变更隔离。
    pub fn snapshot_versions(&self) -> HashMap<String, u64> {
        self.versions
            .lock()
            .expect("rtdata_watch versions 锁中毒")
            .clone()
    }

    /// 游标快照（P7）：版本表 + epoch + marks 三元组，ksub 连接 snapshot 阶段
    /// **之前**初始化、Lagged 重同步后重建——调用方须**先**取快照再全量读
    /// （之后的 mark 版本 > 游标，下次 tick 报脏，不丢帧）。
    pub fn cursor(&self) -> WatchCursor {
        let versions = self.versions.lock().expect("rtdata_watch versions 锁中毒");
        // 锁内读 marks：versions 快照与 marks 计数对齐（同临界区不变量）。
        WatchCursor {
            versions: versions.clone(),
            epoch: self.epoch(),
            marks: self.marks.load(Ordering::Relaxed),
        }
    }

    /// 版本表设备数（设备注册表口径，P9）。
    pub fn device_count(&self) -> usize {
        self.versions
            .lock()
            .expect("rtdata_watch versions 锁中毒")
            .len()
    }

    /// 脏集增量查询（P7）：epoch 不等 → `Dirty::Full`（**不动游标**——调用方
    /// 全量重读后 `cursor()` 重建）；相等且 marks 相等 → **O(1) 早退**空 Delta
    /// （表自游标建立后无任何 mark/backfill 变更，免全表扫描——每连接每 tick
    /// 从 O(N) 降到 O(1)，N×M 并发订阅下显著）；否则锁内收集"新出现或版本 >
    /// 游标"的设备并推进游标（游标副作用：无新事件则下次查询为空集）。
    pub fn dirty_since(&self, cur: &mut WatchCursor) -> Dirty {
        if self.epoch() != cur.epoch {
            return Dirty::Full;
        }
        // O(1) 早退：marks 与表状态锁内对齐（mark/backfill 同临界区递增），
        // 相等即无任何变更——不进锁不全扫。
        if self.marks.load(Ordering::Relaxed) == cur.marks {
            return Dirty::Delta(Vec::new());
        }
        let versions = self.versions.lock().expect("rtdata_watch versions 锁中毒");
        // 锁内定格 marks：精确对应本次扫描到的表状态（锁释放后的新 mark 会
        // 使 marks 再次超前，下轮报脏，不丢帧）。
        let current_marks = self.marks.load(Ordering::Relaxed);
        let mut dirty = Vec::new();
        for (devid, ver) in versions.iter() {
            if cur.versions.get(devid).is_some_and(|last| *last >= *ver) {
                continue; // 版本已追平，不脏
            }
            dirty.push(devid.clone());
            cur.versions.insert(devid.clone(), *ver);
        }
        cur.marks = current_marks;
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
                // 新设备入表（游标 last 无记录即报脏）——marks 同临界区 +1，
                // 否则 O(1) 早退会吞掉 backfill 新增；已有设备不动。
                if let std::collections::hash_map::Entry::Vacant(e) = v.entry(devid.to_string()) {
                    e.insert(0);
                    self.marks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// spawn 监督消费循环（M4）：消费循环本体在 [`Self::watch_loop`]，外层
    /// [`supervise`] 兜 task panic——recv Err 三分类（Lagged/Closed/Restart）
    /// 只覆盖消息层失败，task 自身 panic（未来临界区引入 unwrap/锁中毒等）
    /// 的 JoinHandle 无人 await、panic 不进 tracing，版本表静默冻结全部
    /// ksub 连接，症状与审查 High#2 完全相同且既有机制够不到。
    ///
    /// 返回 JoinHandle 供测试 join；进程编排层可不 join（token 级联取消即退）。
    pub fn spawn(
        self: &Arc<Self>,
        client: lux::EmbeddedClient,
        token: CancellationToken,
    ) -> JoinHandle<()> {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            let inner_svc = Arc::clone(&svc);
            let inner_client = client.clone();
            let inner_token = token.clone();
            supervise(&svc, move || {
                let svc = Arc::clone(&inner_svc);
                let client = inner_client.clone();
                let token = inner_token.clone();
                tokio::spawn(async move { svc.watch_loop(client, token).await })
            })
            .await;
        })
    }

    /// 事件消费主循环（原 spawn 内联体，M4 抽出供监督重启）：先 `KEYS rtdata:*`
    /// 铺底（存量设备注册），再 `ksubscribe("rtdata:*")` 消费 key 事件（同步建
    /// 订阅、RAII 退订——drop 即 kunsub；不能走 RESP `cmd("KSUB")`，lux 显式拦截）。
    ///
    /// 退出与重建（P7"Lagged/退订异常 → epoch+1"兑现）：token 取消或订阅
    /// Closed（lux 关闭，重建无意义）→ 退订结束；Lagged → `bump_epoch` +
    /// 重铺 KEYS 后原地继续；**其余异常 → 退订重建**——epoch+1 强制连接全量
    /// 重读、重铺 KEYS 补注册断订期间设备、退避防 tight loop 后重建订阅。
    /// 静默退出会让版本表停更，所有 ksub 连接冻结在旧值（审查 High#2）。
    async fn watch_loop(self: Arc<Self>, client: lux::EmbeddedClient, token: CancellationToken) {
        self.backfill_keys(&client).await;
        // H3 双 bump 第二跳标志：Restart 重建完成后在**新订阅建立后**再
        // bump 一次——初代迭代不 bump（启动铺底在前，无丢事件窗口）。
        let mut need_rebump = false;
        'subscribe: loop {
            let mut sub = client.ksubscribe("rtdata:*");
            // H3 bump2：护"重建窗口新建连接"——bump1 只护退订前的存量
            // 连接；窗口内（bump1 后、新订阅生效前）建立的游标 epoch 与
            // bump1 后相等且版本未被（无订阅而丢失的）事件推进，
            // dirty_since 恒空 Delta → 窗口内写永不推送。bump2 使其
            // epoch stale → Full 全量重读。守门单测
            // `watch_restart_double_bump_covers_midwindow_cursors`。
            if need_rebump {
                self.bump_epoch();
            }
            // 消费式置位（写在 if 后防死赋值）：本轮起恒 bump2——初代迭代
            // 的 false 来自下方初值，启动铺底在前无丢事件窗口。
            need_rebump = true;
            loop {
                tokio::select! {
                    _ = token.cancelled() => break 'subscribe,
                    r = sub.recv() => match r {
                        Ok(msg) => {
                            if matches!(msg.kind, lux::EmbeddedMessageKind::KeyEvent) {
                                if let Some(devid) = msg.channel.strip_prefix("rtdata:") {
                                    // payload=b"del" 也 mark（P9）：版本 bump 后
                                    // 消费方 HGETALL 空 → diff_scope 自然出 removed。
                                    self.mark(devid);
                                }
                            }
                        }
                        Err(e) => match classify_failure(&e) {
                            SubFailureAction::ContinueLagged(skipped) => {
                                warn!(
                                    "[rtdataWatch] 订阅滞后丢 {skipped} 事件，epoch+1 全量重同步并重铺 KEYS"
                                );
                                self.bump_epoch();
                                self.backfill_keys(&client).await;
                            }
                            SubFailureAction::Exit => {
                                warn!("[rtdataWatch] 订阅已关闭，watcher 退出");
                                break 'subscribe;
                            }
                            SubFailureAction::Restart => {
                                warn!(
                                    "[rtdataWatch] 订阅异常，退订重建（epoch+1 + 重铺 KEYS + 退避）: {e}"
                                );
                                break; // 内层退出 → 外层重建
                            }
                        },
                    }
                }
            }
            // 异常重建序（H3 双 bump）：先退订（RAII drop → kunsub，先于
            // 重建新订阅），bump1 让持有旧 epoch 的存量连接全量重读，重铺
            // KEYS 补注册断订期间的设备，退避防异常风暴 tight loop，然后
            // 外层重建 ksubscribe——循环顶部的 need_rebump（已置 true）在
            // 新订阅建立后触发 bump2，护重建窗口内新建的连接。
            drop(sub);
            self.bump_epoch();
            self.backfill_keys(&client).await;
            tokio::time::sleep(RESTART_BACKOFF).await;
        }
        // 'subscribe 跳出时 sub 随作用域结束 drop（RAII kunsub），先于
        // client 离场。
    }
}

/// Restart/监督重启退避（防异常风暴 tight loop）；测试档缩短使监督单测的
/// 两代 panic 注入不拖慢测试（2×50ms）。
#[cfg(not(test))]
const RESTART_BACKOFF: Duration = Duration::from_secs(1);
#[cfg(test)]
const RESTART_BACKOFF: Duration = Duration::from_millis(50);

/// 监督循环（M4）：工厂产内层任务；`Ok(())`（token 取消/订阅 Closed）正常
/// 收尾；`JoinErr`（panic）→ error 日志 + `bump_epoch` 全量重同步 + 退避 +
/// 重产——版本表静默冻结是审查 High#2 的等价症状，三分类只覆盖 recv Err
/// 不覆盖 task panic。单测 `watch_supervise_restarts_on_panic` 以 panic
/// 工厂注入锚定。
async fn supervise<F>(svc: &RtdataWatchService, mut factory: F)
where
    F: FnMut() -> JoinHandle<()>,
{
    loop {
        match factory().await {
            Ok(()) => break,
            Err(e) => {
                error!("[rtdataWatch] watcher task panic: {e}; epoch+1 全量重同步 + 退避重启");
                svc.bump_epoch();
                tokio::time::sleep(RESTART_BACKOFF).await;
            }
        }
    }
}

/// 订阅 recv 失败的处置分类（P7"退订异常 → epoch+1"的裁决点）。
#[derive(Debug, PartialEq, Eq)]
enum SubFailureAction {
    /// Lagged(n)：丢 n 事件，通道仍活——原地继续（warn + epoch+1 + 重铺 KEYS）。
    ContinueLagged(u64),
    /// Closed：lux 侧订阅已关闭（进程/服务退出）——watcher 退出，重建无意义。
    Exit,
    /// 其余异常：退订重建（epoch+1 + 重铺 KEYS + 退避后重建 ksubscribe）。
    Restart,
}

/// 失败分类纯函数（单测锚定 `watch_failure_classification`）：Lagged 带丢事件
/// 数原地继续；Closed 退出；其他一律 Restart——不得静默退出冻结版本表。
fn classify_failure(e: &lux::LuxError) -> SubFailureAction {
    match e {
        lux::LuxError::SubscriptionLagged(skipped) => SubFailureAction::ContinueLagged(*skipped),
        lux::LuxError::SubscriptionClosed => SubFailureAction::Exit,
        _ => SubFailureAction::Restart,
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

    /// dirty_since 增量语义:落后才报脏;查询推进游标(下次无新事件
    /// 则无脏);新设备(游标无记录)报脏。
    #[test]
    fn watch_dirty_delta() {
        let svc = RtdataWatchService::new();
        // 游标先于事件建立(真实序:连接建游标在前,事件在后)。
        let mut cur = svc.cursor();
        svc.mark("a");
        svc.mark("a"); // 同设备两次事件,仍只报一个脏设备(集合语义)

        match svc.dirty_since(&mut cur) {
            Dirty::Delta(v) => assert_eq!(v, vec!["a".to_string()]),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }

        // 游标已被 dirty_since 推进 → 无新事件时再查无脏
        match svc.dirty_since(&mut cur) {
            Dirty::Delta(v) => assert!(v.is_empty(), "无新脏设备,却报了 {v:?}"),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }

        // 新设备(游标无记录且版本表有)→ Delta 含之
        svc.mark("b");
        match svc.dirty_since(&mut cur) {
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

        let mut cur = WatchCursor {
            versions: HashMap::from([("a".to_string(), svc.version("a").unwrap())]),
            epoch: e0,
            marks: 0,
        };
        let d = svc.dirty_since(&mut cur);
        assert!(matches!(d, Dirty::Full), "epoch 变更强制 Full");
    }

    /// WatchCursor 游标族(P7 + marks O(1) 早退):cursor() 三元组快照;
    /// dirty_since 快路径(marks 相等)后新 mark 不得被早退吞掉——marks 与表
    /// 状态在锁内对齐是早退正确性的前提。
    #[test]
    fn watch_cursor_marks_alignment() {
        let svc = RtdataWatchService::new();
        // 游标先建立(空表),事件在后——游标建立后的变更才在报脏范围内。
        let mut cur = svc.cursor();
        svc.mark("a");
        match svc.dirty_since(&mut cur) {
            Dirty::Delta(v) => assert_eq!(v, vec!["a".to_string()]),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }
        // 无新事件:空 Delta(marks 相等 O(1) 早退路径)。
        match svc.dirty_since(&mut cur) {
            Dirty::Delta(v) => assert!(v.is_empty(), "无新事件应无脏: {v:?}"),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }
        // 快路径空查询之后的新 mark:marks 已推进,不得被早退吞掉。
        svc.mark("a");
        match svc.dirty_since(&mut cur) {
            Dirty::Delta(v) => assert_eq!(v, vec!["a".to_string()], "新事件须报脏: {v:?}"),
            Dirty::Full => panic!("epoch 未变应为 Delta"),
        }
    }

    /// 新服务 epoch 从 0 起步(未发生 Lagged/重同步时恒 0)。
    #[test]
    fn watch_epoch_zero_when_unchanged() {
        let svc = RtdataWatchService::new();
        assert_eq!(svc.epoch(), 0, "起步 epoch=0");
    }

    /// snapshot_versions:返回版本表拷贝(ksub 连接游标基线/Resync 重建用);
    /// 与表后续变更隔离(拷贝语义)。
    #[test]
    fn watch_snapshot_versions_is_table_copy() {
        let svc = RtdataWatchService::new();
        assert!(svc.snapshot_versions().is_empty(), "空表快照为空 map");
        svc.mark("a");
        svc.mark("a");
        svc.mark("b");
        let snap = svc.snapshot_versions();
        assert_eq!(
            snap,
            HashMap::from([("a".to_string(), 2), ("b".to_string(), 1)]),
            "快照 = 当前版本表"
        );
        // 拷贝语义:快照后 mark 不影响已取快照。
        svc.mark("a");
        assert_eq!(snap["a"], 2, "快照与表后续变更隔离");
    }

    /// 订阅 recv 失败的处置分类(P7"退订异常→epoch+1"裁决点):Lagged 原地继续
    /// (带丢事件数);Closed(lux 关闭,重启无意义)退出;**其余异常必须 Restart**
    /// ——静默退出会让版本表停更,所有 ksub 连接冻结在旧值(审查 High#2)。
    #[test]
    fn watch_failure_classification() {
        use lux::LuxError;
        // Lagged(7) → 原地继续,丢事件数随变体带回
        assert_eq!(
            classify_failure(&LuxError::SubscriptionLagged(7)),
            SubFailureAction::ContinueLagged(7)
        );
        // Closed → 退出(lux 侧订阅已关,重建无意义)
        assert_eq!(
            classify_failure(&LuxError::SubscriptionClosed),
            SubFailureAction::Exit
        );
        // 其余异常 → 退订重建(epoch+1 + 重铺 KEYS + 退避)
        assert_eq!(
            classify_failure(&LuxError::Protocol("io".into())),
            SubFailureAction::Restart
        );
        assert_eq!(
            classify_failure(&LuxError::Command("x".into())),
            SubFailureAction::Restart
        );
    }

    /// H3 双 bump 不变量（守门测试；重启编排 e2e 红不可达——循 Phase 2
    /// 裁决落单元层）：Restart 重建序 = bump1（护退订窗口存量连接）→
    /// [重建窗口：退避/重铺，无订阅，事件丢失且版本不动] → 重建 ksubscribe →
    /// bump2（护窗口内新建连接）。本测锁死：**窗口内建立的游标在 bump2 后
    /// 必须判 Full**——单 bump 下其 epoch 与当前相等、版本未推进，
    /// dirty_since 恒空 Delta，窗口内的写永不推送（静默旧值）。
    #[test]
    fn watch_restart_double_bump_covers_midwindow_cursors() {
        let svc = RtdataWatchService::new();
        svc.mark("devA");
        // bump1：Restart 路径退订后立即执行
        svc.bump_epoch();
        // 重建窗口内新建连接取游标（模拟 SSE 连接在 1s 退避期间接入）
        let mut mid = svc.cursor();
        assert_eq!(
            mid.epoch(),
            svc.epoch(),
            "窗口内游标 epoch 与 bump1 后当前值相等——单 bump 的盲区所在"
        );
        // 对照（论证双 bump 必要性）：若到此为止（单 bump），无新事件的
        // 窗口内游标查询为空 Delta——窗口内的写被静默吞掉。
        match svc.dirty_since(&mut mid) {
            Dirty::Delta(v) => assert!(v.is_empty(), "单 bump 下窗口游标本就追平: {v:?}"),
            Dirty::Full => panic!("epoch 相等不应 Full"),
        }
        // 重建订阅完成后 bump2：窗口内游标必须被强制 Full 全量重读
        let mut mid2 = svc.cursor();
        svc.bump_epoch();
        assert!(
            matches!(svc.dirty_since(&mut mid2), Dirty::Full),
            "双 bump 后窗口内游标必须 Full（否则重建窗口丢帧，审查 H3）"
        );
        // bump 前存量连接同样被覆盖（bump1/bump2 均使其 stale）
        let mut old = WatchCursor {
            versions: HashMap::from([("devA".to_string(), svc.version("devA").unwrap())]),
            epoch: 0,
            marks: 0,
        };
        assert!(
            matches!(svc.dirty_since(&mut old), Dirty::Full),
            "bump 前存量连接游标必须 Full"
        );
    }

    /// M4 监督不变量：内层任务 panic → bump_epoch + 退避 + 重产，正常返回
    /// （token 取消/Closed）→ 退出不再 bump。panic 工厂注入（前两代 panic、
    /// 第三代正常收尾）——epoch 恰 +2、工厂恰被调 3 次；退避走测试档 50ms。
    #[tokio::test]
    async fn watch_supervise_restarts_on_panic() {
        let svc = RtdataWatchService::new();
        let e0 = svc.epoch();
        let calls = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&calls);
        supervise(&svc, move || {
            let i = c.fetch_add(1, Ordering::SeqCst);
            if i < 2 {
                tokio::spawn(async move { panic!("监督测试注入 panic #{i}") })
            } else {
                tokio::spawn(async {})
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3, "两 panic 后第三代正常收尾");
        assert_eq!(svc.epoch(), e0 + 2, "每次 panic 恰一次 bump（全量重同步）");
    }
}
