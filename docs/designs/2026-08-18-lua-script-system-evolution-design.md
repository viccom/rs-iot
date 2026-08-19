# rs-iot Lua 脚本系统演进设计（多角色脚本平台）

> 日期：2026-08-18（v0.5 更新于 2026-08-19：全部 Open Questions 定案 + 冲突/风险分析 + 附录 B）
> 版本：v0.5（设计草案；Q1-Q19 全部决议，含 §9 架构冲突与实施风险分析）
> **状态**：draft（设计完备，可进入 writing-plans 拆分）
> **实施**：未开始（前置 [2026-08-06-lua-script-collector-design.md](2026-08-06-lua-script-collector-design.md) 已实施）
> **遗留**：全部（Open Questions 为下一轮议程）
> 流程：调研（mlua 生态 / FreeIOE / IIoT 趋势）→ 本 spec → 分章深化 → writing-plans → 分阶段实施
> 前置：`2026-08-06-lua-script-collector-design.md`（luaClient 采集系统，已落地，本文是其演进）

## 版本演进记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v0.1 | 2026-08-18 | 初版：定位与原则、词汇表、脚本/stdlib 管理、能力分级与扩展规程、四角色模型（含 MQTT 平台适配）、未来方向映射、路线图、ADR D1-D6 |
| v0.2 | 2026-08-18 | §3 深化：require 改显式命名空间四层（`local.` 前缀防遮蔽/防投毒）、registry.lua 单一事实源清单、分发模型 D1-D4（搭宿主便车 + 站点共享库 + 脚本包导出导入）、「明确不做」清单；ADR +D7/D8；OQ +Q13-Q15 |
| v0.3 | 2026-08-18 | §3 重构：官方分发体系——rsiot-lualib 官方源（git 仓+CI+静态托管+ed25519 签名 index，非服务端软件）、rs-iot 内置同步客户端（验签/原子替换/pin）、只读镜像（换 upstream URL）、标志信任链（官方/已修改/本地/导入四态）、embed 降级为种子兜底；ADR D7 修订 +D9/D10；OQ +Q16-Q18 |
| v0.4 | 2026-08-19 | 三决策定案：①官方源静态托管（GitHub Pages/对象存储，Q16✅）；②embed 种子保留，规模预算几十库/≤500KB（重型件只走在线）；③require 命名规范定稿——官方保留段封闭集（algo/codec/proto/cloud/util/oo）+ 第三方强制两段式 `作者.库名[.子模块]`，`local.` 前缀取消（D8 修订，+D11）；官方脚本模板装 `scripts/<name>/` + manifest `@source` 安装器写入（Q18✅） |
| v0.5 | 2026-08-19 | 全部 OQ 定案（用户「全按推荐」）：Q1-Q17/Q19 决议登记见上表；§5.3 mqtt-north 契约定稿、§5.1 on_timer 澄清 + 反馈环路防护、§5.0 插件形态（lua_core+三薄插件）、§4.5 护栏参数定稿；新增 §9 架构冲突与实施风险分析、附录 B 性能/死锁评估摘要 |

## 设计问题决议登记（Q1-Q19，v0.5 全部定案）

| # | 问题 | 定案 | 落点 |
|---|---|---|---|
| Q1 | manifest 格式 | **main.lua 头部注释块**；`@param` 语法约定：类型 + 必填/可选 + 默认值 + `枚举:a\|b\|c` / `范围:1-3600`；表达力不够时再升级 manifest.lua（迁移=格式转换脚本） | §2.1 |
| Q2 | 能力卡载体 | **结构体常量表 + 一致性测试锁**（mock VM 遍历断言每张卡的函数真注册在 ctx 表）；文档/前端能力表从卡数组序列化生成。不上声明宏（mlua 闭包注册形态不合适） | §4.2 |
| Q3 | 频控/时长预算 | `callback_timeout_ms` 可配（默认 10s）；超限**跳过本轮 + stat 计数 + 连续超限走 fail_threshold → degraded**；R2 频控超限**向脚本返回错误**（可感知退避）；同族护栏全收：`ctx:request`/http 超时宿主 clamp ≤30s、hub `tx.send` 包 2s 超时丢样计数（防控制面反压） | §4.5 |
| Q4 | secret 链路 | **lux 全局 `secrets` Hash**（凭证天然跨实例共享）+ 实例配置 `secrets: []` 显式授权清单；`ctx:secret_get(ref)` 函数恒注入、越权引用报错；REST 写入管理员/读取只返掩码；审计 ref 级不落值 | §5.3（独立 spec） |
| Q5 | mqtt-north 契约 | **混合批量**（满 N 条或 T 秒先到先触发，`batch_max_count`/`batch_max_secs`）；up 返回数组**宿主逐条投递**、失败记 stat 不中断；**重连必重调 auth()**（token 型认证刷新）；可选 `will()` 回调出 LWT payload；宿主另注入全局 `cfg` 表 | §5.3 |
| Q6 | transform 触发 | `subscribe: ["dev1", "dev2.temp*"]` **宿主过滤**（可选，空 = 纯定时计算）；**EventBus Rtdata 载荷扩展带 values**（SSE 前端实时值同步受益；lagged 丢帧=尽量处理语义）；周期回调统一 **`on_timer`**；**反馈环路防护**：宿主保证实例上报的样本不再触发自身订阅（自环），跨实例环靠配置责任 + 异常频率告警 | §5.1 |
| Q7 | rhai 处置 | **冻结**：标 deprecated、不接线不投入；Lua transform 实战半年后复审（移除或接线） | §5.1 |
| Q8 | 首发平台 | **EMQX（契约验证，不依赖外部账号）→ OneNET（平台适配实战）双首发**；aliyun 列第二批 | §3.2 |
| Q9 | Sparkplug 范围 | **编解码 + 有限 state-aware**：宿主管 NBIRTH/NDEATH 证书与 keepalive，`birth_payload()` 脚本回调出点表模板；Store-Forward 远期。protobuf 依赖走 feature 门控 | §6 |
| Q10 | MQTT 透传 request | **reply topic 模板**：app_config 配 `req_topic`/`resp_topic`，request = publish + 订阅 resp 等首帧、超时弃订；实现前先 spike（§9 风险表 R2） | §5.4 |
| Q11 | sync 热加载 | **sync 完成 → 全部 lua 实例 reload**（VM 重建、require 重执行天然生效；失败回退旧 VM；dry-run 版本校验兜底） | §3.6 D1 |
| Q12 | 告警模型 | **最小闭环**：EventBus 新变体 `Alarm{level, source, key, message, ts}` + lux `alarms` Hash 活跃清单 + SSE 推送 + REST 查询；脚本侧 `ctx:fire_event(key, level, msg)` / `ctx:clear_event(key)`；判定逻辑脚本化、存储与多播归宿主。独立 spec 于 D 阶段前 | §5.1 |
| Q13 | `_lib/` 管理 | **复用 scripts REST 模式**：现有端点加路径类型参数（script\|lib），版本快照同规则落 `_lib/<org>/versions/`；前端 Lua.vue 加「库」tab 复用编辑器组件；权限不变（JWT + 审计） | §3.6 D3 |
| Q14 | 脚本包格式 | **zip**（manifest + main.lua + lib/ + `PACKAGE.json` 元数据：导出机器/时间/stdlib 版本声明/清单）；同名**覆盖**（自动版本快照兜底可回滚）；实例配置不进包；MCP `rsiot_export_script`/`rsiot_import_script` | §3.6 D4 |
| Q15 | 共享库热加载 | `_lib/` 变更 → **全实例 reload**（watcher 改 Recursive 监听目录树）；精准依赖追踪列为 v2 优化 | §3.1 |
| Q16 | 官方源托管 | ✅ v0.4 定案：GitHub Pages / 对象存储静态托管 | §3.5 |
| Q17 | 密钥治理 | **单密钥 + key-id 预留**：初始一把 ed25519（CI encrypted secret 保管制，组织流程双审批）；轮换 = 新钥签 index、rs-iot 双公钥过渡验签一个版本后移除旧钥；CRL 不做（规模未到，密钥丢失靠全量重发恢复） | §3.7 |
| Q18 | 官方脚本安装 | ✅ v0.4 定案：`scripts/<name>/` 原名 + manifest `@source` 徽标 | §3.7 |
| Q19 | 插件形态（v0.5 新增） | **lua_core 公共层 + 三薄插件**：`luaClient`（collector + MQTT 透传，Source）/ `luaTransform`（transform + alarm，Source）/ `luaNorth`（mqtt-north，**Sink**——Sink trait 接线还旧账）；沙箱/引擎/loader/stdlib/能力包全在 lua_core，插件只做角色契约与事件循环组装 | §5.0 |

