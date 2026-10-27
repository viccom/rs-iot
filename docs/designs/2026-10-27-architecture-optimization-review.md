# rs-iot 架构优化评估（2026-10-27）

> **状态**：in-review（评估结论，待逐项裁决；无代码改动）
> **日期**：2026-10-27
> **范围**：全仓架构级评估（分层/耦合/可观测性/热路径/配置面/依赖工程），**不含**已修复的 P1 缺陷与文档漂移
> **依据**：三路并行架构审查（分层 / 功能耦合 / 规范纪律）+ 主会话独立依赖矩阵与卫生统计；全部行号为 HEAD `d188155` 实测
> **前置结论**：本轮审查**未发现 P1 结构性缺陷**——依赖矩阵严格单向（`lib.rs 装配 → {api,pipeline} → {plugin,model,history} → {db,types,error,event_bus,host}`），无环、无反向依赖（`db→api` 仅存在于一条历史注释）。因此本文是**增量优化**，不是返工建议。
> **关联**：[架构详解](../architecture.md) §四（已实施的良好实践与技术债登记）｜[三路审查原始结论](#附录-a-审查方法与证据边界)

## 0. 一句话总结

骨架正确，差距在**四层"最后一公里"**：①数据可信没有总账；②依赖不可复现导致 1/4 代码无法验证；③元数据类型擦除（`bind` 全字符串）；④领域服务层缺位使 `api/` 持续变厚。其余为演进型债务，宜按触发点偿还，不宜提前抽象。

## 1. 评估方法与证据边界

**做了什么**：三路并行子代理（分层架构 / 功能域耦合 / 规范纪律）+ 主会话独立做模块依赖矩阵（`crate::` 引用计数）、卫生统计（生产段 `unwrap/expect`、`unsafe`、`Arc<Mutex>`）、配置与 feature 面枚举；对每条 P1 结论做代码级复核。

**没做什么（显式声明）**：
- **未编译 `history` feature**：`../tsink_fork` 在本机是 2 行占位 stub（`Cargo.toml` 无依赖、`src/lib.rs` 2 行注释），`cargo build --features history` 报 3 个 `unresolved imports`。故历史链（约占代码量 1/4）**在本机零运行期验证能力**，相关结论仅基于静态阅读与 08-29 计划的转述。
- 未跑全量 `--all-features` 测试（同上原因）。
- 未做真机协议验证（opcua/s7/eip 需外部设备或模拟器）。
- 性能论断引用的是仓库内既有 bench 记录（`docs/designs/2026-08-29-history-100k-improvement-plan.md`、`tag-registry-plan` 性能节），未重跑。

## 2. 结论总表（按改进价值排序）

| # | 议题 | 性质 | 影响面 | 成本 | 触发条件 |
|---|---|---|---|---|---|
| 1 | 依赖不可复现（tsink stub / lux path） | 工程可信度 | 历史链 1/4 代码零验证 | 中 | **立即** |
| 2 | 主链路数据丢失无总账 | 可观测性 | 现场故障归因不可答 | 低（~30 行） | **立即** |
| 3 | `DeviceBinding.bind` 类型擦除 | 类型设计 | 每协议重复解析 + 错误晚发现 | 中 | 下次改模型域 |
| 4 | `Effective` 快照隐式扇入 | 架构可见性 | 测试隔离 + 依赖图不可见 | 中 | 下次触碰 cache |
| 5 | `write_batch` 热路径双重 clone | 热路径 | 50 万点/秒下每点 2 次多余分配 | 极低 | 顺手 |
| 6 | `api/` 承担领域逻辑 | 分层 | REST 面增长即变厚 | 中高 | 随 REST 面增长 |
| 7 | 四协议客户端骨架复制 | 重复 | 第 6 协议边际成本高 | 中 | **第 6 个协议动工前** |
| 8 | `AppConfig` 持续膨胀 | 配置面 | 新子系统加节即膨胀 | 低（立规则） | 下次加子系统 |
| 9 | `Sample.values` 堆分配密度 | 热路径（深层） | 吞吐余量而非正确性 | 高 | 压测不达标时 |

## 3. 逐项详述

### 3.1 依赖不可复现 —— 最该优先解决

**事实**：
- `Cargo.toml:30` `tsink = { path = "../tsink_fork", optional = true }`；本机 `../tsink_fork` 为占位 stub（`Cargo.toml` 仅 package 元信息，`src/lib.rs` 2 行注释："stub: 占位 manifest，仅使 optional path dep 可解析"）。
- `cargo build --features history` → 3 个 `unresolved imports tsink::{Aggregation, DataPoint, Label, QueryOptions, Storage, StorageBuilder, TimestampPrecision, TsinkError, Value, WalSyncMode}`。
- `Cargo.toml:27` `lux = { path = "../lux" }`——lux 是必需的 path 依赖，且本项目已依赖其内部细节（`EmbeddedClient` 无 `Debug`、`ServerConfig` 无 Builder，均被 `expect`/手写构造依赖）。

**后果**（按严重度）：
1. **历史存储整条主线在本机不可验证**——`src/history/`（`mod/rows/writer/engine` 约 4.7k 行）+ `routes_history.rs` + `routes_metrics/` 只能静态阅读。
2. `--all-features` 结构性失败 → CI 对应 job 不可能绿（本轮已让本地门禁显式跳过并打印原因，但那只是止损）。
3. path 依赖无版本锚 → 换机/换人都需要按 `docs/ops/lux-upstream-sync-strategy.md` 手工恢复补丁，且 tsink 需要特定 tag（`rs-iot-1b-baseline` @ 277e744）。

**建议**（按可行性排序）：
- **A（推荐）**：`git submodule` + 版本锚（lux 与 tsink 各 pin commit），CI 用 submodule 拉取；本地开发一次 `git submodule update --init` 即完整。
- **B**：`vendor/` 目录 + `.cargo/config.toml` 的 `[source]` 替换（离线友好，但仓库体积大）。
- **C**：发布私有 registry / 或把 lux 的 2 个本地补丁以 `[patch]` 形式回归上游（治本但需上游权限，CLAUDE.md 已声明无 PR 权限）。
- 无论哪种，**都需要先解决 CI 的私有依赖可达性**（`RUST_DEPS_TOKEN`，见 `.github/workflows/ci.yml` 头部注释）。

**验收判据**：`cargo test --all-features --lib` 在干净 clone 上一条命令可跑通；`just lint` 不再需要"stub 环境自动跳过"分支。

### 3.2 主链路数据丢失无总账 —— 性价比最高的 30 行

**事实**（逐点实测）：

| 丢弃点 | 位置 | 当前处理 | 是否计数 |
|---|---|---|---|
| Source → hub 发送失败 | `data_sim/mod.rs:117`、`opcua_client/mod.rs:421`、`s7_client/mod.rs:626`、`eip_client/mod.rs:549`、`self_monitor/mod.rs:349`、`modbus_client/mod.rs:463` 等 | `if tx.send(sample).await.is_err() { break; }`（退出采集循环） | **否** |
| dispatcher → 已死 worker | `pipeline.rs:99-102` | `if dead[idx] { continue; }`（注释："静默丢弃"） | **否** |
| dispatcher → worker channel 关闭 | `pipeline.rs:110-113` | `dead[idx] = true` + `error!` 一次 | **否**（只有日志） |
| worker 退出前 best-effort flush | `pipeline.rs:188-190` | 调 `flush_batch`，失败走 `maybe_warn`（≥1s 去重） | 计入 `write_fail_n`（部分） |

**对照**：历史旁路有教科书级不变量——`history/rows.rs:48-56` 注释明写
`sent + dropped + skipped + ts_rejected + len_rejected == fan_out 遍历的 (tag,value) 总数`，
五个出口桶齐全且被 e2e 断言。**主链路没有对应物。**

**现有计数器**（`src/process_metrics.rs`）：`pipeline_processed_count` / `pipeline_write_fail_count` /
`pipeline_batch_count` / `pipeline_workers_active` / `pipeline_worker_deaths_count` / `hub_depth_last|max`
——**有"出"有"深度"，缺"入"**。没有 `samples_in` 这类入口计数，就无法建立
`in - processed == 各处丢弃之和` 的恒等式。

**后果**：现场问"这 5 分钟少收 3000 点，是采集没采到 / hub 堵了 / worker 死了 / lux 写失败？"
——主链路答不出，历史链答得出。这是**可运维性**的实质差距，不是洁癖。

**建议**（约 30 行）：
1. `SampleSender` 出口加 `samples_out: AtomicU64`（`send`/`try_send` 成功即 +1）；
2. `pipeline.rs` 的 `dead[idx]` 分支加 `samples_dropped_dead_worker`，`send_res.is_err()` 分支加 `samples_dropped_closed`；
3. 各 Source 的 `tx.send(...).is_err()` 分支加 `samples_dropped_source_exit`（或统一在 `SampleSender` 内计数后返回）；
4. `/metrics` 与 `/sysstatus` 暴露，并加一条单测断言恒等式（照抄 `history/rows.rs` 的写法）。

**验收判据**：压测跑一轮后 `samples_out - pipeline_processed_count == 三个 dropped 之和`（0 或差额可解释）。

### 3.3 `DeviceBinding.bind` 类型擦除

**事实**：`src/types.rs:236-243`
```rust
pub struct DeviceBinding {
    pub devid: String,
    #[serde(default)]
    pub bind: BTreeMap<String, String>,   // 值恒为字符串
}
```
注释自述"如 `{"slave_id":"2"}`；`DeviceSpec.slave_id` 迁移目标"。

**后果**：
1. **错误发现晚**：`slave_id: "abc"` 能通过 `devreg/set` 存入 lux，直到采集 tick 才 warn。
2. **每个协议重复实现解析/默认值/校验**——`bind.get("slave_id").and_then(|s| s.parse().ok()).unwrap_or(1)` 这类代码会在每个协议里出现。
3. **对外契约是字符串**：`/api/v1/devices` 与 MCP 工具返回的也是字符串，消费方要自行转型。

**建议**：设备模型已具备参数化地址机制（`model/addr.rs`：`{name}` 占位符 + `+N` 整数偏移）与三态覆盖（`TagOverride` 的 `Option<Option<T>>`），**bind 的类型化是同一原则的未完成部分**：
- 模型增加 `bind_schema: { slave_id: { type: "u8", default: 1, range: [1,247] } }`；
- `model::validate` 在 `devreg/set` 时按 schema 校验（把错误提前到写入时，与 `TagOverride` 的校验口径一致）；
- `expand_to_specs` 产出类型化视图，插件侧拿到的是已解析结构（插件仍可保留 `bind` 原始串用于协议特有字段）。

**注意**：这是**契约变更**（影响 REST 请求体、`DeviceBinding` 序列化、全部协议插件），需要一次 ADR 与迁移路径（旧 `bind` 字符串形态可作为兼容期输入）。

### 3.4 `Effective` 快照的隐式扇入

**事实**：
- `src/model/cache.rs`：`static CACHE: OnceLock<RwLock<Arc<HashMap<String, TagProps>>>>`
- 消费方四方：`pipeline.rs`（每 flush 热路径）、`history/rows.rs`（fan_out 白名单）、`model/discovery.rs`（`note_once` 比对）、`api/routes_tag.rs`（effective 视图）。
- `cache::reload` 尾部还联动两个兄弟子系统：`discovery::retain_modeled` + `history::on_effective_reloaded`（`cache.rs:38-41`）。

**后果**：
1. **模块图上看不见这条扇入**——它不在 `HostContext` / `AppState` 里，静态分析无法发现"改 effective 会影响谁"。
2. **测试隔离靠约定**：`cache.rs` 测试注释自认"CACHE 是进程级全局，测试并行共享——各用例用互不重叠的 model_id/devid"。本轮审查实测到一次由此引发的 e2e flaky（`model_registry_e2e::discovery_list_sorted...` 跨文件并行偶发失败，单文件 3/3 稳定）。
3. 多进程/多租户演进时是硬障碍。

**建议**：`Arc<ModelCache>` 经 `HostContext` 注入（与 `rtdata_watch` 的注入方式一致）；`reload` 的两个联动改为订阅者注册（`Vec<Box<dyn Fn(&HashMap<...>)>>` 或 channel）。存量 `OnceLock` 可作为"默认实例"的惰性初始化保留，**不必一次性迁完**。

**验收判据**：`grep -c "OnceLock" src/model/cache.rs` 归零或仅剩测试辅助；e2e 全量并行（不限 `--test-threads`）稳定。

### 3.5 `write_batch` 热路径双重 clone（零风险小改）

**事实**：`src/db/rtdata.rs:86-89`
```rust
argv.push(key);
for (f, v) in pairs {
    argv.push(f.clone());   // field 名 clone
    argv.push(v.clone());   // cell JSON clone
}
```
`by_devid` 已拥有 `(String, String)`，聚合时再 clone 一份，随后 `argv.iter().map(|s| s.as_str())` 又借一次。

**后果**：50 万点/秒门禁下每点 2 次纯多余 `String` 分配（~2×点数量/秒）。bench 到 2.1–2.4M 说明有余量，但这是纯浪费。

**建议**：聚合直接产出 `Vec<&str>` 借用（`argv` 改为 `Vec<&str>`，`key` 单独持有），去掉两次 clone。**改动 <10 行，无行为变化，有 `db::rtdata::batch_tests` 5 个测试兜底。**

### 3.6 `api/` 承担领域逻辑

**事实**：
- `src/api/routes_lua.rs` 实现段 718 行（`#[cfg(test)]` 在 719 行），其中 handler 仅 6 个；其余是脚本资产域逻辑：`resolve_script_path`(:311 路径白名单)、`write_with_versioning`(:445 版本化写入)、`rollback_script`(:237)、`scan_libs`(:478) 等。**同一资产域的执行侧规则在 `plugin/lua_core/loader.rs`**（`SCRIPTS_ROOT`/`route()`/`RESERVED_TOP`）。
- `src/api/rtdata_watch.rs` 915 行，含 `RtdataWatchService`(:56)、`supervise` 自愈(:385)、`classify_failure`(:414)、`LaggedGate`(:477)——是**带监督的常驻后台服务**，被 `lib.rs:382-388` 常驻构造并 spawn。
- `users`/`instances` 两张核心表无 `db/` 封装：`src/api/` 内裸 RESP 命令 16 处（`routes_meta.rs:150`、`routes_group.rs:44`、`routes_user.rs:51` 等）。`db/` 已有 8 个域模块（alarms/dev_registry/secrets/ext_token/identity/stat/luakv/rtdata），唯独漏这两张。

**判断**：这不是编码错误，是**缺少领域服务层**——`service/` 目录名已被 Windows Service/systemd 占用，真正的领域服务无处安放，只能堆进 `api/`。

**建议**（分三小步，各自独立可做）：
1. `rtdata_watch` 移出 `api/` → 顶层模块（与 `io_trace`/`log_ringbuffer`/`event_bus` 同级），api 只留 `routes_rtdata_subscribe.rs` 薄路由；
2. 脚本资产操作抽 `plugin/lua_core/asset_store.rs`（save/versioning/rollback/scan/path 校验），routes_lua 退化为参数解析——**路径白名单这类安全逻辑也由此与 loader 同层统一**；
3. 补 `db/instances.rs` / `db/users.rs` 薄 CRUD + 校验钩子，消灭 api 层裸 RESP。

**不做的理由**：`model/store.rs` 直连 lux 是**有据可查的边界决策**（头注声明"口径同 db/dev_registry"），保留即可，但建议在架构文档中显式写明"model 域自带存储访问"，避免后来者以为是漏封装。

### 3.7 四协议客户端骨架复制

**事实**：opcua/s7/eip/modbus 的 `run` 是同一骨架——`tokio::select!` 四臂（`opcua:354` / `s7:485` / `eip:340` / `modbus:342`）+ 惰性连接 + tick 计时 + `tracker.tick_ok/fail` + 退出 flush，每家约 80–120 行同构。

**已发生的漂移**（实证）：
- `opcua_client/mod.rs:377` 与 `eip_client/mod.rs:370` 用 `session_reconnect_count`
- `s7_client/mod.rs:508` 用 `reconnect_count`
- `lua_north/mod.rs:579` 又用 `reconnect_count`

同一语义三个键名 → `/inst/stats` 的 `extra` 字段跨插件不一致，监控面板需要特判。

**建议**：抽 `plugin::client_driver`（泛型 over 协议会话）——`loop { select! }` + 惰性连接 + tick 记账 + tracker 为框架代码，协议侧只实现 `connect() / read_tick() / handle_write()` 三个 async trait 方法。

**时机**：**不要现在做**。四份直白代码的维护成本目前低于一个错误抽象的代价。**第 6 个协议（IEC-104，CLAUDE.md 路线中有）动工前**是正确触发点——那时先让新协议走新驱动，存量按-touch 迁移。

### 3.8 `AppConfig` 持续膨胀

**事实**：`src/config.rs` `AppConfig` 14 字段，其中 7 个是子配置节（`lux/mqtt/log/bridge/pipeline/history/rtdata`）+ 7 个散键（`api_port`/`api_bind_host`/`jwt_secret`/`lua_stdlib_upstream`/`lua_stdlib_pin`/`event_bus_capacity`/`hwid`）。`HistoryConfig` 单节已 17 字段（含健康阈值、告警阈值）。

**判断**：当前尚可控（`history` 有完整 `validate()` + 默认值漂移测试，是好范式），但每加一个子系统就多一节，且散键已开始出现（`lua_stdlib_*` 两键裸放顶层，与分节惯例不一致）。

**建议**：**立规则而非现在重构**——新子系统进 `config.rs` 必须同时提供 `validate()` 与"默认值漂移"单测（照抄 `HistoryConfig`）；散键逐步归入子节（`lua_stdlib_*` → `[lua]`）。

### 3.9 `Sample.values` 的堆分配密度（深层，暂不动）

**事实**：`src/types.rs:99-107`
```rust
pub struct Sample {
    pub hwid: Hwid,
    pub devid: String,
    pub values: BTreeMap<String, SampleValue>,
    pub timestamp: i64,
}
```
`BTreeMap` = 每 tag 一次节点分配 + 有序插入；`devid: String` = 每 Sample 一次分配。

**判断**：在 2.1–2.4M 点/秒的实测下**不是瓶颈**，属"余量"而非"缺陷"。改成 `SmallVec`/`Box<[(Arc<str>, SampleValue)]>` 会牵动全部协议插件与 `write_batch`，性价比需实测支撑。

**建议**：**压测不达标时再评估**；当前只做 3.5 那处零风险优化。

## 4. 建议的实施顺序

**第一批（立即可做，成本低收益高）**
1. 依赖可复现（3.1）——先解决 CI 私有依赖可达性，再选 submodule/vendor；
2. 主链路丢数据计数（3.2）——约 30 行 + 1 个恒等式单测；
3. `write_batch` 零拷贝（3.5）——<10 行。

**第二批（下次触碰时顺手还）**
4. `Effective` 显式注入（3.4）——同时修掉测试 flaky；
5. `bind` 类型化（3.3）——需 ADR + 兼容期，单独排期。

**第三批（按触发点）**
6. `api/` 三小步（3.6）——各自独立，可随手做；
7. `client_driver`（3.7）——**等第 6 个协议**；
8. `AppConfig` 规则（3.8）——下次加子系统时执行；
9. `Sample` 分配（3.9）——压测不达标时再评估。

## 5. 明确不建议做的事

| 项 | 理由 |
|---|---|
| 提前抽 `client_driver`（四协议尚少） | 四份直白代码优于一个错误抽象；等第 6 协议 |
| 一次性迁移全部 `OnceLock` | 存量有清晰属主与文档；只改 `cache`（3.4）即可 |
| 重写 `Sample` 为紧凑布局 | 当前吞吐有余量；改动面横跨全部协议插件 |
| 把 `model/store.rs` 收进 `db/` | 该直连是有据可查的边界决策，不是漏封装 |
| 为 api 层引入完整 DDD 分层 | 当前 28 个 routes 的规模不支持这一层抽象成本 |

## 附录 A：审查方法与证据边界

**方法**：三路并行子代理（分层架构 / 功能域耦合 / 规范纪律）+ 主会话独立交叉验证（依赖矩阵、卫生统计、配置/feature 枚举、关键结论代码复核）。

**证据边界**：
- 行号基于 HEAD `d188155`；后续改动需重新定位。
- `history` feature 在本机不可编译（tsink stub），历史链结论为静态阅读 + 既有 bench 记录转述。
- 未跑真机协议验证；未重跑性能 bench。
- 生产段 `unwrap/expect` 统计：全库 26 处（多为 `expect("锁中毒")` 类合理惯用）；`unsafe` 全库 1 处（Windows `SetConsoleCtrlHandler` FFI）。
- `Arc<Mutex>` 生产段 1 处；`Mutex` 12 处（多为 `OnceLock<Mutex<…>>` 单例）；`OnceLock` 单例 5 处：`model/cache.rs`、`model/discovery.rs`、`history/engine.rs`、`process_metrics.rs`、`self_monitor/mod.rs`。

**已确认的良好实践**（评审时应保留，勿在优化中破坏）：
- 双总线把 N×M 收敛为 N+M；lux 为唯一数据交汇点；
- channel 背压语义逐条显式（hub 512 send 背压 / worker 256 / CtlRegistry 64 try_send 满丢计数 / event broadcast 256 Lagged 自弃 / history 有界满即丢计数）；
- 历史旁路五桶计数不变量（`rows.rs:48-56`）；
- `rtdata_watch` 一致性工程（epoch / 双 bump / LaggedGate 退避 / supervise 自愈）；
- 插件契约正交（Source/Sink/Processor，参数即依赖，trait 无状态）；`inventory` 静态注册 + `find_source` 优雅降级；
- 实例生命周期（`claim_starting` 原子认领 + supervisor 唯一 join 判 Failed + 在飞启动取消令牌）。

## 附录 B：与既有技术债登记的关系

[架构详解](../architecture.md) §四 已登记 4 项 P2 技术债（api 层过厚 / 四协议骨架复制 / effective 隐式扇入 / `db/dev_registry.rs` 命名错位）。本文 3.6 / 3.7 / 3.4 是其展开与处置建议；命名错位一项属低价值改名，未纳入本文排序（如需可作单独 chore）。
