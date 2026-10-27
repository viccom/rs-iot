# rs-iot 架构详解（模块职责 / 存储布局 / 环境依赖）

> **状态**：活文档（随模块结构演进；改动 `src/` 顶层模块或 lux 表布局时同步本文）
>
> 本文是 `CLAUDE.md`「项目地图」的展开版。CLAUDE.md 保留数据流图与扩展点（高频速查），
> 本文承载模块职责明细、lux Hash 布局、环境依赖细节。
> 分层与依赖方向的**审查结论**见 §四（2026-10-27 架构审查）。

## 一、数据流与双总线（速览）

```
── 数据上行 ─────────────────────────────────────────────────────────
N × Source(dataSim/opcua/modbus/s7/eip/lua/sidecar) ─► hub(mpsc 512)
  ─► pipeline dispatcher(按 devid FNV-1a 分片) ─► N worker(Processor + 攒批 50ms)
  ─► lux rtdata:<devid>
  读出口：GET /api/v1/rtdatas · SSE /rtdatas/subscribe(ksub 缺省) · luaNorth 北向(pull)

── 控制下行（CtlRegistry，多源写入口 → 实例 cmd_rx）───────────────────
MQTT <hwid>/command ─┐
POST /api/v1/command/send ─┼─► CtlRegistry.try_send(64) ─► 实例 Source::run 的 cmd_rx ─► handle_write
未来北向 UA Server/DA/Modbus slave/104 ─┘
管理面：REST :8790 (axum + JWT) ─► InstanceManager（动态启停）/ send_ctl
```

**关键不变量**：lux rtdata 是南北向唯一交汇点（single source of truth）；单 Source 启停不影响管道；
写命令必经 CtlRegistry（channel 满丢弃，不阻塞数据面）。

## 二、模块职责（`src/`，按数据流顺序）

**模块职责**（`src/`，按数据流顺序）：

