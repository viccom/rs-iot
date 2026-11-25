# rs-iot 文档索引

> 本文件是 `docs/` 的唯一索引。新增/移动/改名/归档文档必须同步更新本表（规范见 [conventions/documentation.md](conventions/documentation.md)）。
>
> 状态标记：**已实施** = implemented（头部块含实施 commit/代码/测试）；**部分实施** = partially-implemented（头部块列明遗留）；**待实施** = accepted；**讨论中** = draft；**活文档** = 随代码演进持续维护。单篇文档的实施详情（commit/代码/测试/遗留）以其头部块为准。
>
> 历史归档：2026-08-17 归档 30 个过时文档（git 66945dd）；2026-08-19 六类分域重组（`plans/` 已于 2026-10-27 正式并入 `designs/`，check_docs 禁止其复活）（`plan-*` 平铺与 `superpowers/specs/`、`plans/` 并入下述结构）。

## 主题链路视图（决策 → 设计 → 代码 → 测试 → 文档）

按功能主题看全链路，一眼定位"每个功能的来龙去脉与实施状态"：

| 主题 | 决策（why） | 设计（build） | 代码 | 测试 | 指南/参考 |
|---|---|---|---|---|---|
| 南向协议接入 | [feature 裁剪](decisions/2026-08-07-feature-gating-protocols.md)（已实施） | [eipClient](designs/2026-08-17-eip-client-design.md)（已实施·真机待验） | `src/plugin/builtin/{opcua,modbus,s7,eip}_client/` | `tests/{s7,modbus_multi_slave,eip_client,feature_gate}_e2e.rs` | AGENTS.md 验证规则节 |
| Lua 脚本系统 | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | [采集系统](designs/2026-08-06-lua-script-collector-design.md)（已实施）→ [平台演进](designs/2026-08-18-lua-script-system-evolution-design.md)（已实施·阶段 A/B/C） | `src/plugin/lua_core/` + `src/plugin/builtin/{lua_client,lua_north,lua_transform}/` | `tests/lua_*_e2e.rs` + lua_core 内单测 | [lua-script-guide](guides/lua-script-guide.md) |
| 外部插件 bridge | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | —（[协议规约](reference/external-plugin-protocol.md)即设计） | `src/plugin/builtin/sidecar_client/` | `tests/sidecar_e2e.rs` | [开发指南](guides/external-plugin-dev-guide.md)、`examples/ext-plugin-*` |
| 插件内核演进 | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | [plugin-kernel-evolution](designs/2026-11-04-plugin-kernel-evolution-design.md)（讨论中） | 目标 `src/plugin/{descriptor,runtime,catalog}.rs`（未实施） | 随实施建立 | [plugin-development-guide](guides/plugin-development-guide.md) |
| 可观测性（M4+P1） | — | [packet-trace](designs/2026-08-10-packet-trace-design.md)（部分实施）· [log-ringbuffer](designs/2026-08-10-log-ringbuffer-design.md)（已实施） | `src/io_trace.rs`、`src/log_ringbuffer.rs`、`src/api/routes_{metrics,trace,logs}.rs` | `tests/{trace,log,metrics}_e2e.rs` | [埋点规范](conventions/instrumentation-conventions.md)、[rest-api](reference/rest-api.md) §4.10+ |
| AI 网关集成 | [技术选型](decisions/2026-08-12-gateway-tech-selection.md)（已实施） | [v3 设计](designs/2026-08-11-ai-agent-integration-v3-design.md)（已实施）→ [分层演进](designs/2026-08-18-multi-system-gateway-design.md)（**待实施**） | `crates/rsiot-gateway/`、`crates/rsiot-mcp/` | gateway/mcp crate 内单测 | [部署指南](ops/deploy-gateway-and-mcp.md)、[reasonix 版本兼容](ops/reasonix-version-compat.md)（v1.25.3 基线 / v2.35.0 实测 / v2.58.1 代码核对） |
| 设备模型注册表（物模型） | [元数据唯一来源决策](decisions/2026-08-25-device-model-registry.md)（已实施） | [四期实施计划](designs/2026-08-25-device-model-registry-plan.md)（**已实施**·M-1~M-4 全并） | M-1~M-4 全并入（`src/model/` 九模块 + 六插件模型驱动 + lua/sidecar v2 + 发现引擎 + tags 退役 + 前端四面板） | `tests/model_registry_e2e.rs` + `tests/tag_effective_e2e.rs` + `tests/history_bi_e2e.rs` | 决策文档附录 · 决策文档 §3 业界对标 |
| 历史时序存储（tsink） | [tsink 选型决策](decisions/2026-08-24-history-storage-tsink-selection.md)（已实施·含实测对比） | [总设计与路线图](designs/2026-08-24-history-storage-master-plan.md)（**部分实施**·ADR 权威版；Phase 1a/1b/2/3 与 10 万点四轮已落地，Phase 4 压测/5 演进待）→ [Tag Registry](designs/2026-08-24-tag-registry-plan.md)（已实施·Phase 1a 地基；tags 表于 M-4 退役被 devreg.overrides 吸收）→ [写路径 Phase 1b](designs/2026-08-25-history-write-path-plan.md)（已实施·四门禁全过·已并 master aedd1cf）→ [读+订阅 Phase 2](designs/2026-08-25-history-read-subscribe-plan.md)（已实施·feat/history-read-sub）→ [治理收口 Phase 3](designs/2026-08-26-history-phase3-governance-plan.md)（已实施·feat/history-phase3·治理 metrics/前端历史查询/init_failed+栈溢出 e2e/浏览器实测） | Phase 1b/2/3 已实施 + BI 大屏端点 stats/batch（2026-10-29 P1，81a0216）：`src/history/` + `src/api/routes_history.rs` + `src/api/rtdata_watch.rs`（订阅 dirty-set 服务；读侧 effective() 缓存见 `src/model/cache.rs`） | `tests/history_{dataflow,write,kill_recovery,read}_e2e.rs` + `tests/rtdata_{subscribe,watch}_e2e.rs` + `tests/tag_effective_e2e.rs` + `tests/history_bi_e2e.rs` | 决策文档附录（实测基线/工件位置） |
| 现场部署与测试 | — | — | `field-kit/` | `field-kit` e2e 脚本 | [field-e2e-windows](ops/field-e2e-windows.md) |

