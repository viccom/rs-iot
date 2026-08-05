//! 实时数据缓存：把 Sample 最新值写入 lux Hash `rtdata:<devid>`。
//!
//! 对标 Lite_IOT `bDB.HandleSyncDataQueue → GlobalDBMap[devid][key]=[ts,localTime,value]`。
//! 区别：rs-iot 不再维护内存 map + sync 队列双套，直接落 lux Hash——
//! 持久化（Tiered WAL）、REST `/rtdatas` 直接 HGETALL、并发交给 lux。

use crate::error::RsIotError;
use crate::types::SampleValue;
use lux::EmbeddedClient;

/// 写入一条 Sample 的所有变量到 `rtdata:<devid>`。
/// field = `<devid>.<tag>`（若 tag 已含 devid 则不重复），value = JSON `[ts, localTime, value]`。
pub async fn write(
    c: &EmbeddedClient,
    hwid: &str,
    devid: &str,
    values: &std::collections::BTreeMap<String, SampleValue>,
    timestamp: i64,
) -> Result<(), RsIotError> {
    let key = format!("rtdata:{devid}");
    let local_time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    for (tag, val) in values {
        let field = if tag.contains(devid) {
            tag.clone()
        } else {
            format!("{devid}.{tag}")
        };
        let val_str = match val {
            SampleValue::Float(f) => f.to_string(),
            SampleValue::Int(i) => i.to_string(),
            SampleValue::Bool(b) => b.to_string(),
            SampleValue::Str(s) => s.clone(),
        };
        let cell = serde_json::to_string(&serde_json::json!([timestamp, local_time, val_str]))?;
        c.execute("HSET", &[&key, &field, &cell]).await?;
    }
    let _ = hwid; // 预留：未来可按 hwid 分 key
    Ok(())
}
