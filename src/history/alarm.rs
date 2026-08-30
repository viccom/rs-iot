//! 历史丢弃水位内建告警（Task 18，P6 收官）+ 引擎僵尸告警（Task A/#3，
//! writer.rs 遗留 TODO 收口）：不依赖现场脚本的系统级告警。
//!
//! 2026-08-29 压测暴露 92% 丢弃期间无任何主动告警——Q12 告警闭环（lua
//! `ctx:fire_event` → lux alarms → REST/SSE）存在但触发全靠现场脚本。本模块
//! 用 [`crate::history::rows::HistoryStats`] 的原子计数做内建判定：
//!
//! - **判定**：5s 周期快照 `dropped`/`sent` 累计值，相邻差分得本窗口增量；
//!   **双条件**（严格大于，均须满足）——速率 `delta_dropped > 10/s` 且窗口占比
//!   `delta_dropped/(delta_dropped+delta_sent) > 1%`。单速率阈值在部署规模间
//!   两头失准（小部署 50/100 pts/s 丢一半也够不着 100/s 门槛；100K 场景
//!   0.1% 噪声反而触发）；单占比在小流量上分子噪声抖动（偶发 5/50=10%）。
//!   双条件 = 绝对量下限 + 相对量显著性，两面兼顾。
//! - **动作**：`tracing::warn!`（非 error——error 级属僵尸告警）+
//!   [`crate::db::alarms::fire`]（level=warn，source 由宿主填，
//!   对齐 lua fire_event 惯例）+ EventBus SSE Fire 帧。
//! - **僵尸告警**（[`ClosedAlarmWatch`]）：周期读 `closed_latched` 锁存位，
//!   置位即 fire **error 级** `rsiot.history_closed`（引擎已僵尸、数据将全
//!   丢，比 drop 的 warn 更高）——writer 是独立 std 线程无 lux 句柄不可达
//!   fire，读侧周期任务收口该上行。
//! - **去抖/恢复**：只 fire 不自动 clear（工业惯例——告警消除需人知悉，
//!   REST DELETE 人工确认）；"同一活动期不重复 fire"以 **lux 键存在性**为
//!   活动期判据：键在（未清除）恒不刷（count 不递增、SSE 不重发）；人工
//!   清除即活动期结束，若故障仍在（超阈/锁存位仍置位）下一拍再触发
//!   （count 重开）——确认后仍故障则重新告警的运维闭环。closed_latched
//!   一次置位永不复位，人工清除后会持续重触发——**有意为之**：引擎确实
//!   还僵尸着，持续故障持续可见（重启恢复才是根治）。
//! - **挂载**：独立 spawn 任务（lib.rs `run_app_with_shutdown`，照
//!   `log_cleanup::spawn_cleanup_task` 先例）。不进 pipeline worker 上下文
//!   （N worker 各自判定需选举，且周期判定不应进数据面热循环）；不借
//!   selfMonitor 周期（那是用户可调的上报节拍 1-3600s，告警节拍是基础设施
//!   关注点，解耦）。engine=None（feature off/init_failed）时 `dropped`
//!   恒 0 判定无意义，spawn 点直接不启动（最小面）。
//! - 周期与阈值先常量后配置（计划 Task 18 约定；Task B/#4 阈值配置化）；
//!   lux 故障 fail-soft：去抖查询/fire 失败仅 warn 本拍跳过，下一拍重试。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use lux::EmbeddedClient;
use tokio_util::sync::CancellationToken;

use crate::event_bus::{AlarmAction, EventBus, EventPayload};
use crate::history::rows::HistoryStats;

