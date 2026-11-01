use crate::error::RsIotError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// EventBus broadcast 容量默认值（R6/C 阶段 T2）。pub 供无完整配置的场景
/// （install/uninstall 管理命令的 `init_tracing(None)`）引用同一默认。
pub const DEFAULT_EVENT_BUS_CAPACITY: usize = 256;

/// 默认 JWT 密钥（与 Lite_IOT 对齐的兼容值）。**可预测的公开常量**——
/// 生产部署必须显式配置强密钥；启动时对默认值打 error 提示（不拒启，
/// 审计 B3 裁定非破坏护栏）。
pub const DEFAULT_JWT_SECRET: &str = "qwerty1234567890asdfgh1234567890";

/// sidecar 心跳失联判定默认秒数（bridge 服务端默认值）。
///
/// **单源纪律（审计 B6）**：`BridgeRegistry::new()` 用它构造默认心跳值，sidecar_client
/// task 再从 `BridgeRegistry::hb_timeout()` 取——两端不得各自定义同一失联参数，
/// 漂移会出现 server 判失联/client 自认健康的幽灵断连。`[bridge].heartbeat_timeout_secs`
/// 配置项经 `BridgeRegistry::with_hb_timeout()` 生效（下限 1s 钳制），本常量只是缺省。
pub const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 15;

/// [history] partition_secs 上界（T9）：36500 天秒数，与 retention_days 上界
/// 对齐——分区窗口超过保留上界无意义。u64 显式后缀防 format! 独立表达式
/// 整型推断回退 i32 溢出（deny(arithmetic_overflow)）。
const MAX_PARTITION_SECS: u64 = 36_500 * 86_400;

/// 应用总配置。从 toml 加载，缺失字段走 Default。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// lux 嵌入数据库配置
    pub lux: LuxConfig,
    /// REST API 端口（M3）
    pub api_port: u16,
    /// REST API 绑定地址（审计 B3 增量配置：与 lux/bridge 的 bind_host 姿态
    /// 对齐；默认 "0.0.0.0" **保持既有行为不变**，跨机部署需 JWT 强密钥 +
    /// 显式绑定）。旧配置文件无此键走默认。
    pub api_bind_host: String,
    /// JWT 签名密钥（HS256，32 字节）。默认值与 Lite_IOT 对齐。
    pub jwt_secret: String,
    /// 北向 MQTT broker（T8 起等价生成默认 luaNorth 实例 mqtt-pub-default；
    /// 细粒度控制可自建 luaNorth 实例）
    pub mqtt: MqttConfig,
    /// 日志配置（终端 + 可选文件轮转）。
    pub log: LogConfig,
    /// 外部插件 bridge 配置（进程外南向插件经 TCP 接入）。
    pub bridge: BridgeConfig,
    /// pipeline 数据面配置（worker 数等）。
    pub pipeline: PipelineConfig,
    /// [history] 历史时序存储配置（L2 部署期开关，详见 `HistoryConfig`）。
    #[serde(default)]
    pub history: HistoryConfig,
    /// [rtdata] 实时数据订阅配置（读源实现选型，详见 `RtdataConfig`）。
    #[serde(default)]
    pub rtdata: RtdataConfig,
    /// [meta] 画面/主题文档版本管理配置（P0，design 2026-10-30-screen-versioning）。
    #[serde(default)]
    pub meta: MetaConfig,
    /// Lua stdlib 官方源/镜像 upstream 基址（设计 §3.6 D1/D2）。
    /// `POST /api/v1/lua/stdlib/sync` body 未传 upstream 时用它；内网镜像 =
    /// 官方静态目录完整拷贝，改此键指向镜像即切换。v1 占位官方 Pages URL（T13 建置）；
    /// 正式发布后此处改正式 URL（rsiot-lualib docs/PUBLISH.md runbook）。
    pub lua_stdlib_upstream: String,
    /// Lua stdlib 持久化 pin 版本（§3.3：默认锁定；`auto` 跟随 latest 须显式）。
    /// sync 解析顺序：body.pin（显式版本或 "auto"）> 此键 > 锁定本地已装版本。
    /// None = 未显式配置（回落"锁定本地已装版本"，首装无 index 时取 latest）。
    pub lua_stdlib_pin: Option<String>,
    /// EventBus broadcast 容量（R6/C 阶段 T2）。Rtdata 载荷带 samples 后事件
    /// 变大，慢 SSE 消费者更易 lagged 丢帧；持续增长 `rs_iot_sse_lagged_total`
    /// 时可调大此值（默认 256 对标 hub）。**内存口径**：ring 常驻 ≈ 容量 × 单事件
    /// 体积（带值 Rtdata 可达数十 KB，256 上限约数十 MB）——调大前先估内存。
    pub event_bus_capacity: usize,
    /// 部署期指定的网关 ID（hwid 覆盖）。空 = 启动时按硬件自动生成
    /// （`hwid::hardware_id()`，sha256(CPU brand+ARCH) 前缀 eiot——同型号 CPU 会撞号，
    /// 多网关接同一平台需在部署时指定唯一值）。
    #[serde(default)]
    pub hwid: String,
}