## 0. 设计总纲

**一个定位**：脚本系统是网关的**可编程数据平面**（programmable data plane）——覆盖「协议适配、报文解析、数据编排、平台接入」这层需要频繁变化、无法随宿主二进制迭代的逻辑；性能、可靠性、安全性永远留在 Rust 宿主。

**三个原则**：

1. **宿主管资源，脚本管逻辑**：连接、重连、TLS、持久化、权限在 Rust；认证算法、payload 映射、解析规则在 Lua。脚本永远拿不到原始 socket / 文件 / 进程。
2. **能力即接口（Capability as API）**：宿主给脚本的每个能力都显式分层、显式门控、显式版本化——能力面就是产品面，加能力要走规程。
3. **一套沙箱，多种角色**：所有脚本角色共用同一个 mlua 沙箱 VM 模型、同一个管理面（REST / Vue / MCP / 热加载 / 版本）、同一套安全护栏。不为一类脚本另起炉灶。

## 1. 词汇表统一（设计的前置债务）

现状存在两套 L 编号冲突：ctx API 的 L1-L6（能力层，`src/plugin/builtin/lua_client/api/mod.rs`）与 README/transform 的 L1-L3（插件形态层）。多角色设计无法在歧义词汇上展开，统一如下：

| 术语 | 定义 |
|---|---|
| **C0-C7（Capability 层）** | 取代 ctx API 现行 L1-L6 叫法：C0 纯函数、C1 链路、C2 数据、C3 控制、C4 持久、C5 网络、C6 可观测、C7 高性能算法（Rust 加速）。仅指「宿主赋予脚本的能力」 |
| **L1-L3（插件形态层）** | 保留 README 原语义（核心协议插件 / 数据转换 / WASM），仅指插件形态 |
| **Script / Instance** | 脚本模板（`scripts/<name>/`）与运行实例（lux `instances`，现有概念） |
| **Role（角色）** | 脚本的运行契约类型：collector / transform / mqtt-north / alarm（§5） |
| **Stdlib** | 官方基础库（源仓 rsiot-lualib 治理，在线同步到本地 `_stdlib/`，embed 种子兜底，§3.5-3.6） |
| **能力卡（Capability Card）** | 一个能力单元的元数据登记（§4.2） |

## 2. 脚本系统管理设计

### 2.1 模板与实例分离（补齐 manifest）

现状 `scripts/<name>/main.lua` + 实例配置 `script=<name>` 已是模板/实例分离雏形，缺模板的自我描述。新增 **manifest 约定**（格式见 Q1，先以 main.lua 头部注释块示意）：

```lua
-- @role mqtt-north          -- 角色：宿主据此校验回调完整性、选择能力包
-- @api_ver 2                -- API_VER 协商（§4.4）
-- @stdlib >=1.2             -- 依赖的 stdlib 版本范围
-- @source official@1.2.0    -- 来源徽标（安装器写入：official/imported/local；作者不写，手工新建缺省 local）
-- @param host    string  必填  平台接入域名
-- @param product string  必填  ProductKey
-- @param secret_ref string 必填 凭证引用（lux key，非明文）
-- @param period   number  可选 上报周期秒，默认 5
```

`@param` 声明即**前端表单生成协议**（参考 FreeIOE `conf.json` 的表单式声明）：Vue 面板按 role + manifest 自动渲染配置表单，同模板多实例配不同参数（InstanceManager 多实例机制现成）。

### 2.2 生命周期

```
draft（编辑/保存即版本快照 scripts/<name>/versions/，已有）
  → test（dry-run：按 role 校验必需回调 + manifest 合法性 + API_VER/STDLIB_VER 兼容 + 沙箱空跑 on_init）
  → publish（绑定实例配置，REST/前端已有）
  → running（notify 热加载，reload_vm 失败回退旧 VM，已有）
  → rollback（已有）
```

新增项仅三个：dry-run 按 role 校验必需回调（collector 必须有 `collect`，mqtt-north 必须有 `auth`+`up`）；manifest 解析；脚本导入时的 sha256 校验和登记（为未来签名留位，不过度设计）。

### 2.3 管理面

REST 六端点（`src/api/routes_lua.rs`）、Vue 三级树编辑器（`frontend/src/views/Lua.vue`）、MCP 三工具（`crates/rsiot-mcp`）、notify 热加载——全部按 role 维度自然扩展（脚本列表加 role 徽标），管理面几乎零新建。这是现有架构最大红利。

## 3. 纯 Lua 基础库（Stdlib）管理设计

### 3.1 require 命名空间（v0.4 定稿：官方保留段 + 第三方两段式强制）

**路由规则**（按首段判别，取代 v0.2 的 `local.` 前缀方案——后者取消，尚未实现无兼容负担）：

```
require("json")           → builtin    Rust 注册（首段 ∈ 内置名单，优先级最高）
require("algo.crc16")     → stdlib     首段 ∈ 官方保留段 → _stdlib/algo/crc16.lua（验签只读；空则种子兜底）
require("viccom.calcex")  → 站点共享   首段 = 作者/组织名 → _lib/viccom/calcex.lua（REST 管理，§3.6 D3）
require("myutil")         → 脚本私有   单段 → scripts/<name>/lib/myutil.lua（现状不变）
```

