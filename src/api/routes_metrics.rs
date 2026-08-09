//! Prometheus `/metrics` 端点(M4 指标的标准化机器可读出口)。spec §3-§6。

// 本模块是 /metrics 端点的 render 层:helper + render_process 是纯函数,Task 4 的
// `metrics` handler 接入前非测试路径暂无调用方,故整模块标 `expect(dead_code)`。
// handler 接入后此处零 dead_code,`unused_expectations` 会提示移除本属性。
#![expect(dead_code)]

use crate::process_metrics::ProcessMetrics;
use std::sync::atomic::Ordering;

/// Prometheus exposition 文本 Content-Type(供 Task 4 handler 设置响应头)。
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
fn render_process(m: &ProcessMetrics, hwid: &str, version: &str, cpu: f32, mem: u64) -> String {
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
    ] {
        family_header(&mut s, name, help, "counter");
        metric_line(&mut s, name, "", &val.to_string());
    }

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
        // 全部 14 个 metric family 都在场(防误删某 family 静默通过)
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("# TYPE "))
                .count(),
            14,
            "应输出 14 个 metric family"
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
}