/// lux ServerConfig 的子集映射（仅暴露我们关心的字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LuxConfig {
    /// RESP 端口。默认 6390，避开本机 redis 6379。
    pub port: u16,
    /// 绑定地址。默认 loopback。
    pub bind_host: String,
    /// RESP 密码（D-LUX-1）。仅非 loopback 绑定时生效——启动校验
    /// （`db::validate_lux_bind`）强制要求非空，否则拒绝启动；loopback
    /// 部署保持无密码单机信任模型（缺省空串）。
    pub password: String,
    /// 数据目录（lux.dat 快照 + Tiered WAL 落盘点）。
    pub data_dir: String,
    /// 后台快照间隔（秒）。Tiered 下用于截断 WAL，不可为 0。
    pub save_interval_secs: u64,
}

/// 北向 MQTT broker 配置。
///
/// T8 迁移语义：编排层（`run_app_with_shutdown`）据此**等价生成**默认 luaNorth
/// 实例（`mqtt-pub-default`，内置 mqttPub 兼容脚本 `scripts/_builtin-north-compat/`）
/// ——host 非空即生成，`host = ""` 为显式关闭；细粒度控制（独立脚本/批量节奏/
/// 多 broker）可自建 luaNorth 实例（REST `instances` 配置，与默认实例互不干扰）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MqttConfig {
    /// broker 主机（公网或本地 mosquitto 等）。空串 = 不生成默认北向实例。
    pub host: String,
    /// broker 端口（默认 1883）。
    pub port: u16,
    /// 用户名（匿名 broker 留 None）。
    pub username: Option<String>,
    /// 密码（支持 `secret:<ref>` 引用——luaNorth 连接序列解析 lux secrets 表）。
    pub password: Option<String>,
    /// 北向发布周期（秒）：映射默认实例的 `period` 与 `batch_max_secs`——每隔该
    /// 时长从 lux 抽取 rtdata 发布。
    pub publish_interval_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            lux: LuxConfig::default(),
            // 2026-08-26 用户裁决统一 8790（测试验证阶段无兼容负担；
            // rsiot-mcp 缺省 RSIOT_BASE 与 rsiot-gateway 默认基址同为 8790，
            // 三处口径自此一致——原 8780 为 Lite_IOT 沿袭值）。
            api_port: 8790,
            api_bind_host: "0.0.0.0".into(),
            jwt_secret: DEFAULT_JWT_SECRET.into(),
            mqtt: MqttConfig::default(),
            log: LogConfig::default(),
            bridge: BridgeConfig::default(),
            pipeline: PipelineConfig::default(),
            history: HistoryConfig::default(),
            rtdata: RtdataConfig::default(),
            meta: MetaConfig::default(),
            lua_stdlib_upstream: "https://rsiot.github.io/lualib/".into(),
            lua_stdlib_pin: None,
            event_bus_capacity: DEFAULT_EVENT_BUS_CAPACITY,
            hwid: String::new(),
        }
    }
}

