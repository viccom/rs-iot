//! fan-out 出口行/旁路计数/发送句柄与白名单过滤（Task 4，P6）。
//!
//! 本文件**无条件编译、零 tsink 类型依赖**：`HistoryHandle` 被 pipeline 热路径
//! 持有（`HostContext.history`），若含 tsink 类型则 L1 裁剪（feature off）时
//! 调用点需要 cfg 散布——中间类型 `HistoryRow` 只用 `SampleValue`，热路径纯
//! Rust 可测（D4 类型保真在 writer 侧映射 tsink Value）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::types::{tag_key, Sample, SampleValue, TagProps};

/// fan-out 出口行：`fan_out` 产出、writer 线程消费的热路径中间类型
/// （刻意不含 tsink 类型，P6）。
///
/// `deadband` 在 fan-out 时从 effective 快照的 `TagProps` 取——writer 不回读
/// 全局缓存，快照与行一致（运行期改 deadband 对在途行不生效，下一批生效）。
#[derive(Debug, Clone)]
pub struct HistoryRow {
    /// 设备 ID（series label `devid`）。
    pub devid: String,
    /// 边缘端硬件 ID（series label `hwid`）。
    pub hwid: String,
    /// 标签原名（tsink metric；P1 非空即合法，不做字符替换）。
    pub tag: String,
    /// 毫秒时间戳（`Sample.timestamp` 秒 ×1000，D4）。
    pub ts_ms: i64,
    /// 采样值（writer 侧映射 tsink Value）。
    pub value: SampleValue,
    /// 绝对死区（`TagProps.deadband` 快照；None = 无死区）。
    pub deadband: Option<f64>,
}

/// 历史旁路计数：e2e 确定性与 Phase 3 metrics 的数据源。
///
/// 计数不变量：`sent + dropped + skipped` == fan_out 遍历的 (tag,value) 总数
/// ——每个出口恰好落入其一。`written/deadbanded/lane_mismatch/errors` 由
/// writer 线程在 flush 状态机中递增。
#[derive(Debug, Default)]
pub struct HistoryStats {
    /// fan_out try_send 成功行数。
    pub sent: AtomicU64,
    /// writer insert_rows 成功行数。
    pub written: AtomicU64,
    /// channel 满即丢（try_send 失败）行数，含 sender 全断后残留。
    pub dropped: AtomicU64,
    /// 死区抑制行数（P4 仅数值；last_val 不更新）。
    pub deadbanded: AtomicU64,
    /// 白名单外（tag 空/effective 缺卡/history!=true）行数（L3）。
    pub skipped: AtomicU64,
    /// lane 型变剔除行数（P3）。
    pub lane_mismatch: AtomicU64,
    /// insert_rows 失败行数（fail-soft 整批丢弃）。
    pub errors: AtomicU64,
}

/// fan-out 发送句柄：pipeline 侧持有，clone 语义与 tokio mpsc Sender 一致。
///
/// `send` 是 `try_send` 直通——有界 channel 满即丢（D3），绝不阻塞采集热路径。
#[derive(Debug)]
pub struct HistoryHandle {
    /// 有界 channel 发送端（drop 即触发 writer 排空退出）。
    tx: mpsc::Sender<HistoryRow>,
    /// 旁路计数共享句柄。
    pub stats: Arc<HistoryStats>,
}

/// `send` 失败原因（紧凑错误面）：行本身按 D3 语义即丢，不随错误回传
/// （tokio `TrySendError` 会携带整行回传，热路径错误面保持零载荷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFail {
    /// 有界 channel 已满（D3 满即丢主路径）。
    Full,
    /// 接收端已断（writer 线程退出后的残余发送）。
    Closed,
}

impl HistoryHandle {
    /// 组装句柄（tx 与 stats 由 spawn_writer 成对创建）。
    pub fn new(tx: mpsc::Sender<HistoryRow>, stats: Arc<HistoryStats>) -> Self {
        Self { tx, stats }
    }

    /// 旁路计数引用。
    pub fn stats(&self) -> &HistoryStats {
        &self.stats
    }

    /// 发送一行（`try_send` 直通，失败原因折叠为 [`SendFail`]）：
    /// 满即丢由调用方计数（fan_out 内置，独立调用方自行处理 `Err`）。
    pub fn send(&self, row: HistoryRow) -> Result<(), SendFail> {
        self.tx.try_send(row).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendFail::Full,
            mpsc::error::TrySendError::Closed(_) => SendFail::Closed,
        })
    }
}

