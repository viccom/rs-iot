# rs-iot 文档索引

> 本文件是 `docs/` 的唯一索引。新增/移动/改名/归档文档必须同步更新本表（规范见 [conventions/documentation.md](conventions/documentation.md)）。
>
> 状态标记：**已实施** = implemented（头部块含实施 commit/代码/测试）；**部分实施** = partially-implemented（头部块列明遗留）；**待实施** = accepted；**讨论中** = draft；**活文档** = 随代码演进持续维护。单篇文档的实施详情（commit/代码/测试/遗留）以其头部块为准。
>
> 历史归档：2026-08-17 归档 30 个过时文档（git 66945dd）；2026-08-19 六类分域重组（`plan-*` 平铺与 `superpowers/specs/`、`plans/` 并入下述结构）。

## 主题链路视图（决策 → 设计 → 代码 → 测试 → 文档）

按功能主题看全链路，一眼定位"每个功能的来龙去脉与实施状态"：

| 主题 | 决策（why） | 设计（build） | 代码 | 测试 | 指南/参考 |
|---|---|---|---|---|---|
| 南向协议接入 | [feature 裁剪](decisions/2026-08-07-feature-gating-protocols.md)（已实施） | [eipClient](designs/2026-08-17-eip-client-design.md)（已实施·真机待验） | `src/plugin/builtin/{opcua,modbus,s7,eip}_client/` | `tests/{s7,modbus_multi_slave,eip_client,feature_gate}_e2e.rs` | CLAUDE.md 验证规则节 |
| Lua 脚本系统 | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | [采集系统](designs/2026-08-06-lua-script-collector-design.md)（已实施）→ [平台演进](designs/2026-08-18-lua-script-system-evolution-design.md)（已实施·阶段 A/B/C） | `src/plugin/lua_core/` + `src/plugin/builtin/{lua_client,lua_north,lua_transform}/` | `tests/lua_*_e2e.rs` + lua_core 内单测 | [lua-script-guide](guides/lua-script-guide.md) |
| 外部插件 bridge | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | —（[协议规约](reference/external-plugin-protocol.md)即设计） | `src/plugin/builtin/sidecar_client/` | `tests/sidecar_e2e.rs` | [开发指南](guides/external-plugin-dev-guide.md)、`examples/ext-plugin-*` |
| 可观测性（M4+P1） | — | [packet-trace](designs/2026-08-10-packet-trace-design.md)（部分实施）· [log-ringbuffer](designs/2026-08-10-log-ringbuffer-design.md)（已实施） | `src/io_trace.rs`、`src/log_ringbuffer.rs`、`src/api/routes_{metrics,trace,logs}.rs` | `tests/{trace,log,metrics}_e2e.rs` | [埋点规范](conventions/instrumentation-conventions.md)、[rest-api](reference/rest-api.md) §4.10+ |
| AI 网关集成 | [技术选型](decisions/2026-08-12-gateway-tech-selection.md)（已实施） | [v3 设计](designs/2026-08-11-ai-agent-integration-v3-design.md)（已实施）→ [分层演进](designs/2026-08-18-multi-system-gateway-design.md)（**待实施**） | `crates/rsiot-gateway/`、`crates/rsiot-mcp/` | gateway/mcp crate 内单测 | [部署指南](ops/deploy-gateway-and-mcp.md) |
| 设备模型注册表（物模型） | [元数据唯一来源决策](decisions/2026-08-25-device-model-registry.md)（已实施） | [四期实施计划](designs/2026-08-25-device-model-registry-plan.md)（**已实施**·M-1~M-4 全并） | M-1~M-4 全并入（`src/model/` 九模块 + 六插件模型驱动 + lua/sidecar v2 + 发现引擎 + tags 退役 + 前端四面板） | `tests/model_registry_e2e.rs` + `tests/tag_effective_e2e.rs` | 决策文档 §3 业界对标 |
| 历史时序存储（tsink） | [tsink 选型决策](decisions/2026-08-24-history-storage-tsink-selection.md)（待实施·含实测对比） | [总设计与路线图](designs/2026-08-24-history-storage-master-plan.md)（待实施·ADR 权威版）→ [Tag Registry](designs/2026-08-24-tag-registry-plan.md)（已实施·Phase 1a 地基；tags 表于 M-4 退役被 devreg.overrides 吸收）→ [写路径 Phase 1b](designs/2026-08-25-history-write-path-plan.md)（已实施·四门禁全过·已并 master aedd1cf）→ [读+订阅 Phase 2](designs/2026-08-25-history-read-subscribe-plan.md)（已实施·feat/history-read-sub）→ [治理收口 Phase 3](designs/2026-08-26-history-phase3-governance-plan.md)（已实施·feat/history-phase3·治理 metrics/前端历史查询/init_failed+栈溢出 e2e/浏览器实测） | Phase 1b/2/3 已实施：`src/history/` + `src/api/routes_history.rs` + `src/api/rtdata_watch.rs`（读侧 effective() 缓存 = `src/model/cache.rs`） | `tests/history_{dataflow,write,kill_recovery,read}_e2e.rs` + `tests/rtdata_{subscribe,watch}_e2e.rs` + `tests/tag_effective_e2e.rs` | 决策文档附录（实测基线/工件位置） |
| 现场部署与测试 | — | — | `field-kit/` | `field-kit` e2e 脚本 | [field-e2e-windows](ops/field-e2e-windows.md) |

