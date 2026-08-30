//! cardinality 运行期热重校准 e2e（Task 19，P5/R4——2026-08-29 压测实锤回归）。
//!
//! 压测事故场景：空库启动 → auto 校准 0（tsink 收到 1）→ 运行期 REST 建模
//! 10 万白名单 → 全部写入被 `CardinalityLimitExceeded` 拒（176 万点）直到重启。
//! 本文件锚定修复语义：白名单经 store 写路径变更（set_model/set_device →
//! `cache::reload`）后，引擎 cardinality **只放大**热重校准——
//! - 扩张：重算 auto_cardinality，大于已设值时 `set_cardinality_limit`；
//! - 缩小：limit 保持（白名单缩小由 fan_out L3 过滤天然生效，缩 limit 反而
//!   使已存在 series 删除后不可重建，无收益有风险）；
//! - 显式配置（`cardinality_limit>0`）：运维硬上限，不参与运行期重校准
//!   （M0 止血方案即"显式配大值"，语义必须保持可预期）。
#![cfg(feature = "history")]
// SERIALIZE 的 std MutexGuard 刻意跨 await 持有——串行语义要求锁覆盖整个
// async 用例体（tests/history_dataflow_e2e.rs 同款先例），async Mutex 无法
// 建立该排他。
#![allow(clippy::await_holding_lock)]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rs_iot::config::HistoryConfig;
use rs_iot::history::engine::maybe_init;
use rs_iot::types::{ChildSpec, DeviceSpec, TagProps};
use tsink::{DataPoint, Label, Row, TsinkError};

/// 同 binary 用例串行化锁：`model::cache` 的 effective 与 history 引擎侧
/// cardinality 重校准注册点均为进程级全局（tests/history_dataflow_e2e.rs
/// SERIALIZE 同款约定），串行消除跨用例覆盖竞态（集成测试是独立进程，
/// 跨 binary 无共享）。
static SERIALIZE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取串行锁（poison 容错：前者 panic 不应级联卡死后继用例）。
fn serialize() -> std::sync::MutexGuard<'static, ()> {
    SERIALIZE.lock().unwrap_or_else(|p| p.into_inner())
}

/// 唯一临时目录：tag + pid + 纳秒时钟，测试并行与 CI 并发 run 不冲突。
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsiot-hist-cardrecalib-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 当前 Unix 毫秒（retention enforced 下取 now 附近 ts；互异毫秒防同 ts）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_millis() as i64
}

/// 与生产 writer 约定一致的 series labels：[devid, hwid]（字典序）。
fn series_labels(devid: &str) -> Vec<Label> {
    vec![Label::new("devid", devid), Label::new("hwid", "it-hwid")]
}

/// 经 storage 直写一个新 series（单行批；metric 互异即新 series——
/// CardinalityLimitExceeded 的拒绝点正是 insert_rows 的新 series 准入）。
fn write_series(
    engine: &rs_iot::history::engine::HistoryEngine,
    metric: &str,
    ts_ms: i64,
) -> Result<(), TsinkError> {
    engine
        .storage()
        .insert_rows(&[Row::with_labels(
            metric,
            series_labels("cdev1"),
            DataPoint::new(ts_ms, 1.0),
        )])
        .map(|_| ())
}

/// 建一个设备模型：`tag_count` 个 history=true tag（经 store 写路径
/// set_model/set_device → refresh_after_write → reload → 联动重校准）。
async fn register_whitelisted_model(client: &lux::EmbeddedClient, devid: &str, tag_count: usize) {
    let children = (0..tag_count)
        .map(|i| ChildSpec {
            label: format!("归档tag{i}"),
            value: format!("ctag{i}"),
            r#type: "float".into(),
            props: Some(TagProps {
                history: true,
                ..TagProps::default()
            }),
        })
        .collect();
    let _bindings = common::register_devices_as_models(
        client,
        "card_recalib_it",
        "dataSim",
        vec![DeviceSpec {
            label: "cardinality重校准e2e设备".into(),
            value: devid.into(),
            children,
            slave_id: None,
        }],
    )
    .await;
}