/// 告警键（lux alarms hash field；source 域前缀防与脚本自定义键撞名，
/// ≤128 字符同 lua validate_key 口径）。
const ALARM_KEY: &str = "rsiot.history_drop";
/// 级别：warn（error 级是 [`ClosedAlarmWatch`] 僵尸告警的档位）。
const ALARM_LEVEL: &str = "warn";
/// 来源（宿主填，脚本不可伪造语义同 lua 面 source=inst_id）：网关自身，
/// 对齐 selfMonitor devid=`rsiot` 的自监控身份。
const ALARM_SOURCE: &str = "rsiot";
/// 判定周期（秒）：对齐 selfMonitor 默认上报周期 5s——足够平滑单批脉冲、
/// 检测延迟 ≤2 拍（10s），与平台侧图表粒度同拍。
const PERIOD_SECS: u64 = 5;
/// 速率下限（行/s，严格大于）：绝对量门槛——测试环境 10/s 级丢弃也该知道。
const RATE_FLOOR_PER_SEC: u64 = 10;
/// 窗口占比阈值（严格大于）：相对量门槛——介于 sysstatus 三态 degrading
/// （>0.5%，T17）与 dropping（>5%）之间的"值得推人告知"线；累计口径归
/// T17，本处用窗口口径（恢复后不因陈旧累计误判新活动）。
const RATIO_THRESHOLD: f64 = 0.01;

/// 双条件判定（纯函数，period_secs 供测试注入语义化速率门槛）：
/// 速率 `delta_dropped > RATE_FLOOR_PER_SEC × period_secs`（整数域严格大于）
/// **且**窗口占比 `delta_dropped / (delta_dropped + delta_sent) >
/// RATIO_THRESHOLD`。
fn exceeds(delta_dropped: u64, delta_sent: u64, period_secs: u64) -> bool {
    if period_secs == 0 {
        return false;
    }
    // 速率下限（整数域严格大于，无浮点边界歧义）。
    if delta_dropped <= RATE_FLOOR_PER_SEC.saturating_mul(period_secs) {
        return false;
    }
    // 窗口占比（严格大于）：此处 delta_dropped ≥ 51 且 total ≥ delta_dropped，
    // 分母恒非零。
    let total = delta_dropped + delta_sent;
    delta_dropped as f64 / total as f64 > RATIO_THRESHOLD
}

/// 丢弃水位看护：持有上一拍累计快照，`tick` 差分判定。
///
/// 单实例由 spawn 任务独占（无并发面）；测试直接构造驱动多拍。
#[derive(Debug, Default)]
pub(crate) struct DropAlarmWatch {
    /// 上一拍 (dropped, sent) 累计快照；None = 首拍建基线不判定。
    prev: Option<(u64, u64)>,
}

