//! fan-out 出口行/旁路计数/发送句柄与白名单过滤（Task 4，P6）。
//!
//! 本文件**无条件编译、零 tsink 类型依赖**：`HistoryHandle` 被 pipeline 热路径
//! 持有（`HostContext.history`），若含 tsink 类型则 L1 裁剪（feature off）时
//! 调用点需要 cfg 散布——中间类型 `HistoryRow` 只用 `SampleValue`，热路径纯
//! Rust 可测（D4 类型保真在 writer 侧映射 tsink Value）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// 跨批回卷严格更旧 ts 丢弃行数（D-T3：迟到样本不落盘，语义变更回退
    /// revert；闭合"回卷后再卷"双点——max 方案不成立，复核测试实证）。
    pub stale_ts_dropped: AtomicU64,
    /// 时间戳域坏行数（M1/D-C：`ts×1000` 溢出或超出 `[now−7d, now+1h]`
    /// 时界窗——坏 ts 单行拒收，不再连坐整批）。
    pub ts_rejected: AtomicU64,
    /// 超长行数（M2：tag > 64KiB / devid、hwid > 16KiB（UTF-8 字节）——
    /// 超长行单行拒收，不再连坐整批）。
    pub len_rejected: AtomicU64,
    /// 写错误分桶行数（P3 按 kind 计数，Σ == errors；顺序同 [`WriteErrorKind::ALL`]）。
    pub error_rows: [AtomicU64; 5],
    /// Closed 桶首增锁存（P4 僵尸上行，运行态可变载体）：writer 首次记入
    /// Closed 桶（StorageClosed/StorageShuttingDown——引擎关停/僵尸）时置位，
    /// **一次为真永不复位**（引擎无自愈路径）。sysstatus 据此上行
    /// `degraded: true`（[`crate::api`] history 块新增字段，静态三态 `engine`
    /// 不动）；由 [`HistoryStats::record_error_rows`] 单入口置位，writer 不
    /// 直接 store。
    pub closed_latched: AtomicBool,
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

    /// [`HistoryStats::error_rows`] 桶数组下标。派生自 [`WriteErrorKind::ALL`]
    /// 的位置（审查 L4：手写 match 与 ALL 字面量是两处独立硬编码，错位时
    /// 记数/读数闭环自洽不报警——单源派生后只剩 ALL 一处序）。
    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|k| *k == self)
            .expect("ALL 覆盖全部变体（枚举定义穷尽性保证）")
    }
}

impl HistoryStats {
    /// 记 n 行写失败（P3 单入口：`errors` 与 kind 槽同增，Σkind == errors
    /// 不变量由此保证——writer 失败路径不直接 fetch_add `errors`）。
    ///
    /// 返回值（P4 僵尸上行）：本次记录是否伴随 **Closed 桶首增**（此前 Closed
    /// 计数为 0）——true 时同步置位 [`HistoryStats::closed_latched`]，writer
    /// 侧据此打一次性 ERROR 告警日志（一条，不刷屏）；非首增或非 Closed 恒
    /// false。
    pub fn record_error_rows(&self, kind: WriteErrorKind, n: u64) -> bool {
        self.errors.fetch_add(n, Ordering::SeqCst);
        let prev = self.error_rows[kind.index()].fetch_add(n, Ordering::SeqCst);
        let first_closed = kind == WriteErrorKind::Closed && prev == 0;
        if first_closed {
            self.closed_latched.store(true, Ordering::SeqCst);
        }
        first_closed
    }

    /// 读 kind 桶累计行数（渲染/测试对账用）。
    pub fn error_rows_of(&self, kind: WriteErrorKind) -> u64 {
        self.error_rows[kind.index()].load(Ordering::SeqCst)
    }
}

/// series 分片路由（T14）：对 (devid, tag) 二元组做 FNV-1a 64 位散布——
/// 长度前缀注入使 ("a","bc") 与 ("ab","c") 字节流不同（二元组语义，任一
/// 维折叠都会碰撞）；FNV-1a 常数无随机种子，同 series 恒同分片（亲和的
/// 根基：死区/lane/乱序状态机的单写者全序），散布对高基数 series 域近似
/// 均匀。`shards<=1` 恒 0（单分片=单写线程现状）。
pub(crate) fn shard_route(devid: &str, tag: &str, shards: usize) -> usize {
    if shards <= 1 {
        return 0;
    }
    /// FNV-1a 64 位标准素数。
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    /// FNV-1a 64 位 offset basis。
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    fn mix(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h ^= u64::from(b);
            *h = h.wrapping_mul(FNV_PRIME);
        }
    }
    let mut h = FNV_OFFSET;
    mix(&mut h, &(devid.len() as u64).to_le_bytes());
    mix(&mut h, devid.as_bytes());
    mix(&mut h, &(tag.len() as u64).to_le_bytes());
    mix(&mut h, tag.as_bytes());
    (h % shards as u64) as usize
}