impl Default for LuxConfig {
    fn default() -> Self {
        Self {
            port: 6390,
            bind_host: "127.0.0.1".into(),
            password: String::new(),
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
            heartbeat_timeout_secs: DEFAULT_HEARTBEAT_TIMEOUT_SECS,
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

/// [history] 历史时序存储配置(L2 部署期开关;L1=Cargo feature,L3=TagProps.history)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// L2 部署期开关:false 时不初始化引擎、不获取 .tsink.lock
    pub enabled: bool,
    /// tsink 数据目录(独立于 lux data_dir;.tsink.lock 进程锁所在)
    pub data_dir: String,
    /// 引擎内存预算(MiB),D6
    pub memory_limit_mb: usize,
    /// series 基数上限;0=按 effective() history=true 计数×1.2 自动校准,D6
    pub cardinality_limit: usize,
    /// 保留期(天),D6
    pub retention_days: u64,
    /// WAL 同步:0=PerAppend 严格档,>0=Periodic(ms),P9
    pub wal_sync_ms: u64,
    /// WAL 大小上限(MiB),D6
    pub wal_size_limit_mb: usize,
    /// 有界 channel 容量(try_send 满即丢+计数),D3
    pub channel_capacity: usize,
    /// 单写线程攒批行数上限,D3
    pub batch_max_rows: usize,
    /// 关停排空超时(秒;超时放弃,WAL 回放兜底),D10
    pub close_timeout_secs: u64,
    /// 单 chunk 点数上限(active 内存主控)。0=不透传,走 tsink 引擎默认 2048;
    /// 非 0 透传 tsink `with_chunk_points`(入参被钳位到 [1,65535])。
    /// 10 万 series 场景建议 ≤512:active 稳态 ≈ series×cap×40B
    /// (2048→~4.1GB / 512→~1GB,2026-08-29 压测审查)
    pub chunk_points: usize,
    /// 分区窗口(秒)。0=不透传,走 tsink 引擎默认 3600;非 0 透传 tsink
    /// `with_partition_duration`。窗口越小 head 越早 seal(内存回收越快),
    /// 但乱序跨窗写入受 tsink max_active_partition_heads_per_series 约束
    pub partition_secs: u64,
    /// P1 seal 门控:后台 flush 拍长(ms)。0=tsink 默认 250ms。拍长只是
    /// 「seal 候选判定的最小时延」,不再等于「每拍 seal」——实际 seal 由
    /// [`Self::seal_min_points`]/[`Self::max_head_age_secs`]/WAL 阈值决定
    pub flush_interval_ms: u64,
    /// P1 seal 门控:当前 head 最小 seal 点数。0=按 chunk_points/8 推导
    /// (引擎默认 chunk 2048 → 256 点)。低速现场靠它把段粒度从「每拍 1 点」
    /// 拉回百点级,把 catalog 全量重写成本摊薄 ~2 个数量级
    pub seal_min_points: usize,
    /// P1 seal 门控:当前 head 最长存活秒数,到期强制 seal(低速兜底:
    /// 保证 WAL 与 active 内存有界)。0=tsink 默认 300s
    pub max_head_age_secs: u64,
    /// P1 catalog 最小重写间隔秒(段注册表缓存是派生数据,启动可由段目录
    /// 重建)。0=tsink 默认 30s;checkpoint/优雅关闭锚点强制刷新
    pub catalog_persist_secs: u64,
    /// 写线程数(T14,10 万点压测)。1=单写线程(行为与现状完全一致,零回归);
    /// n>1 启用 N 写线程 series 亲和分片:同一 series(devid+tag)恒定路由到
    /// 同一 writer(稳定 hash),死区/lane/乱序状态机的单写者全序保持;每分片
    /// 独立有界 channel(容量 = channel_capacity÷N 向上取整)。压测建议 4
    pub writer_threads: usize,
    /// sysstatus history.health degrading 档阈值(Task B/#4,T17 配置化):
    /// 累计占比 dropped/(sent+dropped) **严格大于**此值转 degrading。
    /// (0,1] 且须 < [`Self::health_dropping_ratio`];默认 0.005(=T17 原常量,
    /// serde default 零漂移)。不支持 0 禁用(errors>0 降档独立于占比,
    /// 恒 ok 会掩盖错误面)
    pub health_degrading_ratio: f64,
    /// sysstatus history.health dropping 档阈值(Task B/#4,T17 配置化):
    /// 累计占比严格大于此值转 dropping(最高档优先)。(0,1];默认 0.05
    pub health_dropping_ratio: f64,
    /// 丢弃告警速率阈值(行/s,严格大于;Task B/#4,T18 配置化):与
    /// [`Self::alarm_drop_ratio`] 双条件均须满足才 fire。≥0;默认 10.0
    /// (=T18 原常量)。0 合法=只按占比判(小部署免速率门槛)
    pub alarm_drop_rate_per_sec: f64,
    /// 丢弃告警窗口占比阈值(严格大于;Task B/#4,T18 配置化):
    /// delta_dropped/(delta_dropped+delta_sent) > 此值。(0,1];默认 0.01
    pub alarm_drop_ratio: f64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            data_dir: "./data/history".into(),
            memory_limit_mb: 256,
            cardinality_limit: 0,
            retention_days: 30,
            wal_sync_ms: 500,
            wal_size_limit_mb: 1024,
            channel_capacity: 65536,
            batch_max_rows: 8192,
            close_timeout_secs: 15,
            chunk_points: 0,
            partition_secs: 0,
            flush_interval_ms: 0,
            seal_min_points: 0,
            max_head_age_secs: 0,
            catalog_persist_secs: 0,
            writer_threads: 1,
            // Task B/#4：默认值 = T17/T18 原硬编码常量（serde default 零漂移）
            health_degrading_ratio: 0.005,
            health_dropping_ratio: 0.05,
            alarm_drop_rate_per_sec: 10.0,
            alarm_drop_ratio: 0.01,
        }
    }
}