**命名规范（ADR D11）**：

- **官方库**：首段 ∈ **官方保留段封闭集** `{algo, codec, proto, cloud, util, oo}` + builtin 名单（`json` 等）。官方身份由命名空间本身表达；保留段引用只查 `_stdlib/`，miss 即错——**永不回落到本地层**（防投毒核心规则）。
- **第三方库**：**强制 ≥2 段** `作者或组织.库名`（如 `viccom.calcex`），第三段起支持任意深度（库内子模块，段直接映射路径：`viccom.calcex.poller` → `_lib/viccom/calcex/poller.lua`）。单段名进不了共享层。
- **加载器强制项**（非约定）：拒绝单段引用共享/stdlib 层；拒绝第三方引用保留段开头（组织撞保留段名需换名，同 npm scope/PyPI 保留名惯例）。
- 防穿越规则（`loader.rs:25-51` 模式）各层照旧；stdlib 与热加载交互见 Q11；共享库热加载影响面见 Q15。

### 3.2 目录规范（按用途而非来源；即官方源仓 `stdlib/` 的内部结构，同步后镜像为本地 `_stdlib/`）

```
stdlib/
├── util/    retry, tablex(equals/copy), sstack, ssrc
├── codec/   basexx(base64/hex/url)
├── algo/    crc16(modbus), hashings(md5/sha*/crc32/fcs16/sum...), hmac
├── proto/   stream_buffer(分帧), frame(定长/长度字段分帧——待补)
├── cloud/   alink, onenet, huawei, tencent...（平台「配方」）
└── oo/      middleclass（可选）
```

`cloud/` 是 stdlib 中战略价值最高的一层：**把「各家平台接入」从整脚本降维成配方库**——脚本是胶水（选配方、填参数、特例处理），配方是可复用可测试的纯函数（如阿里云三元组签名 = hmac + base64 组合，全在 stdlib 内闭环）。

### 3.3 版本与兼容

- `STDLIB_VER` 语义化 MAJOR.MINOR；**版本轴 = 本地安装版本**（本地 index 锁定），非宿主版本属性（v0.2 语义，已随官方分发体系修订）。脚本 manifest 声明兼容范围，dry-run 与本地 index 比对，不兼容拒载并给可行动错误（「本机 stdlib 1.4 满足要求」/「脚本要求 2.0，请同步新版官方库」）。
- 同步策略：默认 `pin <ver>` 锁定；`auto`（跟随 latest）由管理员显式开启。升级 = sync 新版 + 引用实例自动 reload。
- **兼容矩阵**三轴：宿主版本 × API_VER × STDLIB_VER——stdlib 可能依赖 ctx API，故源端 index 每版声明 `min_api_ver`/`min_host_ver`，sync 时预检，宿主过旧拒装该版（保持旧版继续运行）。矩阵维护于 `docs/guides/lua-script-guide.md`。
- 弃用规程：API 标记 deprecated 后保留 ≥ 2 个 MINOR 版本，官方仓 CHANGELOG 记录替代品。
- **每库自检约定**：`require("algo.crc16").selftest()` 返回 bool——官方源 CI 对全部 stdlib 冒烟 + sync 后本地抽样（搬入 FreeIOE 代码的质量闸门，其库内有实锤语法错误先例：calc.lua:308、log.lua:119/125/131）。

### 3.4 来源与许可登记

每个搬入库保留原版权声明 + 追加 `-- vendored from freeioe@<commit> <date>`，`stdlib/CHANGELOG.md` 登记来源、许可（FreeIOE 主仓 MIT；hashings/nums 带 John Schember MIT 头）、本地修改。

### 3.5 官方源（rsiot-lualib）与 index 清单

**形态判断（ADR D9）**：官方源不是服务端软件，而是 **git 仓 + CI 发布 + 静态 HTTP 托管 + ed25519 签名 index**——唯一可信源头、在线同步、只读镜像、官方标志四性质全部达成，服务端零开发零运维。**托管形态已定案（Q16✅）：GitHub Pages / 对象存储静态托管**。

```
rsiot-lualib/                 ← GitHub 仓（开发态；FreeIOE 搬运库的治理地）
├── stdlib/algo/crc16.lua     ← 基础库源码
├── stdlib/cloud/onenet.lua
├── scripts/onenet-north/     ← 官方脚本模板（main.lua + manifest）
└── dist/                     ← CI 生成（静态托管 Pages/对象存储）
    ├── index.json            ← 全量清单：路径/sha256/版本/min_api_ver/min_host_ver/license/source
    └── index.json.sig        ← ed25519 签名（私钥在 CI secret，公钥编译进 rs-iot）
```

发布流程：PR 合并 → CI 收集文件 → 生成 index → 签名 → 发布静态目录。版本管理 = git tag + index 版本字段。

**本地 index**（`_stdlib/.index.json`，sync 落盘）：本地安装的模块/版本/来源/hash——dry-run 版本校验、来源徽标判定（§3.8）、脚本侧 `stdlib.list()/info(mod)` 只读 API 的数据源。

### 3.6 分发模型（v0.3 定稿：D1-D5）

| 层 | 场景 | 机制 |
|---|---|---|
| **D1 官方源在线同步**（主通道） | 官方基础库 + 官方脚本模板分发 | rs-iot 内置同步客户端：拉 upstream index → **验签 → 逐文件 sha256 校验 → 原子替换** `_stdlib/`（库）/ `scripts/<name>/`（官方模板，hash 记入本地 index 供徽标判定）；REST `GET /lua/stdlib`、`POST /lua/stdlib/sync`、`POST /lua/stdlib/pin`；同步后引用实例自动 reload |
| **D2 只读镜像**（内网） | 内网/隔离网络 | 镜像 = 官方静态目录完整拷贝（index+签名随内容走，镜像无法篡改注入）；内网管理员周期同步官方源（wget --mirror/rclone/极简脚本）；rs-iot 仅改 `stdlib_upstream` 配置指向镜像，「换新镜像升级」= 换 URL + sync |
| **D3 站点共享库** | 现场私有跨脚本库 | `scripts/_lib/<作者>/<库名>.lua`（两段式命名，§3.1），复用 REST save/versions/rollback 管理模式（Q13）；企业多机可用自有手段同步该目录，不进官方通道 |
| **D4 脚本包导出/导入** | 用户自扩展库/脚本的多机分发 | zip（manifest + main.lua + 私有 lib + stdlib 版本声明，**不含 stdlib 本体**）；导入端 dry-run 校验兼容；导入内容徽标「导入」（Q14） |
| **D5 embed 种子**（bootstrap 兜底，已定案保留） | 内网冷启动（无网络无镜像） | 宿主二进制内嵌常用核心种子，**规模预算：几十个库 / ≤500KB 纯文本**（algo/codec/util 精选；hashings 重型件如 whirlpool 单文件 900 行只走在线同步）；`_stdlib/` 为空时解包兜底；种子与宿主同仓同测（API 天然兼容）；本地 index 存在时以 index 为准消歧 |