## guides/ — 开发与使用指南（怎么用/怎么开发）

| 文档 | 说明 | 状态 |
|---|---|---|
| [plugin-development-guide.md](guides/plugin-development-guide.md) | 插件体系权威规范：Source/Sink/Processor、命名/目录、测试门槛（review PR 准绳） | 活文档 |
| [lua-script-guide.md](guides/lua-script-guide.md) | luaClient 动态脚本采集：协议解析、ctx API L1-L6、沙箱护栏 | 活文档 |
| [external-plugin-dev-guide.md](guides/external-plugin-dev-guide.md) | 外部南向插件从零开发总纲：三条扩展路径选型 + bridge 接入 | 活文档 |

## reference/ — 接口参考（是什么）

| 文档 | 说明 | 状态 |
|---|---|---|
| [rest-api.md](reference/rest-api.md) | 管理 API 全表：JWT 鉴权、/api/v1 路由、lux 持久层 | 活文档 |
| [external-plugin-protocol.md](reference/external-plugin-protocol.md) | sidecarClient bridge 二进制协议规约 v1（帧格式/鉴权/握手） | v1 已实施 |

## conventions/ — 规范约束（必须遵守）

| 文档 | 说明 | 状态 |
|---|---|---|
| [documentation.md](conventions/documentation.md) | 文档管理规范：分域/命名/元数据/实施块/索引/归档流程 + 校验脚本 | 有效 |
| [instrumentation-conventions.md](conventions/instrumentation-conventions.md) | 实例指标埋点宪法（M4）：核心 6 字段、extra 扩展、Prometheus 命名 | 活文档 |

## decisions/ — 决策记录（为什么选 X，ADR）

| 文档 | 说明 | 状态 |
|---|---|---|
| [2026-08-07-plugin-extension-strategy.md](decisions/2026-08-07-plugin-extension-strategy.md) | 插件扩展策略：动态库 vs SDK crate vs sidecar → 走 sidecar+lua | 已实施 |
| [2026-08-07-feature-gating-protocols.md](decisions/2026-08-07-feature-gating-protocols.md) | 内置协议 Cargo feature 裁剪（方案 B）评估与实施 | 已实施 |
| [2026-08-12-gateway-tech-selection.md](decisions/2026-08-12-gateway-tech-selection.md) | API Gateway 技术选型调研：axum 0.8 + reqwest 手写反代（全部 file:line 引证） | 已实施 |
| [2026-08-24-history-storage-tsink-selection.md](decisions/2026-08-24-history-storage-tsink-selection.md) | 历史时序存储选型：tsink 内嵌（fork 自管）+ 独立 server 形态实测对比 + 单机 10 万点位级定位 | 待实施 |
| [2026-08-25-device-model-registry.md](decisions/2026-08-25-device-model-registry.md) | 设备模型注册表（物模型）：元数据唯一来源原则（宪法级）+ 全局设备实体 + tags 表吸收 + 严格校验 | 已实施 |

## designs/ — 设计定稿（怎么做，日期命名）