impl HistoryConfig {
    /// 边界校验（H4）：enabled=true 时引擎组装前置检查，非法配置在
    /// `maybe_init` 入口走既有 init_failed Err 通道（error! + 引擎 None +
    /// /history 503 + 主程序继续），不以巨值乘法路径硬闯组装。上限取值的
    /// 依据：MiB→字节/天→秒换算在上限内不可能溢出（`build_storage` 的
    /// saturating 乘法是校验之外的双保险）。`wal_sync_ms`/`close_timeout_secs`
    /// 不设限——0 有合法语义（PerAppend 严格档 / 立即放弃等待）。
    pub fn validate(&self) -> Result<(), String> {
        if self.memory_limit_mb < 1 || self.memory_limit_mb > 1 << 20 {
            return Err(format!(
                "memory_limit_mb 必须在 1..={} MiB，实际 {}",
                1 << 20,
                self.memory_limit_mb
            ));
        }
        if self.wal_size_limit_mb < 1 || self.wal_size_limit_mb > 1 << 20 {
            return Err(format!(
                "wal_size_limit_mb 必须在 1..={} MiB，实际 {}",
                1 << 20,
                self.wal_size_limit_mb
            ));
        }
        if self.retention_days < 1 || self.retention_days > 36_500 {
            return Err(format!(
                "retention_days 必须在 1..=36500 天，实际 {}",
                self.retention_days
            ));
        }
        if self.channel_capacity < 1 || self.channel_capacity > 1 << 24 {
            return Err(format!(
                "channel_capacity 必须在 1..={}，实际 {}",
                1 << 24,
                self.channel_capacity
            ));
        }
        if self.batch_max_rows < 1 || self.batch_max_rows > 1 << 20 {
            return Err(format!(
                "batch_max_rows 必须在 1..={}，实际 {}",
                1 << 20,
                self.batch_max_rows
            ));
        }
        if self.cardinality_limit > 1 << 24 {
            return Err(format!(
                "cardinality_limit 必须在 0..={}（0=自动校准），实际 {}",
                1 << 24,
                self.cardinality_limit
            ));
        }
        if self.data_dir.is_empty() {
            return Err("data_dir 不能为空".to_string());
        }
        // T9（2026-08-29 压测调参面）：上界 65535 = tsink builder 钳位上界
        // （u16::MAX）——越界透传会被 tsink 静默钳位（配置谎言），显式拒绝；
        // 下界 0 有合法语义（不透传走引擎默认 2048）
        if self.chunk_points > 65_535 {
            return Err(format!(
                "chunk_points 必须在 0..=65535（0=引擎默认 2048），实际 {}",
                self.chunk_points
            ));
        }
        // T9：上界 36500 天（秒）= retention_days 上界对齐——分区窗口超过
        // 保留上界无意义；0 有合法语义（不透传走引擎默认 3600s）
        if self.partition_secs > MAX_PARTITION_SECS {
            return Err(format!(
                "partition_secs 必须在 0..={MAX_PARTITION_SECS}（0=引擎默认 3600s），实际 {}",
                self.partition_secs
            ));
        }
        // T14：writer_threads 下界 1（单写线程，0 无 auto 语义）；上界 64——
        // 每线程 8MiB 栈，64×8MiB=512MiB 已是荒谬配置，显式拒绝防误配
        if self.writer_threads < 1 || self.writer_threads > 64 {
            return Err(format!(
                "writer_threads 必须在 1..=64（1=单写线程零回归），实际 {}",
                self.writer_threads
            ));
        }
        // Task B/#4（阈值配置化）：health 两键 (0,1] 且 degrading < dropping
        // （档位有序——相等会使三档退化为两档，拒绝）；alarm rate ≥ 0
        // （0 合法=只按占比判）、ratio (0,1]。均不支持 0 禁用判档（errors>0
        // 降档独立于占比，恒 ok 会掩盖错误面）
        if self.health_degrading_ratio <= 0.0 || self.health_degrading_ratio > 1.0 {
            return Err(format!(
                "health_degrading_ratio 必须在 (0, 1]，实际 {}",
                self.health_degrading_ratio
            ));
        }
        if self.health_dropping_ratio <= 0.0 || self.health_dropping_ratio > 1.0 {
            return Err(format!(
                "health_dropping_ratio 必须在 (0, 1]，实际 {}",
                self.health_dropping_ratio
            ));
        }
        if self.health_degrading_ratio >= self.health_dropping_ratio {
            return Err(format!(
                "health_degrading_ratio ({}) 必须小于 health_dropping_ratio ({})（档位有序）",
                self.health_degrading_ratio, self.health_dropping_ratio
            ));
        }
        if self.alarm_drop_rate_per_sec < 0.0 {
            return Err(format!(
                "alarm_drop_rate_per_sec 必须 ≥ 0（0=只按占比判），实际 {}",
                self.alarm_drop_rate_per_sec
            ));
        }
        if self.alarm_drop_ratio <= 0.0 || self.alarm_drop_ratio > 1.0 {
            return Err(format!(
                "alarm_drop_ratio 必须在 (0, 1]，实际 {}",
                self.alarm_drop_ratio
            ));
        }
        Ok(())
    }
}

/// [rtdata] 实时数据订阅读源实现选型（P8）。
///
/// serde 线上名小写：`sub_impl = "ksub" | "poll"`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubImpl {
    /// lux `ksubscribe("rtdata:*")` key 事件驱动（缺省）：watcher 常驻维护
    /// per-devid 版本表，订阅连接按脏集精准 HGETALL（P7/P9）。
    #[default]
    Ksub,
    /// 每 tick 轮询 `read_scope`（原实现，显式回退开关）：watcher 不启动，
    /// lux 无订阅者零开销承诺保持（P8）。
    Poll,
}

/// [rtdata] 实时数据订阅配置（订阅升级 Phase 2，P8）。
///
/// 缺省 `sub_impl = "ksub"`——默认开，否则功能成暗线；回退 poll 仅需显式
/// 配置一个键（`sub_impl = "poll"`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RtdataConfig {
    /// 订阅读源实现（ksub 事件驱动 / poll 轮询）。
    pub sub_impl: SubImpl,
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
                let mut cfg: AppConfig = toml::from_str(&s).map_err(|e| {
                    RsIotError::Config(format!("parse config {}: {e}", p.display()))
                })?;
                cfg.meta.sanitize();
                Ok(cfg)
            }
            _ => Ok(Self::default()),
        }
    }
}