官方通道单向（官方→网关）；用户内容永不进官方通道，亦无签名 → 进不了 `_stdlib/` 命名空间（前缀隔离 + 验签双保险，ADR D8/D10）。

### 3.7 标志与信任链（四态徽标）

徽标判定 = **manifest `@source` 来源声明（安装器写入，作者不可伪造）+ 文件 hash 对本地 index** 双信号：

| 徽标 | 判定 | 能力 |
|---|---|---|
| **官方** | `@source official@<ver>` 且 sha256 与本地 index 一致 | 只读（REST 不可编辑），随 sync 更新/回滚 |
| **已修改**（官方 fork） | 官方安装后被 REST 编辑，hash 与 index 不符 | 可编辑；可「恢复官方版」（重新 sync） |
| **本地/自定义** | 手工/REST 新建，`@source local`（缺省） | 全权编辑 |
| **导入** | D4 脚本包导入，`@source imported@<来源机器>` | 同本地，记录来源与时间 |

`@source` 由安装渠道追加写入 manifest（sync / import / REST 新建各写各的），脚本作者无权声明官方身份；命名空间（§3.1 保留段 vs `作者.库名`）是第三重信号——库看首段、脚本模板看 `@source`+hash。

签名链：ed25519 公钥内置 rs-iot，index 携 key-id 支持多公钥与轮换（轮换流程见 Q17）。前端脚本树/库面板按徽标分区展示（官方区只读+同步按钮；本地区全编辑能力）。官方脚本模板安装位置已定案（Q18✅）：装 `scripts/<name>/` 原名（开箱即用，实例配置零特殊化），官方身份靠徽标。

**供应链安全对策**（在线拉代码是新增攻击面）：HTTPS + index 验签（镜像被劫持无法注入）+ 默认 pin + sync 需管理员权限（REST 鉴权 / MCP CriticalWrite 档）+ 同步审计日志。

### 3.8 明确不做（防过度设计）

- **服务端「lualib 管理软件」**（带数据库/Web 管理界面的源服务）：静态签名目录达成全部性质（ADR D9）。
- **luarocks 集成 / 多版本共存 / lockfile**：单机单版本轴（本地 index 锁定）下无意义；依赖校验 = 范围声明一次比较。
- **stdlib 按实例启停/授权**：纯函数库无风险面（C0/R0）。
- **用户内容上行通道**（提交到官方源）：官方源只有 PR 一种入口（治理流程），不做运行时上行。
- **库独立热更**：库变更经显式 sync 动作 + reload，不随脚本保存即热更；开发期用 `_lib/`（notify 热加载已有）过渡，正式化走官方仓 PR。

## 4. 宿主能力扩展机制与规程（本设计核心）

### 4.1 能力分级 C0-C7

现有 17 个 ctx API 重新归位，未来沿此坐标系扩展：

| 层 | 语义 | 现有 | 规划 |
|---|---|---|---|
| C0 | 纯函数（无副作用） | `now/time` | `json`、`hex/base64`、时间格式化、`uuid` |
| C1 | 链路通讯 | `request/transport` | MQTT 透传 backend（§5.4）、远期 server 侧 |
| C2 | 数据读写 | `set_value(s)/get_value/get_device/list_devices` | 订阅变更通知 |
| C3 | 控制联动 | `send_command` | — |
| C4 | 持久状态 | `kv_set/kv_get` | `secret_get`（受限前缀，R3） |
| C5 | 网络出站 | `http_get/post` | mqtt publish（北向）、订阅 |
| C6 | 可观测 | `log_*` | `fire_event`（告警事件） |
| C7 | 高性能算法（Rust 加速） | — | `sparkplug` 编解码、FFT/滤波、摘要/加密、推理 |

**stdlib vs C7 判据**：帧级短数据、纯 Lua 可达性能 → stdlib（crc/md5/分帧）；需要 Rust 依赖树（serde_json/protobuf）、大流量、平台级语义（sparkplug）→ C7 宿主注册。「底层/上层/高性能接口如何扩展」的答案不是按层次扩展，是按「风险级 × 是否需要宿主资源」两级筛。

### 4.2 风险分级 R0-R3 与能力卡

每个能力单元登记一张能力卡（代码载体见 Q2，同时是文档的单一事实源）：

```
name:      "ctx:http_get"
layer:     C5     risk: R2（网络出站）
feature:   lua-http   default: on（collector）/ off（transform）
limits:    body ≤1MB, timeout 5s, 域名白名单（新增）
ratelimit: N 次/秒/实例（新增 token bucket）
audit:     全调用审计日志（已有）
api_ver:   1
```

- **R0 纯函数**：无护栏要求，仅输入大小上限。
- **R1 受控副作用**（log/kv/set_value）：现有护栏不变。
- **R2 网络**（http/mqtt pub）：白名单 + 大小限额 + 频控。频控是现有安全模型唯一的结构洞——现护栏管「单次执行」不管「调用频率」。
- **R3 权限敏感**（send_command、secret 读取、`#` 通配订阅）：**实例配置显式授权**才注入该函数。角色能力包默认不含 R3。

「脚本安全性 vs 宿主安全性」的回答：**能力不是全局给的，是按实例的能力包注入的；R3 永远显式授权 + 审计**。

### 4.3 新能力接入规程（RFC-lite 六步）

1. **提案**：场景、为什么现有能力组合不够、替代方案。
2. **安全分析**：定风险级、枚举滥用面（脚本失控最坏情况）、定限额与频控。
3. **规格**：API 签名、错误码约定（沿用 `nil, err`）、能力卡定稿。
4. **实现**：stdlib 或宿主注册，按 §4.1 判据；**禁止**暴露 fd/socket/路径/环境变量。
5. **测试**：正常路径 + 滥用用例（超大输入、高频调用、嵌套炸弹 json、并发 kv 膨胀）。
6. **同步**：`lua-script-guide.md`、Vue 面板、MCP 工具描述、兼容矩阵，同 PR 更新。

### 4.4 API_VER 协商

脚本 manifest 声明 `@api_ver N`，宿主持有 `[MIN_VER, CUR_VER]`：`N > CUR` 拒载（提示升级宿主），`N < MIN` 拒载（提示升级脚本）。弃用走两版窗口（§3.3）。这是「逐步扩展」能长期优雅的唯一保证。

### 4.5 沙箱护栏升级（现有三护栏之外补五个，参数已定稿）

现有：库裁剪（禁 io/os/debug/package）、指令 hook（死循环熔断）、内存限额（默认 32MB）。补充（Q3 定案，含附录 B 性能评估的修复项）：