impl DropAlarmWatch {
    /// 一拍判定：快照差分 → 超阈且非活动期 → warn 日志 + lux fire + SSE。
    ///
    /// 首拍仅建基线（进程刚起无历史增量可判，也不对"存在累计值"本身告警
    /// ——计数是本进程生命周期口径）；lux 读写失败 fail-soft 跳过本拍。
    pub(crate) async fn tick(
        &mut self,
        stats: &HistoryStats,
        lux: &EmbeddedClient,
        bus: &EventBus,
    ) {
        let dropped = stats.dropped.load(Ordering::Relaxed);
        let sent = stats.sent.load(Ordering::Relaxed);
        let Some((prev_dropped, prev_sent)) = self.prev.replace((dropped, sent)) else {
            return; // 首拍建基线
        };
        // 计数器进程内单调；saturating 防御性回卷（外部不可能，代价为零）。
        let delta_dropped = dropped.saturating_sub(prev_dropped);
        let delta_sent = sent.saturating_sub(prev_sent);
        if !exceeds(delta_dropped, delta_sent, PERIOD_SECS) {
            return;
        }
        // 去抖：活动期判据 = lux 键存在性。键在（上次 fire 未被人工清除）
        // 恒不刷——count 不递增、SSE 不重发；坏 JSON 记录 get 视为不存在，
        // 走 fire 覆盖修复。lux 读失败 fail-soft 跳过本拍（下拍重试）。
        match crate::db::alarms::get(lux, ALARM_KEY).await {
            Ok(Some(_)) => return, // 同活动期去抖
            Err(e) => {
                tracing::warn!("[history] 丢弃水位告警去抖查询失败(本拍跳过): {e}");
                return;
            }
            Ok(None) => {} // 无活动告警 → 开新活动期
        }
        let total = delta_dropped + delta_sent;
        let rate = delta_dropped as f64 / PERIOD_SECS as f64;
        let pct = delta_dropped as f64 / total as f64 * 100.0;
        let message = format!(
            "历史归档丢弃超阈：近{PERIOD_SECS}s丢弃{delta_dropped}行(约{rate:.0}行/s,占比{pct:.1}%),\
             归档channel满即丢(fail-soft)持续——请检查历史写入吞吐/白名单规模"
        );
        let ts = crate::event_bus::now_iso8601();
        match crate::db::alarms::fire(lux, ALARM_KEY, ALARM_LEVEL, ALARM_SOURCE, &message, &ts)
            .await
        {
            Ok(crate::db::alarms::FireOutcome::Stored(_)) => {
                // warn 级（error 留给 T3 closed_latched 引擎僵尸）；key 级审计
                // 照 lua fire_event 惯例，判定数值是排障上下文随行。
                tracing::warn!(
                    "[history] 丢弃水位告警已触发 key={ALARM_KEY} level={ALARM_LEVEL} \
                     by={ALARM_SOURCE}: 近{PERIOD_SECS}s丢{delta_dropped}行/约{rate:.0}行/s/占比{pct:.1}%"
                );
                bus.publish(EventPayload::Alarm {
                    action: AlarmAction::Fire,
                    key: ALARM_KEY.to_string(),
                    level: ALARM_LEVEL.to_string(),
                    source: ALARM_SOURCE.to_string(),
                    message,
                    ts,
                });
            }
            Ok(crate::db::alarms::FireOutcome::Capped) => {
                // 基数上限拒新键（同 lua 面护栏语义）：仅 warn，下拍重试。
                tracing::warn!(
                    "[history] 丢弃水位告警拒存:活跃告警达基数上限{}(运维清除后恢复)",
                    crate::db::alarms::ALARM_MAX_KEYS
                );
            }
            Err(e) => tracing::warn!("[history] 丢弃水位告警fire失败(本拍跳过): {e}"),
        }
    }
}

/// closed_latched 僵尸告警键（Task A/#3，writer.rs 遗留 TODO 收口）。
const CLOSED_ALARM_KEY: &str = "rsiot.history_closed";
/// 级别：error（引擎已僵尸、数据将全丢——比 drop 告警的 warn 更高）。
const CLOSED_ALARM_LEVEL: &str = "error";

/// 引擎僵尸看护（无状态）：读 [`HistoryStats::closed_latched`] 锁存位，
/// 置位即 fire error 级系统告警（去抖语义同 [`DropAlarmWatch`]）。
pub(crate) struct ClosedAlarmWatch;

