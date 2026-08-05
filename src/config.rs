use anyhow::Result;
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
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            lux: LuxConfig::default(),
            api_port: 8780,
            jwt_secret: "qwerty1234567890asdfgh1234567890".into(),
            mqtt: MqttConfig::default(),
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
        }
    }
}

impl AppConfig {
    /// 加载配置文件；路径为 None 或文件不存在时返回默认配置。
    pub fn load_or_default(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(p) if p.exists() => {
                let s = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("read config {}: {e}", p.display()))?;
                let cfg: AppConfig = toml::from_str(&s)
                    .map_err(|e| anyhow::anyhow!("parse config {}: {e}", p.display()))?;
                Ok(cfg)
            }
            _ => Ok(Self::default()),
        }
    }
}