/// fan-out 发送句柄：pipeline 侧持有，clone 语义与 tokio mpsc Sender 一致。
///
/// `send` 是 `try_send` 直通——有界 channel 满即丢（D3），绝不阻塞采集热路径。
/// T14 多写线程分片：句柄持有**每分片**一个 channel 发送端，`send` 按
/// [`shard_route`] 把行路由到其 series 的亲和分片（同 series 恒同 writer）；
/// 满即丢按**被路由分片**的容量计。
///
/// `Clone` 语义与 tokio mpsc `Sender` 一致（Task 5：engine 持原件、`handle()`
/// clone 交给 `HostContext.history`，pipeline 侧 clone 随 worker fan-out）——
/// clone 复制整个分片面，任一 clone 存活则全部分片保持连接。
#[derive(Debug, Clone)]
pub struct HistoryHandle {
    /// 每分片有界 channel 发送端（分片数 = writer_threads，启动时固定；
    /// 全部 clone drop 才触发对应 writer 排空退出）。
    txs: Vec<mpsc::Sender<HistoryRow>>,
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
    /// 组装句柄（txs 与 stats 由 spawn_writer 成对创建；分片数 = txs.len()）。
    pub fn new(txs: Vec<mpsc::Sender<HistoryRow>>, stats: Arc<HistoryStats>) -> Self {
        Self { txs, stats }
    }

    /// 旁路计数引用。
    pub fn stats(&self) -> &HistoryStats {
        &self.stats
    }