| 文档 | 说明 | 状态 |
|---|---|---|
| [2026-08-06-lua-script-collector-design.md](designs/2026-08-06-lua-script-collector-design.md) | Lua 脚本采集系统完整设计（luaClient，M10） | 已实施 |
| [2026-08-10-packet-trace-design.md](designs/2026-08-10-packet-trace-design.md) | 报文 trace 开关（P1 子系统 2，阶段 1） | **部分实施**（阶段 2 后置） |
| [2026-08-10-log-ringbuffer-design.md](designs/2026-08-10-log-ringbuffer-design.md) | 日志 ring buffer（P1 子系统 3） | 已实施 |
| [2026-08-11-ai-agent-integration-v3-design.md](designs/2026-08-11-ai-agent-integration-v3-design.md) | AI Agent 集成 v3：独立 API Gateway 程序（v1/v2 已归档） | 已实施 |
| [2026-08-17-eip-client-design.md](designs/2026-08-17-eip-client-design.md) | eipClient：罗克韦尔 AB EtherNet/IP 南向插件（rust-ethernet-ip 1.2.0） | 已实施（真机待验） |
| [2026-08-18-multi-system-gateway-design.md](designs/2026-08-18-multi-system-gateway-design.md) | 多系统网关分层：gateway-core 通用引擎 + rsiot-gateway 薄组装 | **待实施** |
| [2026-08-18-lua-script-system-evolution-design.md](designs/2026-08-18-lua-script-system-evolution-design.md) | Lua 脚本系统演进：多角色脚本平台 v0.8（阶段 A/B/C/D 已实施） | **已实施**（A/B/C 已合入 master；D 于 worktree feat/lua-stage-d 待合并） |
| [2026-08-19-lua-script-system-stage-a-plan.md](designs/2026-08-19-lua-script-system-stage-a-plan.md) | 阶段 A 实施计划：13 task TDD（lua_core/护栏/stdlib/manifest/源仓） | **已实施** |
| [2026-08-20-lua-script-system-stage-b-plan.md](designs/2026-08-20-lua-script-system-stage-b-plan.md) | 阶段 B 实施计划：12 task（luaNorth/Sink 接线/mqtt-north 契约/secret/cloud 配方/透传 spike-2） | **已实施** |
| [2026-08-20-lua-script-system-stage-c-plan.md](designs/2026-08-20-lua-script-system-stage-c-plan.md) | 阶段 C 实施计划：9 task（luaTransform 订阅型/EventBus 带值/subscribe 过滤/自环防护/on_timer/rhai 冻结） | **已实施**（已合入 master） |
| [2026-08-21-alarm-minimal-loop-design.md](designs/2026-08-21-alarm-minimal-loop-design.md) | 告警最小闭环设计（Q12 独立 spec：fire/clear 跨角色能力 + lux alarms + SSE/REST，不建独立角色壳） | **已实施**（随阶段 D，待合并） |
| [2026-08-21-lua-stage-d-plan.md](designs/2026-08-21-lua-stage-d-plan.md) | 阶段 D 实施计划（评估收窄：安全最小集 M3+TLS/告警 Q12/清偿批/串口 open 异步安全/upstream 发布准备） | **已实施**（已合入 master） |
| [2026-08-21-api-doc-frontend-gap-closure-plan.md](designs/2026-08-21-api-doc-frontend-gap-closure-plan.md) | REST 文档与前端 UI 缺口补齐计划（5 端点文档/apitest 按钮/secret·stdlib·写命令三 UI + vitest） | **已实施**（已合入 master `736333f`） |
| [2026-08-21-self-monitor-device-plan.md](designs/2026-08-21-self-monitor-device-plan.md) | rs-iot 自监控设备计划（selfMonitor：rsiot 设备 17 只读+3 可写标签/删 sim1 demo/hwid 覆盖键） | **已实施**（待合并） |
| [2026-08-21-device-ownership-design.md](designs/2026-08-21-device-ownership-design.md) | 设备归属模型设计（dev2inst 软关联/SampleSender 拦截/级联闭环 + lux 非 loopback 强制认证 + secrets AEAD + devices 死表废弃） | **部分实施**（T1-T4 归属闭环已实施） |
| [2026-08-21-device-ownership-plan.md](designs/2026-08-21-device-ownership-plan.md) | 设备归属 + lux/RESP 收口 + secrets 加密实施计划（8 task） | **已实施**（待合并） |
| [2026-08-23-startinst-double-start-plan.md](designs/2026-08-23-startinst-double-start-plan.md) | startinst 唯一启动守卫实施计划（活跃态二次启动 409，修幽灵双任务） | 已实施 |
| [2026-08-23-rtdata-subscribe-plan.md](designs/2026-08-23-rtdata-subscribe-plan.md) | 实时数据订阅接口实施计划（OPC UA 式：snapshot 全量 + 周期 datachange，SSE，外部客户端） | 已实施 |
| [2026-08-24-history-storage-master-plan.md](designs/2026-08-24-history-storage-master-plan.md) | 历史时序存储总设计：三层开关/ADR 全表 D1-D11/Phase 1b-5 路线/tsink 语义陷阱/fork backlog/实测基线附录 | 已实施（1a/1b/2/3 已落地；Phase 4 压测/5 演进待） |
| [2026-08-25-device-model-registry-plan.md](designs/2026-08-25-device-model-registry-plan.md) | 设备模型注册表实施计划（devmodels/devreg/discoveries + 四期 M-1~M-4 TDD + 现状处置清单 + 新会话执行指引） | 已实施（M-1~M-4 全并） |
| [2026-08-24-tag-registry-plan.md](designs/2026-08-24-tag-registry-plan.md) | 标签/设备元数据统一存储实施计划（Tag Registry：lux tags 表 + 三路径登记 + REST，历史存储前置地基） | 已实施（tags 表已于 M-4 退役，演进见 [2026-08-25-device-model-registry-plan.md](designs/2026-08-25-device-model-registry-plan.md)） |
| [2026-08-25-history-write-path-plan.md](designs/2026-08-25-history-write-path-plan.md) | 历史写路径实施计划（Phase 1b：tsink path 依赖/L1-L3 三层开关/engine D6 组装/单写线程 deadband+去重+lane/四门禁 e2e 验收记录） | **已实施**（master aedd1cf，2026-08-25；四门禁全过） |
| [2026-08-25-history-read-subscribe-plan.md](designs/2026-08-25-history-read-subscribe-plan.md) | 历史读路径+KSUB 订阅升级实施计划（Phase 2：/api/v1/history raw+downsample/503 矩阵/RtdataWatchService dirty-set/双 impl 等价） | **已实施**（feat/history-read-sub，2026-08-25） |
| [2026-08-26-history-phase3-governance-plan.md](designs/2026-08-26-history-phase3-governance-plan.md) | 历史存储 Phase 3 余项收口实施计划（治理 metrics 7 新 family/HistoryTelemetry 内存水位/前端历史查询面板/init_failed+栈溢出双 e2e/浏览器嵌入实测七项+2 bug 修复） | **已实施**（feat/history-phase3，2026-08-26） |