## guides/ — 开发与使用指南（怎么用/怎么开发）

| 文档 | 说明 | 状态 |
|---|---|---|
| [plugin-development-guide.md](guides/plugin-development-guide.md) | 插件体系权威规范：Source/Sink/Processor、命名/目录、测试门槛（review PR 准绳） | 活文档 |
| [lua-script-guide.md](guides/lua-script-guide.md) | luaClient 动态脚本采集：协议解析、ctx API L1-L6、沙箱护栏 | 活文档 |
| [external-plugin-dev-guide.md](guides/external-plugin-dev-guide.md) | 外部南向插件从零开发总纲：三条扩展路径选型 + bridge 接入 | 活文档 |
| [testing-and-verification.md](guides/testing-and-verification.md) | 测试与验证手册：`just check` 门禁、按改动面选验证集、协议真机 e2e、前端 rust-embed 陷阱 | 活文档 |

## reference/ — 接口参考（是什么）

| 文档 | 说明 | 状态 |
|---|---|---|
| [rest-api.md](reference/rest-api.md) | 管理 API 全表：JWT 鉴权、/api/v1 路由、lux 持久层 | 活文档 |
| [external-plugin-protocol.md](reference/external-plugin-protocol.md) | sidecarClient bridge 二进制协议规约 v3（帧格式/鉴权/握手/ConfigPush v2+v3 载荷/凭证） | 活文档 |

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
| [2026-08-24-history-storage-tsink-selection.md](decisions/2026-08-24-history-storage-tsink-selection.md) | 历史时序存储选型：tsink 内嵌（fork 自管）+ 独立 server 形态实测对比 + 单机 10 万点位级定位 | 已实施（选型已落地，Phase 1b/2/3 实施） |
| [2026-08-25-device-model-registry.md](decisions/2026-08-25-device-model-registry.md) | 设备模型注册表（物模型）：元数据唯一来源原则（宪法级）+ 全局设备实体 + tags 表吸收 + 严格校验 | 已实施 |
| [2026-08-26-default-compile-surface.md](decisions/2026-08-26-default-compile-surface.md) | 默认编译面收敛：default=[lua,lua-http,opcua,modbus]，s7/eip/transform 显式启用 | 已实施 |
| [2026-08-31-n5-longrun-degradation-fixes.md](decisions/2026-08-31-n5-longrun-degradation-fixes.md) | 长跑退化 n5 修复决策：长时运行下采集/管道/告警域的退化修复取舍 | 已实施 |
| [2026-08-27-model-extension-slot.md](decisions/2026-08-27-model-extension-slot.md) | 设备模型开放扩展槽（extra）与 `x_` 键契约：前向兼容信封 + 宿主不解释 + 外置应用定位边界 | 已实施 |
| [2026-11-01-upstream-library-fork-policy.md](decisions/2026-11-01-upstream-library-fork-policy.md) | 上游库缺陷处置策略：三层（插件规避 / 最小 fork / 上游 PR）；tokio-s7 fork 4 补丁（S-1/S-4/S-5/S-9）、rust-ethernet-ip 先规避+PR、async-opcua/tokio-modbus 不 fork；含 `[patch.crates-io]` 形态、CI 适配、治理与退出条件 | **已实施**（fork 已接入 832c8e2；CI 可达性/上游 PR 待办） |
| [2026-08-31-n5-longrun-degradation-fixes.md](decisions/2026-08-31-n5-longrun-degradation-fixes.md) | N5 长跑退化修复执行决策：锁解耦竞态自愈加固/payload 损坏语义放宽/输出段预算/tiering 风险登记 | 已实施（D-N5-3 登记不做） |

