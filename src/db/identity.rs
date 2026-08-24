//! 身份与配置的 lux Hash 存储。对标 Lite_IOT `bRedka/bredka.go`。
//!
//! Hash 表清单（与 Lite_IOT 1:1）：
//! - `users`     : field=userId, value=JSON{userid,username,group}
//! - `passwords` : field=userId, value=bcrypt hash
//! - `groups`    : field=groupId, value=JSON 数组 ["userId",...]
//! - `instances` : field=instId, value=JSON 实例配置（M2）
//! - `devices`   : field=devId, value=JSON 设备配置（M3）
//! - `children`  : field=devId, value=JSON 变量列表（M3）
//! - `accesskey` : string，全局访问密钥（M3）
//! - `ext_tokens`: Hash，field=instId, value=JSON{token_hash,created_at,enabled}（M7 外部插件 bridge）
//! - `luakv:<instId>` : Hash，field=key，value=JSON（M4 脚本本地持久化状态）
//! - `stat:<instId>`  : string，JSON 实例状态快照（M7 luaClient 运行指标：status/collect 计数/脚本版本）
//! - `rtdata:<devId>` : Hash，field=tag, value=[ts, localTime, value]（M2 实时数据缓存）
//! - `devmodels` : Hash，field=model_id, value=DeviceModel JSON（设备模型注册表 M-1，`src/model/`）
//! - `devreg`    : Hash，field=devid, value=DeviceEntry JSON（全局设备注册表 M-1）
//! - `discoveries`: Hash，field=tag_key, value=Discovery JSON（发现队列 M-4，`src/model/discovery.rs`——未建模标签待人工 adopt）
//! - `tags`      : **已退役**（M-4 T3）——Phase 1a 运行时属性卡被 devreg.overrides
//!   吸收（D-M3）；遗留数据只读导出过渡（GET /api/v1/tags/legacy-export），升级
//!   后可人工删除

use crate::error::RsIotError;
use lux::{EmbeddedClient, EmbeddedValue};

/// 首次启动初始化：建 admin/admin + AdminG + UserG（幂等）。
///
/// 对标 Lite_IOT `bredka.InitDB`。重复执行不会覆盖已存在的记录。
#[tracing::instrument(skip(c))]
pub async fn bootstrap(c: &EmbeddedClient) -> Result<(), RsIotError> {
    // 管理员用户 admin/admin
    if !hash_exists(c, "users", "admin").await? {
        let profile = r#"{"userid":"admin","username":"管理员","group":"AdminG"}"#;
        c.execute("HSET", &["users", "admin", profile]).await?;
        let hashed = bcrypt::hash("admin", bcrypt::DEFAULT_COST)?;
        c.execute("HSET", &["passwords", "admin", &hashed]).await?;
        tracing::info!("created initial admin user (admin/admin) — please change the password");
    }

    // 用户组
    if !hash_exists(c, "groups", "AdminG").await? {
        c.execute("HSET", &["groups", "AdminG", r#"["admin"]"#])
            .await?;
    }
    if !hash_exists(c, "groups", "UserG").await? {
        c.execute("HSET", &["groups", "UserG", r#"[""]"#]).await?;
    }

    Ok(())
}

/// `HEXISTS key field` → bool
pub async fn hash_exists(c: &EmbeddedClient, key: &str, field: &str) -> Result<bool, RsIotError> {
    let v = c.execute_value("HEXISTS", &[key, field]).await?;
    Ok(matches!(v, EmbeddedValue::Int(1)))
}
