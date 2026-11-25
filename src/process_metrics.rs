//! 进程级指标（M4）：核心框架咽喉部位的 AtomicU64 计数器，REST /sysstatus 直读。
//!
//! 不落 lux（热路径无 await；进程级语义是本进程实时健康，重启归零合理）。
//! 注入 HostContext（所有 task 共享）+ AppState（REST 直读）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// 进程级指标集合（Instant/Atomic 均实现 Debug，故 derive Debug 通过 missing_debug_implementations）。
#[derive(Debug)]
pub struct ProcessMetrics {
    /// 进程启动时刻（REST 算 uptime）。
    pub started: Instant,
    // 控制下行（CtlRegistry）
    /// 控制命令投递尝试次数（含成功/drop/not_found）。
    pub ctl_send_count: AtomicU64,
    /// 控制命令因 channel 满被丢弃次数。
    pub ctl_dropped_count: AtomicU64,
    /// 控制命令未找到目标实例次数。
    pub ctl_not_found_count: AtomicU64,
    // 数据上行（pipeline）
    /// pipeline 处理成功的 Sample 数。
    pub pipeline_processed_count: AtomicU64,
    /// pipeline 写 lux 失败次数。
    pub pipeline_write_fail_count: AtomicU64,
    /// pipeline 提交批次数(算平均批大小 = processed/batch)。
    pub pipeline_batch_count: AtomicU64,
    /// pipeline 活跃 worker 数(gauge)。
    pub pipeline_workers_active: AtomicU64,
    /// pipeline worker 异常死亡数(panic;counter)。
    pub pipeline_worker_deaths_count: AtomicU64,
    // 主链路守恒账（2026-10-27 架构评估 §3.2）：此前只有"出"（processed）与"深度"
    // （hub_depth），缺"入"与"丢"。稳态恒等式：
    //   samples_out − pipeline_processed == dropped_dead_worker + dropped_closed
    // `pipeline_processed` 的口径是"已冲刷的整批"：写 lux 失败的批次同样计入（失败另有
    // `pipeline_write_fail` 计次），故写失败不破坏上式。差额的可解释来源：
    // 在途（hub/worker channel 与攒批 buf 内）、关停残留（取消后未 dispatch 的存量与被
    // 打断的那一次 send）、worker panic 时其 buf 内样本、Processor 返回 None 丢弃的样本
    // （Passthrough 下恒 0）、`write_batch` prep 出错的整批（`write_fail` 只计 1 次）。
    // dropped_source_exit 是**未进入 hub** 的样本，不在 samples_out 内，单独看。
    /// Source 出口成功送入 hub 的 Sample 数（主链路入口）。
    pub samples_out_count: AtomicU64,
    /// dispatcher 丢弃：目标 worker 已退出（分片停入库）之后的样本。
    pub samples_dropped_dead_worker_count: AtomicU64,
    /// dispatcher 丢弃：向刚关闭的 worker channel 发送失败的那一个样本。
    pub samples_dropped_closed_count: AtomicU64,
    /// Source 丢弃：向已关闭的 hub 发送失败（采集循环随之退出）。
    pub samples_dropped_source_exit_count: AtomicU64,
    // hub 背压（gauge）
    /// hub channel 最新深度（gauge）。
    pub hub_depth_last: AtomicU64,
    /// hub channel 历史最大深度（gauge）。
    pub hub_depth_max: AtomicU64,
    // SSE 消费（R6 可观测）
    /// SSE 连接 broadcast lag 累计丢帧数（慢消费者；Rtdata 载荷带值后事件变大更易触发）。
    pub sse_lagged_count: AtomicU64,
    // 北向（mqtt）——T8 迁移说明：mqttPub 直启已删（北向改默认 luaNorth 实例
    // mqtt-pub-default），下列字段不再有运行时写入方（恒 0 / 恒 false）；实时
    // 计数在实例 stat 的 extra（publish_ok/publish_fail/reconnect_count，T7）。
    // 字段与方法保留是为 /metrics 与 /sysstatus 端点形状稳定（抓取端不破）。
    /// mqtt 发布成功次数（T8 后恒 0，见上方迁移说明）。
    pub mqtt_publish_count: AtomicU64,
    /// mqtt 发布失败次数（T8 后恒 0，见上方迁移说明）。
    pub mqtt_publish_fail_count: AtomicU64,
    /// mqtt 当前是否已连接（gauge，1=true/0=false；T8 后恒 false）。
    pub mqtt_connected: AtomicBool,
    // 画面/主题/符号版本管理（P0，design 2026-10-30-screen-versioning §5.2；symbol = P3a 2026-10-31）
    // 索引约定：0=screen 1=theme 2=symbol（routes_meta::MetaKind::metric_idx，三 kind 同构）。
    /// 版本快照创建数（捕获发生时 +1；判重命中不计）。
    pub meta_versions_created: [AtomicU64; 3],
    /// if_version 409 次数（写者竞争温度计——持续增长 = 有写者没接乐观锁）。
    pub meta_version_conflicts: [AtomicU64; 3],
    /// 回滚次数。
    pub meta_rollbacks: [AtomicU64; 3],
    /// 紧缩执行次数（频繁 = cap 配置过小或 TTL 过长）。
    pub meta_compactions: [AtomicU64; 3],
}

