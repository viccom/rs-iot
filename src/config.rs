use crate::error::RsIotError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 应用总配置。从 toml 加载，缺失字段走 Default。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// lux 嵌入数据库配置
    pub lux: LuxConfig,
    /// REST API 端口（M3）
    pub api_port: u16,
    /// JWT 签名密钥（HS256，32 字节）。默认值与 Lite_IOT 对齐。
    pub jwt_secret: String,
    /// 北向 MQTT broker（mqttPub 默认实例连接目标）
    pub mqtt: MqttConfig,
    /// 日志配置（终端 + 可选文件轮转）。
    pub log: LogConfig,
    /// 外部插件 bridge 配置（进程外南向插件经 TCP 接入）。
    pub bridge: BridgeConfig,
    /// pipeline 数据面配置（worker 数等）。
    pub pipeline: PipelineConfig,
}

/// lux ServerConfig 的子集映射（仅暴露我们关心的字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LuxConfig {
    /// RESP 端口。默认 6390，避开本机 redis 6379。
    pub port: u16,
    /// 绑定地址。默认 loopback。
    pub bind_host: String,
    /// 数据目录（lux.dat 快照 + Tiered WAL 落盘点）。
    pub data_dir: String,
    /// 后台快照间隔（秒）。Tiered 下用于截断 WAL，不可为 0。
    pub save_interval_secs: u64,
}

/// 北向 MQTT broker 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MqttConfig {
    /// broker 主机（公网或本地 mosquitto 等）。
    pub host: String,
    /// broker 端口（默认 1883）。
    pub port: u16,
    /// 用户名（匿名 broker 留 None）。
    pub username: Option<String>,
    /// 密码。
    pub password: Option<String>,
    /// 北向发布周期（秒）：`MqttPub` 每隔该时长从 lux 抽取 rtdata 发布。
    pub publish_interval_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            lux: LuxConfig::default(),
            api_port: 8780,
            jwt_secret: "qwerty1234567890asdfgh1234567890".into(),
            mqtt: MqttConfig::default(),
            log: LogConfig::default(),
            bridge: BridgeConfig::default(),
            pipeline: PipelineConfig::default(),
        }
    }
}

impl Default for LuxConfig {
    fn default() -> Self {
        Self {
            port: 6390,
            bind_host: "127.0.0.1".into(),
            data_dir: "./data/lux".into(),
            save_interval_secs: 300,
        }
    }
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 1883,
            username: None,
            password: None,
            publish_interval_secs: 5,
        }
    }
}

/// 外部插件 bridge 配置（sidecarClient：进程外南向插件经 TCP 接入）。
///
/// `enable=false` 默认，现有部署零影响（不占端口、不起 acceptor）。
/// 跨机部署需显式改 `bind_host="0.0.0.0"`；同机用 loopback 最安全。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeConfig {
    /// 是否启用 bridge（监听端口接受外部插件连接）。
    pub enable: bool,
    /// 绑定地址。默认 `127.0.0.1`（同机，零局域网暴露）；跨机改 `0.0.0.0`。
    pub bind_host: String,
    /// 监听端口。默认 8781。
    pub port: u16,
    /// 心跳超时（秒）：外部插件超过该时长未发心跳则判失联。
    pub heartbeat_timeout_secs: u64,
    /// 接入握手超时（秒）：TCP 连接建立后多久内必须收到 Hello 帧。
    pub accept_hello_timeout_secs: u64,
    /// 最大并发外部连接数（防连接风暴/DoS；acceptor 用 Semaphore 限并发握手）。
    pub max_connections: usize,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            enable: false,
            bind_host: "127.0.0.1".into(),
            port: 8781,
            heartbeat_timeout_secs: 15,
            accept_hello_timeout_secs: 10,
            max_connections: 512,
        }
    }
}

/// pipeline 数据面配置。
///
/// 阶段1 单 worker（bench 已证明够 50 万/秒）；阶段2 起用多 worker（auto/手动）。
//
// 注：用 `#[derive(Default)]` 而非文件内其他 Config 的 `impl Default`——
// `workers: 0 == usize::default()`，clippy `derivable_impls` 强制 derive（CI `-D warnings` 门槛）；
// 语义与手写 `impl Default { workers: 0 }` 字节等价。其他 Config 因字段需非默认值（port=6390 等）
// 无法 derive，故用 impl——本结构体例外。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    /// worker 数。0 = auto(min(cpu,8))；阶段1 用 1，阶段2 起用 auto/手动。
    pub workers: usize,
}