| 模块 | 职责 |
|---|---|
| `lib.rs` | 库入口。`run_app_with_shutdown` 编排全链路：lux 启动 → identity bootstrap → hub/InstanceManager/MqttPublisher/pipeline/axum 全部挂到 `CancellationToken` |
| `main.rs` | CLI 入口（run / install / uninstall / service 子命令），clap 解析 |
| `config.rs` | `AppConfig`：toml 加载，缺失字段走 Default（api_port 8780 / lux 6390 / mqtt 5s） |
| `error.rs` | `RsIotError`（thiserror + `#[non_exhaustive]`）+ `TResult<T>` 别名 |
| `types.rs` | 数据模型 + newtype ID（见编码约定） |
| `host.rs` | `HostContext`（lux client + hwid + shutdown + `CtlRegistry` 命令路由表 + `BridgeRegistry` 外部连接分发），注入每个插件 task |
| `model/` | **设备模型注册表域（物模型，M-1 起；决策 2026-08-25 元数据唯一来源）**：`types.rs`（DeviceModel/TagTemplate/DeviceEntry/TagOverride 三态覆盖/DeviceBinding + serde）、`addr.rs`（参数化地址 resolve 纯函数：`{name}` 占位符 + `+N` 整数偏移）、`effective.rs`（模板默认∪覆盖合并 + 顶层 unit/desc 垫底）、`validate.rs`（结构自校验 + 引用完整性：模型存在/params 齐/覆盖 tag∈模型）、`store.rs`（lux `devmodels`/`devreg` CRUD + 删除引用守卫 + update 引用预检）、`migrate.rs`（旧内联点表一次性迁移）、`cache.rs`（effective 物化缓存，写路径刷新）、`expand.rs`（装配期绑定展开，M-2）、`discovery.rs`（**发现引擎 M-4：pipeline 出口未建模标签→`discoveries` 队列（effective O(1) 比对 + once-set 热路径 + per-entry 60s 节流聚合 + LRU 5000 摊还逐出 + glob_match）+ clear/sweep/adopt 对账）——REST 面 `routes_model.rs`/`routes_devreg.rs`（**过渡路径 `/api/v1/devreg*`，M-2 接管 `/api/v1/devices` 系**）+ `routes_discovery.rs`（队列/清除/adopt 一键建模）+ `routes_tag.rs`（**M-4 改造：effective 视图 + devreg.overrides 三态写入 + legacy-export**；旧 `db/tag_registry.rs` tags CRUD 已随 tags 表退役删除）+ `POST /api/v1/migrate/inline-devices` |
| `plugin/` | `traits.rs`（Source/Sink/Processor 契约）、`registry.rs`（inventory 静态注册）、`instance.rs`（生命周期管理 + `send_ctl` 写命令投递 + **M3 supervisor：task 异常退出标 Failed + 告警**）、`builtin/`（datasim 模拟采集 / opcua_client OPC UA 采集 / modbus_client Modbus TCP/RTU(serial/over-tcp/over-udp) 采集 / s7_client 西门子 S7 全系列采集 / **eip_client 罗克韦尔 AB EtherNet/IP 采集（rust-ethernet-ip，批量读 + tag 位寻址）** / lua_north 北向发布（mqtt-north，`[mqtt]` 兼容迁移） / **sidecar_client 外部插件 bridge**）、`ticker.rs`（采集 ticker jitter helper：FNV-1a 确定性相位错峰，`pipeline::route` 复用同算法 pub(crate)） |
| `pipeline.rs` | dispatcher（hub 唯一消费者，按 devid FNV-1a 分片）+ N worker（各持 `EmbeddedClient::clone` 独立 session Mutex，攒批 50ms → `db::rtdata::write_batch` B 形态 HSET 多对；worker panic 由 dispatcher join 检测补 gauge + 计死亡；退出 best-effort flush + cancel 级联 + join） |
| `history/` | 历史时序存储写/读路径（旁路）：**写（Phase 1b）**——`rows.rs` fan_out 白名单过滤（effective() L3）/有界 channel（满即丢+计数）/单写线程 8MiB 栈 + deadband/批内去重/lane 防混型；`engine.rs` StorageBuilder D6 资源边界组装 + D10 close；**读（Phase 2）**——`mod.rs` HistoryReader trait 族（无条件编译，tsink 类型不出 engine.rs）+ `engine.rs` EngineReader raw/downsample（秒↔ms、P4 类型映射、P12 错误分层），REST 面 `src/api/routes_history.rs`（`GET /api/v1/history`，见 docs/reference/rest-api.md §4.18）；**Phase 3 治理（2026-08-26）**——`rows.rs` 写错误五型分桶（WriteErrorKind，Σkind==errors）+ 死区评估计数（削减率分母）、`mod.rs` HistoryTelemetry trait（内存水位/series 引擎口径，无条件编译）+ `engine.rs` impl、`lib.rs` maybe_init 全程 8MiB spawn_scoped 专线（陷阱 4：Windows debug 主线程 init 栈溢出 0xC00000FD，见 `tests/history_init_stack_e2e.rs`）；三层开关 L1 feature `history` 默认 off / L2 `[history].enabled` / L3 `TagProps.history`，见 `docs/designs/2026-08-25-history-write-path-plan.md` + `2026-08-25-history-read-subscribe-plan.md` + `2026-08-26-history-phase3-governance-plan.md` |
| `transform/` | `passthrough`（默认）/ `rhai`（feature `transform`，默认开启——**已冻结 deprecated** Q7：不接线不投入，数据加工走 luaTransform） |
| `db/` | `build_lux_config`、`shutdown_lux`（先 SAVE）、`identity::bootstrap`（幂等建 admin）、`rtdata::write_batch`（B 形态：聚合 devid → 逐 devid HSET 多对，per-devid 容错单 devid 失败不丢整批；`write` 单 Sample test-only 兼容入口）、`ext_token::issue/verify/revoke`（外部插件 token，hmac_sha256） |
| `api/` | axum 路由 + JWT 中间件（auth.rs）+ 各 routes_*.rs + rust-embed 静态前端；`routes_history.rs` 历史读端点（§4.18）、`rtdata_watch.rs` RtdataWatchService（ksubscribe("rtdata:*") 消费 + per-devid 版本表 + epoch 重同步——`/rtdatas/subscribe` ksub 读源底座，`[rtdata].sub_impl` ksub/poll 双读源）、`routes_metrics/`（render.rs+mod.rs）Phase 3 治理 family——archived_tags/series/内存水位/死区评估与削减率/写错误五桶分型（spawn_blocking 采 telemetry，仅引擎 ok 输出，§4.10） |
| `service/` | Windows Service（`windows-service` dispatch）/ systemd unit 生成 |
| `hwid.rs` | `eiot` 前缀 + sha256(CPU brand + ARCH) 取 8 位，每机稳定 |

## 三、存储与运行环境

**lux Hash 布局**（对标 Lite_IOT `bRedka`，清单见 `src/db/identity.rs` 头注）：
`users` / `passwords`(bcrypt) / `groups` / `instances` / `devices` / `children` / `accesskey` / `ext_tokens`(外部插件 token: field=instId, value=JSON{token_hash,created_at,enabled}) / `rtdata:<devId>`（field=`<devid>.<tag>`，value=`[ts, localTime, value]` JSON） / `devmodels`（设备模型：field=model_id，value=DeviceModel JSON{params, tags[参数化地址+模板默认 props]}，M-1） / `devreg`（全局设备注册表：field=devid，value=DeviceEntry JSON{model 引用, params, overrides 三态覆盖}；删除级联解绑实例引用+dev2inst+rtdata） / `discoveries`（发现队列：field=tag_key，value=Discovery JSON{devid,tag,first_ts,last_ts,count,sample_value}；M-4 发现引擎出口——pipeline flush 未建模标签 + lua/sidecar 校验闸拒收点登记（D-M4；effective O(1) 比对 + 60s 节流聚合 + LRU 5000 + seen_modeled 随 effective 快照修剪），人工 adopt 一键建模，见 `src/model/discovery.rs`） / `tags`（**已退役 M-4**：Phase 1a 运行时属性卡被 devreg.overrides 吸收（D-M3）；遗留数据经 `GET /api/v1/tags/legacy-export` 只读导出过渡一个版本，见 designs/2026-08-25-device-model-registry-plan.md）。