impl ProcessMetrics {
    /// meta_* 计数数组构造（三 kind：screen/theme/symbol，全 0）。
    fn init_meta_array() -> [AtomicU64; 3] {
        std::array::from_fn(|_| AtomicU64::new(0))
    }

    /// 构造（所有计数器 0，started = now）。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            ctl_send_count: AtomicU64::new(0),
            ctl_dropped_count: AtomicU64::new(0),
            ctl_not_found_count: AtomicU64::new(0),
            pipeline_processed_count: AtomicU64::new(0),
            pipeline_write_fail_count: AtomicU64::new(0),
            pipeline_batch_count: AtomicU64::new(0),
            pipeline_workers_active: AtomicU64::new(0),
            pipeline_worker_deaths_count: AtomicU64::new(0),
            samples_out_count: AtomicU64::new(0),
            samples_dropped_dead_worker_count: AtomicU64::new(0),
            samples_dropped_closed_count: AtomicU64::new(0),
            samples_dropped_source_exit_count: AtomicU64::new(0),
            hub_depth_last: AtomicU64::new(0),
            hub_depth_max: AtomicU64::new(0),
            sse_lagged_count: AtomicU64::new(0),
            mqtt_publish_count: AtomicU64::new(0),
            mqtt_publish_fail_count: AtomicU64::new(0),
            mqtt_connected: AtomicBool::new(false),
            meta_versions_created: Self::init_meta_array(),
            meta_version_conflicts: Self::init_meta_array(),
            meta_rollbacks: Self::init_meta_array(),
            meta_compactions: Self::init_meta_array(),
        }
    }

    /// ctl_send_count +1（Relaxed）。
    pub fn ctl_send(&self) {
        self.ctl_send_count.fetch_add(1, Ordering::Relaxed);
    }
    /// ctl_dropped_count +1。
    pub fn ctl_dropped(&self) {
        self.ctl_dropped_count.fetch_add(1, Ordering::Relaxed);
    }
    /// ctl_not_found_count +1。
    pub fn ctl_not_found(&self) {
        self.ctl_not_found_count.fetch_add(1, Ordering::Relaxed);
    }
    /// pipeline 写失败 +1。
    pub fn pipeline_write_fail(&self) {
        self.pipeline_write_fail_count
            .fetch_add(1, Ordering::Relaxed);
    }
    /// pipeline 写失败 +n(批量;I1:per-devid 容错时一次 flush 多 devid 失败)。
    pub fn pipeline_write_fail_n(&self, n: usize) {
        self.pipeline_write_fail_count
            .fetch_add(n as u64, Ordering::Relaxed);
    }
    /// pipeline 处理成功 +n(批量)。
    pub fn pipeline_processed_batch(&self, n: usize) {
        self.pipeline_processed_count
            .fetch_add(n as u64, Ordering::Relaxed);
    }
    /// pipeline 提交批次 +1。
    pub fn pipeline_batch(&self) {
        self.pipeline_batch_count.fetch_add(1, Ordering::Relaxed);
    }
    /// 设活跃 worker 数。
    pub fn set_workers_active(&self, n: usize) {
        self.pipeline_workers_active
            .store(n as u64, Ordering::Relaxed);
    }
    /// worker 退出 -1。
    ///
    /// 契约：调用次数 ≤ `set_workers_active` 设的值，且须在 `set_workers_active`
    /// 之后调用。对称使用（启动时 `set_workers_active(n)` + 退出时 n×`worker_exited`）
    /// 保证不会下溢。`fetch_sub` 在 0 时 wrap 到 `u64::MAX`（AtomicU64 无下溢保护），
    /// 故 worker 退出路径须保证只调一次（pipeline worker 的 `select!` 两 break 分支
    /// 出循环后单次调用，对称成立）。
    pub fn worker_exited(&self) {
        self.pipeline_workers_active.fetch_sub(1, Ordering::Relaxed);
    }
    /// worker 异常死亡(panic)+1(I2:dispatcher join 检测 panic 时补调 worker_exited
    /// 后计死亡,运维可统计 worker 历史死亡次数)。
    pub fn worker_died(&self) {
        self.pipeline_worker_deaths_count
            .fetch_add(1, Ordering::Relaxed);
    }
    /// Source 出口成功送入 hub +1（主链路入口）。
    pub fn samples_out(&self) {
        self.samples_out_count.fetch_add(1, Ordering::Relaxed);
    }
    /// dispatcher 丢弃：目标 worker 已退出之后的样本 +1。
    pub fn samples_dropped_dead_worker(&self) {
        self.samples_dropped_dead_worker_count
            .fetch_add(1, Ordering::Relaxed);
    }
    /// dispatcher 丢弃：向刚关闭的 worker channel 发送失败 +1。
    pub fn samples_dropped_closed(&self) {
        self.samples_dropped_closed_count
            .fetch_add(1, Ordering::Relaxed);
    }
    /// Source 丢弃：向已关闭的 hub 发送失败 +1。
    pub fn samples_dropped_source_exit(&self) {
        self.samples_dropped_source_exit_count
            .fetch_add(1, Ordering::Relaxed);
    }
    /// 更新 hub 深度 gauge（last + fetch_max）。
    pub fn hub_depth(&self, depth: usize) {
        let d = depth as u64;
        self.hub_depth_last.store(d, Ordering::Relaxed);
        self.hub_depth_max.fetch_max(d, Ordering::Relaxed);
    }
    /// SSE lag 丢帧 +n（R6：每次 Lagged 携带丢失条数，按 n 累加；持续增长提示
    /// 调大 `event_bus_capacity` 或排查慢 SSE 消费者）。
    pub fn sse_lagged(&self, n: u64) {
        self.sse_lagged_count.fetch_add(n, Ordering::Relaxed);
    }
    /// mqtt publish 成功 +1（T8 后无调用方——旧 mqtt_pub 已删；保留为端点形状）。
    pub fn mqtt_publish(&self) {
        self.mqtt_publish_count.fetch_add(1, Ordering::Relaxed);
    }
    /// mqtt publish 失败 +1（T8 后无调用方——旧 mqtt_pub 已删；保留为端点形状）。
    pub fn mqtt_publish_fail(&self) {
        self.mqtt_publish_fail_count.fetch_add(1, Ordering::Relaxed);
    }
    /// 更新 mqtt 连接状态（T8 后无调用方——旧 mqtt_pub 已删；保留为端点形状）。
    pub fn mqtt_connected(&self, connected: bool) {
        self.mqtt_connected.store(connected, Ordering::Relaxed);
    }

    // ---------- 画面/主题版本管理（P0 §5.2；idx 0=screen 1=theme） ----------

    /// 版本快照创建 +1（捕获发生时）。
    pub fn meta_versions_created(&self, idx: usize) {
        self.meta_versions_created[idx].fetch_add(1, Ordering::Relaxed);
    }
    /// if_version 409 +1（写者竞争温度计）。
    pub fn meta_version_conflict(&self, idx: usize) {
        self.meta_version_conflicts[idx].fetch_add(1, Ordering::Relaxed);
    }
    /// 回滚 +1。
    pub fn meta_rollback(&self, idx: usize) {
        self.meta_rollbacks[idx].fetch_add(1, Ordering::Relaxed);
    }
    /// 紧缩执行 +1（频繁 = cap 过小或 TTL 过长）。
    pub fn meta_compaction(&self, idx: usize) {
        self.meta_compactions[idx].fetch_add(1, Ordering::Relaxed);
    }
}