/// [meta] 画面/主题文档版本管理配置（P0，design 2026-10-30-screen-versioning §5.1）。
///
/// 版本键族 `sv:{kind}:{id}:{seq|meta|v:{seq}}` 的治理参数：正文 TTL 自动淘汰
/// 旧版本（无删除代码），容量上限触发紧缩（保留最新 cap 条）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetaConfig {
    /// 版本正文 TTL 秒（`sv:...:v:{seq}` SETEX）。默认 30 天；0 视为默认
    /// （sanitize 钳制——TTL 无"永不过期"档，防 AI 高频写画面无限累积）。
    pub versions_ttl_secs: u64,
    /// 每画面保留版本数上限。紧缩触发线 HLEN > 2×cap，触发后按 seq 降序保留
    /// 最新 cap 条且正文仍在的条目。钳制 1..=500（sanitize）。
    pub versions_cap: usize,
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            versions_ttl_secs: 2_592_000,
            versions_cap: 50,
        }
    }
}

impl MetaConfig {
    /// 加载期钳制（对齐深度审查对配置键的护栏要求）：ttl 0 → 默认 30 天；
    /// cap 钳 1..=500（0/超大值静默回合法域，不 fail 启动——meta 是治理面
    /// 而非安全面，激进拒绝不划算）。
    pub fn sanitize(&mut self) {
        if self.versions_ttl_secs == 0 {
            self.versions_ttl_secs = 2_592_000;
        }
        self.versions_cap = self.versions_cap.clamp(1, 500);
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

    #[test]
    fn lua_stdlib_upstream_default_and_override() {
        // 缺省官方源占位（T13 建置）；内网镜像改此键切换（§3.6 D2）
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.lua_stdlib_upstream, "https://rsiot.github.io/lualib/");
        let cfg: AppConfig =
            toml::from_str(r#"lua_stdlib_upstream = "http://mirror.local/lualib/""#).unwrap();
        assert_eq!(cfg.lua_stdlib_upstream, "http://mirror.local/lualib/");
    }

    /// §3.3 pin 策略（阶段 A 审查修复：默认锁定语义落地）：缺省 None（回落锁定
    /// 本地已装版本，由 sync 端点解析）；显式配置即持久化锁定。
    #[test]
    fn lua_stdlib_pin_default_and_override() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.lua_stdlib_pin, None, "缺省不配置（锁定本地已装版本）");
        let cfg: AppConfig = toml::from_str(r#"lua_stdlib_pin = "1.2.0""#).unwrap();
        assert_eq!(cfg.lua_stdlib_pin.as_deref(), Some("1.2.0"));
    }

