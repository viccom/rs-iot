//! effective 物化缓存（M-1 T8；模式同 Phase 1a `declared`）。
//!
//! 数据源 = devmodels ∪ devreg（`effective_all` 纯函数物化），key =
//! `tag_key(devid, tag)`。读侧 [`effective()`] Arc 换手零拷贝（Phase 1b 历史
//! 白名单/死区过滤、M-4 发现引擎的 O(1) 消费接口——master plan D11）。
//!
//! 失效策略 = 写路径主动刷新（store 四写函数成功后 `reload`，best-effort：
//! 失败 warn 保留旧快照，下次写/启动兜底）。KSUB 跨进程失效后置（当前
//! 单进程 EmbeddedClient，无跨写方）。

use crate::error::RsIotError;
use crate::types::TagProps;
use lux::EmbeddedClient;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

static CACHE: OnceLock<RwLock<Arc<HashMap<String, TagProps>>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<HashMap<String, TagProps>>> {
    CACHE.get_or_init(|| RwLock::new(Arc::new(HashMap::new())))
}

/// 当前 effective 快照（Arc clone 廉价；未装载时空表）。
pub fn effective() -> Arc<HashMap<String, TagProps>> {
    // 毒化恢复：reload 临界区 panic 后旧快照仍完整有效，读比崩好（L-4）。
    cell().read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// 全量重建（启动装载 + 写路径失效）。返回物化表（`tag_key → TagProps`）。
///
/// 重建后同步修剪发现引擎 seen_modeled（快照中消失的 key 不再被短路——
/// 模型删 tag/设备删除后同 tag 数据重新进发现队列，无需重启自愈）。
pub async fn reload(c: &EmbeddedClient) -> Result<HashMap<String, TagProps>, RsIotError> {
    let models = crate::model::store::list_models(c).await?;
    let devs = crate::model::store::list_devices(c).await?;
    let map = crate::model::effective::effective_all(&models, &devs);
    *cell().write().unwrap_or_else(|p| p.into_inner()) = Arc::new(map.clone());
    crate::model::discovery::retain_modeled(&map);
    Ok(map)
}

/// 写路径主动刷新（store 写函数成功后调用；best-effort 保留旧快照）。
pub(crate) async fn refresh_after_write(c: &EmbeddedClient) {
    if let Err(e) = reload(c).await {
        tracing::warn!("[model] effective 缓存刷新失败（保留旧快照）: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::store;
    use crate::model::types::{DeviceEntry, DeviceModel, TagOverride, TagTemplate};

    async fn test_client(tag: &str) -> lux::EmbeddedClient {
        let cfg = crate::config::AppConfig {
            lux: crate::config::LuxConfig {
                port: 0,
                data_dir: std::env::temp_dir()
                    .join(format!("modelcache-{tag}-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = lux::run_with_config(crate::db::build_lux_config(&cfg))
            .await
            .expect("lux");
        handle.client()
    }

    /// 注意：CACHE 是进程级全局，测试并行共享——各用例用互不重叠的
    /// model_id/devid（Phase 1a SEEN 同款约定）。
    async fn seed(c: &EmbeddedClient, model_id: &str, devid: &str, deadband: Option<Option<f64>>) {
        store::set_model(
            c,
            &DeviceModel {
                model_id: model_id.into(),
                tags: vec![TagTemplate {
                    name: "ua".into(),
                    data_type: "f32".into(),
                    addr: Some("holding:0:f32".into()),
                    props: TagProps {
                        history: true,
                        deadband: Some(0.5),
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        store::set_device(
            c,
            &DeviceEntry {
                devid: devid.into(),
                model: model_id.into(),
                overrides: [(
                    "ua".to_string(),
                    TagOverride {
                        deadband,
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reload_materializes_effective_by_tag_key() {
        let c = test_client("load").await;
        seed(&c, "mc_m1", "mc_d1", None).await;
        // 断言走 reload 返回值（确定性）——全局 effective() 快照被并行测试的
        // 写路径刷新覆盖属正常共享，不作全局断言。
        let map = reload(&c).await.unwrap();
        assert_eq!(map.len(), 1, "一设备一 tag");
        let props = map.get("mc_d1.ua").expect("tag_key 规范化键");
        assert!(props.history, "模板默认");
        assert_eq!(props.deadband, Some(0.5), "无覆盖继承模板");
    }

    /// 计划 T8 验收：写后读新值（写路径主动刷新）。
    ///
    /// 写路径刷新在 `set_device` 内 await 完成；全局快照可能被并行测试的刷新
    /// 覆盖——读不到则重写重读（重新触发本实例刷新；上限 2s）。
    #[tokio::test]
    async fn store_write_refreshes_cache_read_new_value() {
        let c = test_client("write-refresh").await;
        seed(&c, "mc_m2", "mc_d2", None).await;
        let overridden = DeviceEntry {
            devid: "mc_d2".into(),
            model: "mc_m2".into(),
            overrides: [(
                "ua".to_string(),
                TagOverride {
                    deadband: Some(None),
                    history: Some(false),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            store::set_device(&c, &overridden).await.unwrap();
            if let Some(props) = effective().get("mc_d2.ua") {
                assert_eq!(props.deadband, None, "显式关死区生效（写后读新值）");
                assert!(!props.history, "覆盖 history=false 生效");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "写路径刷新未在全局缓存生效（2s）"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn delete_device_evicts_from_cache() {
        let c = test_client("del-evict").await;
        seed(&c, "mc_m3", "mc_d3", None).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            store::delete_device(&c, "mc_d3").await.unwrap();
            if !effective().contains_key("mc_d3.ua") {
                return; // 删除后缓存条目驱逐（写路径刷新）
            }
            assert!(
                std::time::Instant::now() < deadline,
                "删除驱逐未在全局缓存生效（2s）"
            );
            // 删除非幂等（再删 None）：重新种设备再删，重新触发刷新。
            seed(&c, "mc_m3", "mc_d3", None).await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn reload_empty_tables_ok() {
        let c = test_client("empty").await;
        assert!(reload(&c).await.unwrap().is_empty());
    }

    /// 毒化恢复（审查 L-4）：reload 临界区 panic 毒化后，effective() 读旧
    /// 快照不得 panic——否则该次毒化后所有 pipeline worker 在 flush 热路径
    /// 连环崩（每次调用必 panic，dispatcher 的 panic 补调机制救不了）。
    /// 全局静态锁毒化对其他测试无害：修复后读写均走 into_inner。
    #[test]
    fn effective_read_survives_lock_poison() {
        let join = std::thread::Builder::new()
            .spawn(|| {
                let _g = cell().write().unwrap();
                panic!("注入毒化");
            })
            .unwrap();
        let _ = join.join(); // Err(panic) → 锁已毒化
        let _ = effective(); // 红：当前 read().unwrap() panic；绿：返回旧快照
    }
}