1. **单回调时长预算**：每次回调调用包 `tokio::timeout`，`callback_timeout_ms` 可配（默认 10s）；超限**跳过本轮 + stat 计数**，连续超限走既有 `fail_threshold` → degraded。超时后该次 call_async future 被 drop、协程停在 await 点——VM 保留但本次半途而废，依赖脚本幂等；连续超限触发 degraded 后由 supervisor 决定是否重建 VM（VM 状态残留语义见 §9 风险 R1）。spike-1 实测（2026-08-19）：协程 await 中被 drop 后 VM 仍可执行新代码、全局半途态确定可见（t.a=1 已写、t.b 未写）、新回调可正常再调，VM 可复用且状态可预期，取方案 a（接受半途态 + 要求回调幂等）。
2. **R2 频控**：每实例 token bucket（默认 20 次/s，能力卡声明），超限**向脚本返回错误** `nil, "rate limited"`（可感知可退避）。
3. **超时宿主 clamp**：`ctx:request(frame, timeout_ms)` 与 `ctx:http_*(timeout)` 的脚本侧超时一律 `min(脚本值, 30s)`——脚本写错一个超大超时不再冻结实例事件循环（附录 B 冻结面②）。
4. **hub send 防反压**：collect/set_value 的 `tx.send` 包 2s 超时，超限丢样 + 计数——控制面（stop/reload/cmd）永不被数据面反压冻结（附录 B 冻结面①）。
5. **输入验证规范**：json decode 大小/深度上限、字符串长度上限、kv value 上限（`luakv.rs` 16KB 上限模式推广到所有入口）。

## 5. 多角色脚本架构

### 5.0 插件形态（Q19 定案：lua_core + 三薄插件）

```
src/plugin/lua_core/            ← 公共层（从 lua_client/ 演化重构）
├── engine.rs                   ← 沙箱 VM（三护栏 + Q3 新护栏）
├── loader.rs                   ← 命名空间路由 require（§3.1）
├── api/                        ← ctx 能力注册（能力包按角色裁剪）
├── manifest.rs                 ← 头部注释块解析 + dry-run 校验
└── stdlib/                     ← embed 种子 + 本地 index + sync 客户端
src/plugin/builtin/lua_client/      ← 薄壳：collector + MQTT 透传（Source）
src/plugin/builtin/lua_transform/   ← 薄壳：transform + alarm（Source，订阅型）
src/plugin/builtin/lua_north/       ← 薄壳：mqtt-north（Sink——Sink trait 首个接线者）
```

角色契约与事件循环组装在各薄壳，沙箱/引擎/loader/stdlib/能力包全在 lua_core——「一套沙箱，多种角色」原则的代码形态。luaNorth 走 Sink 同时完成 §5.5 的 MqttPublisher 实例化旧账。

### 5.1 角色模型：角色 = 回调契约 + 能力包 + 资源画像

| 角色 | 状态 | 回调契约 | 能力包 | 说明 |
|---|---|---|---|---|
| **collector** | 已有 | `on_init/collect/on_data/on_write` | C1-C6（现状不动） | 南向采集，链路必须 |
| **transform** | 新增 | `on_init/on_timer(period)/on_rtdata(devid,tag,value)` | C0/C2/C4/C6（**无 C1/C3/C5**） | 数据加工/清洗/计算；`subscribe` 可选（空 = 纯定时） |
| **mqtt-north** | 新增 | `on_init/auth(cfg)/up(batch)/down(topic,payload)/will()/on_connack/on_disconnect` | C0/C2/C4/C5/C6，R3 下行显式授权 | 北向平台适配 |
| **alarm** | 规划 | `on_timer/on_rtdata` + `ctx:fire_event/clear_event` | C2/C4/C6 + EventBus Alarm 变体 | 告警规则（Q12，独立 spec） |

**transform 关键决策（D1）**：不进 pipeline Processor 插槽，做成**订阅型实例**。pipeline 是同步逐点路径（`src/pipeline.rs:138-143` 明确警示慢处理器阻塞 worker），且 `Sample` 无质量戳是「不动」硬约束（`pipeline.rs:5`）；订阅型（luaClient 事件循环扩展 `Evt::Rtdata`，EventBus 的 Rtdata 变体已存在）完全复用现有 select! 循环、热加载、沙箱。Processor 插槽留给未来 Rust 原生处理器。

**rhai 处置（D4）**：rhai transform 已实现未接线（`lib.rs:237-238` 恒 Passthrough）且与 Lua transform 定位重叠——本设计取 Lua 为唯一脚本体系，rhai 标记「待决清理项」，不再投入（Q7）。

### 5.2 角色与能力包矩阵

| 能力 | collector | transform | mqtt-north | alarm |
|---|---|---|---|---|
| C0 纯函数 | ✓ | ✓ | ✓ | ✓ |
| C1 request/transport | ✓ | ✗ | ✗ | ✗ |
| C2 数据读写 | ✓ | ✓ | ✓ | ✓ |
| C3 send_command | ✓ | ✗ | R3 授权 | ✗ |
| C4 kv / secret | ✓ | ✓ | kv ✓ / secret R3 | ✓ |
| C5 http / mqtt pub | ✓（lua-http） | ✗ | ✓ | ✗ |
| C6 log / fire_event | ✓ | ✓ | ✓ | ✓（核心） |

### 5.3 MQTT 北向适配（mqtt-north）——三层分工

```
连接层（Rust，rumqttc）  连接/重连/TLS/keepalive/会话/订阅管理/背压——脚本零感知
会话层（宿主⇄脚本回调）  auth() 算凭证 → 连接；up() 组报文 → 发布；down() 解报文 → 命令
数据层（stdlib cloud/）  各平台配方：签名算法、topic 规则、payload 模板（纯函数，可单测）
```

回调契约草图（设计稿，非实现；定稿见 Q5）：

```lua
function auth(cfg)
    -- 各云平台认证全不同且随时间演进，这是脚本化的杀手级场景：
    -- 阿里: cloud.alink.sign(...) → {client_id, username, password=hmac_sha1(...)}
    -- OneNET: cloud.onenet.token(...)      华为: hmac_sha256(deviceSecret, ts)
    -- AWS IoT: 签名在 Rust TLS/X.509 层完成，auth 返回 nil 即可
    return { client_id=..., username=..., password=..., keepalive=60 }
end

function up(batch)   -- batch: {{devid, values={tag=v}, ts}, ...}
    local out = {}
    for _, s in ipairs(batch) do
        out[#out+1] = { topic = cloud.alink.topic_property_post(cfg.product, cfg.device),
                        payload = cloud.alink.property_post(s.devid, s.values, s.ts),
                        qos = 1 }
    end
    return out       -- 宿主逐条发布，失败重试并记 stat
end

function down(topic, payload)   -- 宿主订阅配置 topic，消息到此
    local cmd = cloud.alink.parse_set(topic, payload)  -- thing.service.property.set
    if cmd then ctx:send_command(cmd.inst, cmd.devid, cmd.tag, cmd.value) end
end
```