impl ClosedAlarmWatch {
    /// 一拍判定：锁存位置位且非活动期 → error 日志 + lux fire + SSE。
    ///
    /// closed_latched 一次置位永不复位（锁存语义），无差分基线可言——
    /// 每拍直接读位；lux 读写失败 fail-soft 跳过本拍。**重触发语义**：
    /// 人工清除后锁存位仍置位 → 下一拍重新 fire（新活动期 count 重开）
    /// ——持续故障持续可见（引擎确实还僵尸着，静默即失明）；与 drop
    /// 告警"确认后仍故障则重新告警"的运维闭环同构。
    pub(crate) async fn tick(&self, stats: &HistoryStats, lux: &EmbeddedClient, bus: &EventBus) {
        if !stats.closed_latched.load(Ordering::SeqCst) {
            return; // 引擎健在（Closed 桶从未首增）
        }
        // 去抖：活动期判据 = lux 键存在性（同 DropAlarmWatch 惯例）——
        // 键在（未清除）恒不刷；坏 JSON 记录 get 视为不存在，走 fire 覆盖
        // 修复；lux 读失败 fail-soft 跳过本拍（下拍重试）。
        match crate::db::alarms::get(lux, CLOSED_ALARM_KEY).await {
            Ok(Some(_)) => return, // 同活动期去抖
            Err(e) => {
                tracing::warn!("[history] 僵尸告警去抖查询失败(本拍跳过): {e}");
                return;
            }
            Ok(None) => {} // 无活动告警 → 开新活动期
        }
        let message = "历史引擎已关闭（僵尸态），归档停止，需重启恢复";
        let ts = crate::event_bus::now_iso8601();
        match crate::db::alarms::fire(
            lux,
            CLOSED_ALARM_KEY,
            CLOSED_ALARM_LEVEL,
            ALARM_SOURCE,
            message,
            &ts,
        )
        .await
        {
            Ok(crate::db::alarms::FireOutcome::Stored(_)) => {
                // error 级（引擎已僵尸、数据将全丢——比 drop 告警的 warn 高）；
                // 与 writer 首增时的一次性 error! 呼应：这里承载周期可见性。
                tracing::error!(
                    "[history] 引擎僵尸告警已触发 key={CLOSED_ALARM_KEY} \
                     level={CLOSED_ALARM_LEVEL} by={ALARM_SOURCE}: {message}"
                );
                bus.publish(EventPayload::Alarm {
                    action: AlarmAction::Fire,
                    key: CLOSED_ALARM_KEY.to_string(),
                    level: CLOSED_ALARM_LEVEL.to_string(),
                    source: ALARM_SOURCE.to_string(),
                    message: message.to_string(),
                    ts,
                });
            }
            Ok(crate::db::alarms::FireOutcome::Capped) => {
                // 基数上限拒新键（同 lua 面护栏语义）：仅 warn，下拍重试。
                tracing::warn!(
                    "[history] 引擎僵尸告警拒存:活跃告警达基数上限{}(运维清除后恢复)",
                    crate::db::alarms::ALARM_MAX_KEYS
                );
            }
            Err(e) => tracing::warn!("[history] 引擎僵尸告警fire失败(本拍跳过): {e}"),
        }
    }
}