/// 场景一（压测回归，红）：空白名单启动引擎（auto 校准空表 → limit=1）→
/// 运行期经 store 路径建模 10 个 history=true tag → 10 个新 series 直写
/// **全部成功**。旧代码（仅启动校准一次）下第 2 个 series 起以
/// `CardinalityLimitExceeded` 被拒——正是压测实锤的"运行期建模全拒直到重启"。
#[tokio::test]
async fn runtime_whitelist_growth_hot_recalibrates_cardinality() {
    let _serial = serialize();

    let lux_dir = unique_dir("e2e-lux");
    let hist_dir = unique_dir("e2e-hist");
    let (lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 空白名单启动（压测首启场景：空库 → auto 校准 0 → 兜底 1）
    let cfg = HistoryConfig {
        enabled: true,
        data_dir: hist_dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    };
    let mut engine = maybe_init(&cfg, &HashMap::new())
        .expect("enabled=true 必须返回 Some")
        .expect("临时目录上引擎初始化必须成功");

    // 旧代码自证（红证据锚点）：空白名单下第 2 个 series 必被 limit=1 拒
    assert!(
        matches!(write_series(&engine, "c_pre_1", now_ms()), Ok(())),
        "limit=1 下第 1 个 series 必须成功"
    );
    assert!(
        matches!(
            write_series(&engine, "c_pre_2", now_ms() + 1),
            Err(TsinkError::CardinalityLimitExceeded { .. })
        ),
        "limit=1 下第 2 个 series 必被 CardinalityLimitExceeded 拒（启动校准仍生效）"
    );

    // 2. 运行期建模 10 个白名单 tag（store 写路径触发 reload → 联动）
    register_whitelisted_model(&client, "cdev1", 10).await;

    // 3. 10 个新 series 直写全部成功：重校准后 limit=ceil(10×1.2)=12
    //    （旧代码：limit 恒 1，第 2 个起全拒——本断言红）
    let base = now_ms() + 100;
    for i in 0..10usize {
        let res = write_series(&engine, &format!("c_hot_{i}"), base + i as i64);
        assert!(
            matches!(res, Ok(())),
            "白名单扩张后第 {} 个新 series 写入必须成功（热重校准只放大），实际: {:?}",
            i + 1,
            res.map_err(|e| e.to_string()).err()
        );
    }

    engine.close(Duration::from_secs(cfg.close_timeout_secs));
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = common::stop_lux(lux_handle).await;
    let _ = std::fs::remove_dir_all(&lux_dir);
}

/// 场景二（只放大钉子）：白名单缩小后 limit **不缩**——删设备 → reload →
/// effective 归零（auto 重算值=1），但已放大的 limit 保持，白名单外新
/// series 仍可入（错误实现缩回 1 会使写入被拒 → 红）。
#[tokio::test]
async fn whitelist_shrink_keeps_cardinality_limit() {
    let _serial = serialize();

    let lux_dir = unique_dir("e2e-lux");
    let hist_dir = unique_dir("e2e-hist");
    let (lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 先建模 10 tag 再启动（auto=12）
    register_whitelisted_model(&client, "cdev2", 10).await;
    let effective = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload effective 必须成功");
    assert_eq!(
        rs_iot::history::engine::auto_cardinality(&effective),
        12,
        "构造校验：10 白名单 tag → auto 校准 12"
    );

    let cfg = HistoryConfig {
        enabled: true,
        data_dir: hist_dir.to_string_lossy().into_owned(),
        ..HistoryConfig::default()
    };
    let mut engine = maybe_init(&cfg, &effective)
        .expect("enabled=true 必须返回 Some")
        .expect("临时目录上引擎初始化必须成功");

    // 2. 删设备 → 白名单归零（store 写路径触发 reload → 联动重算 auto=1）
    rs_iot::model::store::delete_device(&client, "cdev2")
        .await
        .expect("delete_device 必须成功");
    let effective_after = rs_iot::model::cache::reload(&client)
        .await
        .expect("reload 必须成功");
    assert!(
        effective_after.values().all(|p| !p.history),
        "构造校验：删除后白名单归零（auto 重算值=1 < 已设 12）"
    );

    // 3. limit 未缩：新 series 直写仍成功（缩到 1 会 CardinalityLimitExceeded）
    assert!(
        matches!(write_series(&engine, "c_shrink_probe", now_ms()), Ok(())),
        "白名单缩小后 limit 必须保持（只放大不缩小）——新 series 写入不得被拒"
    );

    engine.close(Duration::from_secs(cfg.close_timeout_secs));
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = common::stop_lux(lux_handle).await;
    let _ = std::fs::remove_dir_all(&lux_dir);
}

/// 场景三（显式配置钉子）：`cardinality_limit>0` 是运维硬上限，运行期
/// 白名单扩张**不**重校准（M0 止血方案即"显式配大值"，语义可预期不被
/// 运行期 auto 绕过）——建模 10 tag 后第 3 个 series 仍必须被拒。
#[tokio::test]
async fn explicit_cardinality_limit_not_recalibrated() {
    let _serial = serialize();

    let lux_dir = unique_dir("e2e-lux");
    let hist_dir = unique_dir("e2e-hist");
    let (_lux_handle, client) = common::start_lux(lux_dir.to_str().expect("utf8")).await;

    // 1. 显式 limit=2 + 空白名单启动
    let cfg = HistoryConfig {
        enabled: true,
        data_dir: hist_dir.to_string_lossy().into_owned(),
        cardinality_limit: 2,
        ..HistoryConfig::default()
    };
    let mut engine = maybe_init(&cfg, &HashMap::new())
        .expect("enabled=true 必须返回 Some")
        .expect("临时目录上引擎初始化必须成功");

    // 2. 运行期建模 10 tag（store 写路径触发 reload——显式模式联动 no-op）
    register_whitelisted_model(&client, "cdev3", 10).await;

    // 3. 显式上限保持：第 3 个 series 必须仍被 CardinalityLimitExceeded 拒
    assert!(matches!(write_series(&engine, "c_exp_1", now_ms()), Ok(())));
    assert!(matches!(
        write_series(&engine, "c_exp_2", now_ms() + 1),
        Ok(())
    ));
    assert!(
        matches!(
            write_series(&engine, "c_exp_3", now_ms() + 2),
            Err(TsinkError::CardinalityLimitExceeded { limit: 2, .. })
        ),
        "显式 cardinality_limit=2 不得被运行期 auto 重校准绕过（第 3 个 series 必拒）"
    );

    engine.close(Duration::from_secs(cfg.close_timeout_secs));
    let _ = std::fs::remove_dir_all(&hist_dir);
    let _ = std::fs::remove_dir_all(&lux_dir);
}