**回调契约（Q5 定案）**：混合批量——宿主收拢样本，满 `batch_max_count`（默认 50）或 `batch_max_secs`（默认 = period）先到先触发 `up(batch)`；`up` 返回数组由**宿主逐条投递**（qos 按条声明，失败记 stat 不中断，qos1 重试归 rumqttc）；**每次重连必重调 `auth()`**（token 型认证过期刷新，要求幂等）；可选 `will()` 回调在 connect 前调出 LWT payload；宿主注入全局 `cfg` 表（app_config 解析产物），`auth(cfg)` 参数形态保留。

**认证/结构适配必须脚本化的论证**：阿里三元组 HMAC-SHA1、OneNET token 资源串、腾讯 RSA、华为 HMAC-SHA256 时间戳、AWS SigV4/X.509、Azure SAS——每家不同、同家还分一机一密/一型一密、且平台侧持续演进。Rust 硬编码任何一家都是死路；而它们全是「短输入纯计算」（hmac/base64/时间戳），恰是 stdlib 配方 + auth 回调组合的最优场景。

**凭证安全（D6）**：密钥不进脚本源码、不进 manifest——`auth(cfg)` 用 `ctx:secret_get(ref)`（C4 受限前缀 `secret:`，R3 级）从 lux 读取，凭证经 REST/MCP 由运维管理，审计日志永不落明文。

### 5.4 MQTT 透传南向（io_engine 第 4 种 backend，Q10 定案）

mqtt 不只是北向。io_engine 增加 **mqtt backend**（与 tcp/serial/udp 并列）：常驻订阅透传 topic → `on_data(frame)`；`ctx:request` = publish `req_topic` + 订阅 `resp_topic` 等首帧、超时弃订（**reply topic 模板**，app_config 配置；correlation id 需协议配合不现实故不取）。**collector 脚本完全不感知链路是 MQTT 还是串口**（`ctx:transport()` 返回 `"mqtt"`），帧解析逻辑全复用。链路形态归宿主，报文语义归脚本——这是对「灵活类型协议」的统一回答。实现注意：rumqttc AsyncClient 自带 poll task，与 io_engine 独占 backend 模型有适配张力，**实施前先 spike**（§9 风险 R2）。

### 5.5 北向实例化改造（还旧账）

MqttPublisher 从 `lib.rs:231-234` 硬编码单例收敛为 luaNorth（mqtt-north，Q19）的一个默认实例配置——Sink trait + `start_sink`（已预留无调用者，`instance.rs:121-150`）接线，走 InstanceManager 全套（多 broker 并存、按实例启停、stat 埋点、状态事件）；现有 `AppConfig.mqtt` 配置节保留解析并等价生成默认实例（无损迁移，§9.1）。

## 6. 未来工业物联网方向 → 定位校准

2025-2026 主线趋势与脚本系统的映射：

| 趋势 | 内容 | 对脚本系统的要求 |
|---|---|---|
| **UNS / ISA-95 扁平化** | 中心 MQTT broker + 语义化层级 topic，边缘网关是 OT→UNS 的桥 | tag→UNS topic 语义映射脚本化（mqtt-north 的 topic 规划层）；`local-broker`（rumqttd feature 已预留）让网关成为小 UNS 骨架 |
| **Sparkplug B** | 工业 MQTT 事实标准：topic 规范 + protobuf 载荷 + state-aware（Birth/Death） | 编解码进 **C7 Rust**（protobuf 依赖）；Birth/Death 由宿主连接生命周期驱动；metric 模板/别名映射脚本化（范围见 Q9） |
| **边缘流处理**（eKuiper/ThingsBoard 规则链模式） | SQL/脚本化规则、窗口聚合、边缘清洗 | transform 角色 = 轻量规则引擎的 Lua 形态；窗口调度归宿主（on_timer 覆盖滚动窗），脚本只写聚合函数。不做 eKuiper 全家桶，守住「数据编排」边界 |
| **云边协同/远程运维** | 配置下发、脚本 OTA、远程诊断 | REST+MCP 管理面已是差异化底座；FreeIOE conf_helper「模板+设备清单」模式作为多设备批量配置参考 |
| **边缘 AI/预测性维护** | 振动分析、异常检测下沉边缘 | C7 算法口设计为可增长集合（FFT/滤波/推理），**脚本编排不实现**——性能边界即安全边界 |

**定位收束**：未来 IIoT 网关 = 「MQTT 中心架构的边缘节点 + 可编程适配层」。rs-iot 的 Rust 骨架（hub/pipeline/lux/实例体系）承担前者，Lua 脚本系统承担后者——角色模型 + 能力分级 + stdlib 配方库即这层「可编程适配层」的完整形态。

## 7. 分阶段路线图（里程碑与验证标准，暂不实施）

| 阶段 | 内容 | 验证标准 |
|---|---|---|
| **A 机制地基** | lua_client → lua_core 重构（Q19，纯搬移不改行为）；manifest（含 @source）+ API_VER + dry-run；require 命名空间路由（保留段/两段式强制拒绝）+ 本地 index + embed 种子；官方源仓 rsiot-lualib 建制 + 同步客户端（D1/D2）；`_lib/` 站点共享库（Q13/Q15）；能力卡制度（Q2）；护栏五项（Q3，含 clamp/防反压）；**spike-1：mlua 回调超时 drop 后 VM 状态语义**（§9 R1） | 重构后现有 lua 测试全绿（零行为变化）；modbus-like 示例改用 stdlib crc16 行为不变；命名违规引用被拒；离线冷启动种子兜底；镜像切换验证；spike-1 结论落 §4.5 |
| **B 北向突破** | luaNorth 薄壳 + Sink 接线 + MqttPublisher 实例化迁移（旧账）；mqtt-north 契约（Q5）；secret 链路（Q4）；cloud/ 配方 EMQX→OneNET（Q8）；**spike-2：MQTT 透传 backend 独占模型适配**（§9 R2） | EMQX 环境契约闭环（auth/up/down/LWT/重连）；OneNET 端到端（认证→上报→下行命令）；凭证不出 lux；旧 mqtt 配置无损迁移 |
| **C 数据加工** | luaTransform 薄壳；EventBus Rtdata 载荷带值（SSE 同步改）；subscribe 过滤 + **自环防护**（Q6 硬性验收）；on_timer；rhai 冻结标注（Q7） | 一个清洗/聚合脚本上线；自环测试通过（transform 输出不触发自身）；pipeline 零改动 |
| **D 标准与高级** | Sparkplug（Q9，protobuf feature）；alarm 角色（Q12 独立 spec）；MQTT 透传 backend 正式版（Q10）；脚本包导入导出（Q14） | Sparkplug 兼容验证；告警经 SSE 推送；透传脚本在 MQTT 链路上行为与串口一致 |

## 8. 关键设计决策记录（ADR）

