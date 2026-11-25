//! `/metrics` 端点 render 层(Prometheus 文本格式化的纯函数)。spec §3-§6。

use crate::db::stat::InstanceStat;
use crate::history::rows::{HistoryStats, WriteErrorKind};
use crate::plugin::instance::InstState;
use crate::process_metrics::ProcessMetrics;
use std::sync::atomic::Ordering;

/// Prometheus exposition 文本 Content-Type(供 metrics handler 设置响应头)。
pub(crate) const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// 写 metric family 头(`# HELP` + `# TYPE`),每 family 一次。
fn family_header(s: &mut String, name: &str, help: &str, ty: &str) {
    s.push_str(&format!("# HELP {name} {help}.\n"));
    s.push_str(&format!("# TYPE {name} {ty}\n"));
}

/// 写一行 metric:`name{labels} value`。
fn metric_line(s: &mut String, name: &str, labels: &str, val: &str) {
    s.push_str(&format!("{name}{labels} {val}\n"));
}

/// label 值 escape(Prometheus 规范:`"` / `\` / 换行)。
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// 进程级指标 → Prometheus 文本(标识 + 资源 + ctl/pipeline/hub/mqtt)。
pub(crate) fn render_process(
    m: &ProcessMetrics,
    hwid: &str,
    version: &str,
    cpu: f32,
    mem: u64,
) -> String {
    let mut s = String::new();
    let rel = Ordering::Relaxed;

    // 进程标识(info)
    family_header(
        &mut s,
        "rs_iot_info",
        "rs-iot 进程标识(hwid/version)",
        "gauge",
    );
    metric_line(
        &mut s,
        "rs_iot_info",
        &format!(
            "{{hwid=\"{}\",version=\"{}\"}}",
            escape_label(hwid),
            escape_label(version)
        ),
        "1",
    );

    // 资源
    family_header(&mut s, "rs_iot_uptime_seconds", "进程运行秒数", "gauge");
    metric_line(
        &mut s,
        "rs_iot_uptime_seconds",
        "",
        &m.started.elapsed().as_secs().to_string(),
    );
    family_header(
        &mut s,
        "rs_iot_process_cpu_pct",
        "进程 CPU 占用百分比",
        "gauge",
    );
    metric_line(&mut s, "rs_iot_process_cpu_pct", "", &cpu.to_string());
    family_header(
        &mut s,
        "rs_iot_process_mem_bytes",
        "进程常驻内存字节",
        "gauge",
    );
    metric_line(&mut s, "rs_iot_process_mem_bytes", "", &mem.to_string());

    // 控制下行(counter)
    for (name, help, val) in [
        (
            "rs_iot_ctl_send_total",
            "控制命令投递尝试总数",
            m.ctl_send_count.load(rel),
        ),
        (
            "rs_iot_ctl_dropped_total",
            "控制命令 channel 满丢弃数",
            m.ctl_dropped_count.load(rel),
        ),
        (
            "rs_iot_ctl_not_found_total",
            "控制命令未找到目标实例数",
            m.ctl_not_found_count.load(rel),
        ),
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

    // 数据上行(counter)
    for (name, help, val) in [
        (
            "rs_iot_pipeline_processed_total",
            "pipeline 处理成功 Sample 数",
            m.pipeline_processed_count.load(rel),
        ),
        (
            "rs_iot_pipeline_write_fail_total",
            "pipeline 写 lux 失败次数",
            m.pipeline_write_fail_count.load(rel),
        ),
        (
            "rs_iot_pipeline_batch_total",
            "pipeline 提交批次数(平均批大小 = processed/batch)",
            m.pipeline_batch_count.load(rel),
        ),
        (
            "rs_iot_pipeline_worker_deaths_total",
            "pipeline worker 异常死亡数(panic)",
            m.pipeline_worker_deaths_count.load(rel),
        ),
        // 主链路守恒账(架构评估 §3.2):稳态下
        // samples_out − pipeline_processed == dropped_dead_worker + dropped_closed;
        // dropped_source_exit 是未进入 hub 的样本(不在 samples_out 内)。
        (
            "rs_iot_samples_out_total",
            "Source 出口成功送入 hub 的 Sample 数(主链路入口)",
            m.samples_out_count.load(rel),
        ),
        (
            "rs_iot_samples_dropped_dead_worker_total",
            "dispatcher 丢弃:目标 worker 已退出(分片停入库)之后的样本",
            m.samples_dropped_dead_worker_count.load(rel),
        ),
        (
            "rs_iot_samples_dropped_closed_total",
            "dispatcher 丢弃:向刚关闭的 worker channel 发送失败的样本",
            m.samples_dropped_closed_count.load(rel),
        ),
        (
            "rs_iot_samples_dropped_source_exit_total",
            "Source 丢弃:向已关闭的 hub 发送失败(采集循环随之退出)",
            m.samples_dropped_source_exit_count.load(rel),
        ),
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

    // pipeline worker 活跃数(gauge;下降=worker 异常退出,该 devid 分片停入库)
    family_header(
        &mut s,
        "rs_iot_pipeline_workers",
        "pipeline 活跃 worker 数(下降=worker 异常退出,该分片停入库)",
        "gauge",
    );
    metric_line(
        &mut s,
        "rs_iot_pipeline_workers",
        "",
        &m.pipeline_workers_active.load(rel).to_string(),
    );

    // hub 背压(gauge)
    family_header(&mut s, "rs_iot_hub_depth", "hub channel 当前深度", "gauge");
    metric_line(
        &mut s,
        "rs_iot_hub_depth",
        "",
        &m.hub_depth_last.load(rel).to_string(),
    );
    family_header(
        &mut s,
        "rs_iot_hub_depth_max",
        "hub channel 历史最大深度",
        "gauge",
    );
    metric_line(
        &mut s,
        "rs_iot_hub_depth_max",
        "",
        &m.hub_depth_max.load(rel).to_string(),
    );

    // SSE lag(R6/C 阶段 T2:慢消费者 broadcast 丢帧累计;持续增长提示调大 event_bus_capacity)
    family_header(
        &mut s,
        "rs_iot_sse_lagged_total",
        "SSE 连接 broadcast lag 累计丢帧数",
        "counter",
    );
    metric_line(
        &mut s,
        "rs_iot_sse_lagged_total",
        "",
        &m.sse_lagged_count.load(rel).to_string(),
    );

    // 北向 mqtt
    family_header(
        &mut s,
        "rs_iot_mqtt_publish_total",
        "mqtt 发布成功次数",
        "counter",
    );
    metric_line(
        &mut s,
        "rs_iot_mqtt_publish_total",
        "",
        &m.mqtt_publish_count.load(rel).to_string(),
    );
    family_header(
        &mut s,
        "rs_iot_mqtt_publish_fail_total",
        "mqtt 发布失败次数",
        "counter",
    );
    metric_line(
        &mut s,
        "rs_iot_mqtt_publish_fail_total",
        "",
        &m.mqtt_publish_fail_count.load(rel).to_string(),
    );
    family_header(
        &mut s,
        "rs_iot_mqtt_connected",
        "mqtt 当前是否已连接(1/0)",
        "gauge",
    );
    metric_line(
        &mut s,
        "rs_iot_mqtt_connected",
        "",
        if m.mqtt_connected.load(rel) { "1" } else { "0" },
    );

    // 画面/主题版本管理（P0，design 2026-10-30-screen-versioning §5.2）。
    // label kind: screen/theme/symbol（routes_meta 三 MetaKind 同构，symbol=P3a）；conflicts 持续
    // 增长 = 有写者没接 if_version 乐观锁；compactions 频繁 = cap 过小或 TTL 过长。
    for (name, help, vals) in [
        (
            "rs_iot_meta_versions_created_total",
            "画面/主题/符号版本快照创建数(判重命中不计)",
            &m.meta_versions_created,
        ),
        (
            "rs_iot_meta_version_conflicts_total",
            "画面/主题/符号 if_version 409 次数(写者竞争温度计)",
            &m.meta_version_conflicts,
        ),
        (
            "rs_iot_meta_rollbacks_total",
            "画面/主题/符号回滚次数",
            &m.meta_rollbacks,
        ),
        (
            "rs_iot_meta_compactions_total",
            "画面/主题/符号版本元数据紧缩执行次数(频繁=cap 过小或 TTL 过长)",
            &m.meta_compactions,
        ),
    ] {
        family_header(&mut s, name, help, "counter");
        for (idx, kind) in ["screen", "theme", "symbol"].iter().enumerate() {
            metric_line(
                &mut s,
                name,
                &format!("{{kind=\"{kind}\"}}"),
                &vals[idx].load(rel).to_string(),
            );
        }
    }

    s
}

/// per-instance 渲染输入(handler 从 mgr.status() + config_snapshot + lux stat 组装)。
pub(crate) struct InstanceRow {
    pub(crate) inst_id: String,
    pub(crate) state: InstState,
    pub(crate) app_code: String,
    pub(crate) host_kind: &'static str,
    /// 插件级 health + 计数;None = 实例刚启动尚未 flush stat。
    pub(crate) health: Option<InstanceStat>,
}

/// InstState → lowercase 字符串(穷尽 match,新变体编译期强制审查;规则 7)。
fn state_str(s: &InstState) -> &'static str {
    match s {
        InstState::Starting => "starting",
        InstState::Running => "running",
        InstState::Stopping => "stopping",
        InstState::Stopped => "stopped",
        InstState::Failed => "failed",
    }
}

/// per-instance metric family 规格:(name, help, type, value getter)。
/// 抽出别名避免内联复杂类型触发 clippy::type_complexity。
type InstanceFamilySpec = (
    &'static str,
    &'static str,
    &'static str,
    fn(&InstanceStat) -> String,
);

/// per-instance 指标 → Prometheus 文本。
///
/// info family 输出所有实例(含无 stat 的,health 空串);tick/fail/duration/ts
/// 只输出有 stat 的实例。`# HELP`/`# TYPE` 每 family 一次。
pub(crate) fn render_instances(rows: &[InstanceRow]) -> String {
    let mut s = String::new();

    // info family(每实例一行)
    family_header(
        &mut s,
        "rs_iot_instance_info",
        "实例分类与状态(inst_id/app_code/host_kind/state/health)",
        "gauge",
    );
    for r in rows {
        let health = r.health.as_ref().map(|h| h.status.as_str()).unwrap_or("");
        let labels = format!(
            "{{inst_id=\"{}\",app_code=\"{}\",host_kind=\"{}\",state=\"{}\",health=\"{}\"}}",
            escape_label(&r.inst_id),
            escape_label(&r.app_code),
            r.host_kind,
            escape_label(state_str(&r.state)),
            escape_label(health),
        );
        metric_line(&mut s, "rs_iot_instance_info", &labels, "1");
    }

    // 有 stat 的实例(用于 tick/fail/duration/ts)
    let with_stat: Vec<(&InstanceRow, &InstanceStat)> = rows
        .iter()
        .filter_map(|r| r.health.as_ref().map(|h| (r, h)))
        .collect();

    // tick/fail/duration/ts(仅有 stat 的实例;元组数组对齐 render_process 约定)
    let families: &[InstanceFamilySpec] = &[
        (
            "rs_iot_instance_tick_total",
            "实例活动成功累计次数",
            "counter",
            |h: &InstanceStat| h.tick_count.to_string(),
        ),
        (
            "rs_iot_instance_fail_total",
            "实例活动失败累计次数",
            "counter",
            |h: &InstanceStat| h.fail_count.to_string(),
        ),
        (
            "rs_iot_instance_last_activity_duration_ms",
            "实例最近一次活动耗时(毫秒)",
            "gauge",
            |h: &InstanceStat| h.last_activity_duration_ms.to_string(),
        ),
        (
            "rs_iot_instance_last_activity_ts",
            "实例最近一次活动墙上时间(unix 秒)",
            "gauge",
            |h: &InstanceStat| h.last_activity_ts.to_string(),
        ),
    ];
    for (name, help, ty, getter) in families {
        family_header(&mut s, name, help, ty);
        for (r, h) in &with_stat {
            metric_line(
                &mut s,
                name,
                &format!("{{inst_id=\"{}\"}}", escape_label(&r.inst_id)),
                &getter(h),
            );
        }
    }

    s
}

/// 历史治理 telemetry 快照（P4/P5：handler 侧采集后传入，保持 render 层纯函数）。
///
/// `Default`（0/0/0）兼作防御降级值：telemetry 缺席 / `spawn_blocking` JoinErr
/// 时以零快照输出（调用方 `history_stats` Some 前置下理论不可达）。
#[derive(Default)]
pub(crate) struct TelemetrySnapshot {
    /// 内存已用字节（tsink `memory_used()` 口径）。
    pub(crate) memory_used_bytes: u64,
    /// 内存预算字节（D6 `memory_limit_mb` 换算）。
    pub(crate) memory_budget_bytes: u64,
    /// series 数（tsink registry 原子口径，近似——与 list_metrics 物化口径
    /// 可能有 rollup/未剪枝死 series 差异）。
    pub(crate) series: u64,
    /// 内存五分量（Task 6：区分 admission 误拒时"预估虚高"vs"真用满"）。
    pub(crate) memory_components: crate::history::MemoryComponents,
    /// admission 缺口（Task 6：最近一次内存预算拒绝的预估与缺口）。
    pub(crate) admission: crate::history::AdmissionMemoryGap,
    /// P1 flush/seal 门控与 catalog 节流计数（低速率写放大根治的观测面）。
    pub(crate) flush_gate: crate::history::FlushGateMetrics,
}

/// 历史旁路计数器（七计数器 + H1/M11 拒收与跳过计数）+ Phase 3 治理七 family
/// → Prometheus 文本（Phase 3 前置，spec §3/§6 同规；P1/P2/P3）。
///
/// **仅引擎 ok（stats 存在）时被 handler 调用**：disabled/init_failed 态不输出
/// ——零值 counter 会误导抓取方以为归档在跑而数据为零。计数语义与
/// `HistoryStats` 字段一一对应（`sent+dropped+skipped+ts_rejected+len_rejected
/// == fan_out 遍历总数`）。
///
/// 治理扩展：白名单规模/引擎水位 gauge（`t` 快照 + `archived_tags`）、死区
/// 评估 counter 与削减率 gauge（P2：ratio = deadbanded/evaluated，分母 0 时
/// 输出 0）、写错误五桶 labeled counter（P3：Σkind == errors_total，值 0 桶
/// 也输出，齐五桶）。
pub(crate) fn render_history(
    h: &HistoryStats,
    t: &TelemetrySnapshot,
    archived_tags: usize,
) -> String {
    let mut s = String::new();
    let rel = Ordering::Relaxed;
    for (name, help, val) in [
        (
            "rs_iot_history_sent_total",
            "历史旁路 fan_out 发送成功行数",
            h.sent.load(rel),
        ),
        (
            "rs_iot_history_written_total",
            "历史 writer insert_rows 成功行数",
            h.written.load(rel),
        ),
        (
            "rs_iot_history_dropped_total",
            "历史旁路 channel 满丢弃行数(满即丢)",
            h.dropped.load(rel),
        ),
        (
            "rs_iot_history_deadbanded_total",
            "历史死区抑制行数",
            h.deadbanded.load(rel),
        ),
        (
            "rs_iot_history_skipped_total",
            "历史白名单外跳过行数(L3)",
            h.skipped.load(rel),
        ),
        (
            "rs_iot_history_lane_mismatch_total",
            "历史 lane 型变剔除行数(P3)",
            h.lane_mismatch.load(rel),
        ),
        (
            "rs_iot_history_nonfinite_skipped_total",
            "历史 NaN/Inf 非有限值写侧拒收行数(H1)",
            h.nonfinite_skipped.load(rel),
        ),
        (
            "rs_iot_history_dup_ts_skipped_total",
            "历史同 ts 跨批重复跳过行数(M11)",
            h.dup_ts_skipped.load(rel),
        ),
        (
            "rs_iot_history_stale_ts_dropped_total",
            "历史跨批回卷严格更旧 ts 丢弃行数(D-T3:迟到样本不落盘)",
            h.stale_ts_dropped.load(rel),
        ),
        (
            "rs_iot_history_ts_rejected_total",
            "历史时间戳域坏拒收行数(M1:×1000溢出或超[now-7d,now+1h]窗)",
            h.ts_rejected.load(rel),
        ),
        (
            "rs_iot_history_len_rejected_total",
            "历史超长行拒收行数(M2:tag>64KiB/label>16KiB)",
            h.len_rejected.load(rel),
        ),
        (
            "rs_iot_history_errors_total",
            "历史 insert_rows 失败行数(fail-soft 整批丢弃)",
            h.errors.load(rel),
        ),
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

    // 白名单规模与引擎水位（P1 gauge：archived_tags 由 handler 从 effective
    // 快照计数传入，其余三项为 telemetry 快照）。
    for (name, help, val) in [
        (
            "rs_iot_history_archived_tags",
            "历史白名单 tag 数(effective 快照 history=true 计数)",
            archived_tags.to_string(),
        ),
        (
            "rs_iot_history_series",
            "历史引擎 series 数(registry 原子口径,近似)",
            t.series.to_string(),
        ),
        (
            "rs_iot_history_memory_used_bytes",
            "历史引擎内存已用字节(tsink 口径)",
            t.memory_used_bytes.to_string(),
        ),
        (
            "rs_iot_history_memory_budget_bytes",
            "历史引擎内存预算字节(D6 memory_limit)",
            t.memory_budget_bytes.to_string(),
        ),
        // Task 6（2026-08-29 压测观测缺口）：内存五分量——两个总量指标无法
        // 区分 admission 误拒时"预估虚高"与"真用满"；分量口径与 tsink
        // MemoryObservabilitySnapshot 对齐（Task 7 预算口径调整后 used 与
        // 分量的包含关系可能变化,分量读数本身稳定）。
        (
            "rs_iot_history_memory_registry_bytes",
            "历史引擎 series registry 字典内存字节(tsink registry 分量)",
            t.memory_components.registry_bytes.to_string(),
        ),
        (
            "rs_iot_history_memory_metadata_bytes",
            "历史引擎元数据缓存/簿记内存字节(tsink metadata_cache 分量:pending series+可见性摘要+watermarks)",
            t.memory_components.metadata_cache_bytes.to_string(),
        ),
        (
            "rs_iot_history_memory_persisted_index_bytes",
            "历史引擎持久化索引常驻内存字节(chunk refs/时间戳索引/postings)",
            t.memory_components.persisted_index_bytes.to_string(),
        ),
        (
            "rs_iot_history_memory_persisted_mmap_bytes",
            "历史引擎持久化段 mmap 映射字节(页缓存 RSS 口径,随归档增长)",
            t.memory_components.persisted_mmap_bytes.to_string(),
        ),
        (
            "rs_iot_history_memory_active_sealed_bytes",
            "历史引擎 active head+sealed chunk 热数据字节",
            t.memory_components.active_and_sealed_bytes.to_string(),
        ),
        // Task 6：admission 缺口——estimate 大而分量小=预估虚高,active+
        // sealed/registry 逼近预算=真用满;从未拒绝时三读数为 0。
        (
            "rs_iot_history_admission_last_estimate_bytes",
            "历史引擎最近一次内存预算拒绝的批预估增量字节(used+estimate=错误串 required)",
            t.admission.last_estimate_bytes.to_string(),
        ),
        (
            "rs_iot_history_admission_shortfall_bytes",
            "历史引擎最近一次内存预算拒绝的缺口字节(required-budget)",
            t.admission.last_shortfall_bytes.to_string(),
        ),
    ] {
        family_header(&mut s, name, help, "gauge");
        metric_line(&mut s, name, "", &val);
    }

    // P1 seal 门控与 catalog 节流（2026-10-29 低速率写放大根治观测面）：
    // seal_hold 持续增长而 seal_trigger 稀少 = 门控生效（低速攒批）；
    // catalog_persisted 应只在锚点/跨窗口增长，接近 attempts 说明节流失效。
    for (name, help, val) in [
        (
            "rs_iot_history_seal_trigger_points_total",
            "tsink 后台 flush 因 head 达到最小点数而 seal 的次数(P1)",
            t.flush_gate.seal_trigger_points_total,
        ),
        (
            "rs_iot_history_seal_trigger_age_total",
            "tsink 后台 flush 因 head 年龄到期而 seal 的次数(P1 低速兜底)",
            t.flush_gate.seal_trigger_age_total,
        ),
        (
            "rs_iot_history_seal_trigger_wal_total",
            "tsink 后台 flush 因未落盘 WAL 达上限而 seal 的次数(P1 高速安全阀)",
            t.flush_gate.seal_trigger_wal_total,
        ),
        (
            "rs_iot_history_seal_hold_total",
            "tsink 后台 flush 因未达 seal 门槛而保持 head 打开的 series×拍计数(P1)",
            t.flush_gate.seal_hold_total,
        ),
        (
            "rs_iot_history_catalog_persist_attempts_total",
            "tsink catalog 持久化路径进入次数(P1)",
            t.flush_gate.catalog_persist_attempts_total,
        ),
        (
            "rs_iot_history_catalog_persisted_total",
            "tsink catalog 实际写盘次数(P1:稳态应远小于 attempts)",
            t.flush_gate.catalog_persisted_total,
        ),
        (
            "rs_iot_history_catalog_persist_skipped_total",
            "tsink catalog 被节流或内容未变跳过的次数(P1)",
            t.flush_gate.catalog_persist_skipped_total,
        ),
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

    // admission 拒绝累计（Task 6）：counter 语义,持续增长即事故进行中。
    family_header(
        &mut s,
        "rs_iot_history_admission_rejections_total",
        "历史引擎内存预算 admission 拒绝累计次数",
        "counter",
    );
    metric_line(
        &mut s,
        "rs_iot_history_admission_rejections_total",
        "",
        &t.admission.rejections_total.to_string(),
    );

    // 死区评估计数与削减率（P2：分子分母同源同点计数；ratio 口径明写
    // suppressed/evaluated，R8——Phase 4 门禁"削减率 ≥50%"直接对表）。
    let evaluated = h.deadband_evaluated.load(rel);
    family_header(
        &mut s,
        "rs_iot_history_deadband_evaluated_total",
        "历史死区评估行数(P2 削减率分母,deadband=Some 标签的数值点)",
        "counter",
    );
    metric_line(
        &mut s,
        "rs_iot_history_deadband_evaluated_total",
        "",
        &evaluated.to_string(),
    );
    let ratio = if evaluated > 0 {
        h.deadbanded.load(rel) as f64 / evaluated as f64
    } else {
        0.0
    };
    family_header(
        &mut s,
        "rs_iot_history_deadband_reduction_ratio",
        "历史死区削减率(suppressed/evaluated,分母 0 时输出 0)",
        "gauge",
    );
    metric_line(
        &mut s,
        "rs_iot_history_deadband_reduction_ratio",
        "",
        &ratio.to_string(),
    );

    // 写错误五桶分型（P3：Σkind == rs_iot_history_errors_total；值 0 桶也
    // 输出，齐五桶保证抓取方面板字段稳定）。
    family_header(
        &mut s,
        "rs_iot_history_write_errors_total",
        "历史 insert_rows 失败行数按 kind 分桶(Σkind == errors_total)",
        "counter",
    );
    for kind in WriteErrorKind::ALL {
        metric_line(
            &mut s,
            "rs_iot_history_write_errors_total",
            &format!("{{kind=\"{}\"}}", kind.as_str()),
            &h.error_rows_of(kind).to_string(),
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_metrics() -> ProcessMetrics {
        let m = ProcessMetrics::new();
        m.ctl_send_count.store(7, Ordering::Relaxed);
        m.ctl_dropped_count.store(1, Ordering::Relaxed);
        m.hub_depth_last.store(42, Ordering::Relaxed);
        m.hub_depth_max.store(99, Ordering::Relaxed);
        m.mqtt_publish_count.store(5, Ordering::Relaxed);
        m.mqtt_connected.store(true, Ordering::Relaxed);
        m.sse_lagged_count.store(3, Ordering::Relaxed);
        m.meta_versions_created[0].store(2, Ordering::Relaxed);
        m.meta_versions_created[1].store(1, Ordering::Relaxed);
        m.samples_out_count.store(11, Ordering::Relaxed);
        m.samples_dropped_dead_worker_count
            .store(2, Ordering::Relaxed);
        m
    }

    #[test]
    fn render_process_has_help_type_and_values() {
        let m = proc_metrics();
        let out = render_process(&m, "eiot_abc", "0.1.0", 12.5, 8192);
        // 全部 26 个 metric family 都在场(防误删某 family 静默通过)
        // 18 既有 + 4 版本管理(P0 2026-10-30: meta_versions_created/version_conflicts/
        // rollbacks/compactions ×{kind=screen,theme})
        // + 4 主链路守恒账(2026-11-25 架构评估 §3.2)
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("# TYPE "))
                .count(),
            26,
            "应输出 26 个 metric family"
        );
        // 版本管理 label 形状：每 family 三行 {kind="screen"}/{kind="theme"}/{kind="symbol"}
        // （P0 两 kind；symbol = P3a 2026-10-31 第三资源族）
        assert!(out.contains("rs_iot_meta_versions_created_total{kind=\"screen\"} 2"));
        assert!(out.contains("rs_iot_meta_versions_created_total{kind=\"theme\"} 1"));
        assert!(out.contains("rs_iot_meta_versions_created_total{kind=\"symbol\"} 0"));
        assert_eq!(
            out.lines()
                .filter(|l| l.starts_with("rs_iot_meta_"))
                .count(),
            12,
            "四 family × 三 kind 行"
        );
        // HELP / TYPE 每个 family 齐全
        assert!(out.contains("# HELP rs_iot_info "));
        assert!(out.contains("# TYPE rs_iot_info gauge"));
        // info 含 hwid + version label
        assert!(out.contains(r#"rs_iot_info{hwid="eiot_abc",version="0.1.0"} 1"#));
        // counter 带值 + _total 后缀
        assert!(out.contains("# TYPE rs_iot_ctl_send_total counter"));
        assert!(out.contains("rs_iot_ctl_send_total 7"));
        assert!(out.contains("rs_iot_ctl_dropped_total 1"));
        // gauge(hub 深度,无 _total 后缀)
        assert!(out.contains("# TYPE rs_iot_hub_depth gauge"));
        assert!(out.contains("rs_iot_hub_depth 42"));
        assert!(out.contains("rs_iot_hub_depth_max 99"));
        // mqtt
        assert!(out.contains("rs_iot_mqtt_publish_total 5"));
        assert!(out.contains("rs_iot_mqtt_connected 1"));
        // SSE lagged(R6:慢消费者 lag 丢帧计数,>0 提示调大 event_bus_capacity)
        assert!(out.contains("# TYPE rs_iot_sse_lagged_total counter"));
        assert!(out.contains("rs_iot_sse_lagged_total 3"));
        // 主链路守恒账(架构评估 §3.2):入口 + 三个丢弃桶
        assert!(out.contains("# TYPE rs_iot_samples_out_total counter"));
        assert!(out.contains("rs_iot_samples_out_total 11"));
        assert!(out.contains("rs_iot_samples_dropped_dead_worker_total 2"));
        assert!(out.contains("# TYPE rs_iot_samples_dropped_closed_total counter"));
        assert!(out.contains("# TYPE rs_iot_samples_dropped_source_exit_total counter"));
        // 资源
        assert!(out.contains("rs_iot_process_cpu_pct 12.5"));
        assert!(out.contains("rs_iot_process_mem_bytes 8192"));
        assert!(out.contains("rs_iot_uptime_seconds"));
    }

    #[test]
    fn render_process_counters_all_end_with_total() {
        // 命名规范锁定:每个 counter 名以 _total 结尾。
        let m = ProcessMetrics::new();
        let out = render_process(&m, "h", "v", 0.0, 0);
        for line in out.lines() {
            if line.starts_with("# TYPE ") && line.ends_with(" counter") {
                let name = line
                    .trim_start_matches("# TYPE ")
                    .trim_end_matches(" counter");
                assert!(name.ends_with("_total"), "counter {name} 应以 _total 结尾");
            }
        }
    }

    fn row(
        inst_id: &str,
        state: InstState,
        app_code: &str,
        host_kind: &'static str,
        health: Option<InstanceStat>,
    ) -> InstanceRow {
        InstanceRow {
            inst_id: inst_id.into(),
            state,
            app_code: app_code.into(),
            host_kind,
            health,
        }
    }

    fn stat_running() -> InstanceStat {
        InstanceStat {
            status: "running".into(),
            tick_count: 100,
            fail_count: 2,
            last_activity_ts: 1700000000,
            last_activity_duration_ms: 42,
            last_err: String::new(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn render_instances_info_labels() {
        let rows = vec![
            row(
                "opcua-1",
                InstState::Running,
                "opcuaClient",
                "builtin",
                Some(stat_running()),
            ),
            row("lua-1", InstState::Failed, "luaClient", "builtin", None),
        ];
        let out = render_instances(&rows);
        // info family 头一次
        assert!(out.contains("# HELP rs_iot_instance_info "));
        assert!(out.contains("# TYPE rs_iot_instance_info gauge"));
        // 有 stat 的实例:health=running
        assert!(out.contains(
            r#"rs_iot_instance_info{inst_id="opcua-1",app_code="opcuaClient",host_kind="builtin",state="running",health="running"} 1"#
        ));
        // 无 stat 的实例:health 空字符串
        assert!(out.contains(
            r#"rs_iot_instance_info{inst_id="lua-1",app_code="luaClient",host_kind="builtin",state="failed",health=""} 1"#
        ));
    }

    #[test]
    fn render_instances_counters_only_for_stat() {
        let rows = vec![
            row(
                "opcua-1",
                InstState::Running,
                "opcuaClient",
                "builtin",
                Some(stat_running()),
            ),
            row("lua-1", InstState::Failed, "luaClient", "builtin", None),
        ];
        let out = render_instances(&rows);
        // tick/fail/duration/ts 只输出有 stat 的实例
        assert!(out.contains(r#"rs_iot_instance_tick_total{inst_id="opcua-1"} 100"#));
        assert!(out.contains(r#"rs_iot_instance_fail_total{inst_id="opcua-1"} 2"#));
        assert!(out.contains(r#"rs_iot_instance_last_activity_duration_ms{inst_id="opcua-1"} 42"#));
        assert!(out.contains(r#"rs_iot_instance_last_activity_ts{inst_id="opcua-1"} 1700000000"#));
        // 无 stat 的实例不应出现在 tick_total 行
        assert!(
            !out.contains(r#"rs_iot_instance_tick_total{inst_id="lua-1"}"#),
            "无 stat 实例不应有 tick_total 行"
        );
    }

    #[test]
    fn render_instances_escapes_label_special_chars() {
        // inst_id 含引号/反斜杠 → 必须 escape
        let rows = vec![row(
            r#"a"b\c"#,
            InstState::Running,
            "x",
            "builtin",
            Some(stat_running()),
        )];
        let out = render_instances(&rows);
        assert!(
            out.contains(r#"inst_id="a\"b\\c""#),
            "inst_id 特殊字符应被 escape,实际: {out}"
        );
    }

    #[test]
    fn render_instances_empty_has_family_headers_only() {
        // 空输入:5 个 instance family 头(info/tick/fail/duration/ts),无数据行。
        let out = render_instances(&[]);
        assert_eq!(
            out.lines()
                .filter(|l| l.starts_with("# TYPE rs_iot_instance"))
                .count(),
            5,
            "空输入应有 5 个 instance family 头"
        );
        assert!(!out.contains("{inst_id="), "空输入无数据行");
    }

    #[test]
    fn render_instances_escapes_newline_in_label() {
        // inst_id 含换行 → escape 为 \n(Prometheus label 规范)。
        let rows = vec![row(
            "a\nb",
            InstState::Running,
            "x",
            "builtin",
            Some(stat_running()),
        )];
        let out = render_instances(&rows);
        assert!(
            out.contains(r#"inst_id="a\nb""#),
            "换行应 escape 为 \\n,实际: {out}"
        );
    }

    /// 历史计数器预置(每计数器互异值,防串位)。
    fn history_stats() -> crate::history::rows::HistoryStats {
        let h = crate::history::rows::HistoryStats::default();
        h.sent.store(11, Ordering::Relaxed);
        h.written.store(7, Ordering::Relaxed);
        h.dropped.store(3, Ordering::Relaxed);
        h.deadbanded.store(5, Ordering::Relaxed);
        h.skipped.store(2, Ordering::Relaxed);
        h.lane_mismatch.store(1, Ordering::Relaxed);
        h.nonfinite_skipped.store(4, Ordering::Relaxed);
        h.dup_ts_skipped.store(6, Ordering::Relaxed);
        h.stale_ts_dropped.store(3, Ordering::Relaxed);
        h.ts_rejected.store(10, Ordering::Relaxed);
        h.len_rejected.store(12, Ordering::Relaxed);
        h.errors.store(9, Ordering::Relaxed);
        h
    }

    #[test]
    fn render_history_family_count_and_values() {
        let out = render_history(
            &history_stats(),
            &TelemetrySnapshot {
                flush_gate: Default::default(),
                memory_used_bytes: 111,
                memory_budget_bytes: 268_435_456,
                series: 7,
                memory_components: crate::history::MemoryComponents {
                    registry_bytes: 1_000,
                    metadata_cache_bytes: 2_000,
                    persisted_index_bytes: 3_000,
                    persisted_mmap_bytes: 4_000,
                    active_and_sealed_bytes: 5_000,
                },
                admission: crate::history::AdmissionMemoryGap {
                    rejections_total: 2,
                    last_estimate_bytes: 6_000,
                    last_shortfall_bytes: 7_000,
                },
            },
            1,
        );
        // 恰好 34 个 family:8 既有+新增 counter(nonfinite_skipped/dup_ts_skipped/
        // stale_ts_dropped/ts_rejected/len_rejected)+ 7 治理(四水位 gauge + 死区评估 counter +
        // 削减率 gauge + 写错误五桶 labeled counter)+ 8 观测扩展(五内存分量 gauge +
        // admission 拒绝 counter + 预估/缺口 gauge)+ 7 P1 seal 门控/catalog 节流 counter
        assert_eq!(
            out.lines().filter(|l| l.starts_with("# TYPE ")).count(),
            34,
            "history 应输出 34 个 metric family"
        );
        // 每个 family:HELP/TYPE 头 + 数值行
        for (name, val) in [
            ("rs_iot_history_sent_total", "11"),
            ("rs_iot_history_written_total", "7"),
            ("rs_iot_history_dropped_total", "3"),
            ("rs_iot_history_deadbanded_total", "5"),
            ("rs_iot_history_skipped_total", "2"),
            ("rs_iot_history_lane_mismatch_total", "1"),
            ("rs_iot_history_nonfinite_skipped_total", "4"),
            ("rs_iot_history_dup_ts_skipped_total", "6"),
            ("rs_iot_history_stale_ts_dropped_total", "3"),
            ("rs_iot_history_ts_rejected_total", "10"),
            ("rs_iot_history_len_rejected_total", "12"),
            ("rs_iot_history_errors_total", "9"),
        ] {
            assert!(
                out.contains(&format!("# HELP {name} ")),
                "{name} 缺 HELP 头"
            );
            assert!(
                out.contains(&format!("# TYPE {name} counter")),
                "{name} 缺 TYPE counter 头"
            );
            assert!(out.contains(&format!("{name} {val}")), "{name} 缺数值行");
        }
    }

    /// 内存分量与 admission 缺口 family（Task 6，2026-08-29 压测观测缺口）：
    /// 五分量 gauge + admission 拒绝 counter + 预估/缺口 gauge，数值逐一取自
    /// telemetry 快照（防串位：五分量互异千位值）。
    #[test]
    fn render_history_memory_component_and_admission_families() {
        let out = render_history(
            &history_stats(),
            &TelemetrySnapshot {
                flush_gate: Default::default(),
                memory_used_bytes: 111,
                memory_budget_bytes: 268_435_456,
                series: 7,
                memory_components: crate::history::MemoryComponents {
                    registry_bytes: 1_000,
                    metadata_cache_bytes: 2_000,
                    persisted_index_bytes: 3_000,
                    persisted_mmap_bytes: 4_000,
                    active_and_sealed_bytes: 5_000,
                },
                admission: crate::history::AdmissionMemoryGap {
                    rejections_total: 2,
                    last_estimate_bytes: 6_000,
                    last_shortfall_bytes: 7_000,
                },
            },
            1,
        );
        for (name, ty, line) in [
            (
                "rs_iot_history_memory_registry_bytes",
                "gauge",
                "rs_iot_history_memory_registry_bytes 1000",
            ),
            (
                "rs_iot_history_memory_metadata_bytes",
                "gauge",
                "rs_iot_history_memory_metadata_bytes 2000",
            ),
            (
                "rs_iot_history_memory_persisted_index_bytes",
                "gauge",
                "rs_iot_history_memory_persisted_index_bytes 3000",
            ),
            (
                "rs_iot_history_memory_persisted_mmap_bytes",
                "gauge",
                "rs_iot_history_memory_persisted_mmap_bytes 4000",
            ),
            (
                "rs_iot_history_memory_active_sealed_bytes",
                "gauge",
                "rs_iot_history_memory_active_sealed_bytes 5000",
            ),
            (
                "rs_iot_history_admission_rejections_total",
                "counter",
                "rs_iot_history_admission_rejections_total 2",
            ),
            (
                "rs_iot_history_admission_last_estimate_bytes",
                "gauge",
                "rs_iot_history_admission_last_estimate_bytes 6000",
            ),
            (
                "rs_iot_history_admission_shortfall_bytes",
                "gauge",
                "rs_iot_history_admission_shortfall_bytes 7000",
            ),
        ] {
            assert!(
                out.contains(&format!("# HELP {name} ")),
                "{name} HELP 行缺失:\n{out}"
            );
            assert!(
                out.contains(&format!("# TYPE {name} {ty}")),
                "{name} TYPE 行缺失(期望 {ty}):\n{out}"
            );
            assert!(out.contains(line), "数值行缺失(期望 \"{line}\"):\n{out}");
        }
    }

    #[test]
    fn render_history_counters_all_end_with_total() {
        // 命名规范锁定:history counter 名以 _total 结尾(与 render_process 同规)。
        let out = render_history(
            &crate::history::rows::HistoryStats::default(),
            &TelemetrySnapshot::default(),
            0,
        );
        for line in out.lines() {
            if line.starts_with("# TYPE ") && line.ends_with(" counter") {
                let name = line
                    .trim_start_matches("# TYPE ")
                    .trim_end_matches(" counter");
                assert!(name.ends_with("_total"), "counter {name} 应以 _total 结尾");
            }
        }
    }
}
