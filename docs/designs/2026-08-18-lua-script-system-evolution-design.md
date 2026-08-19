# rs-iot Lua 脚本系统演进设计（多角色脚本平台）

> 日期：2026-08-18
> 版本：v0.1（设计草案，已含三轮调研依据，待逐章深化为需求规格书）
> 状态：讨论中 —— 后续按「版本演进记录」迭代，Open Questions 为下一轮议程
> 流程：调研（mlua 生态 / FreeIOE / IIoT 趋势）→ 本 spec → 分章深化 → writing-plans → 分阶段实施
> 前置：`2026-08-06-lua-script-collector-design.md`（luaClient 采集系统，已落地，本文是其演进）

## 版本演进记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v0.1 | 2026-08-18 | 初版：定位与原则、词汇表、脚本/stdlib 管理、能力分级与扩展规程、四角色模型（含 MQTT 平台适配）、未来方向映射、路线图、ADR D1-D6 |

## 待深入问题清单（Open Questions，v0.2 议程）

| # | 问题 | 所属章节 | 初步倾向 |
|---|---|---|---|
| Q1 | manifest 格式定稿：`manifest.lua` vs main.lua 头部注释块；`@param` schema 表达力（枚举/默认值/分组） | §2.1 | 头部注释块起步（零加载成本），表单复杂后再升级 |
| Q2 | C1-C7 能力卡的代码载体：Rust 结构体常量表 vs 声明宏；如何成为文档单一事实源 | §4.2 | 结构体 + 编译期注册（与 inventory 插件注册同构） |
| Q3 | 频控（token bucket）与单回调时长预算的具体参数与超限行为（跳过本轮 vs 降频 vs 实例 Failed） | §4.5 | 超限跳过本轮 + stat 计数，连续超限走 fail_threshold 既有机制 |
| Q4 | secret 引用制的完整链路：lux `secret:` 前缀隔离、REST/MCP 管理端点、审计脱敏规则 | §5.3 | 独立小 spec（涉及 API 面与前端） |
| Q5 | mqtt-north 回调契约定稿：batch 粒度、up 返回值错误语义、QoS/重试归属、LWT 由谁声明 | §5.3 | 宿主管 QoS1 重试；LWT payload 由 `will()` 回调提供 |
| Q6 | transform 角色的触发语义：全量订阅 vs 按 devid/tag 过滤订阅；去抖/合并窗口 | §5.1 | 实例配置声明订阅过滤（`subscribe: ["dev1.*"]` 风格） |
| Q7 | rhai transform 的最终处置：接线保留（表达式级轻转换）vs 移除 | ADR D4 | 待 B/C 阶段验证 Lua transform 覆盖度后决策 |
| Q8 | cloud/ 配方库首发平台选择与验收标准（真实平台联调哪家） | §3.2 | OneNET（流程简）或自建 EMQX（可控），阿里随后 |
| Q9 | Sparkplug B 的 C7 能力卡范围：仅编解码 vs 含 state-aware 会话（Birth/Death/Store-Forward） | §6 | 先编解码 + 宿主驱动 Birth/Death，Store-Forward 远期 |
| Q10 | io_engine MQTT 透传 backend 的 request 语义：publish+临时订阅等回复 vs 一问一答映射 | §5.4 | reply topic 模板约定 + 超时复用现有 request 语义 |
| Q11 | stdlib 三层 require 与热加载的交互：改 stdlib（随宿主版本）是否触发 reload 提示 | §3.1 | stdlib 只随宿主升级；版本差异在 dry-run 期校验 |
| Q12 | 告警角色（alarm）数据模型：EventBus 新变体、lux 告警表、通知通道（SSE/北向）、恢复语义 | §5.1 | 独立小 spec，D 阶段前完成 |

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
| **Stdlib** | 内嵌纯 Lua 基础库（rust-embed），版本随宿主二进制 |
| **能力卡（Capability Card）** | 一个能力单元的元数据登记（§4.2） |

