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
| Lua 脚本系统 | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | [采集系统](designs/2026-08-06-lua-script-collector-design.md)（已实施）→ [平台演进](designs/2026-08-18-lua-script-system-evolution-design.md)（已实施·阶段 A） | `src/plugin/lua_core/` + `src/plugin/builtin/lua_client/` | `tests/lua_*_e2e.rs`（11 个）+ lua_core 内单测 | [lua-script-guide](guides/lua-script-guide.md) |
| 外部插件 bridge | [扩展策略](decisions/2026-08-07-plugin-extension-strategy.md)（已实施） | —（[协议规约](reference/external-plugin-protocol.md)即设计） | `src/plugin/builtin/sidecar_client/` | `tests/sidecar_e2e.rs` | [开发指南](guides/external-plugin-dev-guide.md)、`examples/ext-plugin-*` |
| 可观测性（M4+P1） | — | [packet-trace](designs/2026-08-10-packet-trace-design.md)（部分实施）· [log-ringbuffer](designs/2026-08-10-log-ringbuffer-design.md)（已实施） | `src/io_trace.rs`、`src/log_ringbuffer.rs`、`src/api/routes_{metrics,trace,logs}.rs` | `tests/{trace,log,metrics}_e2e.rs` | [埋点规范](conventions/instrumentation-conventions.md)、[rest-api](reference/rest-api.md) §4.10+ |
| AI 网关集成 | [技术选型](decisions/2026-08-12-gateway-tech-selection.md)（已实施） | [v3 设计](designs/2026-08-11-ai-agent-integration-v3-design.md)（已实施）→ [分层演进](designs/2026-08-18-multi-system-gateway-design.md)（**待实施**） | `crates/rsiot-gateway/`、`crates/rsiot-mcp/` | gateway/mcp crate 内单测 | [部署指南](ops/deploy-gateway-and-mcp.md) |
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

## designs/ — 设计定稿（怎么做，日期命名）

| 文档 | 说明 | 状态 |
|---|---|---|
| [2026-08-06-lua-script-collector-design.md](designs/2026-08-06-lua-script-collector-design.md) | Lua 脚本采集系统完整设计（luaClient，M10） | 已实施 |
| [2026-08-10-packet-trace-design.md](designs/2026-08-10-packet-trace-design.md) | 报文 trace 开关（P1 子系统 2，阶段 1） | **部分实施**（阶段 2 后置） |
| [2026-08-10-log-ringbuffer-design.md](designs/2026-08-10-log-ringbuffer-design.md) | 日志 ring buffer（P1 子系统 3） | 已实施 |
| [2026-08-11-ai-agent-integration-v3-design.md](designs/2026-08-11-ai-agent-integration-v3-design.md) | AI Agent 集成 v3：独立 API Gateway 程序（v1/v2 已归档） | 已实施 |
| [2026-08-17-eip-client-design.md](designs/2026-08-17-eip-client-design.md) | eipClient：罗克韦尔 AB EtherNet/IP 南向插件（rust-ethernet-ip 1.2.0） | 已实施（真机待验） |
| [2026-08-18-multi-system-gateway-design.md](designs/2026-08-18-multi-system-gateway-design.md) | 多系统网关分层：gateway-core 通用引擎 + rsiot-gateway 薄组装 | **待实施** |
| [2026-08-18-lua-script-system-evolution-design.md](designs/2026-08-18-lua-script-system-evolution-design.md) | Lua 脚本系统演进：多角色脚本平台 v0.5（阶段 A 已实施 + 审查修复） | **已实施**（阶段 A） |
| [2026-08-19-lua-script-system-stage-a-plan.md](designs/2026-08-19-lua-script-system-stage-a-plan.md) | 阶段 A 实施计划：13 task TDD（lua_core/护栏/stdlib/manifest/源仓） | **已实施** |
| [2026-08-20-lua-script-system-stage-b-plan.md](designs/2026-08-20-lua-script-system-stage-b-plan.md) | 阶段 B 实施计划：12 task（luaNorth/Sink 接线/mqtt-north 契约/secret/cloud 配方/透传 spike-2） | 已接受（**待实施**） |

## ops/ — 部署与运维（怎么跑起来）

| 文档 | 说明 | 状态 |
|---|---|---|
| [deploy-gateway-and-mcp.md](ops/deploy-gateway-and-mcp.md) | AI 集成三件套（gateway/mcp/reasonix）现场部署 | implemented（gateway-core 拆分后待核对） |
| [field-e2e-windows.md](ops/field-e2e-windows.md) | Windows 真实工厂级测试场完整手册（配套 `field-kit/`） | 活文档 |
| [lux-upstream-sync-strategy.md](ops/lux-upstream-sync-strategy.md) | lux 上游同步与本地补丁维护策略、风险与应急恢复 | 活文档 |
| [releases/2026-08-20-lua-stage-a.md](ops/releases/2026-08-20-lua-stage-a.md) | 发布说明：Lua 阶段 A（机制地基）——升级必读/新能力/已知限制 | 已发布 |

## docs 之外的文档

- 根 [README.md](../README.md)：项目门面（特性/构建/里程碑）；[CLAUDE.md](../CLAUDE.md)：AI 协作指令 + 项目地图（引用本索引大部分文档）
- 代码就近 README：[examples/lua-scripts/](../examples/lua-scripts/README.md)、[examples/ext-plugin-python/](../examples/ext-plugin-python/README.md)、[examples/ext-plugin-opcda-go/](../examples/ext-plugin-opcda-go/README.md)、[field-kit/](../field-kit/README.md)
- 待补：`crates/rsiot-mcp`、`crates/rsiot-gateway`、`frontend/` 各自的 README