/// 现采进程 CPU%（sysinfo 单核百分比，多核机器可 >100）与常驻内存字节（sysinfo 0.32）。
///
/// `/sysstatus` 与 `/metrics` 复用(DRY)。**定向刷新**（2026-08 CPU 优化）：静态复用
/// `System` 实例 + 只刷本进程——旧实现每次 `System::new_all()` 全系统枚举（本机 383
/// 进程实测 ~41ms 墙钟，含 WMI 等待，CPU 落在 WmiPrvSE），selfMonitor 每 5s tick +
/// 每次 /metrics、/sysstatus 抓取各付一次并阻塞 tokio worker；定向刷新实测 ~10ms
/// 且无 WMI 副作用。互斥串行化并发抓取（调用方均为异步上下文，~10ms 级临界区可忍）。
///
/// 语义注：`cpu_usage()` 为距**上次刷新**的窗口平均（旧实现的"进程启动以来平均"本就
/// 依赖首刷基线，窗口化后更贴近即时值）；两次调用间隔 < sysinfo
/// `MINIMUM_CPU_UPDATE_INTERVAL`（200ms）时保持上次值。
pub fn sample_process_resource() -> (f32, u64) {
    use std::sync::{Mutex, OnceLock};
    static SYS: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let pid = sysinfo::get_current_pid().unwrap_or_else(|_| sysinfo::Pid::from(0_usize));
    let mut sys = SYS
        .get_or_init(|| Mutex::new(sysinfo::System::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .map(|p| (p.cpu_usage(), p.memory()))
        .unwrap_or((0.0, 0))
}

impl Default for ProcessMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment() {
        let m = ProcessMetrics::new();
        m.ctl_send();
        m.ctl_send();
        m.ctl_dropped();
        assert_eq!(m.ctl_send_count.load(Ordering::Relaxed), 2);
        assert_eq!(m.ctl_dropped_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn sse_lagged_accumulates_by_n() {
        // SSE lag 计数(R6):每次 Lagged 携带丢失条数 n,按 n 累加(非每事件 +1)
        let m = ProcessMetrics::new();
        m.sse_lagged(5);
        m.sse_lagged(2);
        assert_eq!(m.sse_lagged_count.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn hub_depth_tracks_max() {
        let m = ProcessMetrics::new();
        m.hub_depth(10);
        m.hub_depth(5);
        m.hub_depth(20);
        assert_eq!(m.hub_depth_last.load(Ordering::Relaxed), 20);
        assert_eq!(m.hub_depth_max.load(Ordering::Relaxed), 20);
    }

    #[test]
    fn sample_process_resource_smoke() {
        // sysinfo 现采,值随环境变化(cpu 首次可能 0);只验封装正确、不 panic、类型对。
        let (cpu, mem) = sample_process_resource();
        let _ = (cpu, mem);
    }

    #[test]
    fn pipeline_batch_and_workers_metrics() {
        let m = ProcessMetrics::new();
        m.pipeline_processed_batch(5);
        m.pipeline_processed_batch(3);
        m.pipeline_batch();
        m.pipeline_batch();
        assert_eq!(m.pipeline_processed_count.load(Ordering::Relaxed), 8);
        assert_eq!(m.pipeline_batch_count.load(Ordering::Relaxed), 2);
        m.set_workers_active(4);
        assert_eq!(m.pipeline_workers_active.load(Ordering::Relaxed), 4);
        m.worker_exited();
        assert_eq!(m.pipeline_workers_active.load(Ordering::Relaxed), 3);
        // worker 异常死亡计数(I2)
        m.worker_died();
        m.worker_died();
        assert_eq!(m.pipeline_worker_deaths_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn main_link_drop_counters_increment_independently() {
        // 主链路守恒账四个计数器各自独立累加（架构评估 §3.2）
        let m = ProcessMetrics::new();
        m.samples_out();
        m.samples_out();
        m.samples_dropped_dead_worker();
        m.samples_dropped_closed();
        m.samples_dropped_closed();
        m.samples_dropped_closed();
        m.samples_dropped_source_exit();
        assert_eq!(m.samples_out_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            m.samples_dropped_dead_worker_count.load(Ordering::Relaxed),
            1
        );
        assert_eq!(m.samples_dropped_closed_count.load(Ordering::Relaxed), 3);
        assert_eq!(
            m.samples_dropped_source_exit_count.load(Ordering::Relaxed),
            1
        );
    }
}