## designs/ — 设计定稿（怎么做，日期命名）

| 文档 | 说明 | 状态 |
|---|---|---|
| [2026-08-06-lua-script-collector-design.md](designs/2026-08-06-lua-script-collector-design.md) | Lua 脚本采集系统完整设计（luaClient，M10） | 已实施 |
| [2026-08-10-packet-trace-design.md](designs/2026-08-10-packet-trace-design.md) | 报文 trace 开关（P1 子系统 2，阶段 1） | **部分实施**（阶段 2 后置） |
| [2026-08-10-log-ringbuffer-design.md](designs/2026-08-10-log-ringbuffer-design.md) | 日志 ring buffer（P1 子系统 3） | 已实施 |
| [2026-08-11-ai-agent-integration-v3-design.md](designs/2026-08-11-ai-agent-integration-v3-design.md) | AI Agent 集成 v3：独立 API Gateway 程序（v1/v2 已归档） | 已实施 |
| [2026-08-17-eip-client-design.md](designs/2026-08-17-eip-client-design.md) | eipClient：罗克韦尔 AB EtherNet/IP 南向插件（rust-ethernet-ip 1.2.0） | 已实施（真机待验） |
| [2026-08-18-multi-system-gateway-design.md](designs/2026-08-18-multi-system-gateway-design.md) | 多系统网关分层：gateway-core 通用引擎 + rsiot-gateway 薄组装 | **待实施** |
| [2026-08-18-lua-script-system-evolution-design.md](designs/2026-08-18-lua-script-system-evolution-design.md) | Lua 脚本系统演进：多角色脚本平台 v0.8（阶段 A/B/C/D 已实施） | **已实施**（A/B/C/D 全部已合入 master，2026-10-27 核实） |
| [2026-08-19-lua-script-system-stage-a-plan.md](designs/2026-08-19-lua-script-system-stage-a-plan.md) | 阶段 A 实施计划：13 task TDD（lua_core/护栏/stdlib/manifest/源仓） | **已实施** |
| [2026-08-20-lua-script-system-stage-b-plan.md](designs/2026-08-20-lua-script-system-stage-b-plan.md) | 阶段 B 实施计划：12 task（luaNorth/Sink 接线/mqtt-north 契约/secret/cloud 配方/透传 spike-2） | **已实施** |
| [2026-08-20-lua-script-system-stage-c-plan.md](designs/2026-08-20-lua-script-system-stage-c-plan.md) | 阶段 C 实施计划：9 task（luaTransform 订阅型/EventBus 带值/subscribe 过滤/自环防护/on_timer/rhai 冻结） | **已实施**（已合入 master） |
| [2026-08-21-alarm-minimal-loop-design.md](designs/2026-08-21-alarm-minimal-loop-design.md) | 告警最小闭环设计（Q12 独立 spec：fire/clear 跨角色能力 + lux alarms + SSE/REST，不建独立角色壳） | **已实施**（随阶段 D 已合入 master） |
| [2026-08-21-lua-stage-d-plan.md](designs/2026-08-21-lua-stage-d-plan.md) | 阶段 D 实施计划（评估收窄：安全最小集 M3+TLS/告警 Q12/清偿批/串口 open 异步安全/upstream 发布准备） | **已实施**（已合入 master） |
| [2026-08-21-api-doc-frontend-gap-closure-plan.md](designs/2026-08-21-api-doc-frontend-gap-closure-plan.md) | REST 文档与前端 UI 缺口补齐计划（5 端点文档/apitest 按钮/secret·stdlib·写命令三 UI + vitest） | **已实施**（已合入 master `736333f`） |
| [2026-08-21-self-monitor-device-plan.md](designs/2026-08-21-self-monitor-device-plan.md) | rs-iot 自监控设备计划（selfMonitor：rsiot 设备 20 只读+3 可写标签（2026-08-30 起；原 17 只读）/删 sim1 demo/hwid 覆盖键） | **已实施**（已合并 master，2026-10-27 核实） |
| [2026-08-21-device-ownership-design.md](designs/2026-08-21-device-ownership-design.md) | 设备归属模型设计（dev2inst 软关联/SampleSender 拦截/级联闭环 + lux 非 loopback 强制认证 + secrets AEAD + devices 死表废弃） | **部分实施**（T1-T4 归属闭环已实施） |
| [2026-08-21-device-ownership-plan.md](designs/2026-08-21-device-ownership-plan.md) | 设备归属 + lux/RESP 收口 + secrets 加密实施计划（8 task） | **已实施**（已合并 master，2026-10-27 核实） |
| [2026-08-23-startinst-double-start-plan.md](designs/2026-08-23-startinst-double-start-plan.md) | startinst 唯一启动守卫实施计划（活跃态二次启动 409，修幽灵双任务） | 已实施 |
| [2026-08-23-rtdata-subscribe-plan.md](designs/2026-08-23-rtdata-subscribe-plan.md) | 实时数据订阅接口实施计划（OPC UA 式：snapshot 全量 + 周期 datachange，SSE，外部客户端） | 已实施 |
| [2026-08-24-history-storage-master-plan.md](designs/2026-08-24-history-storage-master-plan.md) | 历史时序存储总设计：三层开关/ADR 全表 D1-D11/Phase 1b-5 路线/tsink 语义陷阱/fork backlog/实测基线附录 | 部分实施（1a/1b/2/3 与 10 万点四轮已落地；Phase 4 soak 待） |
| [2026-08-25-device-model-registry-plan.md](designs/2026-08-25-device-model-registry-plan.md) | 设备模型注册表实施计划（devmodels/devreg/discoveries + 四期 M-1~M-4 TDD + 现状处置清单 + 新会话执行指引） | 已实施（M-1~M-4 全并） |
| [2026-08-24-tag-registry-plan.md](designs/2026-08-24-tag-registry-plan.md) | 标签/设备元数据统一存储实施计划（Tag Registry：lux tags 表 + 三路径登记 + REST，历史存储前置地基） | 已实施（tags 表已于 M-4 退役，演进见 [2026-08-25-device-model-registry-plan.md](designs/2026-08-25-device-model-registry-plan.md)） |
| [2026-08-25-history-write-path-plan.md](designs/2026-08-25-history-write-path-plan.md) | 历史写路径实施计划（Phase 1b：tsink path 依赖/L1-L3 三层开关/engine D6 组装/单写线程 deadband+去重+lane/四门禁 e2e 验收记录） | **已实施**（master aedd1cf，2026-08-25；四门禁全过） |
| [2026-08-25-history-read-subscribe-plan.md](designs/2026-08-25-history-read-subscribe-plan.md) | 历史读路径+KSUB 订阅升级实施计划（Phase 2：/api/v1/history raw+downsample/503 矩阵/RtdataWatchService dirty-set/双 impl 等价） | **已实施**（feat/history-read-sub，2026-08-25） |
| [2026-08-26-history-review-fix-plan.md](designs/2026-08-26-history-review-fix-plan.md) | 历史存储 Phase 1/2/3 审查修复计划（High×5 + Medium×11 全修，Low×17 延后登记） | **已实施**（2026-08-26 收口） |
| [2026-08-26-nonbreaking-quality-batch-plan.md](designs/2026-08-26-nonbreaking-quality-batch-plan.md) | 非破坏性质量修复包（B1-B7，审计后续） | **已实施**（2026-08-26 收口） |
| [2026-08-26-p0-p1-bugfix-plan.md](designs/2026-08-26-p0-p1-bugfix-plan.md) | P0+P1 九项 BUG 修复计划（P0×1 + P1×8） | **已实施**（2026-08-26 收口） |
| [2026-08-26-schema-compile-tagkey-plan.md](designs/2026-08-26-schema-compile-tagkey-plan.md) | C1-C3：schema 默认值单源 / 默认编译面收敛 / tag_key 串扰修复 | **已实施**（2026-08-26 收口） |
| [2026-08-29-history-100k-improvement-plan.md](designs/2026-08-29-history-100k-improvement-plan.md) | tsink 10 万点容量与可靠性治理总账（R2-R5 四轮 + N5-N9；含 pressure 工件结论） | **已实施**（结论见 §五-K） |
| [2026-09-01-history-round2-governance-plan.md](designs/2026-09-01-history-round2-governance-plan.md) | tsink R2 治理（L0 消费提速 + persisted index 驻留有界 + N7） | **已被取代**（Task 3 已回退 62ade1c；结论并入 08-29 总账 §五-F） |
| [2026-09-01-history-round3-chunk-passthrough-plan.md](designs/2026-09-01-history-round3-chunk-passthrough-plan.md) | tsink R3：chunk 直通（恢复吞吐 + N7 收口） | **已实施**（结论见 08-29 总账 §五-G） |
| [2026-09-02-history-round4-n8-flush-verify-race-plan.md](designs/2026-09-02-history-round4-n8-flush-verify-race-plan.md) | tsink R4：N8 flush verify 对 compaction 替换的容忍语义 | **已实施**（结论见 08-29 总账 §五-I） |
| [2026-09-03-history-round5-p3-watermark-eviction-plan.md](designs/2026-09-03-history-round5-p3-watermark-eviction-plan.md) | tsink R5：P3 水位粗判（消维护拍全量扫） | **已实施**（结论见 08-29 总账 §五-K） |
| [2026-10-27-architecture-optimization-review.md](designs/2026-10-27-architecture-optimization-review.md) | 架构优化评估（9 项议题：依赖可复现/丢数据总账/bind 类型化/effective 扇入/热路径/api 分层/协议骨架/配置面/Sample 分配）+ 实施顺序与"不建议做"清单 | **部分实施**（2026-11-25 落地第 2、5 项：主链路守恒账、write_batch 去 clone；其余 7 项待裁决） |
| [2026-10-27-external-plugin-config-push-v3-design.md](designs/2026-10-27-external-plugin-config-push-v3-design.md) | 外部插件配置下发升级（ConfigPush v3）：补 `app_config`/`devices[].bind` 两层下发 + 版本集合协商 + 凭证解析下发 + 不变量与不做什么清单 | **已实施**（批次 1+2；批次 3 可选未做） |
| [2026-08-26-history-phase3-governance-plan.md](designs/2026-08-26-history-phase3-governance-plan.md) | 历史存储 Phase 3 余项收口实施计划（治理 metrics 7 新 family/HistoryTelemetry 内存水位/前端历史查询面板/init_failed+栈溢出双 e2e/浏览器嵌入实测七项+2 bug 修复） | **已实施**（feat/history-phase3，2026-08-26） |
| [2026-08-26-history-review-fix-plan.md](designs/2026-08-26-history-review-fix-plan.md) | 历史存储 Phase 1/2/3 审查修复计划（High×5 + Medium×11 全修；Low×17 延后 backlog） | **已实施**（2026-08-26，b81ce26 收口） |
| [2026-08-26-nonbreaking-quality-batch-plan.md](designs/2026-08-26-nonbreaking-quality-batch-plan.md) | 非破坏性质量修复包（审计后续 B1-B7：写失败可见性/默认端口等） | **已实施**（2026-08-26，8605980 验收回填） |
| [2026-08-26-p0-p1-bugfix-plan.md](designs/2026-08-26-p0-p1-bugfix-plan.md) | P0+P1 九项 BUG 修复计划（routes_lua minimal 门控 + 八项修复） | **已实施**（2026-08-26，27dd53b 验收回填） |
| [2026-08-26-schema-compile-tagkey-plan.md](designs/2026-08-26-schema-compile-tagkey-plan.md) | schema 默认值单源 / 默认编译面收敛 / tag_key 串扰修复（C1-C3） | **已实施**（2026-08-26，3a25959 验收回填） |
| [2026-08-27-model-extension-slot-design.md](designs/2026-08-27-model-extension-slot-design.md) | 设备模型域扩展性设计（DeviceModel/TagTemplate 开放扩展槽）：flatten 语义/wire 形态/`x_` 键契约与毕业机制/报警外置与存储梯度定位边界/场景驱动演进路线 | **已实施**（扩展槽+键契约+读透出面：REST 校验/MCP 两读工具/前端只读展示） |
| [2026-10-30-screen-versioning-design.md](designs/2026-10-30-screen-versioning-design.md) | 画面/主题文档版本管理与并发控制（routes_meta 演进）：`sv:` 键族 + 内容判重 + if_version 乐观锁 + rollback + SSE screen 事件 + embedded 无事务的失败矩阵 | **已实施**（166fe25/40698d4；2026-10-30 复审修复：锁序/回滚 no-op/错误路径） |
| [2026-10-30-screen-versioning-plan.md](designs/2026-10-30-screen-versioning-plan.md) | 画面/主题版本管理实施计划（P0 第 1 轮 TDD 任务表 A1~A9 + 开工基线漂移记录） | **已实施**（166fe25/40698d4：11 组测试 + §6 六实测全符） |
| [2026-11-01-layering-coupling-review.md](designs/2026-11-01-layering-coupling-review.md) | 分层耦合专项：实测 10 模块强连通分量 → 切三条类型回边（InstState/LogEntry/ModelError 上移）+ 分层门禁 `scripts/check_layers.py` + plugin→history / api→lua_core 收口 | **已实施**（2026-11-01） |
| [2026-11-01-plugin-audit-fix-plan.md](designs/2026-11-01-plugin-audit-fix-plan.md) | 内置插件审查修复跟踪（10 插件 28 项真 BUG：四协议 I/O 超时/RTU 串扰/eip 位寻址+panic/luaTransform 饿死/luaNorth 等待环/静默写 0 拒写/sidecar 日志与注册竞态等）+ 定性不修清单 | **已实施**（A~H 八批 + 门禁全绿） |
| [2026-11-01-protocol-semantics-audit.md](designs/2026-11-01-protocol-semantics-audit.md) | 协议插件协议语义审查台账（Modbus/OPC UA/S7/EtherNet/IP 73 条）：逐条定级 + 框架/插件/上游库归责 + 库源码与探针证据；含 HMI 写通道失效、S7 重连风暴与写应答丢弃、EIP 批量失败不重连、OPC UA 60s 阻塞等 | **已实施**（a27328b/a4b6c1b；未闭环项见 §9.2） |
| [2026-11-04-plugin-kernel-evolution-design.md](designs/2026-11-04-plugin-kernel-evolution-design.md) | 插件内核演进（轻量·优雅·可演进）：统一 descriptor/runtime/catalog；schema 配置、sidecar 2.1、托管进程、SDK/conformance、能力与健康面 | **讨论中**（draft；P0-P2 未实施。2026-11-25 部分裁决：WASM、runtime trait、能力闭集、SDK 不做，其余目标待裁决） |

