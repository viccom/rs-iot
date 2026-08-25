//! RtdataWatchService 端到端(真 lux,Task 4):ksubscribe("rtdata:*") 事件驱动
//! per-devid 版本表。
//!
//! 覆盖(P9):HSET 事件 → mark 版本;启动时一次 `KEYS rtdata:*` 铺底(存量设备
//! 版本 Some(0));DEL 也是 key 事件 → 版本递增。不建 AppState(接线属绿阶段)。

mod common;

use rs_iot::api::rtdata_watch::RtdataWatchService;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 独立临时目录并清空上一轮残留——lux data_dir 跨 run 持久化(lux.dat + WAL),
/// 不清会让存量键"漏进"本轮断言造成假绿(lua_transform_e2e real_chain 先例)。
fn fresh_dir(name: &str) -> std::path::PathBuf {
    let d = common::test_data_dir(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// 轮询断言:2s 内 cond 变真,超时 panic(事件消费是异步的,不能即时断言)。
async fn eventually(msg: &str, cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("2s 内未满足: {msg}"));
}

/// HSET rtdata:devA → watcher 收到 key 事件 → devA 入版本表。
#[tokio::test]
async fn watch_service_marks_dirty_on_hset() {
    let dir = fresh_dir("watch_hset");
    let (handle, client) = common::start_lux(dir.to_str().expect("utf8")).await;
    let svc = std::sync::Arc::new(RtdataWatchService::new());
    let token = CancellationToken::new();
    let join = svc.spawn(client.clone(), token.clone());

    client
        .execute("HSET", &["rtdata:devA", "devA.temp", "42"])
        .await
        .expect("HSET");
    eventually("HSET 后 devA 应有版本", || {
        svc.version("devA").is_some()
    })
    .await;

    token.cancel();
    let _ = join.await;
    common::stop_lux(handle).await;
}

/// spawn 前已有存量 rtdata:devB → 启动 KEYS 铺底后版本表含 devB(版本 Some(0),
/// 锚定:铺底设备不占事件版本号)。
#[tokio::test]
async fn watch_service_initial_keys_backfill() {
    let dir = fresh_dir("watch_backfill");
    let (handle, client) = common::start_lux(dir.to_str().expect("utf8")).await;
    // 存量键写在 spawn 之前:此刻无订阅者,只能靠启动 KEYS 铺底发现
    client
        .execute("HSET", &["rtdata:devB", "devB.t", "1"])
        .await
        .expect("HSET");
    let svc = std::sync::Arc::new(RtdataWatchService::new());
    let token = CancellationToken::new();
    let join = svc.spawn(client.clone(), token.clone());

    eventually("铺底后 devB 版本应为 Some(0)", || {
        svc.device_count() >= 1 && svc.version("devB") == Some(0)
    })
    .await;

    token.cancel();
    let _ = join.await;
    common::stop_lux(handle).await;
}

/// DEL rtdata:devC 也是 key 事件 → 版本递增(diff_scope 据此出 removed,P9)。
#[tokio::test]
async fn watch_service_marks_del() {
    let dir = fresh_dir("watch_del");
    let (handle, client) = common::start_lux(dir.to_str().expect("utf8")).await;
    let svc = std::sync::Arc::new(RtdataWatchService::new());
    let token = CancellationToken::new();
    let join = svc.spawn(client.clone(), token.clone());

    client
        .execute("HSET", &["rtdata:devC", "devC.t", "7"])
        .await
        .expect("HSET");
    eventually("HSET 后 devC 应有版本", || {
        svc.version("devC").is_some()
    })
    .await;
    let before = svc.version("devC");

    client.execute("DEL", &["rtdata:devC"]).await.expect("DEL");
    eventually("DEL 后 devC 版本应递增", || {
        svc.version("devC") > before
    })
    .await;

    token.cancel();
    let _ = join.await;
    common::stop_lux(handle).await;
}