    /// 发送一行（series 亲和路由 + `try_send` 直通，失败原因折叠为
    /// [`SendFail`]）：路由到 `(devid, tag)` 的亲和分片，满即丢由调用方
    /// 计数（fan_out 内置，独立调用方自行处理 `Err`）。
    pub fn send(&self, row: HistoryRow) -> Result<(), SendFail> {
        let shard = shard_route(&row.devid, &row.tag, self.txs.len());
        self.txs[shard].try_send(row).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendFail::Full,
            mpsc::error::TrySendError::Closed(_) => SendFail::Closed,
        })
    }

    /// channel 是否已满（P11 饱和快速路径）：**全部分片**剩余容量为 0 才算
    /// 满——单片满不算（其它 series 的路由片仍有容量，短路不得误丢）；
    /// 单分片（writer_threads=1）退化为单 channel 语义（现状）。
    ///
    /// 语义：容量探测仅是发送前的**先验短路**——探测与实际 `try_send` 之间
    /// 无锁同步（writer 并发消费会释放容量），结果可能瞬时过时；调用方
    /// （`fan_out`）以满探测先行计 `dropped` 并跳过整行构造，后续行的
    /// `try_send` 仍以自身结果为准。
    pub(crate) fn is_full(&self) -> bool {
        self.txs.iter().all(|tx| tx.capacity() == 0)
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
            // P11 饱和快速路径：满即丢先行（T14：全部分片皆满才短路——单片
            // 满由 send 的路由 try_send 结果计 dropped），白名单判定/行构造
            // 整体短路
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
                Some(ts)
                    if ts >= now_ms - TS_WINDOW_PAST_MS && ts <= now_ms + TS_WINDOW_FUTURE_MS =>
                {
                    ts
                }
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

    /// index() 与 ALL 序等价守门（审查 L4，D-F 预期即绿）：派生重构后下标
    /// 序 === ALL 字面量序——错位时 record_error_rows 写错桶而读数闭环自洽，
    /// 本测是唯一报警点。
    #[test]
    fn write_error_kind_index_matches_all_order() {
        for (i, k) in WriteErrorKind::ALL.iter().enumerate() {
            assert_eq!(k.index(), i, "{k:?} 下标必须等于 ALL 序 {i}");
        }
    }

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
            stale_ts_dropped: AtomicU64::new(0),
            ts_rejected: AtomicU64::new(0),
            len_rejected: AtomicU64::new(0),
            error_rows: std::array::from_fn(|_| AtomicU64::new(0)),
            closed_latched: std::sync::atomic::AtomicBool::new(false),
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
        let handle = HistoryHandle::new(vec![tx], fresh_stats());

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
        let handle = HistoryHandle::new(vec![tx], fresh_stats());

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
        let handle = HistoryHandle::new(vec![tx.clone()], fresh_stats());

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
        let handle = HistoryHandle::new(vec![tx], fresh_stats());

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
        let handle = HistoryHandle::new(vec![tx.clone()], fresh_stats());

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
        let handle = HistoryHandle::new(vec![tx], fresh_stats());

        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_secs() as i64;
        let samples = vec![
            sample(
                "dev1",
                "hw1",
                i64::MAX,
                &[("tagA", SampleValue::Float(1.0))],
            ),
            sample(
                "dev1",
                "hw1",
                now_s - 8 * 86400,
                &[("tagA", SampleValue::Float(2.0))],
            ),
            sample(
                "dev1",
                "hw1",
                now_s - 3600,
                &[("tagA", SampleValue::Float(3.0))],
            ),
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
        assert_eq!(
            handle.stats().ts_rejected.load(Ordering::SeqCst),
            2,
            "i64::MAX 溢出与 now−8d 两行必须计 ts_rejected"
        );
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
        let handle = HistoryHandle::new(vec![tx], fresh_stats());

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

    /// 9. Closed 桶首增一次性锁存（P4 僵尸上行）：`record_error_rows` 首次
    ///    记入 Closed 桶（此前计数为 0）返回 true 且 `closed_latched` 置位
    ///    （writer 据此打一次性 ERROR，sysstatus 据此上行 degraded）；后续
    ///    Closed 返回 false（不重复告警）；非 Closed kind 首增也不触发——
    ///    StorageClosed/StorageShuttingDown 意味着引擎关停/僵尸，其余错误
    ///    桶不代表引擎死亡。
    #[test]
    fn record_error_rows_first_closed_latch() {
        let stats = fresh_stats();
        assert!(!stats.closed_latched.load(Ordering::SeqCst), "初始未锁存");
        assert!(
            !stats.record_error_rows(WriteErrorKind::Cardinality, 1),
            "非 Closed 首增不得触发"
        );
        assert!(
            !stats.closed_latched.load(Ordering::SeqCst),
            "Cardinality 错误不得锁存运行态"
        );
        assert!(
            stats.record_error_rows(WriteErrorKind::Closed, 5),
            "Closed 桶首增（此前计数为 0）必须返回 true"
        );
        assert!(
            stats.closed_latched.load(Ordering::SeqCst),
            "Closed 首增必须置位 closed_latched（sysstatus degraded 上行源）"
        );
        assert!(
            !stats.record_error_rows(WriteErrorKind::Closed, 3),
            "Closed 非首增必须返回 false（一次性告警，不刷屏）"
        );
        assert!(
            stats.closed_latched.load(Ordering::SeqCst),
            "锁存一次为真永不复位（引擎无自愈路径）"
        );
        assert_eq!(stats.error_rows_of(WriteErrorKind::Closed), 8);
        assert_eq!(
            stats.errors.load(Ordering::SeqCst),
            9,
            "计数不变量保持：1+5+3"
        );
    }

    /// T14 分片路由：同 series 恒同分片（确定性）+ 域界内 + 单分片恒 0。
    #[test]
    fn shard_route_stable_within_bounds() {
        for (devid, tag) in [("dev1", "tA"), ("dev2", "temp"), ("", "x"), ("d", "")] {
            let first = shard_route(devid, tag, 4);
            // 稳定性：重复调用恒同值（series 亲和的根基）
            for _ in 0..3 {
                assert_eq!(
                    shard_route(devid, tag, 4),
                    first,
                    "({devid},{tag}) 路由必须恒定"
                );
            }
            assert!(first < 4, "路由结果必须落在分片域内");
        }
        // 单分片（默认 1 写线程）恒路由 0
        assert_eq!(shard_route("dev1", "tA", 1), 0);
        assert_eq!(shard_route("任意", "任意", 1), 0);
    }

    /// T14 路由键是 (devid,tag) 二元组整体：("a","bc") 与 ("ab","c") 的
    /// 长度前缀注入字节串不同 → 64 分片下必须路由不同——路由只折叠任一维
    /// （只 hash devid 或只 hash tag）两者必碰撞，本测锚死二元组语义。
    #[test]
    fn shard_route_key_is_devid_tag_pair() {
        assert_ne!(
            shard_route("a", "bc", 64),
            shard_route("ab", "c", 64),
            "(devid,tag) 二元组整体是路由键——任一维折叠都使两 series 碰撞"
        );
    }

    /// T14 分片散布：16 series × 4 分片，每片 4 个（FNV-1a 确定性均匀）——
    /// 路由退化（全落一片/半数片空）必须红。
    #[test]
    fn shard_route_spreads_series() {
        let mut per_shard = [0usize; 4];
        for i in 0..16 {
            per_shard[shard_route(&format!("dev{i}"), "tA", 4)] += 1;
        }
        assert_eq!(
            per_shard,
            [4, 4, 4, 4],
            "16 series 必须均匀散布 4 分片（每片 4）"
        );
    }

    /// T14 分片发送亲和：2 分片 handle 上经 fan_out（生产路径）发 dev1/
    /// dev2 各 5 行（tag=tA，确定性路由到异片）——每片恰好收到**一个**
    /// series 的全部行（亲和），两片皆非空，sent==10、dropped==0。
    #[test]
    fn handle_sharded_send_affinity() {
        let (tx0, mut rx0) = mpsc::channel(64);
        let (tx1, mut rx1) = mpsc::channel(64);
        let handle = HistoryHandle::new(vec![tx0, tx1], fresh_stats());

        let base = now_secs();
        let samples: Vec<Sample> = (0..5i64)
            .flat_map(|i| {
                vec![
                    sample(
                        "dev1",
                        "hw1",
                        base + i,
                        &[("tA", SampleValue::Float(i as f64))],
                    ),
                    sample(
                        "dev2",
                        "hw1",
                        base + i,
                        &[("tA", SampleValue::Float(i as f64))],
                    ),
                ]
            })
            .collect();
        let mut effective = HashMap::new();
        for devid in ["dev1", "dev2"] {
            effective.insert(
                tag_key(devid, "tA"),
                TagProps {
                    history: true,
                    ..TagProps::default()
                },
            );
        }
        fan_out(&samples, &effective, &handle);

        let rows0 = drain(&mut rx0);
        let rows1 = drain(&mut rx1);
        assert_eq!(rows0.len() + rows1.len(), 10, "全部 10 行必须送达");
        assert!(!rows0.is_empty() && !rows1.is_empty(), "两片皆非空（散布）");
        // 亲和不变量：每片的行全部属于同一 devid（series 域不跨片），
        // 且两片分属不同 devid（dev1/dev2 确定性异片）
        let dev0: std::collections::BTreeSet<&str> =
            rows0.iter().map(|r| r.devid.as_str()).collect();
        let dev1: std::collections::BTreeSet<&str> =
            rows1.iter().map(|r| r.devid.as_str()).collect();
        assert_eq!(dev0.len(), 1, "片 0 只能含一个 series 域（亲和）");
        assert_eq!(dev1.len(), 1, "片 1 只能含一个 series 域（亲和）");
        assert_ne!(
            dev0, dev1,
            "dev1/dev2 确定性路由异片（fixture 已验 FNV 分布）"
        );
        assert_eq!(handle.stats().sent.load(Ordering::SeqCst), 10);
        assert_eq!(handle.stats().dropped.load(Ordering::SeqCst), 0);
    }

    /// T14 分片饱和判定：is_full 语义 = **全部**分片皆满——单片满不算满
    /// （其它 series 路由片未满仍可发，P11 快速路径不得误丢）；两片皆满
    /// 才短路。N=1 时退化为单 channel 语义（现状）。
    #[test]
    fn handle_sharded_is_full_all_shards() {
        let (tx0, _rx0) = mpsc::channel(1);
        let (tx1, _rx1) = mpsc::channel(1);
        let handle = HistoryHandle::new(vec![tx0.clone(), tx1.clone()], fresh_stats());

        assert!(!handle.is_full(), "两片皆空不算满");
        tx0.try_send(HistoryRow {
            devid: "dev1".into(),
            hwid: "hw1".into(),
            tag: "tA".into(),
            ts_ms: 1,
            value: SampleValue::Float(0.0),
            deadband: None,
        })
        .expect("空 channel 预填必须成功");
        assert!(
            !handle.is_full(),
            "单片满不算满——另一片仍有容量，饱和短路不得触发"
        );
        tx1.try_send(HistoryRow {
            devid: "dev2".into(),
            hwid: "hw1".into(),
            tag: "tA".into(),
            ts_ms: 1,
            value: SampleValue::Float(0.0),
            deadband: None,
        })
        .expect("空 channel 预填必须成功");
        assert!(handle.is_full(), "两片皆满才算满");
    }
}
