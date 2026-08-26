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

/// fan_out 时界窗下界回看（D-C，毫秒口径）：早于 now−7d 的 ts 拒收——工业采集
/// 正常抖动远小于窗；现场回补历史是显式运维动作，可走 tsink 工具不走本写路径。
const TS_WINDOW_PAST_MS: i64 = 7 * 86_400_000;
/// fan_out 时界窗上界前瞻（D-C，毫秒口径）：晚于 now+1h 的 ts 拒收（容采集端
/// 小幅时钟超前；大幅超前视为错配/恶意）。
const TS_WINDOW_FUTURE_MS: i64 = 3_600_000;
/// fan_out 长度预检上限（M2，UTF-8 字节）：tag ≤ 64KiB——`String::len` 即
/// 字节长；超长行单行拒收计 `len_rejected`，不再以 tsink 拒整批方式连坐。
const TAG_MAX_BYTES: usize = 65_535;
/// fan_out 长度预检上限（M2，UTF-8 字节）：devid/hwid series label ≤ 16KiB。
const LABEL_MAX_BYTES: usize = 16 * 1024;

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
/// 计数不变量：`sent + dropped + skipped + ts_rejected + len_rejected` ==
/// fan_out 遍历的 (tag,value) 总数——每个出口恰好落入其一。`written/deadbanded/
/// deadband_evaluated/lane_mismatch/errors/nonfinite_skipped/dup_ts_skipped`
/// 由 writer 线程在 flush 状态机中递增。
///
/// Phase 3 治理口径（P2/P3）：死区削减率分母 `deadband_evaluated` 与分子
/// `deadbanded` 同源同点计数；写错误按行分桶（[`WriteErrorKind`]），
/// 不变量 `Σ(kind 槽) == errors`——由 [`HistoryStats::record_error_rows`]
/// 单入口保证（writer 不再直接 fetch_add `errors`）。
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
    /// 死区评估行数（P2：deadband=Some 标签的数值点，与 deadbanded 分子同源）。
    pub deadband_evaluated: AtomicU64,
    /// 白名单外（tag 空/effective 缺卡/history!=true）行数（L3）。
    pub skipped: AtomicU64,
    /// lane 型变剔除行数（P3）。
    pub lane_mismatch: AtomicU64,
    /// insert_rows 失败行数（fail-soft 整批丢弃）。
    pub errors: AtomicU64,
    /// NaN/Inf 非有限值写侧拒收行数（H1/D-A：坏质量位非有效测量值，不落盘、
    /// 不毒化 last_val——NaN 参与死区比较恒 false 会使死区永久失效）。
    pub nonfinite_skipped: AtomicU64,
    /// 跨批同 series 同 ts 跳过行数（P5 防双点；M11：计数消除对账黑洞）。
    pub dup_ts_skipped: AtomicU64,
    /// 时间戳域坏行数（M1/D-C：`ts×1000` 溢出或超出 `[now−7d, now+1h]`
    /// 时界窗——坏 ts 单行拒收，不再连坐整批）。
    pub ts_rejected: AtomicU64,
    /// 超长行数（M2：tag > 64KiB / devid、hwid > 16KiB（UTF-8 字节）——
    /// 超长行单行拒收，不再连坐整批）。
    pub len_rejected: AtomicU64,
    /// 写错误分桶行数（P3 按 kind 计数，Σ == errors；顺序同 [`WriteErrorKind::ALL`]）。
    pub error_rows: [AtomicU64; 5],
}

/// 写失败分型桶（writer.rs `classify_err` 产出；计数按行，Σkind == errors）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteErrorKind {
    /// tsink 内存预算超限（`MemoryBudgetExceeded`）。
    MemoryBudget,
    /// series 基数超限（`CardinalityLimitExceeded`）。
    Cardinality,
    /// WAL 大小/磁盘空间不足（`WalSizeLimitExceeded` ∪ `InsufficientDiskSpace`）。
    WalDisk,
    /// 存储关闭/关停中（`StorageClosed` ∪ `StorageShuttingDown`）。
    Closed,
    /// 其余错误（Io、损坏等）。
    Other,
}

