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
    // hub 背压（gauge）
    /// hub channel 最新深度（gauge）。
    pub hub_depth_last: AtomicU64,
    /// hub channel 历史最大深度（gauge）。
    pub hub_depth_max: AtomicU64,
    // 北向（mqtt）
    /// mqtt 发布成功次数。
    pub mqtt_publish_count: AtomicU64,
    /// mqtt 发布失败次数。
    pub mqtt_publish_fail_count: AtomicU64,
    /// mqtt 当前是否已连接（gauge，1=true/0=false）。
    pub mqtt_connected: AtomicBool,
}

impl ProcessMetrics {
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
            hub_depth_last: AtomicU64::new(0),
            hub_depth_max: AtomicU64::new(0),
            mqtt_publish_count: AtomicU64::new(0),
            mqtt_publish_fail_count: AtomicU64::new(0),
            mqtt_connected: AtomicBool::new(false),
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
    /// 更新 hub 深度 gauge（last + fetch_max）。
    pub fn hub_depth(&self, depth: usize) {
        let d = depth as u64;
        self.hub_depth_last.store(d, Ordering::Relaxed);
        self.hub_depth_max.fetch_max(d, Ordering::Relaxed);
    }
    /// mqtt publish 成功 +1。
    pub fn mqtt_publish(&self) {
        self.mqtt_publish_count.fetch_add(1, Ordering::Relaxed);
    }
    /// mqtt publish 失败 +1。
    pub fn mqtt_publish_fail(&self) {
        self.mqtt_publish_fail_count.fetch_add(1, Ordering::Relaxed);
    }
    /// 更新 mqtt 连接状态。
    pub fn mqtt_connected(&self, connected: bool) {
        self.mqtt_connected.store(connected, Ordering::Relaxed);
    }
}

/// 现采进程 CPU%(sysinfo 单核百分比,多核机器可 >100)与常驻内存字节(sysinfo 0.32)。
///
/// `/sysstatus` 与 `/metrics` 复用(DRY)。`System::new_all()` 构造并刷新;
/// `get_current_pid` 返回 `Result`,失败兜底 `Pid::from(0_usize)`。
pub fn sample_process_resource() -> (f32, u64) {
    let sys = sysinfo::System::new_all();
    let pid = sysinfo::get_current_pid().unwrap_or_else(|_| sysinfo::Pid::from(0_usize));
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
}