/// pipeline `flush_batch` 出口旁路：白名单过滤 + 行构造 + 满即丢发送
/// （L3/D3/D4/D5/P1）。
///
/// 逐 Sample 逐 `(tag, value)`：tag 空名、effective 无 `tag_key(devid, tag)`
/// 卡、或 `history != true` 三类一律 `skipped++`（缺卡与显式 false 同语义，
/// history 默认 false 是安全默认）；白名单命中则构造 [`HistoryRow`]
/// （ts_ms = timestamp 秒 ×1000）经 `try_send` 直通——Ok 计 `sent`，
/// Err（满或断开）计 `dropped`，均不阻塞。
pub fn fan_out(samples: &[Sample], effective: &HashMap<String, TagProps>, h: &HistoryHandle) {
    for sample in samples {
        for (tag, value) in &sample.values {
            // L3 白名单三类前置过滤：空名 / 缺卡 / 未开归档
            let props = if tag.is_empty() {
                None
            } else {
                effective.get(&tag_key(&sample.devid, tag))
            };
            let props = match props {
                Some(props) if props.history => props,
                _ => {
                    h.stats.skipped.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
            };
            let row = HistoryRow {
                devid: sample.devid.clone(),
                hwid: sample.hwid.as_ref().to_owned(),
                tag: tag.clone(),
                ts_ms: sample.timestamp * 1000,
                value: value.clone(),
                deadband: props.deadband,
            };
            match h.send(row) {
                Ok(()) => {
                    h.stats.sent.fetch_add(1, Ordering::SeqCst);
                }
                Err(_) => {
                    h.stats.dropped.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use super::*;
    use crate::types::{tag_key, Hwid, Sample, SampleValue, TagProps};

    /// 零值计数器：字段名与全 pub 即 HistoryStats 的公共契约（无 Default 假设）。
    fn fresh_stats() -> Arc<HistoryStats> {
        Arc::new(HistoryStats {
            sent: AtomicU64::new(0),
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            deadbanded: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            lane_mismatch: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    /// 单样本构造：devid/hwid 固定，values 由 (tag, value) 列表填入。
    fn sample(devid: &str, hwid: &str, ts: i64, vals: &[(&str, SampleValue)]) -> Sample {
        Sample {
            hwid: Hwid::new(hwid),
            devid: devid.to_string(),
            values: vals
                .iter()
                .map(|(t, v)| ((*t).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
            timestamp: ts,
        }
    }

    /// 排空接收端（try_recv 至 Err 为止）。
    /// 红阶段构造缺陷修复(非断言,绿阶段代理显式报告):tokio Receiver::try_recv
    /// 要求 &mut self,参数按 &mut 收;断言零改动。
    fn drain(rx: &mut mpsc::Receiver<HistoryRow>) -> Vec<HistoryRow> {
        let mut rows = Vec::new();
        while let Ok(row) = rx.try_recv() {
            rows.push(row);
        }
        rows
    }

    /// 1. 白名单过滤 + 行字段映射：tagA（history=true）发出且字段逐项正确
    ///    （ts_ms=timestamp×1000、value 原样 Float、deadband 透传 None）；
    ///    tagB（history=false，effective 中存在但未开白名单）走 skip 路径——
    ///    与 effective 缺卡同一语义（history 默认 false）。
    #[test]
    fn fan_out_whitelist_only() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        let ts_a = 1_700_000_000i64;
        let ts_b = ts_a + 1;
        let samples = vec![
            sample("dev1", "hw1", ts_a, &[("tagA", SampleValue::Float(1.5))]),
            sample("dev1", "hw1", ts_b, &[("tagB", SampleValue::Float(2.5))]),
        ];
        let mut effective = HashMap::new();
        effective.insert(
            tag_key("dev1", "tagA"),
            TagProps {
                history: true,
                deadband: None,
                ..TagProps::default()
            },
        );
        effective.insert(
            tag_key("dev1", "tagB"),
            TagProps {
                history: false,
                ..TagProps::default()
            },
        );

        fan_out(&samples, &effective, &handle);

        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1, "只有白名单 tagA 恰好一行");
        let r = &rows[0];
        assert_eq!(r.devid, "dev1");
        assert_eq!(r.hwid, "hw1");
        assert_eq!(r.tag, "tagA");
        assert_eq!(r.ts_ms, ts_a * 1000, "ts_ms 必须是 timestamp 秒 ×1000");
        assert_eq!(r.value, SampleValue::Float(1.5));
        assert_eq!(r.deadband, None);

        assert_eq!(handle.stats().sent.load(Ordering::SeqCst), 1);
        assert_eq!(handle.stats().dropped.load(Ordering::SeqCst), 0);
        assert_eq!(
            handle.stats().skipped.load(Ordering::SeqCst),
            1,
            "history=false 的 tagB 与缺卡同走 skip 计数"
        );
    }

    /// 2. tag_key 构造一致性：tag 名已含 devid 前缀（"dev1.temp"）时 types::tag_key
    ///    原样返回（不重复拼接），effective 用该 key 建卡即可命中白名单；
    ///    行内 tag 保持 tag 原名（P1 metric=原名），deadband=Some 透传到行。
    #[test]
    fn fan_out_tag_key_matching() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        // 语义锚点：types::tag_key 对已含 devid 的 tag 原样返回
        assert_eq!(tag_key("dev1", "dev1.temp"), "dev1.temp");

        let ts = 1_700_000_000i64;
        let samples = vec![sample(
            "dev1",
            "hw1",
            ts,
            &[("dev1.temp", SampleValue::Float(3.25))],
        )];
        let mut effective = HashMap::new();
        effective.insert(
            tag_key("dev1", "dev1.temp"),
            TagProps {
                history: true,
                deadband: Some(0.5),
                ..TagProps::default()
            },
        );

        fan_out(&samples, &effective, &handle);

        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1, "key 命中白名单必须发出");
        assert_eq!(rows[0].tag, "dev1.temp", "行内 tag 保持原名（非 tag_key）");
        assert_eq!(rows[0].ts_ms, ts * 1000);
        assert_eq!(rows[0].value, SampleValue::Float(3.25));
        assert_eq!(rows[0].deadband, Some(0.5), "deadband 从 TagProps 透传到行");
        assert_eq!(handle.stats().sent.load(Ordering::SeqCst), 1);
        assert_eq!(handle.stats().skipped.load(Ordering::SeqCst), 0);
    }

    /// 3. 满即丢：cap=1 的 channel 预填 1 行后 fan_out 发 3 个白名单 tag——
    ///    全部 try_send 失败：dropped==3、sent==0（dropped 不计 sent）、
    ///    skipped==0（白名单命中不落 skip）；接收端只有预填行。
    #[test]
    fn fan_out_full_channel_drops() {
        let (tx, mut rx) = mpsc::channel(1);
        // 红阶段构造缺陷修复(非断言,绿阶段代理显式报告):下方预填需继续用 tx,
        // 而 new(tx) 按值移动——改传 tx.clone()(同一 channel,容量语义不变),
        // 使构造与断言意图一致。断言零改动。
        let handle = HistoryHandle::new(tx.clone(), fresh_stats());

        // 预填占满容量（空 channel 上 try_send 必成功）
        tx.try_send(HistoryRow {
            devid: "dev1".into(),
            hwid: "hw1".into(),
            tag: "prefill".into(),
            ts_ms: 1,
            value: SampleValue::Float(0.0),
            deadband: None,
        })
        .expect("空 channel 预填 1 行必须成功");

        let ts = 1_700_000_000i64;
        let samples = vec![sample(
            "dev1",
            "hw1",
            ts,
            &[
                ("tagA", SampleValue::Float(1.0)),
                ("tagB", SampleValue::Float(2.0)),
                ("tagC", SampleValue::Float(3.0)),
            ],
        )];
        let mut effective = HashMap::new();
        for tag in ["tagA", "tagB", "tagC"] {
            effective.insert(
                tag_key("dev1", tag),
                TagProps {
                    history: true,
                    ..TagProps::default()
                },
            );
        }

        fan_out(&samples, &effective, &handle);

        assert_eq!(handle.stats().dropped.load(Ordering::SeqCst), 3);
        assert_eq!(
            handle.stats().sent.load(Ordering::SeqCst),
            0,
            "dropped 不计 sent"
        );
        assert_eq!(handle.stats().skipped.load(Ordering::SeqCst), 0);
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1, "接收端只剩预填行");
        assert_eq!(rows[0].tag, "prefill");
    }

    /// 4. 空 tag 名跳过（P1：非空即合法，空名退化为跳过+计数）：
    ///    空 tag 无从白名单（effective 空），rx 无行、skipped==1、sent==0。
    #[test]
    fn fan_out_empty_tag_skipped() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        let samples = vec![sample(
            "dev1",
            "hw1",
            1_700_000_000i64,
            &[("", SampleValue::Float(1.0))],
        )];
        let effective = HashMap::new();

        fan_out(&samples, &effective, &handle);

        assert!(drain(&mut rx).is_empty(), "空 tag 名不得产生任何行");
        assert_eq!(handle.stats().skipped.load(Ordering::SeqCst), 1);
        assert_eq!(handle.stats().sent.load(Ordering::SeqCst), 0);
        assert_eq!(handle.stats().dropped.load(Ordering::SeqCst), 0);
    }
}