    /// EventBus 容量配置化（C 阶段 T2 / R6）：缺省 256，显式配置覆盖。
    #[test]
    fn event_bus_capacity_default_and_override() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.event_bus_capacity, 256, "默认容量 256（对标 hub）");
        let cfg: AppConfig = toml::from_str("event_bus_capacity = 1024").unwrap();
        assert_eq!(cfg.event_bus_capacity, 1024);
    }

    /// hwid 部署期覆盖键（self-monitor 计划 T1）：缺省空串 = 启动时按硬件自动生成。
    #[test]
    fn hwid_default_empty() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.hwid, "", "缺省空串=启动时按硬件自动生成");
    }

    /// hwid 部署期覆盖键：显式配置即部署期指定（多网关接同一平台时规避同型号 CPU 撞号）。
    #[test]
    fn hwid_toml_override() {
        let cfg: AppConfig = toml::from_str(r#"hwid = "custom-gw-001""#).unwrap();
        assert_eq!(cfg.hwid, "custom-gw-001");
    }

    /// [lux] password（D-LUX-1）：缺省空串——loopback 单机信任模型无需密码；
    /// 非 loopback 绑定时启动校验（`db::validate_lux_bind`）强制要求非空。
    #[test]
    fn lux_password_default_empty() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.lux.password, "", "缺省空串=loopback 无密码单机信任模型");
    }

    /// [lux] password：显式配置覆盖；锁 partial-default 契约（只设 password 其余仍默认）。
    #[test]
    fn lux_password_toml_override() {
        let cfg: AppConfig = toml::from_str(
            r#"[lux]
password = "s3cret""#,
        )
        .unwrap();
        assert_eq!(cfg.lux.password, "s3cret");
        assert_eq!(
            cfg.lux.port, 6390,
            "只设 password，其余 lux 字段仍走 Default"
        );
    }

    /// [history] 默认值（L2 部署期开关默认关：false 时不初始化引擎、不获取
    /// .tsink.lock，现有部署零影响）。
    #[test]
    fn history_config_default_off() {
        let h = AppConfig::default().history;
        assert!(!h.enabled, "L2 默认关：历史写路径零影响");
        assert_eq!(h.data_dir, "./data/history");
        assert_eq!(h.memory_limit_mb, 256);
        assert_eq!(
            h.cardinality_limit, 0,
            "0=按 effective() history=true 计数×1.2 自动校准"
        );
        assert_eq!(h.retention_days, 30);
        assert_eq!(h.wal_sync_ms, 500);
        assert_eq!(h.wal_size_limit_mb, 1024);
        assert_eq!(h.channel_capacity, 65536);
        assert_eq!(h.batch_max_rows, 8192);
        assert_eq!(h.close_timeout_secs, 15);
        // T9（2026-08-29 压测调参面）：缺省 0 = 不透传，走 tsink 引擎默认
        // （chunk 2048 点 / 分区 3600s）
        assert_eq!(
            h.chunk_points, 0,
            "0=引擎默认 2048（不调 with_chunk_points）"
        );
        assert_eq!(
            h.partition_secs, 0,
            "0=引擎默认 3600s（不调 with_partition_duration）"
        );
    }

    /// [history] 完整节显式配置：每字段给非默认值，解析后逐字段覆盖默认。
    #[test]
    fn history_config_toml_explicit() {
        let cfg: AppConfig = toml::from_str(
            r#"[history]
enabled = true
data_dir = "/var/rs-iot/history"
memory_limit_mb = 512
cardinality_limit = 10000
retention_days = 90
wal_sync_ms = 0
wal_size_limit_mb = 2048
channel_capacity = 1024
batch_max_rows = 4096
close_timeout_secs = 30
chunk_points = 512
partition_secs = 1800
writer_threads = 4"#,
        )
        .unwrap();
        let h = cfg.history;
        assert!(h.enabled);
        assert_eq!(h.data_dir, "/var/rs-iot/history");
        assert_eq!(h.memory_limit_mb, 512);
        assert_eq!(h.cardinality_limit, 10000);
        assert_eq!(h.retention_days, 90);
        assert_eq!(h.wal_sync_ms, 0, "0=PerAppend 严格档");
        assert_eq!(h.wal_size_limit_mb, 2048);
        assert_eq!(h.channel_capacity, 1024);
        assert_eq!(h.batch_max_rows, 4096);
        assert_eq!(h.close_timeout_secs, 30);
        // T9：两新键显式配置覆盖默认 0（透传 tsink builder）
        assert_eq!(h.chunk_points, 512);
        assert_eq!(h.partition_secs, 1800);
        // T14：writer_threads 显式 4 覆盖默认 1（N 写线程 series 亲和分片）
        assert_eq!(h.writer_threads, 4);
    }

    /// 无 [history] 节（有最小其他内容）：解析成功，history 走 Default
    /// （缺字段容错是既有约定）。
    #[test]
    fn history_config_missing_section() {
        let cfg: AppConfig = toml::from_str("[pipeline]\nworkers = 2\n").unwrap();
        assert!(
            !cfg.history.enabled,
            "无 [history] 节走 Default（enabled=false）"
        );
    }

    /// [history] partial-default 契约：只设 enabled=true，其余字段仍走 Default。
    #[test]
    fn history_config_partial() {
        let cfg: AppConfig = toml::from_str("[history]\nenabled = true\n").unwrap();
        let h = cfg.history;
        assert!(h.enabled);
        assert_eq!(h.data_dir, "./data/history");
        assert_eq!(h.memory_limit_mb, 256);
        assert_eq!(h.cardinality_limit, 0);
        assert_eq!(h.retention_days, 30);
        assert_eq!(h.wal_sync_ms, 500);
        assert_eq!(h.wal_size_limit_mb, 1024);
        assert_eq!(h.channel_capacity, 65536);
        assert_eq!(h.batch_max_rows, 8192);
        assert_eq!(h.close_timeout_secs, 15);
        // partial-default 契约覆盖两新键：只设 enabled，新键仍走 Default 0
        assert_eq!(h.chunk_points, 0);
        assert_eq!(h.partition_secs, 0);
        // T14：writer_threads 缺省走 Default 1（单写线程，行为与现状一致）
        assert_eq!(h.writer_threads, 1);
    }

    /// Task B/#4：健康档位/告警阈值四键 roundtrip——显式配置覆盖默认；
    /// 缺省四键走 Default **零漂移**（与 T17/T18 硬编码常量一致：
    /// 0.005 / 0.05 / 10.0 / 0.01）。
    #[test]
    fn history_config_threshold_keys_roundtrip() {
        let cfg: AppConfig = toml::from_str(
            r#"[history]
enabled = true
health_degrading_ratio = 0.001
health_dropping_ratio = 0.02
alarm_drop_rate_per_sec = 50.0
alarm_drop_ratio = 0.05"#,
        )
        .unwrap();
        let h = cfg.history;
        assert_eq!(h.health_degrading_ratio, 0.001);
        assert_eq!(h.health_dropping_ratio, 0.02);
        assert_eq!(h.alarm_drop_rate_per_sec, 50.0);
        assert_eq!(h.alarm_drop_ratio, 0.05);

        // 缺省四键走 Default（serde default 保持现行为零漂移）
        let h = toml::from_str::<AppConfig>("[history]\nenabled = true\n")
            .unwrap()
            .history;
        assert_eq!(h.health_degrading_ratio, 0.005, "默认=T17 常量 0.5%");
        assert_eq!(h.health_dropping_ratio, 0.05, "默认=T17 常量 5%");
        assert_eq!(h.alarm_drop_rate_per_sec, 10.0, "默认=T18 常量 10/s");
        assert_eq!(h.alarm_drop_ratio, 0.01, "默认=T18 常量 1%");
    }

    /// Task B/#4：阈值边界校验——health 两键 (0,1] 且 degrading < dropping
    /// （档位有序）；alarm rate ≥ 0（0 合法=只按占比判）、ratio (0,1]。
    /// 0 无"禁用判档"语义（errors>0 降档独立于占比，恒 ok 会掩盖错误面）。
    #[test]
    fn history_config_validate_thresholds() {
        let base = HistoryConfig::default();
        assert_eq!(base.validate(), Ok(()), "默认阈值必须合法");

        // health_degrading_ratio：0 / >1 / ≥dropping（档位失序）拒绝
        let mut c = base.clone();
        c.health_degrading_ratio = 0.0;
        assert!(c.validate().is_err(), "degrading=0 必须拒绝");
        let mut c = base.clone();
        c.health_degrading_ratio = 1.5;
        assert!(c.validate().is_err(), "degrading>1 必须拒绝");
        // health_dropping_ratio：0 / >1 拒绝
        let mut c = base.clone();
        c.health_dropping_ratio = 0.0;
        assert!(c.validate().is_err(), "dropping=0 必须拒绝");
        let mut c = base.clone();
        c.health_dropping_ratio = 1.01;
        assert!(c.validate().is_err(), "dropping>1 必须拒绝");
        // 档位有序：degrading ≥ dropping 拒绝（相等也拒——三档退化为两档）
        let mut c = base.clone();
        c.health_degrading_ratio = 0.05;
        assert!(c.validate().is_err(), "degrading ≥ dropping 必须拒绝");

        // alarm_drop_rate_per_sec：<0 拒绝（0 合法）；alarm_drop_ratio：
        // 0 / >1 拒绝
        let mut c = base.clone();
        c.alarm_drop_rate_per_sec = -1.0;
        assert!(c.validate().is_err(), "rate<0 必须拒绝");
        let mut c = base.clone();
        c.alarm_drop_ratio = 0.0;
        assert!(c.validate().is_err(), "ratio=0 必须拒绝");
        let mut c = base.clone();
        c.alarm_drop_ratio = 1.5;
        assert!(c.validate().is_err(), "ratio>1 必须拒绝");

        // 上确界/下确界合法：dropping=1、ratio=1、rate=0
        let mut c = base.clone();
        c.health_dropping_ratio = 1.0;
        assert_eq!(c.validate(), Ok(()), "dropping 上确界 1 合法");
        let mut c = base.clone();
        c.alarm_drop_ratio = 1.0;
        assert_eq!(c.validate(), Ok(()), "ratio 上确界 1 合法");
        let mut c = base.clone();
        c.alarm_drop_rate_per_sec = 0.0;
        assert_eq!(c.validate(), Ok(()), "rate=0 合法（只按占比判）");
    }

    /// [history] 边界校验拒绝（H4）：≥4 类非法值逐一 Err——零值破坏语义
    /// （channel 无法构造/攒批永不 flush/保留期吞全量数据）、超上限值使
    /// MiB→字节换算逼近溢出域。当前无 validate 实现（编译红起）。
    #[test]
    fn history_config_validate_rejects_out_of_bounds() {
        let base = HistoryConfig::default();

        let mut c = base.clone();
        c.channel_capacity = 0;
        assert!(c.validate().is_err(), "channel_capacity=0 必须拒绝");

        let mut c = base.clone();
        c.batch_max_rows = 0;
        assert!(c.validate().is_err(), "batch_max_rows=0 必须拒绝");

        let mut c = base.clone();
        c.memory_limit_mb = 1 << 40;
        assert!(c.validate().is_err(), "memory_limit_mb=1<<40 必须拒绝");

        let mut c = base.clone();
        c.retention_days = 0;
        assert!(c.validate().is_err(), "retention_days=0 必须拒绝");

        let mut c = base.clone();
        c.data_dir = String::new();
        assert!(c.validate().is_err(), "data_dir 空串必须拒绝");

        // T9 负路径：chunk_points 超 tsink builder 钳位上界（u16::MAX）必须
        // 显式拒绝——透传会被 tsink 静默钳到 65535（配置谎言），不如拒启
        let mut c = base.clone();
        c.chunk_points = 65_536;
        assert!(c.validate().is_err(), "chunk_points>65535 必须拒绝");

        // T9 负路径：partition_secs 超 retention 上界（36500 天秒数）必须拒绝
        let mut c = base.clone();
        c.partition_secs = MAX_PARTITION_SECS + 1;
        assert!(c.validate().is_err(), "partition_secs>36500 天必须拒绝");

        // T14 负路径：writer_threads=0（单写线程是下确界，0 无"auto"语义）
        // 与 >64（8MiB 栈/线程 ×64=512MiB 已是荒谬配置）必须拒绝
        let mut c = base.clone();
        c.writer_threads = 0;
        assert!(c.validate().is_err(), "writer_threads=0 必须拒绝");
        let mut c = base.clone();
        c.writer_threads = 65;
        assert!(c.validate().is_err(), "writer_threads=65 必须拒绝");
    }

    /// [history] 边界校验接受（H4）：默认值、各字段上下确界与 0 哨兵语义
    /// 全部 Ok——wal_sync_ms/close_timeout_secs 不设限（0 有合法语义：
    /// PerAppend 严格档 / 立即放弃等待）。
    #[test]
    fn history_config_validate_accepts_default_and_bounds() {
        assert_eq!(HistoryConfig::default().validate(), Ok(()));

        // 各字段上下确界同时取到仍合法
        let c = HistoryConfig {
            memory_limit_mb: 1 << 20,
            wal_size_limit_mb: 1,
            retention_days: 36_500,
            channel_capacity: 1 << 24,
            batch_max_rows: 1 << 20,
            cardinality_limit: 1 << 24,
            // T9 上下确界：chunk_points 上界 = tsink 钳位上界 65535；
            // partition_secs 上界 = 36500 天（retention 上界）秒数
            chunk_points: 65_535,
            partition_secs: MAX_PARTITION_SECS,
            // T14 上确界：writer_threads=64（最大分片数）
            writer_threads: 64,
            ..HistoryConfig::default()
        };
        assert_eq!(c.validate(), Ok(()), "上下确界组合必须合法");

        // 0 哨兵语义合法：cardinality_limit=0 自动校准；wal_sync_ms=0 =
        // PerAppend；close_timeout_secs=0 = 立即放弃（均不设限字段）；
        // chunk_points/partition_secs=0 = 引擎默认（T9）
        let c = HistoryConfig {
            cardinality_limit: 0,
            wal_sync_ms: 0,
            close_timeout_secs: 0,
            chunk_points: 0,
            partition_secs: 0,
            ..HistoryConfig::default()
        };
        assert_eq!(c.validate(), Ok(()));
    }

    /// [rtdata] 默认值（P8 裁定）：订阅读源缺省 ksub——默认开，否则功能成暗线；
    /// 回退 poll 仅需显式配置一个键。
    #[test]
    fn rtdata_config_default_ksub() {
        let r = RtdataConfig::default();
        assert_eq!(r.sub_impl, SubImpl::Ksub, "P8：缺省 ksub");
        assert_eq!(
            AppConfig::default().rtdata.sub_impl,
            SubImpl::Ksub,
            "AppConfig 缺省同样 ksub"
        );
    }

    /// [rtdata] toml 解析：显式 poll/ksub 覆盖；缺节走 Default（ksub）。
    #[test]
    fn rtdata_config_toml_parse() {
        let cfg: AppConfig = toml::from_str("[rtdata]\nsub_impl = \"poll\"\n").unwrap();
        assert_eq!(cfg.rtdata.sub_impl, SubImpl::Poll, "显式 poll 回退开关");
        let cfg: AppConfig = toml::from_str("[rtdata]\nsub_impl = \"ksub\"\n").unwrap();
        assert_eq!(cfg.rtdata.sub_impl, SubImpl::Ksub);
        let cfg: AppConfig = toml::from_str("[pipeline]\nworkers = 2\n").unwrap();
        assert_eq!(
            cfg.rtdata.sub_impl,
            SubImpl::Ksub,
            "无 [rtdata] 节走 Default（ksub）"
        );
    }

    /// [meta] 加载期钳制（P0 §5.1；复审 L13 补测）：ttl 0 → 默认 30 天，
    /// cap 钳 1..=500。0/超大值静默回合法域（治理面不阻断启动），但绝不能让
    /// cap=0 进入路由——`truncate(0)` 加阈值 0 会让每次写都紧缩并清空历史。
    #[test]
    fn meta_config_sanitize_clamps_zero_and_overflow() {
        let mut m = MetaConfig {
            versions_ttl_secs: 0,
            versions_cap: 0,
        };
        m.sanitize();
        assert_eq!(m.versions_ttl_secs, 2_592_000, "ttl=0 应回落 30 天");
        assert_eq!(m.versions_cap, 1, "cap=0 应钳到 1（否则历史必被清空）");

        let mut m = MetaConfig {
            versions_ttl_secs: 7,
            versions_cap: 10_000,
        };
        m.sanitize();
        assert_eq!(m.versions_ttl_secs, 7, "非零 ttl 原样保留");
        assert_eq!(m.versions_cap, 500, "cap 上限 500");

        // 缺 [meta] 节 → Default 已合法（sanitize 幂等）
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.meta.versions_ttl_secs, 2_592_000);
        assert_eq!(cfg.meta.versions_cap, 50);
    }
}
