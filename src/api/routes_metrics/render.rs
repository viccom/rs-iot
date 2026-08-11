//! `/metrics` 端点 render 层(Prometheus 文本格式化的纯函数)。spec §3-§6。

use crate::db::stat::InstanceStat;
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
        m
    }

    #[test]
    fn render_process_has_help_type_and_values() {
        let m = proc_metrics();
        let out = render_process(&m, "eiot_abc", "0.1.0", 12.5, 8192);
        // 全部 17 个 metric family 都在场(防误删某 family 静默通过)
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("# TYPE "))
                .count(),
            17,
            "应输出 17 个 metric family"
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
}
