//! 配置加载测试：`load_or_default` 的容错语义
//! （无文件 → 默认；部分字段 → 其余默认；缺失字段容错是既有约定）。

use rs_iot::config::AppConfig;
use std::path::PathBuf;

fn temp_cfg(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rsiot-cfg-{name}.toml"))
}

#[test]
fn no_path_returns_default() {
    let cfg = AppConfig::load_or_default(None).expect("default");
    assert_eq!(cfg.api_port, 8780);
    assert_eq!(cfg.lux.port, 6390);
    assert_eq!(cfg.mqtt.host, "localhost");
    assert_eq!(cfg.mqtt.publish_interval_secs, 5);
}

#[test]
fn missing_file_path_returns_default() {
    let missing = temp_cfg("missing");
    let cfg = AppConfig::load_or_default(Some(&missing)).expect("default");
    assert_eq!(cfg.api_port, 8780);
}

#[test]
fn partial_toml_keeps_defaults_for_missing_fields() {
    // 只写顶层 + 一个子节，其余走 serde(default)
    let path = temp_cfg("partial");
    std::fs::write(&path, "api_port = 8790\n").expect("write");
    let cfg = AppConfig::load_or_default(Some(&path)).expect("load");
    assert_eq!(cfg.api_port, 8790);
    assert_eq!(
        cfg.lux.port, 6390,
        "missing [lux] should fall back to default"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn full_toml_overrides_all() {
    let path = temp_cfg("full");
    std::fs::write(
        &path,
        r#"
api_port = 8790
jwt_secret = "test-secret"

[lux]
port = 6391
bind_host = "0.0.0.0"
save_interval_secs = 60

[mqtt]
host = "broker.example.com"
port = 1884
publish_interval_secs = 10
"#,
    )
    .expect("write");
    let cfg = AppConfig::load_or_default(Some(&path)).expect("load");
    assert_eq!(cfg.api_port, 8790);
    assert_eq!(cfg.jwt_secret, "test-secret");
    assert_eq!(cfg.lux.port, 6391);
    assert_eq!(cfg.lux.bind_host, "0.0.0.0");
    assert_eq!(cfg.lux.save_interval_secs, 60);
    assert_eq!(cfg.mqtt.host, "broker.example.com");
    assert_eq!(cfg.mqtt.port, 1884);
    assert_eq!(cfg.mqtt.publish_interval_secs, 10);
    std::fs::remove_file(&path).ok();
}