impl WriteErrorKind {
    /// 全部五桶（顺序即 [`HistoryStats::error_rows`] 下标序，metrics 渲染遍历用）。
    pub const ALL: [WriteErrorKind; 5] = [
        Self::MemoryBudget,
        Self::Cardinality,
        Self::WalDisk,
        Self::Closed,
        Self::Other,
    ];

    /// 桶名（metrics label 口径：小写下划线，P1/P3）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MemoryBudget => "memory_budget",
            Self::Cardinality => "cardinality",
            Self::WalDisk => "wal_disk",
            Self::Closed => "closed",
            Self::Other => "other",
        }
    }

    /// [`HistoryStats::error_rows`] 桽数组下标（`ALL` 顺序即下标序）。
    fn index(self) -> usize {
        match self {
            Self::MemoryBudget => 0,
            Self::Cardinality => 1,
            Self::WalDisk => 2,
            Self::Closed => 3,
            Self::Other => 4,
        }
    }
}

impl HistoryStats {
    /// 记 n 行写失败（P3 单入口：`errors` 与 kind 槽同增，Σkind == errors
    /// 不变量由此保证——writer 失败路径不直接 fetch_add `errors`）。
    pub fn record_error_rows(&self, kind: WriteErrorKind, n: u64) {
        self.errors.fetch_add(n, Ordering::SeqCst);
        self.error_rows[kind.index()].fetch_add(n, Ordering::SeqCst);
    }

    /// 读 kind 桶累计行数（渲染/测试对账用）。
    pub fn error_rows_of(&self, kind: WriteErrorKind) -> u64 {
        self.error_rows[kind.index()].load(Ordering::SeqCst)
    }
}

/// fan-out 发送句柄：pipeline 侧持有，clone 语义与 tokio mpsc Sender 一致。
///
/// `send` 是 `try_send` 直通——有界 channel 满即丢（D3），绝不阻塞采集热路径。
///
/// `Clone` 语义与 tokio mpsc `Sender` 一致（Task 5：engine 持原件、`handle()`
/// clone 交给 `HostContext.history`，pipeline 侧 clone 随 worker fan-out）。
#[derive(Debug, Clone)]
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

    /// channel 是否已满（P11 饱和快速路径）：剩余容量为 0 即满。
    ///
    /// 语义：容量探测仅是发送前的**先验短路**——探测与实际 `try_send` 之间
    /// 无锁同步（writer 并发消费会释放容量），结果可能瞬时过时；调用方
    /// （`fan_out`）以满探测先行计 `dropped` 并跳过整行构造，后续行的
    /// `try_send` 仍以自身结果为准。
    pub(crate) fn is_full(&self) -> bool {
        self.tx.capacity() == 0
    }
}