## ops/ — 部署与运维（怎么跑起来）

| 文档 | 说明 | 状态 |
|---|---|---|
| [deploy-gateway-and-mcp.md](ops/deploy-gateway-and-mcp.md) | AI 集成三件套（gateway/mcp/reasonix）现场部署 | implemented（gateway-core 拆分后待核对） |
| [field-e2e-windows.md](ops/field-e2e-windows.md) | Windows 真实工厂级测试场完整手册（配套 `field-kit/`） | 活文档 |
| [lux-upstream-sync-strategy.md](ops/lux-upstream-sync-strategy.md) | lux 上游同步与本地补丁维护策略、风险与应急恢复 | 活文档 |
| [releases/2026-08-20-lua-stage-a.md](ops/releases/2026-08-20-lua-stage-a.md) | 发布说明：Lua 阶段 A（机制地基）——升级必读/新能力/已知限制 | 已发布 |
| [releases/2026-08-20-lua-stage-b.md](ops/releases/2026-08-20-lua-stage-b.md) | 发布说明：Lua 阶段 B（北向突破）——luaNorth/secret/cloud 配方/透传 | 已发布（待合并） |
| [releases/2026-08-21-lua-stage-c.md](ops/releases/2026-08-21-lua-stage-c.md) | 发布说明：Lua 阶段 C（数据加工）——luaTransform/EventBus 带值/自环防护/rhai 冻结 | 已发布（待合并） |
| [releases/2026-08-21-lua-stage-d.md](ops/releases/2026-08-21-lua-stage-d.md) | 发布说明：Lua 阶段 D（安全与告警）——M3 回显遮蔽/TLS 选项/告警最小闭环/串口 open 行为变化 | 已发布（待合并） |
| [releases/2026-08-21-self-monitor.md](ops/releases/2026-08-21-self-monitor.md) | 发布说明：rs-iot 自监控设备——rsiot 设备 17 只读+3 可写标签/sim1 demo 移除（升级注意）/hwid 覆盖键 | 已发布（待合并） |
| [releases/2026-08-22-device-ownership.md](ops/releases/2026-08-22-device-ownership.md) | 发布说明：设备归属模型——dev2inst 软关联/级联闭环/非 loopback 强制认证（升级必读）/secrets 加密/devices 死表提示 | 已发布（待合并） |

## docs 之外的文档

- 根 [README.md](../README.md)：项目门面（特性/构建/里程碑）；[CLAUDE.md](../CLAUDE.md)：AI 协作指令 + 项目地图（引用本索引大部分文档）
- 代码就近 README：[examples/lua-scripts/](../examples/lua-scripts/README.md)、[examples/ext-plugin-python/](../examples/ext-plugin-python/README.md)、[examples/ext-plugin-opcda-go/](../examples/ext-plugin-opcda-go/README.md)、[field-kit/](../field-kit/README.md)
- 待补：`crates/rsiot-mcp`、`crates/rsiot-gateway`、`frontend/` 各自的 README