**环境依赖**：
- `lux = { path = "../lux" }` — 需 lux 源码在 rs-iot **同级目录**（任何机器/盘符均可；clone `https://github.com/lux-db/lux.git` 到同级 + 按 `docs/ops/lux-upstream-sync-strategy.md` 恢复 2 个本地补丁，或直接从私人 fork `https://git.metme.top/viccom/lux.git` 拉取含补丁的 main）。修改 lux 会直接影响本项目的编译与行为（EmbeddedClient 无 Debug、`ServerConfig` 无 Builder 等特性已被本项目代码 `expect` 依赖）。
- `tsink = { path = "../tsink_fork", optional = true }` — 历史时序存储（Phase 1b）同为**同级目录** path 依赖：clone `https://github.com/viccom/tsink` 到同级 `../tsink_fork` 并 checkout tag `rs-iot-1b-baseline`（@ 277e744，基线 pin）。仅 feature `history` 引入——`--features history` 缺它**不编译**；默认/裁剪构建（feature off）**不需要**该目录。
- **lux 上游同步**：rs-iot 非 lux 维护方（无 PR 权限），以"克隆官方源 + 本地小补丁"跟随上游；同步流程、rebase 冲突决策、风险点与应急恢复见 `docs/ops/lux-upstream-sync-strategy.md`。
- `D:\Go_Codes\Lite_IOT` — 对标参考实现，新功能常按"Lite_IOT 的 X 模块"定位。
- 端口：lux RESP 6390（避开本机 redis 6379）、REST 8780（config.toml 中 8780 被占用临时改 8790）、MQTT 公共 broker broker.emqx.io:1883（开发验证用）。
- 日志：`tracing` + `tracing-subscriber` + `tracing-appender`（tokio-rs 官方）。终端输出始终开启；**`[log].dir` 默认 `./logs`（开箱即落盘，M3）**，daily 轮转 `rs-iot.log.YYYY-MM-DD` + **保留期清理**（`max_days` 默认 7 按天删旧 / `max_size_mb` 默认 0 不限，>0 按总大小删最旧；cleanup task 启动跑一次 + 每 1h，跳过当天活动日志）。**退出 flush**（`init_tracing` 返回 `WorkerGuard`，main/service 持有到退出 drop；service `process::exit` 前显式 drop）。级别优先 `RUST_LOG`，缺省 `[log].level`（默认 info）；显式 `dir=""` 关闭文件层。opcua/modbus 采集/连接/读写均有 `[opcuaClient]`/`[modbusClient]` 前缀日志。

## 四、架构审查结论（2026-10-27，四路并行深查）

**无 P1 结构性缺陷**。实测依赖矩阵严格单向：`lib.rs 装配 → {api, pipeline} → {plugin, model, history} → {db, types, error, event_bus, host}`，
无环、无反向依赖（`db→api` 仅存在于一条历史注释）。

已确认的良好实践：
- **双总线降维**：N 协议 × M 出口 的 N×M 耦合收敛为 N+M；
- **channel 背压全显式**：hub 512（send 背压）/ CtlRegistry 64（try_send 满丢+计数）/ event broadcast 256（Lagged 自弃）/ history 有界（满即丢+计数）；
- **大文件带大测试**：`writer.rs`/`contract.rs`/`modbus_client` 约一半行数是内联测试，生产段 unwrap/expect 全库仅 26 处；
- **共享状态克制**：`Arc<Mutex>` 全库 4 处，`HostContext` 9 字段全为无状态工具句柄。

已知技术债（P2，按改进价值排序）：
1. **api 层承担了本应属领域服务的厚度**：Lua 脚本资产存储（`routes_lua.rs` ~500 行业务逻辑）、
   `RtdataWatchService`（915 行常驻监督服务）、`users`/`instances` 核心表无 db 层封装；
2. **四协议客户端 run 循环骨架复制粘贴**（opcua/s7/eip/modbus 各 ~100 行同构，stat 键名已漂移：
   `session_reconnect_count` vs `reconnect_count`）——建议第 6 个协议（IEC-104）动工前抽 `client_driver`；
3. **`effective` 快照经 OnceLock 被 pipeline 热路径/history/discovery/api 四方隐式扇入**——
   不在 HostContext/AppState 里，模块图不可见；
4. `db/dev_registry.rs` 命名错位（管 `dev2inst` 归属表而非 `devreg` 设备注册表）。

> 完整审查报告（含证据链与逐条复验）见会话记录；修复跟踪见 `CLAUDE.md`「已知技术债」指针。