## ops/ — 部署与运维（怎么跑起来）

| 文档 | 说明 | 状态 |
|---|---|---|
| [history-100k-tuning.md](ops/history-100k-tuning.md) | 10 万点位历史链调优：参数配方、门禁判据、优雅关停与积压治理 | 活文档 |
| [deploy-gateway-and-mcp.md](ops/deploy-gateway-and-mcp.md) | AI 集成三件套（gateway/mcp/reasonix）现场部署 | 活文档（gateway 单 crate；分层设计未实施） |
| [reasonix-version-compat.md](ops/reasonix-version-compat.md) | reasonix 版本兼容性权威源：v1.25.3（field-kit 基线）/ v2.35.0（端到端实测）/ v2.58.1（代码核对）——兼容面/断裂面/各版本部署差异 | 活文档 |
| [field-e2e-windows.md](ops/field-e2e-windows.md) | Windows 真实工厂级测试场完整手册（配套 `field-kit/`） | 活文档 |
| [lux-upstream-sync-strategy.md](ops/lux-upstream-sync-strategy.md) | lux 上游同步与本地补丁维护策略、风险与应急恢复 | 活文档 |
| [iot-demo-limit-alarms.md](ops/iot-demo-limit-alarms.md) | iot-demo 限值报警（limit_alarm transform）运维说明：数据流、阈值表、启动对账、实例复建与历史读阻塞处置 | 活文档 |
| [releases/2026-08-20-lua-stage-a.md](ops/releases/2026-08-20-lua-stage-a.md) | 发布说明：Lua 阶段 A（机制地基）——升级必读/新能力/已知限制 | 已发布 |
| [releases/2026-08-20-lua-stage-b.md](ops/releases/2026-08-20-lua-stage-b.md) | 发布说明：Lua 阶段 B（北向突破）——luaNorth/secret/cloud 配方/透传 | 已发布（已合并 master，2026-10-27 核实） |
| [releases/2026-08-21-lua-stage-c.md](ops/releases/2026-08-21-lua-stage-c.md) | 发布说明：Lua 阶段 C（数据加工）——luaTransform/EventBus 带值/自环防护/rhai 冻结 | 已发布（已合并 master，2026-10-27 核实） |
| [releases/2026-08-21-lua-stage-d.md](ops/releases/2026-08-21-lua-stage-d.md) | 发布说明：Lua 阶段 D（安全与告警）——M3 回显遮蔽/TLS 选项/告警最小闭环/串口 open 行为变化 | 已发布（已合并 master，2026-10-27 核实） |
| [releases/2026-08-21-self-monitor.md](ops/releases/2026-08-21-self-monitor.md) | 发布说明：rs-iot 自监控设备——rsiot 设备 20 只读+3 可写标签（2026-08-30 起；原 17 只读）/sim1 demo 移除（升级注意）/hwid 覆盖键 | 已发布（已合并 master，2026-10-27 核实） |
| [releases/2026-08-22-device-ownership.md](ops/releases/2026-08-22-device-ownership.md) | 发布说明：设备归属模型——dev2inst 软关联/级联闭环/非 loopback 强制认证（升级必读）/secrets 加密/devices 死表提示 | 已发布（已合并 master，2026-10-27 核实） |

## docs 根级

- [architecture.md](architecture.md)：架构详解（数据流/模块职责/lux 布局/环境依赖/2026-10-27 审查结论）——AGENTS.md 项目地图的展开版
- [conventions/documentation.md](conventions/documentation.md)：文档宪法（分域/命名/状态机/索引/校验）

## docs 之外的文档

- 根 [README.md](../README.md)：项目门面（特性/构建/里程碑）；[AGENTS.md](../AGENTS.md)：AI 协作指令 + 项目地图（引用本索引大部分文档）
- 代码就近 README：[examples/lua-scripts/](../examples/lua-scripts/README.md)、[examples/ext-plugin-python/](../examples/ext-plugin-python/README.md)、[examples/ext-plugin-opcda-go/](../examples/ext-plugin-opcda-go/README.md)、[field-kit/](../field-kit/README.md)
- 待补：`crates/rsiot-mcp`、`crates/rsiot-gateway`、`frontend/` 各自的 README