/// 常驻判定任务（lib.rs 引擎在场时启动；照 `log_cleanup::spawn_cleanup_task`
/// 先例：child token 随进程退、interval Skip 漏拍不补偿——漏拍只影响该拍
/// 增量并入下一拍窗口，判定语义不变）。
pub(crate) fn spawn_drop_alarm_task(
    stats: Arc<HistoryStats>,
    lux: EmbeddedClient,
    bus: EventBus,
    shutdown: CancellationToken,
) {
    let token = shutdown.child_token();
    tokio::spawn(async move {
        let mut watch = DropAlarmWatch::default();
        let closed = ClosedAlarmWatch;
        let mut interval = tokio::time::interval(Duration::from_secs(PERIOD_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                // 同拍串行两看护（5s 周期/挂载条件均同，合一任务省一份常驻
                // 开销；drop 的差分基线与 closed 的锁存读互不干扰）。
                _ = interval.tick() => {
                    watch.tick(&stats, &lux, &bus).await;
                    closed.tick(&stats, &lux, &bus).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::alarms;
    use crate::event_bus::EventPayload;

    /// 唯一临时 data_dir（tag 区分用例，防并行测试互扰；照 db/alarms::tests 模式）。
    async fn test_client(tag: &str) -> EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("hist-alarm-{tag}-{}", std::process::id()))
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

    /// 带一个 SSE 订阅者的总线（捕获 Fire 帧断言 publish 面）。
    fn bus_with_sub() -> (EventBus, tokio::sync::broadcast::Receiver<EventPayload>) {
        let bus = EventBus::new(16);
        let rx = bus.subscribe();
        (bus, rx)
    }

    /// 累加 dropped/sent（模拟 fan_out 满即丢与正常发送在拍间的增量）。
    fn bump(stats: &HistoryStats, dropped: u64, sent: u64) {
        stats.dropped.fetch_add(dropped, Ordering::Relaxed);
        stats.sent.fetch_add(sent, Ordering::Relaxed);
    }

    // ---- 1. 判定纯函数：双条件边界（严格大于语义钉死） ----

    /// 双条件组合：双方均过（600/10000≈5.7% 且 120/s）→ true；刚过双门槛
    /// 的最小组合（51 行/5s=10.2/s 且 51/5051≈1.01%）→ true；任一不过
    /// （速率边界恰 50=10×5 / 只过速率 60/10060≈0.6% / 只过占比 30/1000=3%
    /// 但 6/s / 占比恰 1% 边界 100/10000）→ false；零增量与零周期 → false。
    #[test]
    fn exceeds_dual_condition_boundaries() {
        // 双双越过
        assert!(exceeds(600, 10_000, 5), "120/s 且 5.66% 应告警");
        // 刚过双门槛的最小组合（钉死两侧严格大于）
        assert!(exceeds(51, 5_000, 5), "10.2/s 且 1.01% 应告警");
        // 速率边界：恰 50 = 10×5 严格大于不满足
        assert!(!exceeds(50, 0, 5), "恰 10/s 不告警（严格大于）");
        // 只过速率：占比 0.597% < 1%
        assert!(!exceeds(60, 10_000, 5), "速率过而占比不过不告警");
        // 只过占比：30/1000=3% 但速率 6/s
        assert!(!exceeds(30, 970, 5), "占比过而速率不过不告警");
        // 占比恰 1% 边界（100/10000）：严格大于不满足
        assert!(!exceeds(100, 9_900, 5), "恰 1% 不告警（严格大于）");
        // 零增量 / 零周期
        assert!(!exceeds(0, 0, 5));
        assert!(!exceeds(600, 10_000, 0), "零周期不判定");
    }

    // ---- 2. 红测2：低于阈值 / 首拍基线 / 计数回卷防御 → 无告警 ----

    /// 低于阈值不告警：首拍只建基线（即便累计值巨大也不对存量告警——
    /// 计数是本进程口径）；次拍增量 10 行（2/s）远低于门槛 → alarms 空；
    /// 纯 sent 增量、以及防御性回卷（快照 > 现值 → 增量饱和为 0）同不告警。
    #[tokio::test]
    async fn tick_below_threshold_and_baseline_no_alarm() {
        let c = test_client("below").await;
        let (bus, _rx) = bus_with_sub();
        let stats = HistoryStats::default();
        let mut w = DropAlarmWatch::default();

        // 存量巨大（1M 丢弃）——首拍建基线，不对存量告警
        bump(&stats, 1_000_000, 100_000);
        w.tick(&stats, &c, &bus).await;
        assert!(
            alarms::list(&c).await.unwrap().is_empty(),
            "首拍建基线不得告警（存量非本拍增量）"
        );

        // 次拍增量 10 行（2/s）+10k sent → 低于双门槛
        bump(&stats, 10, 10_000);
        w.tick(&stats, &c, &bus).await;
        assert!(
            alarms::list(&c).await.unwrap().is_empty(),
            "低于阈值不得告警"
        );

        // 纯 sent 增量（dropped 零增量）不告警
        bump(&stats, 0, 5_000);
        w.tick(&stats, &c, &bus).await;
        assert!(alarms::list(&c).await.unwrap().is_empty());

        // 防御性回卷：快照高于现值（现值 dropped≈1_000_010/sent≈115_000）
        // → 增量饱和 0，不告警不 panic（夹具取高于现值的快照，钉 saturating 路径）
        let mut w2 = DropAlarmWatch {
            prev: Some((2_000_000, 300_000)),
        };
        w2.tick(&stats, &c, &bus).await;
        assert!(alarms::list(&c).await.unwrap().is_empty());
    }

    // ---- 3. 红测1：超阈 fire + 去抖（同活动期不重复） ----

    /// 超阈一拍：alarms 表出现系统告警（key/level/source/count=1/中文文案）
    /// + SSE 恰一条 Fire 帧；同活动期（键未清除）下一拍仍超阈**不重复
    /// fire**（count 不递增、清单不增、SSE 不再发）。
    #[tokio::test]
    async fn tick_fires_alarm_and_debounces_within_activity() {
        let c = test_client("fire").await;
        let (bus, mut rx) = bus_with_sub();
        let stats = HistoryStats::default();
        let mut w = DropAlarmWatch::default();

        // 首拍基线
        w.tick(&stats, &c, &bus).await;

        // 超阈拍：600 行/5s（120/s）+10k sent（5.66%）
        bump(&stats, 600, 10_000);
        w.tick(&stats, &c, &bus).await;

        let items = alarms::list(&c).await.unwrap();
        assert_eq!(items.len(), 1, "超阈拍应产生一条活跃告警");
        let it = &items[0];
        assert_eq!(it.key, ALARM_KEY, "告警键必须是系统内建键");
        assert_eq!(it.level, "warn", "水位告警是 warn 级（error 留给僵尸态）");
        assert_eq!(it.source, ALARM_SOURCE, "source 由宿主填（网关自身）");
        assert_eq!(it.count, 1, "活动期首 fire count=1");
        assert!(
            it.message.contains("丢弃"),
            "告警文案应携带丢弃语义（中文）：{}",
            it.message
        );

        // SSE：恰一条 Fire 帧，字段对齐
        let frame = rx
            .try_recv()
            .expect("超阈 fire 应 publish 一条 SSE Fire 帧");
        match frame {
            EventPayload::Alarm {
                action,
                key,
                level,
                source,
                message,
                ts,
            } => {
                assert!(matches!(action, AlarmAction::Fire), "应为 Fire 动作");
                assert_eq!(key, ALARM_KEY);
                assert_eq!(level, "warn");
                assert_eq!(source, ALARM_SOURCE);
                assert_eq!(message, it.message, "SSE 文案与 lux 落库文案一致");
                assert!(!ts.is_empty());
            }
            other => panic!("应为 Alarm 帧，实得 {other:?}"),
        }

        // 去抖：同活动期（键未清除）再超阈一拍——不重复 fire
        bump(&stats, 600, 10_000);
        w.tick(&stats, &c, &bus).await;
        let items = alarms::list(&c).await.unwrap();
        assert_eq!(items.len(), 1, "同活动期不得新增告警条目");
        assert_eq!(items[0].count, 1, "同活动期去抖——count 不递增（不刷）");
        assert!(rx.try_recv().is_err(), "同活动期去抖——SSE 不重发 Fire 帧");
    }

    // ---- 4. 恢复语义：人工清除 = 活动期结束；仍超阈则再触发 ----

    /// 只 fire 不自动 clear（无任何"回落自动清除"路径——人工确认知悉）；
    /// 人工清除后若丢弃仍超阈，下一拍重新 fire（count 重开=1，新活动期），
    /// SSE 再发一条 Fire 帧——"确认后仍故障则重新告警"的运维闭环。
    #[tokio::test]
    async fn rearm_after_manual_clear() {
        let c = test_client("rearm").await;
        let (bus, mut rx) = bus_with_sub();
        let stats = HistoryStats::default();
        let mut w = DropAlarmWatch::default();

        // 基线 → 超阈 fire
        w.tick(&stats, &c, &bus).await;
        bump(&stats, 600, 10_000);
        w.tick(&stats, &c, &bus).await;
        assert_eq!(alarms::list(&c).await.unwrap().len(), 1);
        let _ = rx.try_recv(); // 消费首帧

        // 速率回落低于阈（+5 行）——不自动 clear：告警仍在（人工确认语义）
        bump(&stats, 5, 10_000);
        w.tick(&stats, &c, &bus).await;
        assert_eq!(
            alarms::list(&c).await.unwrap().len(),
            1,
            "回落不自动 clear——消除需人工确认（REST DELETE）"
        );

        // 人工清除（模拟 REST/运维）→ 活动期结束
        alarms::clear(&c, ALARM_KEY).await.unwrap();
        assert!(alarms::list(&c).await.unwrap().is_empty());

        // 仍超阈的下一拍 → 重新 fire（新活动期 count 重开=1）+ SSE 再发
        bump(&stats, 600, 10_000);
        w.tick(&stats, &c, &bus).await;
        let items = alarms::list(&c).await.unwrap();
        assert_eq!(items.len(), 1, "清除后仍超阈应重新告警");
        assert_eq!(items[0].count, 1, "新活动期 count 重开（fire 语义=新键）");
        let frame = rx.try_recv().expect("重新 fire 应再发 SSE Fire 帧");
        assert!(matches!(
            frame,
            EventPayload::Alarm {
                action: AlarmAction::Fire,
                ..
            }
        ));
    }

    // ---- 5. closed_latched 引擎僵尸告警（Task A/#3：writer.rs TODO 收口） ----

    /// 未置位无告警；置位 → error 级系统告警（key/level/source/count=1/
    /// 中文文案含"僵尸"与"重启"）+ SSE Fire 帧；同活动期（键未清除）下一拍
    /// 不重复（count 不递增、SSE 不重发）；人工清除后锁存位仍置位 → 下一拍
    /// 重新 fire（新活动期 count 重开=1——closed_latched 永不复位，持续
    /// 故障持续可见）。
    #[tokio::test]
    async fn closed_latched_fires_error_alarm_with_debounce_and_rearm() {
        let c = test_client("closed").await;
        let (bus, mut rx) = bus_with_sub();
        let stats = HistoryStats::default();
        let w = ClosedAlarmWatch;

        // 未置位：无告警（正常态不得误报）
        w.tick(&stats, &c, &bus).await;
        assert!(
            alarms::list(&c).await.unwrap().is_empty(),
            "closed_latched 未置位不得告警"
        );

        // 置位 → error 级系统告警
        stats.closed_latched.store(true, Ordering::SeqCst);
        w.tick(&stats, &c, &bus).await;
        let items = alarms::list(&c).await.unwrap();
        assert_eq!(items.len(), 1, "锁存位置位应产生一条活跃告警");
        let it = &items[0];
        assert_eq!(it.key, CLOSED_ALARM_KEY, "告警键必须是系统内建僵尸键");
        assert_eq!(
            it.level, "error",
            "引擎僵尸是 error 级（比 drop 告警的 warn 更高——数据将全丢）"
        );
        assert_eq!(it.source, "rsiot", "source 由宿主填（网关自身）");
        assert_eq!(it.count, 1, "活动期首 fire count=1");
        assert!(
            it.message.contains("僵尸") && it.message.contains("重启"),
            "告警文案应携带僵尸态与重启指引（中文）：{}",
            it.message
        );

        // SSE：恰一条 Fire 帧，字段对齐
        let frame = rx
            .try_recv()
            .expect("僵尸 fire 应 publish 一条 SSE Fire 帧");
        match frame {
            EventPayload::Alarm {
                action,
                key,
                level,
                source,
                message,
                ts,
            } => {
                assert!(matches!(action, AlarmAction::Fire), "应为 Fire 动作");
                assert_eq!(key, CLOSED_ALARM_KEY);
                assert_eq!(level, "error");
                assert_eq!(source, "rsiot");
                assert_eq!(message, it.message, "SSE 文案与 lux 落库文案一致");
                assert!(!ts.is_empty());
            }
            other => panic!("应为 Alarm 帧，实得 {other:?}"),
        }

        // 去抖：同活动期（键未清除）下一拍仍置位——不重复 fire
        w.tick(&stats, &c, &bus).await;
        let items = alarms::list(&c).await.unwrap();
        assert_eq!(items.len(), 1, "同活动期不得新增告警条目");
        assert_eq!(items[0].count, 1, "同活动期去抖——count 不递增（不刷）");
        assert!(rx.try_recv().is_err(), "同活动期去抖——SSE 不重发 Fire 帧");

        // 人工清除 → 活动期结束；锁存位仍置位 → 下一拍重新 fire（count 重开）
        alarms::clear(&c, CLOSED_ALARM_KEY).await.unwrap();
        assert!(alarms::list(&c).await.unwrap().is_empty());
        w.tick(&stats, &c, &bus).await;
        let items = alarms::list(&c).await.unwrap();
        assert_eq!(items.len(), 1, "清除后仍置位应重新告警（引擎确实还僵尸着）");
        assert_eq!(items[0].count, 1, "新活动期 count 重开（fire 语义=新键）");
        assert_eq!(items[0].level, "error");
        let frame = rx.try_recv().expect("重新 fire 应再发 SSE Fire 帧");
        assert!(matches!(
            frame,
            EventPayload::Alarm {
                action: AlarmAction::Fire,
                ..
            }
        ));
    }
}