## 2. 脚本系统管理设计

### 2.1 模板与实例分离（补齐 manifest）

现状 `scripts/<name>/main.lua` + 实例配置 `script=<name>` 已是模板/实例分离雏形，缺模板的自我描述。新增 **manifest 约定**（格式见 Q1，先以 main.lua 头部注释块示意）：

```lua
-- @role mqtt-north          -- 角色：宿主据此校验回调完整性、选择能力包
-- @api_ver 2                -- API_VER 协商（§4.4）
-- @stdlib >=1.2             -- 依赖的 stdlib 版本范围
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

### 3.1 三层 require 解析

```
require("json")        → ① 宿主内置模块（Rust 注册，mlua preload）
require("algo.crc16")  → ② 共享 stdlib（rust-embed 内嵌）
require("myhelper")    → ③ 脚本私有 scripts/<name>/lib/（现状，不变）
```

路径白名单/防穿越逻辑（`loader.rs`）天然复用；stdlib 与热加载的交互见 Q11。

### 3.2 目录规范（按用途而非来源）

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

- `STDLIB_VER` 语义化 MAJOR.MINOR，随宿主二进制发布；脚本 manifest 声明兼容范围，dry-run 校验。
- **兼容矩阵**三轴：宿主版本 × API_VER × STDLIB_VER，维护于 `docs/guides/lua-script-guide.md`。
- 弃用规程：API 标记 deprecated 后保留 ≥ 2 个 MINOR 版本，`stdlib/CHANGELOG.md` 记录替代品。
- **每库自检约定**：`require("algo.crc16").selftest()` 返回 bool——宿主启动时或 CI 对全部 stdlib 冒烟（搬入 FreeIOE 代码的质量闸门，其库内有实锤语法错误先例：calc.lua:308、log.lua:119/125/131）。

### 3.4 来源与许可登记

每个搬入库保留原版权声明 + 追加 `-- vendored from freeioe@<commit> <date>`，`stdlib/CHANGELOG.md` 登记来源、许可（FreeIOE 主仓 MIT；hashings/nums 带 John Schember MIT 头）、本地修改。

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

### 4.5 沙箱护栏升级（现有三护栏之外补三个）

现有：库裁剪（禁 io/os/debug/package）、指令 hook（死循环熔断）、内存限额（默认 32MB）。补充：

1. **单回调时长预算**：每次 `collect/up/down` 调用 wall-clock 超时（tokio timeout 包裹，超时记 stat 并跳过本轮）——指令 hook 只防死循环，不防「慢」。
2. **R2 频控**：每实例 token bucket，防脚本打爆出口。
3. **输入验证规范**：json decode 大小/深度上限、字符串长度上限、kv value 上限（`luakv.rs` 16KB 上限模式推广到所有入口）。

## 5. 多角色脚本架构

### 5.1 角色模型：角色 = 回调契约 + 能力包 + 资源画像

| 角色 | 状态 | 回调契约 | 能力包 | 说明 |
|---|---|---|---|---|
| **collector** | 已有 | `on_init/collect/on_data/on_write` | C1-C6（现状不动） | 南向采集，链路必须 |
| **transform** | 新增 | `on_init/on_timer(period)/on_rtdata(dev,tag,value)/collect` | C0/C2/C4/C6（**无 C1/C3/C5**） | 数据加工/清洗/计算 |
| **mqtt-north** | 新增 | `on_init/auth(cfg)/up(batch)/down(topic,payload)/on_connack/on_disconnect` | C0/C2/C4/C5/C6，R3 下行显式授权 | 北向平台适配 |
| **alarm** | 规划 | `on_timer/on_rtdata` + `ctx:fire_event` | C2/C4/C6 + EventBus 新变体 | 告警规则（Q12，独立 spec） |

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

**认证/结构适配必须脚本化的论证**：阿里三元组 HMAC-SHA1、OneNET token 资源串、腾讯 RSA、华为 HMAC-SHA256 时间戳、AWS SigV4/X.509、Azure SAS——每家不同、同家还分一机一密/一型一密、且平台侧持续演进。Rust 硬编码任何一家都是死路；而它们全是「短输入纯计算」（hmac/base64/时间戳），恰是 stdlib 配方 + auth 回调组合的最优场景。

**凭证安全（D6）**：密钥不进脚本源码、不进 manifest——`auth(cfg)` 用 `ctx:secret_get(ref)`（C4 受限前缀 `secret:`，R3 级）从 lux 读取，凭证经 REST/MCP 由运维管理，审计日志永不落明文。

### 5.4 MQTT 透传南向（io_engine 第 4 种 backend）

mqtt 不只是北向。io_engine 增加 **mqtt backend**（与 tcp/serial/udp 并列）：订阅透传 topic → `on_data(frame)`；`ctx:request` → publish + 等 reply topic（语义见 Q10）。**collector 脚本完全不感知链路是 MQTT 还是串口**（`ctx:transport()` 返回 `"mqtt"`），帧解析逻辑全复用。链路形态归宿主，报文语义归脚本——这是对「灵活类型协议」的统一回答。

### 5.5 北向实例化改造（还旧账）

MqttPublisher 从 `lib.rs:231-234` 硬编码单例改造为 mqttClient 插件实例——Sink trait + `start_sink`（已预留无调用者，`instance.rs:121-150`）接线，走 InstanceManager 全套（多 broker 并存、按实例启停、stat 埋点、状态事件）。原单例行为等价收敛为一个默认实例配置。

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
| **A 机制地基** | 词汇统一（C0-C7）、manifest + API_VER + dry-run 校验、三层 require + stdlib 框架 + 种子库（crc16/basexx/json）、能力卡制度、频控/时长预算护栏 | 现有沙箱测试全绿；modbus-like 示例改用 stdlib crc16 行为不变；兼容矩阵成文 |
| **B 北向突破** | MqttPublisher 实例化（还 Sink 旧账）、mqtt-north 角色 + auth/up/down 契约、cloud/ 配方库首发 1-2 家（Q8）、secret 引用制（Q4） | 对接一个真实平台端到端：认证→上报→下行命令；凭证不出 lux |
| **C 数据加工** | transform 角色（订阅型，Evt::Rtdata 扩展）、订阅过滤（Q6）、rhai 待决标记落地（Q7） | 一个清洗/聚合脚本实例上线；pipeline 零改动 |
| **D 标准与高级** | Sparkplug（C7，Q9）、alarm 角色（Q12）、窗口聚合、MQTT 透传 backend（Q10） | Sparkplug 兼容验证；告警经 SSE 推送 |

## 8. 关键设计决策记录（ADR）

| # | 决策 | 理由 |
|---|---|---|
| D1 | transform 走订阅型实例，不进 pipeline Processor | 避开 Sample「不动」硬约束与同步阻塞风险（pipeline.rs:138-143），复用全部现有机制 |
| D2 | MQTT 连接/会话归 Rust 宿主，脚本只做 auth/payload 映射 | 重连专业性 + 沙箱内无 socket + 审计可控 |
| D3 | 能力层统一编号 C0-C7，废弃 ctx 侧 L1-L6 叫法 | 消解与插件形态 L1-L3 的词汇冲突 |
| D4 | Lua 为唯一脚本体系，rhai 标记待决清理 | 两套脚本并存必然分裂管理面与文档（Q7 终审） |
| D5 | json 走 Rust 注册（C0/C7），纯算法走 stdlib | 按「是否需要宿主依赖树」分工 |
| D6 | 凭证引用制（`secret:` 前缀 + R3 授权），密钥永不进脚本源码与日志 | 安全边界与审计要求 |

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