/// pipeline `flush_batch` 出口旁路：白名单过滤 + 行构造 + 满即丢发送
/// （L3/D3/D4/D5/P1/P11）。
///
/// 逐 Sample 逐 `(tag, value)`：**P11 饱和快速路径先行**——channel 满时
/// 该行直接计 `dropped` 并跳过（不构造 tag_key、不判白名单、不构造行，
/// 零分配）；即满时非白名单行也计 `dropped`，仅非饱和路径区分 `skipped`。
/// 非饱和时按 L3 三类前置过滤：tag 空名、effective 无 `tag_key(devid, tag)`
/// 卡、或 `history != true` 一律 `skipped++`（缺卡与显式 false 同语义，
/// history 默认 false 是安全默认）；白名单命中后再过 **M1 时界钳制**：
/// `timestamp.checked_mul(1000)` 溢出或 `ts_ms` 落在 `[now_ms−7d,
/// now_ms+1h]`（D-C 窗，批首取一次 now）之外 → `ts_rejected++` 单行拒收
/// ——坏时间戳不再以 tsink 拒整批的方式连坐同批好行；随后 **M2 长度预检**：
/// tag > 64KiB / devid、hwid > 16KiB（UTF-8 字节）→ `len_rejected++` 单行
/// 拒收（同理防连坐）；通过者构造 [`HistoryRow`] 经 `try_send` 直通——Ok 计
/// `sent`，Err（满或断开）计 `dropped`，均不阻塞。
///
/// 计数本地累积（P11）：五计数在循环内只动本地变量，循环外每 flush
/// 一次性原子合并（SeqCst 保持），消除逐行原子争用。
pub fn fan_out(samples: &[Sample], effective: &HashMap<String, TagProps>, h: &HistoryHandle) {
    let (mut sent, mut dropped, mut skipped, mut ts_rejected, mut len_rejected) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    // 批首一次 now_ms（D-C）：整批同一时间基，行间判定不因取时刻漂移抖动；
    // 时钟异常（epoch 前）取 0 → 所有行落窗外拒收，fail-soft 且 ts_rejected 可观测。
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    for sample in samples {
        for (tag, value) in &sample.values {
            // P11 饱和快速路径：满即丢先行，白名单判定/行构造整体短路
            if h.is_full() {
                dropped += 1;
                continue;
            }
            // L3 白名单三类前置过滤：空名 / 缺卡 / 未开归档
            let props = if tag.is_empty() {
                None
            } else {
                effective.get(&tag_key(&sample.devid, tag))
            };
            let props = match props {
                Some(props) if props.history => props,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            // M1 时界钳制：ts×1000 溢出或超出 [now−7d, now+1h] 单行拒收
            let ts_ms = match sample.timestamp.checked_mul(1000) {
                Some(ts) if ts >= now_ms - TS_WINDOW_PAST_MS && ts <= now_ms + TS_WINDOW_FUTURE_MS => ts,
                _ => {
                    ts_rejected += 1;
                    continue;
                }
            };
            // M2 长度预检：tag/devid/hwid 超长（UTF-8 字节）单行拒收——
            // tsink 对超长行拒整批，写入侧预检避免连坐同批好行。
            if tag.len() > TAG_MAX_BYTES
                || sample.devid.len() > LABEL_MAX_BYTES
                || sample.hwid.as_ref().len() > LABEL_MAX_BYTES
            {
                len_rejected += 1;
                continue;
            }
            let row = HistoryRow {
                devid: sample.devid.clone(),
                hwid: sample.hwid.as_ref().to_owned(),
                tag: tag.clone(),
                ts_ms,
                value: value.clone(),
                deadband: props.deadband,
            };
            match h.send(row) {
                Ok(()) => sent += 1,
                Err(_) => dropped += 1,
            }
        }
    }
    let stats = &h.stats;
    stats.sent.fetch_add(sent, Ordering::SeqCst);
    stats.dropped.fetch_add(dropped, Ordering::SeqCst);
    stats.skipped.fetch_add(skipped, Ordering::SeqCst);
    stats.ts_rejected.fetch_add(ts_rejected, Ordering::SeqCst);
    stats.len_rejected.fetch_add(len_rejected, Ordering::SeqCst);
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
    /// 绿阶段仅扩构造补新字段（deadband_evaluated/error_rows/nonfinite_skipped/
    /// dup_ts_skipped），断言零改动。
    fn fresh_stats() -> Arc<HistoryStats> {
        Arc::new(HistoryStats {
            sent: AtomicU64::new(0),
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            deadbanded: AtomicU64::new(0),
            deadband_evaluated: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            lane_mismatch: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            nonfinite_skipped: AtomicU64::new(0),
            dup_ts_skipped: AtomicU64::new(0),
            ts_rejected: AtomicU64::new(0),
            len_rejected: AtomicU64::new(0),
            error_rows: std::array::from_fn(|_| AtomicU64::new(0)),
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

    /// 当前 Unix 秒（M1 时界窗后测试取当前时刻附近的 ts，writer.rs 同款）。
    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_secs() as i64
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
    ///    ts 取当前时刻（M1 时界窗 [now−7d, now+1h] 后固定历史 ts 会被拒收，
    ///    本测锚定白名单/字段映射而非时界——时界另见测试 7）。
    #[test]
    fn fan_out_whitelist_only() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        let ts_a = now_secs();
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
    ///    ts 取当前时刻（M1 时界窗，同测试 1 说明）。
    #[test]
    fn fan_out_tag_key_matching() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        // 语义锚点：types::tag_key 对已含 devid 的 tag 原样返回
        assert_eq!(tag_key("dev1", "dev1.temp"), "dev1.temp");

        let ts = now_secs();
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

    /// 5. 饱和快速路径（P11）：cap=1 预填满后发 2 行——tagA（history=true
    ///    白名单）与 tagB（history=false 非白名单）。新语义：满即丢先行，
    ///    该行直接计 dropped，不再判定白名单（不构造 tag_key/不构造行）——
    ///    dropped==2、skipped==0、sent==0；接收端只剩预填行。
    ///    现实现白名单判定先于容量检查：tagA 走 dropped==1、tagB 走
    ///    skipped==1——本测试锁定该行为翻转（R8 根因修复）。
    #[test]
    fn fan_out_saturation_short_circuit() {
        let (tx, mut rx) = mpsc::channel(1);
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
            ],
        )];
        let mut effective = HashMap::new();
        effective.insert(
            tag_key("dev1", "tagA"),
            TagProps {
                history: true,
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

        assert_eq!(
            handle.stats().dropped.load(Ordering::SeqCst),
            2,
            "满即丢先行:白名单 tagA 与非白名单 tagB 都直接计 dropped"
        );
        assert_eq!(
            handle.stats().skipped.load(Ordering::SeqCst),
            0,
            "饱和行不再判定白名单,tagB 不单独落 skipped"
        );
        assert_eq!(
            handle.stats().sent.load(Ordering::SeqCst),
            0,
            "dropped 不计 sent"
        );
        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1, "接收端只剩预填行");
        assert_eq!(rows[0].tag, "prefill");
    }

    /// 6. 写错误分型不变量（P3）：`record_error_rows(kind, n)` 同时增 `errors`
    ///    与对应 kind 槽——任意调用序列后 Σ(kind 槽) == errors 总数（按行计数
    ///    可对账）；`WriteErrorKind::ALL` 恰五桶且 `as_str()` 与 P1/P3 的
    ///    metrics label 口径一致。
    #[test]
    fn error_kind_invariant() {
        let stats = fresh_stats();

        stats.record_error_rows(WriteErrorKind::MemoryBudget, 3);
        stats.record_error_rows(WriteErrorKind::Other, 2);

        assert_eq!(
            stats.errors.load(Ordering::SeqCst),
            5,
            "record_error_rows 必须同时增 errors 总数（3+2）"
        );
        assert_eq!(
            stats.error_rows_of(WriteErrorKind::MemoryBudget),
            3,
            "kind 槽必须记入 MemoryBudget 行数"
        );
        assert_eq!(
            stats.error_rows_of(WriteErrorKind::Other),
            2,
            "kind 槽必须记入 Other 行数"
        );
        assert_eq!(
            stats.error_rows_of(WriteErrorKind::Cardinality),
            0,
            "未记入的 kind 槽保持零"
        );

        // 不变量：Σ(kind 槽) == errors（P3 分型和可对账）
        let per_kind = WriteErrorKind::ALL.map(|k| stats.error_rows_of(k));
        let sum: u64 = per_kind.iter().sum();
        assert_eq!(
            sum,
            stats.errors.load(Ordering::SeqCst),
            "不变量：Σkind 槽 == errors 总数"
        );

        // 五桶 as_str 精确锁定（与 /metrics 的 kind label 口径一致）
        let strs: Vec<&str> = WriteErrorKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            strs,
            [
                "memory_budget",
                "cardinality",
                "wal_disk",
                "closed",
                "other"
            ],
            "ALL 五桶顺序与 as_str 字符串必须精确"
        );
    }

    /// 7. fan_out 时界钳制（M1/D-C）：白名单命中但 timestamp 域坏的三行——
    ///    i64::MAX（×1000 溢出）、now−8d（早于 7d 下界）→ ts_rejected==2 且
    ///    均不发送；now−1h（窗内）→ 正常发送（sent==1、行 ts 精确映射）。
    ///    当前实现对前两者照发（红：rows.len()==3 / ts_rejected==0）。
    #[test]
    fn fan_out_timestamp_window_rejects() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_secs() as i64;
        let samples = vec![
            sample("dev1", "hw1", i64::MAX, &[("tagA", SampleValue::Float(1.0))]),
            sample("dev1", "hw1", now_s - 8 * 86400, &[("tagA", SampleValue::Float(2.0))]),
            sample("dev1", "hw1", now_s - 3600, &[("tagA", SampleValue::Float(3.0))]),
        ];
        let mut effective = HashMap::new();
        effective.insert(
            tag_key("dev1", "tagA"),
            TagProps {
                history: true,
                ..TagProps::default()
            },
        );

        fan_out(&samples, &effective, &handle);

        let rows = drain(&mut rx);
        assert_eq!(
            rows.len(),
            1,
            "溢出与 now−8d 两行必须拒收，只有窗内 now−1h 一行发出"
        );
        assert_eq!(rows[0].ts_ms, (now_s - 3600) * 1000, "窗内行 ts 精确映射");
        assert_eq!(handle.stats().ts_rejected.load(Ordering::SeqCst), 2, "i64::MAX 溢出与 now−8d 两行必须计 ts_rejected");
        assert_eq!(handle.stats().sent.load(Ordering::SeqCst), 1);
        assert_eq!(handle.stats().dropped.load(Ordering::SeqCst), 0);
        assert_eq!(handle.stats().skipped.load(Ordering::SeqCst), 0);
    }

    /// 8. fan_out 长度预检（M2）：白名单命中后 tag 超长（70KiB > 64KiB 上限）
    ///    → len_rejected==1 且不发送——超长行不再以 tsink 拒整批方式连坐
    ///    同批好行；同批窗内正常 tag 照常发送。当前实现照发（红：len_rejected==0）。
    #[test]
    fn fan_out_oversize_tag_rejected() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = HistoryHandle::new(tx, fresh_stats());

        let big_tag = "x".repeat(70 * 1024);
        let now = now_secs();
        let samples = vec![
            sample("dev1", "hw1", now, &[(&big_tag, SampleValue::Float(1.0))]),
            sample("dev1", "hw1", now, &[("ok_tag", SampleValue::Float(2.0))]),
        ];
        let mut effective = HashMap::new();
        effective.insert(
            tag_key("dev1", &big_tag),
            TagProps {
                history: true,
                ..TagProps::default()
            },
        );
        effective.insert(
            tag_key("dev1", "ok_tag"),
            TagProps {
                history: true,
                ..TagProps::default()
            },
        );

        fan_out(&samples, &effective, &handle);

        let rows = drain(&mut rx);
        assert_eq!(rows.len(), 1, "超长 tag 单行拒收，正常 tag 照发不连坐");
        assert_eq!(rows[0].tag, "ok_tag");
        assert_eq!(
            handle.stats().len_rejected.load(Ordering::SeqCst),
            1,
            "超长 tag 必须计 len_rejected"
        );
        assert_eq!(handle.stats().sent.load(Ordering::SeqCst), 1);
        assert_eq!(handle.stats().ts_rejected.load(Ordering::SeqCst), 0);
        assert_eq!(handle.stats().skipped.load(Ordering::SeqCst), 0);
    }
}