| # | 决策 | 理由 |
|---|---|---|
| D1 | transform 走订阅型实例，不进 pipeline Processor | 避开 Sample「不动」硬约束与同步阻塞风险（pipeline.rs:138-143），复用全部现有机制 |
| D2 | MQTT 连接/会话归 Rust 宿主，脚本只做 auth/payload 映射 | 重连专业性 + 沙箱内无 socket + 审计可控 |
| D3 | 能力层统一编号 C0-C7，废弃 ctx 侧 L1-L6 叫法 | 消解与插件形态 L1-L3 的词汇冲突 |
| D4 | Lua 为唯一脚本体系，rhai 标记待决清理 | 两套脚本并存必然分裂管理面与文档（Q7 终审） |
| D5 | json 走 Rust 注册（C0/C7），纯算法走 stdlib | 按「是否需要宿主依赖树」分工 |
| D6 | 凭证引用制（`secret:` 前缀 + R3 授权），密钥永不进脚本源码与日志 | 安全边界与审计要求 |
| D7 | **（v0.3 修订）** stdlib 主通道 = 官方源在线同步，embed 降级为种子兜底（原 v0.2：全量内嵌随宿主） | 库独立于宿主版本演进；官方统一版本管理与可信源；内网冷启动由种子保底（§3.6 D1/D5） |
| D8 | **（v0.4 修订）** require 按首段路由（builtin → 保留段 stdlib → 第三方两段式共享 → 单段私有），禁止遮蔽与跨层回落（原 v0.2：`local.` 前缀分流，已取消） | 零遮蔽歧义；保留段永不回落本地层是防投毒核心规则（§3.1） |
| D9 | 官方源 = git 仓 + CI + 静态托管 + 签名 index，非服务端软件 | 「唯一可信源/在线同步/只读镜像/官方标志」四性质由静态签名目录全部达成；服务端零开发零运维（§3.5） |
| D10 | 标志信任链四态（官方/已修改/本地/导入），签名内容与本地内容命名空间隔离 | 官方身份可机器判定（`@source` + sha256 vs 本地 index 双信号）；用户内容进不了 `_stdlib/`（§3.7） |
| D11 | 命名规范强制：官方保留段封闭集 `{algo,codec,proto,cloud,util,oo}`；第三方库强制两段式 `作者.库名[.子模块...]` | 官方身份由命名空间直观表达；第三方归属可追溯（作者/组织段）；合规由加载器强制拒绝而非约定（§3.1） |

## 9. 架构冲突与实施风险分析（v0.5）

**总体结论：无架构级冲突**——全部决议均可映射到现有挂载点（Source/Sink trait、InstanceManager、EventBus、REST/Vue/MCP 管理面、lua 沙箱三护栏），不需要推翻任何核心结构；hub/pipeline/lux/Sample 均零改动。风险集中在三个技术不确定点（需 spike）与两处旧账偿还，且已被阶段化路线图隔离。

### 9.1 与现有架构的接触点盘点（逐项）

| 决议 | 接触的现有代码 | 冲突性质 | 处置 |
|---|---|---|---|
| Q1/Q2/Q4/Q11/Q12/Q13/Q14/Q17 | 新增文件/端点/lux 表项/依赖 | **无冲突**（纯新增，复用既有机制） | 直接实施 |
| Q3 护栏 | `mod.rs` select! 分支、`api/mod.rs` push_sample、`http.rs` | 行为变更（超时 clamp/丢样），现有脚本用 200ms 级超时无感知 | 防御性收紧，现有测试扩用例 |
| Q19 lua_core 重构 | `lua_client/` 3300 行目录重组 | **重构风险**（非行为风险），现有 900+ 行测试保护 | A 阶段首项，纯搬移不改行为 |
| Q5/Q19 luaNorth | `lib.rs:231-234` MqttPublisher 单例、Sink 注册表空置、`instance.rs:121-150` start_sink 无调用者 | **旧账偿还**：Sink 侧 API 管理面可能未区分南北向实例启停 | B 阶段核查 REST 实例 CRUD 对 sink 的覆盖，缺则补 |
| Q5 配置迁移 | `AppConfig.mqtt` 节 | 兼容性：现有 mqtt 配置需等价映射为默认实例 | 保留旧节解析 → 自动生成默认实例，无损迁移 |
| Q6 EventBus 带值 | `event_bus.rs` EventPayload::Rtdata、`pipeline.rs:240-253` emit、SSE `routes_events.rs` 消费者 | 载荷结构变更，SSE 前端需同步改；broadcast(256) 容量下大值挤占 | C 阶段一次改齐（载荷/SSE/前端同 PR） |
| Q15 watcher | `hotload.rs` NonRecursive 监听 | `_lib/` 多级目录需 Recursive 模式 | 小改 + 全实例 reload 策略 |
| Q9 protobuf | Cargo 依赖树 | 新依赖（prost），feature 门控 | 按 feature-gating 既有惯例（`docs/decisions/2026-08-07-feature-gating-protocols.md`） |

### 9.2 风险登记表（按等级）

| # | 风险 | 等级 | 说明 | 缓解 |
|---|---|---|---|---|
| **R1** | mlua 回调超时 drop 后 VM 状态残留：call_async future 被 drop 时协程停在 await 点，脚本内存态可能半途而废（写了一半的 table、未配对的资源） | **高（语义不确定）** | Q3 时长预算的核心实现点；VM 不死但状态一致性无保证 | **spike-1（A 阶段）**：实验验证 drop 后 VM 可用性与状态表现；结论二选一——a) 接受半途态 + 文档要求回调幂等；b) 超时即重建 VM（丢内存态，kv 持久层兜底） |
| **R2** | MQTT 透传 backend 与 io_engine 独占模型张力：rumqttc AsyncClient 自带 poll task + 常驻订阅多路复用，与 IoBackend「独占连接、request/recv_idle 二分」模型不适配 | **中高（模型张力）** | Q10；resp topic 等待与 on_data 被动帧来自同一订阅流需路由区分 | **spike-2（B 阶段）**：1-2 天原型验证适配层；不成则降级 Q10 方案 C（只 on_data + publish 单向，脚本自组） |
| **R3** | transform 反馈环路：订阅 rtdata → set_value 写回 → 再触发事件，自环打满回调预算；跨实例环（A↔B）更隐蔽 | **中（设计已防自环）** | Q6 已定自环防护（上报样本不触发自身订阅）；跨实例环无法静态消除 | C 阶段自环测试为硬性验收项；跨实例环加异常频率告警（事件量阈值） |
| **R4** | Sink 侧管理面缺口暴露：start_sink 从未接线，REST 实例 CRUD/状态事件/stat 对 sink 实例的覆盖度未验证 | **中** | Q19 luaNorth 首次吃这条路径，未知坑 | B 阶段首项核查 + 补齐，早于 mqtt-north 主体 |
| **R5** | MqttPublisher 迁移期双通道并存：旧单例未拆干净前与 luaNorth 实例重复发布 | **低** | 迁移是开关切换不是并行运行 | 单测锁「同一配置只起一个发布通道」；迁移后旧代码删除 |
| **R6** | EventBus 载荷放大：Rtdata 带 values 后 broadcast(256) 更易 lagged，SSE 慢消费者丢帧 | **低** | lagged = transform「尽量处理」语义可接受 | 容量配置化；SSE 消费端 lag 计数可观测 |
| **R7** | lua_core 重构回归：3300 行搬移期间测试红 | **低** | 纯搬移 + 现有测试全绿为准入/准出条件 | A 阶段独立 commit，先搬后改 |