/// 日志配置。终端输出始终开启；`dir` 非空时额外写文件（daily 轮转 + 保留期清理）。
///
/// - `dir` 默认 `./logs`（开箱即落盘，修复 scenario-gap §3 🔴 P0）；显式 `dir=""` 关闭文件层。
/// - `max_days`/`max_size_mb` 控制保留期清理（M3，见 `lib::purge_old_logs`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// 日志文件目录。空 = 仅终端；非空（如 `./logs`）= 额外写文件。
    pub dir: String,
    /// 日志级别（仅当未设 `RUST_LOG` 时作为缺省；`RUST_LOG` 优先）。
    pub level: String,
    /// 按文件名日期保留最近 N 天（0 = 不按天删）。
    pub max_days: u64,
    /// 目录总大小上限（MB）；0 = 不限，>0 时按最旧优先删直到低于阈值。
    pub max_size_mb: u64,
    /// 日志 ring buffer 容量（P1 子系统 3，默认 2000）。内存快照最近 N 条供 API 查询。
    pub ring_buffer_capacity: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            dir: "./logs".into(),
            level: "info".into(),
            max_days: 7,
            max_size_mb: 0,
            ring_buffer_capacity: 2000,
        }
    }
}

impl AppConfig {
    /// 加载配置文件；路径为 None 或文件不存在时返回默认配置。
    pub fn load_or_default(path: Option<&Path>) -> Result<Self, RsIotError> {
        match path {
            Some(p) if p.exists() => {
                let s = std::fs::read_to_string(p)
                    .map_err(|e| RsIotError::Config(format!("read config {}: {e}", p.display())))?;
                let cfg: AppConfig = toml::from_str(&s).map_err(|e| {
                    RsIotError::Config(format!("parse config {}: {e}", p.display()))
                })?;
                Ok(cfg)
            }
            _ => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_config_default_enables_file_logging() {
        let c = LogConfig::default();
        // 默认落盘 ./logs（修复 scenario-gap §3 🔴 P0「默认未启用文件日志」）
        assert_eq!(c.dir, "./logs");
        assert_eq!(c.level, "info");
        assert_eq!(c.max_days, 7);
        assert_eq!(c.max_size_mb, 0);
    }

    #[test]
    fn log_config_toml_default_section() {
        // 无 [log] section 的 toml → Default（dir=./logs）
        let toml = "";
        let cfg: AppConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.log.dir, "./logs");
        assert_eq!(cfg.log.max_days, 7);
    }

    #[test]
    fn log_config_empty_dir_disables_file_layer() {
        // 显式 dir="" 关闭文件层（保留逃生口）；同时锁 partial-default 契约：
        // 用户只设 dir，其余字段仍走 Default
        let toml = "[log]\ndir = \"\"\n";
        let cfg: AppConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.log.dir, "");
        assert_eq!(cfg.log.max_days, 7);
        assert_eq!(cfg.log.max_size_mb, 0);
    }

    #[test]
    fn log_config_ring_buffer_capacity_default() {
        // 缺字段走 Default 2000（struct-level #[serde(default)] 委托 Default impl）
        let cfg: LogConfig = toml::from_str(r#"level = "info""#).unwrap();
        assert_eq!(cfg.ring_buffer_capacity, 2000);
    }

    #[test]
    fn log_config_ring_buffer_capacity_explicit() {
        // 显式值覆盖默认
        let cfg: LogConfig = toml::from_str(
            r#"level = "info"
ring_buffer_capacity = 500"#,
        )
        .unwrap();
        assert_eq!(cfg.ring_buffer_capacity, 500);
    }

    #[test]
    fn pipeline_config_default_auto() {
        // 缺 [pipeline] section → Default(workers=0=auto);阶段1 单 worker 由 Task 7 的 auto_workers 把 0 转 1
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.pipeline.workers, 0, "默认 0=auto");
    }

    #[test]
    fn pipeline_config_workers_explicit() {
        // 显式 [pipeline] workers=N 覆盖默认;锁 partial-default 契约:用户只设 workers,缺字段仍走 Default
        let cfg: AppConfig = toml::from_str("[pipeline]\nworkers = 4").unwrap();
        assert_eq!(cfg.pipeline.workers, 4);
    }
}