### 9.3 风险与阶段的关系

风险随阶段递增（A 低 → B 中 → C/D 中高），且两个最不确定点（R1/R2）均以 spike 前置于所属阶段首项——**先验证语义，再投入实现**；任一 spike 结论为负都有已定义的降级路径（R1→b 重建 VM；R2→单向透传），不阻塞阶段主体。整体判定：**改进风险可控**——无不可逆决策，全部负面场景均有既定回退。

## 附录 A：调研依据（三份摘要）

### A.1 mlua 生态（2026-08 调研）

- mlua 0.12（lua54/vendored/async/send/serde/macros）：无 Lua 侧 json 模块（serde 仅 Rust↔Lua 值桥）；`preload_module` 机制契合三层 require。
- 官方 `mlua-stdlib`（khvzak，0.1.0）方向正但太早期，关注待成熟后替换自研 json/regex。
- 第三方绑定 crate（mlua-json/mlua-crc16/mlua-batteries 等）存在 mlua 版本锁定与架构边界冲突，仅作参考实现；自建薄注册层为主。
- 参考项目：Lune（Serde/DateTime/Regex/Task 标准库分层）、wezterm（宿主自建 API 面）——主流做法即「宿主自建薄层」。

### A.2 FreeIOE 资产（I:\code\freeioe，MIT，skynet 架构）

- **A 类零成本可搬（纯 Lua，仅依赖 string/table/math）约 6,400 行**：`hashings/`（20 文件：md5/sha1/sha256/sha512/sha3/blake2/ripemd160/whirlpool/crc32/adler32/sum/sum16/fcs16/fcs32/hmac/pbkdf2）、`nums/`（uintn/uintb/bn 大数）、`hwtest/crc16.lua`（Modbus CRC16 38 行）、`basexx.lua`（base64/hex/url）、`app/utils/stream_buffer.lua`（分帧）、`cov.lua`（变化上报）、`utils/retry.lua`、`utils/table/equals.lua`、`middleclass.lua`。
- **设计借鉴（C 类，Rust 实现）**：on_run 返回值动态周期 + 指数退避、API_VER 协商、事件限流（threshold buffer）、声明式点表（inputs/outputs/commands + vt 类型转换）、超时哨兵 + 通道 reopen。
- **不搬**：ubox（ubus 专用）、sysinfo/gcom/gpios/leds/services（io.popen/OpenWrt 绑定）、db//http//pkg/（skynet+C 绑定）、log/（有语法坑）。
- **已知缺陷**（搬入须逐文件验）：calc.lua:308 语法错误、log.lua:119/125/131 语法错误、device.lua:271-274 变量错位、sys.lua:154-159 判断反、device.lua:569 share() 键不一致。

### A.3 rs-iot 架构现状（挂载点 vs 缺口，2026-08-18 勘察）

**现成挂载点**：Source/Sink/Processor 三插槽 + inventory 注册 + InstanceManager 多实例（`src/plugin/`）；hub mpsc 单消费者背压链 + CtlRegistry 控制下行（`src/pipeline.rs`/`src/host.rs`）；EventBus broadcast + SSE（`src/event_bus.rs`，Rtdata 变体已存在）；rumqttc 0.25 依赖已备；脚本资产管理闭环（REST 六端点 + Vue 编辑器 + MCP 三工具 + 热加载/回滚）；沙箱三护栏（`lua_client/engine.rs`）；stat 埋点与 supervisor 自愈。

**缺口**：luaClient 强绑定 IO 链路（无链路角色被迫配假 endpoint，`lua_client/mod.rs:110-131`）；回调模型仅采集四回调；无 MQTT ctx API；MqttPublisher 硬编码单例（`lib.rs:231-234`），Sink 注册表空、start_sink 无调用者；无告警系统与历史时序表；RhaiProcessor 未接线（pipeline 恒 Passthrough）；Sample 无质量戳（不动硬约束）；云平台对接零代码；两套 L 编号冲突（本设计 §1 消解）。

## 附录 B：性能 / 死锁 / 资源评估摘要（2026-08-19 勘察，结论已并入 §4.5）

**评估范围**：`lua_client/mod.rs` 主循环、`engine.rs` 护栏、17 个 ctx API（`api/mod.rs`/`http.rs`）、io_engine 四 backend、`hotload.rs`。

**健康面**：全链路 async 桥接（无同步 IO 阻塞 tokio worker）；每实例单 task 独占 VM 无锁竞争（唯一 std Mutex 临界区无 await，不构成死锁条件）；指令 hook 限单次纯计算约 10-30ms；防抖/防峰到位（MissedTickBehavior::Skip、相位散布、recv_idle 50ms sleep 防忙循环、重连被 period 限速）。

**三个冻结面**（单线程事件循环被无界 await 冻结，期间 stop/reload/cmd 全部失效）：

1. hub 背压传导：`tx.send().await`（collect 分支 / push_sample）在 hub(512) 满时挂起——pipeline/lux 严重变慢时全部实例集体冻结。→ §4.5 第 4 项（2s 超时丢样）。
2. 脚本可控合法无限等待：`ctx:request` 超时参数 u64 直用、http 超时只 clamp 下限——脚本写 `3600000` 冻结实例一小时。→ §4.5 第 3 项（clamp ≤30s）。
3. 被动帧反压级联：IO task 在 `on_data_tx.send().await` 无界等待，主循环忙时 data_rx(64) 填满 → IO task 冻结 → request 也进不去。→ 建议改 try_send + 丢弃计数（P3，未入 §4.5，实现时一并处理）。

**性能短板**（按影响排序）：reqwest Client 每请求新建（TLS/连接池不复用，建议进程级 OnceLock，P2）；1MB 上限检查在下载完成后（先全量进内存，建议 chunk 流式截断，P4）；idle 静默成帧固有尾延迟（快 TCP 场景每 request 多付 idle 等待，由 stdlib proto/frame 长度字段成帧解决）；mlua send feature 固定微量税（架构需要，接受）。

**资源面**：每实例 1 VM + 2 task + 1 watcher 线程，100 实例量级 tokio 无压力；内存最坏 32MB×实例（限额是护栏非常态，实际 <1MB）；versions/ 快照无限累积（建议按数量滚动）；watcher 线程泄漏已修复（tokio task + token）。

**修复优先级**：P0 超时 clamp → P1 hub send 超时 → P2 reqwest 复用 → P3 on_data try_send → P4 HTTP 流式截断 →（设计层）成帧策略可配。P0/P1/P3 已并入 §4.5；P2/P4 为实现期优化项。
