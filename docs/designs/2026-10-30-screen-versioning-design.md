# 画面/主题文档版本管理与并发控制设计（routes_meta 演进）

> **状态**：accepted（定稿待实施；第 1 轮 P0 工作流的后端权威源）
> **日期**：2026-10-30
> **实施**：未开始
> **代码**：计划新增 `src/api/routes_meta.rs` 版本层（同文件扩展）；不新增模块
> **测试**：计划新增 `tests/meta_versioning.rs`（§6 十一组）
> **遗留**：全部（§8 全部实施步骤）

> 上游总体设计见 hmi-studio
> `docs/plans/2026-10-30-ai-assistant-and-extension-system-design.md`（P0 工作流）。
> 本文是 rs-iot 侧实现的单一权威源：存储键族、API 契约、捕获策略、指标与测试。
>
> 实证基线（勿凭印象改）：
> - `src/api/routes_meta.rs`（2026-10-30 实读，含 22ee8b0 修复后形态）
> - `lux/src/command.rs` 命令解析器全量枚举（2026-10-30 grep 实证）
> - `lux/src/lib.rs` 事务/embedded 路径取证（2026-10-30 实读 :1246、:2010-2031、:3270-3320）
> - `src/event_bus.rs` 事件枚举与发布 API（:55 / :129 / :195）
> - `src/api/mod.rs:44` `AppState.lux: EmbeddedClient`（无事务语义的依据）
> - `src/api/routes_lua.rs` 版本快照范式（文件系统 copies——screens 不适用，仅语义对标）
> - 实测体积：直连 8790 取 4 个真实画面（§1.3）
>
> **2026-10-30 深度审查修订记录**（改动 + 依据）：
> 1. 版本键族从 `screen:{id}:v*` 改为**独立前缀 `sv:`**——原方案依赖 `list()` 的
>    `KEYS screen:*` + `valid_id` 拒绝冒号这一**隐式**保护才不污染列表（§1.2 取证），
>    脆弱；改前缀从根上隔离，并新增第 10 组回归测试固化
> 2. 显式取证 lux **embedded 路径无事务**（原文档只写"非原子"未给依据），据此新增
>    §4.3 **失败矩阵**（5 种失败位置 × 后果 × 处理）——这是原设计最大的遗漏：多命令
>    写入的失败语义未定义
> 3. 新增 §1.5 事件挂载点精确位置（`event_bus.rs` 三处），原文档只写"扩枚举"
> 4. 新增 `?if_version=0` 迁移期语义（seq 缺失视为 0，使 GET→PUT 回传无需特判）
> 5. §5.3 容量核算改为**实测体积**驱动（原用 128KiB 上限估算，高估 3~8 倍）
> 6. 新增 §3.6 作者提取的具体改法（auth 中间件 Extension 注入，原文档只说"需补提取"）
> 7. 测试从 9 组扩到 11 组（加列表隔离回归、失败矩阵）
> 8. 明确判重对 **e2e 播种幂等**的既有价值并固化为测试（原文档未意识到这一交互）
>
> ---

## 0. 目标与非目标

**做什么**：给 `screen:{id}` / `theme:{id}` 文档存储补齐治理面，使人与 AI 多作者并存可运维：

1. **版本历史**——每次内容变更前自动快照旧内容，可列出、可取回、可回滚
2. **并发控制**——乐观锁（If-Match 语义），防多写者（编辑器 / hmi-mcp AI / 控制台 AI）互相静默覆盖
3. **审计**——每版记录作者（JWT sub；agent 会话天然以专用 sub 呈现）
4. **变更通知**——新增 SSE 事件 kind `screen`，已打开的画面/列表页可感知

**不做**（明确排除）：
- 不做画面 schema 校验（既有分工不变：本服务只做存储，内容由消费方门禁）
- 不做草稿/发布两态（决策台账：第一版 PUT 即生效，审批闸 + 版本回滚兜底）
- 不做主题（themes）的独立 UI——机制随 MetaKind 泛化天然覆盖，消费方按需启用
- 不做版本 diff（列表返回元数据 + 单版取回，diff 由消费端两版对比实现）

## 1. 事实基线

### 1.1 lux 命令面（版本存储设计的硬约束）

`lux/src/command.rs` 解析器全量枚举（2026-10-30 实证），本设计相关：

| 可用 | 说明 |
|---|---|
| `HSET key field value` | **单 field**（`args.len() == 3`，command.rs:705）——多字段需多次调用 |
| `HGET` / `HGETALL` / `HMGET` / `HLEN` / `HEXISTS` / `HINCRBY` | hash 读族齐全 |
| `SET` / `GET` / `SETNX` / `SETEX` / `EXPIRE` / `TTL` / `PERSIST` | string + TTL 齐全 |
| `INCR` / `INCRBY` | 计数器可用（head 版本号递增） |
| `MGET` / `MSET` / `DEL` / `EXISTS` / `KEYS <pattern>` | KEYS 有仓内先例（routes_meta.rs 列表同款）；**MGET 用于版本正文存活批量判定**（§3.3） |
| `LPUSH` / `LRANGE` / `LLEN` / `LINDEX` | list 读族可用 |

**不可用**（解析器无）：`HDEL`、`ZREM`、`LTRIM`、`SCAN`、`ZREVRANGE`。
注意 `lux/src/eviction.rs:281` 的 `is_write_command` 识别 `HDEL`，但那是淘汰策略的
**命令分类表**，`command.rs` 解析器无该分支——分类表不代表已实现。

**事务语义（决定原子性边界，2026-10-30 取证）**：
- `MULTI`/`EXEC`/`WATCH`/`DISCARD` 的真实现存在于 **RESP 服务路径**
  （`lux/src/lib.rs:3270-3320` 的 `handle_command`：`begin_exec_transaction` +
  shard version 校验 + EXECABORT）；
- **embedded 路径无事务**：`execute_command_pipeline_outputs`
  （`lux/src/lib.rs:1246`）只做批量执行（pipeline），不保证隔离；`validate_embedded_command`
  （`lux/src/lib.rs:2010-2031`）对 `CommandKind::Transaction` 仅"放行不报错"，不建事务状态。

推论（两条，都直接影响设计）：
1. **rs-iot 的 `AppState.lux` 是 `EmbeddedClient`**（`src/api/mod.rs:44`），故版本捕获的
   "读旧→存版→写新" 是**多个独立命令、非原子**——失败矩阵必须显式定义（§4.3）
2. 元数据容量收敛不能靠 HDEL 单删，需「DEL 整键 + 重写保留集」的紧缩策略（§4.4）

### 1.2 routes_meta 现状

- 键形态：`screen:{id}` / `theme:{id}` hash，单 field `doc`（完整 JSON 文本原样）
- 约束：id `^[a-z0-9][a-z0-9-]{0,63}$`；单文档 ≤128KiB；JSON 语法校验
- `MetaKind { key_prefix, list_field }` 泛化结构（screens/themes 同构）——版本机制按
  同一泛化实现，两 kind 免费共用
- put/delete 已带错误处理（22ee8b0）；无版本、无并发校验、无作者记录
- auth：`Claims.sub = user_id`（`src/api/auth.rs:19-45`）——**中间件层可取，但本轮四个
  meta handler 都未接收**，需补提取（§3.6）
- **列表实现的关键约束**（`routes_meta.rs:207-235`）：`list()` 用 `KEYS {prefix}:*`
  → `ids_from_keys` 做 `strip_prefix("{prefix}:")` → `valid_id` 过滤。
  即**任何以 `screen:` 开头的键都会进入 id 候选集**。若版本键命名为
  `screen:{id}:vseq`，strip 后得 `{id}:vseq`，含冒号被 `valid_id` 拒绝——列表**恰好**
  不污染，但这是脆弱巧合：依赖"id 里不能有冒号"这一条间接保护，一旦将来放宽 id
  词法即静默泄漏。故本设计**采用独立前缀 `sv:`**（§2.1），从根上不进入 `screen:*` 的
  扫描面。

### 1.3 实测体积基线（2026-10-30 直连 8790 实测）

| 画面 | doc 体积 |
|---|---|
| catalog-demo（图元陈列室，最大） | 40,396 B |
| home-meter-power | 15,661 B |
| water-demo-m3 | 12,829 B |
| bi-station | 11,178 B |

当前共 11 个画面。**最大 40KiB、典型 11~16KiB** —— 版本存储预算按此核算（§5.3）：
极端情形（`cap=50` × 最大画面）单画面 2MB，全库 11 画面最坏 22MB；按典型体积
约 6~8MB。量级可接受，且 TTL 会持续淘汰正文。

### 1.4 lua 脚本版本范式对比（为什么不照抄）

`routes_lua.rs`：覆盖前 copy 旧文件到 `versions/<unix_ts>.lua`，列目录即列表，rollback
即复制回来。**语义可对标，机制不可照搬**——lua 脚本是文件系统资产（目录枚举/复制免费），
画面是 lux hash field（无枚举 field 手段、无单 field 删除）。捕获语义（**记录旧内容**、
rollback 前先快照当前）沿用 lua 先例。

### 1.5 events SSE 现状（screen 事件的挂载点）

- 枚举：`src/event_bus.rs:55 EventPayload`（Log/Packet/State/Rtdata/Alarm 五变体）
- kind 映射：`event_bus.rs:129 fn kind()` —— SSE `event:` 头由它决定
- 发布：`EventBus::publish(payload)`（`event_bus.rs:195`），`AppState.event_bus` 已在
  所有 handler 可达（`src/api/mod.rs:54`）
- 订阅端点：`routes_events.rs`，`parse_types` 纯函数做 kind 过滤，**新增 kind 无需改
  订阅端点**——只需枚举加变体 + kind() 加分支（编译器强制穷尽）+ 发布点。

新增 `Screen` 变体的成本 = 3 处（enum / kind / publish 调用点），payload 形状见 §3.5。

## 2. 存储设计（键族）

### 2.1 命名：独立前缀 `sv:`（不放 `screen:` 命名空间内）

理由见 §1.2：`list()` 的 `KEYS screen:*` + `valid_id` 过滤构成一个**隐式**边界，版本键
不能依赖它。独立前缀同时解决三件事：不污染列表、不放大列表扫描面、删除级联可精确
（`KEYS sv:{id}:*`）。

`{kind}` = `screen` | `theme` 的元素标识（§2.3 统一用 `sv:{kind}:{id}:*` 以支持向后
扩展主题版本）：

| 键 | 类型 | 内容 | 生命周期 |
|---|---|---|---|
| `screen:{id}` | hash | field `doc` = 当前文档（**不变**，消费方零感知） | 随画面 |
| `sv:screen:{id}:seq` | string(INCR) | 单调递增版本号 | 随画面 |
| `sv:screen:{id}:meta` | hash | field = 版本号 → 元数据 JSON | 紧缩收敛（§4.4） |
| `sv:screen:{id}:v:{seq}` | string | 该版本正文（完整 doc JSON） | **SETEX**，TTL 到期自动淘汰 |

### 2.2 版本元数据 JSON（~140B）

```json
{ "seq": 7, "ts": 1790000000, "author": "admin", "size": 43210, "sha": "a1b2c3d4e5f60718" }
```

- `seq`：该内容成为 head 时的版本号（与 vmeta field、正文键后缀一致）
- `author`：JWT sub。hmi-mcp 用专用 JWT（sub=`hmi-mcp`）——agent 写作无需额外标记位，
  作者字段天然可分辨人/AI
- `sha`：内容 sha256 前 16 hex（`sha2` 已在 rs-iot 依赖中，`Cargo.toml:55`，无新依赖），
  用于判重（§4.1）与列表展示
- `size`：正文字节数（列表页无需读正文即知体积）

### 2.3 删除级联

`DELETE /screens/{id}`：
1. `DEL screen:{id}`
2. `DEL sv:screen:{id}:seq`、`DEL sv:screen:{id}:meta`
3. `KEYS sv:screen:{id}:v:*` → 逐个 `DEL`

第 3 步显式删的意义：正文虽带 TTL，但 30 天内同 slug 重建会读到上一世代的孤儿版本
（版本号从 0 重新开始 → seq 撞车 → 用户看到"自己没写过的历史"）。键空间有界
（≤ cap 量级/画面），与既有 `list()` 的 KEYS 同量级，无需 SCAN。

### 2.4 theme 同构

`MetaKind` 泛化已存在，版本机制以 `kind_prefix`（`sv:screen` / `sv:theme`）参数化。
themes 前端当前未用，但机制免费覆盖；**不为 theme 单独设计 UI**（范围排除项）。

## 3. 捕获与写入语义

### 3.1 捕获时机（对标 lua "覆盖前快照旧文件"）

内容变化且校验通过的 PUT：
1. `HGET screen:{id} doc` 取旧正文 + `HGET sv:...:seq` 取当前 seq
2. `sha256(旧)` vs `sha256(新)`：**相同 → 不建版、不 INCR、直接返回当前 version**（判重）
3. 不同（或旧 doc 不存在 = 首次创建）：
   - 旧存在 → `SETEX sv:...:v:{cur_seq}` 存旧正文 + `HSET sv:...:meta {cur_seq}` 元数据
   - `INCR sv:...:seq` → new_seq
   - `HSET screen:{id} doc 新正文`
   - `publish(Screen{action:"put", version:new_seq, actor})`
4. 响应 `{status:"ok", version:new_seq}`

首次创建（旧 doc 不存在）：无旧内容可快照 → 只 `INCR`（0→1）+ 写 head，**不产生版本
条目**。语义：版本 1 是初始内容，无"上一版"。

**遗留画面首次改动**（迁移期实证路径）：本机制上线前已存在的画面（如当前 11 个长期
资产）没有 seq 键 → 视为 0。第一次改动时：old doc 存在但 seq=0 → 捕获旧内容标为
**版本 0**（"机制上线前的原貌"）→ INCR 0→1 → 写 head。于是任何画面在任何时刻都满足
"versions 列表最后一条 = 可回退的最老形态"，无特例分支。

### 3.2 并发控制（if_version 乐观锁）

`PUT /screens/{id}?if_version=N`：

- 缺省 = 无条件写（**完全向后兼容**，现有编辑器/hmi-studio/e2e 播种零改动）
- 提供且 `N != 当前 seq` → `409 {"error":"version conflict","current_version":M}`，
  **不捕获、不写**（失败不留痕）
- 匹配 → 按 §3.1 正常写入

**`version=0` 的语义**（迁移期关键）：seq 键缺失视为 0——即"本机制上线前就存在、此后
从未被改过的画面"。因此 `?if_version=0` 是合法的"我读过它、且它仍是原始版"断言；
客户端从 GET 拿到的 `version` 原样回传即为正解，无需区分 0 与 N。

**为什么放 query 而非 header/body**：PUT body 是裸 doc JSON（既有契约，`validate_put`
直接按字节校验），塞版本号进 body 会破坏"原样透传"语义；`If-Match` header 需要 ETag
语义配套（响应也得发 ETag），引入不必要的缓存层耦合。query 参数最贴近既有形状。

**为什么不是强一致**：见 §4.3（embedded 无事务）——两个并发 PUT 都读到 seq=7 时都能
通过校验。防护的是"读到旧版本后长时间编辑再保存"（人手/agent 的实际模式），不是
毫秒级竞争。毫秒竞争的兜底是 §4.3 的失败矩阵 + 内容判重（两个并发写同内容 → 一版）。

### 3.3 读取契约（向后兼容扩展）

- `GET /screens/{id}` → `{id, doc, version:N}`（新增 `version`，旧消费方忽略）
- `GET /screens/{id}/versions` → `{versions:[{seq,ts,author,size,sha,expired}]}`，
  按 seq 降序，至多 `versions_cap` 条；`expired` 由 `MGET` 正文键批量判定（一次往返）
- `GET /screens/{id}/versions/{seq}` → `{id,seq,ts,author,doc}`；正文缺失 → 410
  `{"error":"version body expired"}`；元数据无该 seq → 404

### 3.4 回滚

`POST /screens/{id}/rollback` body `{"to_version":5}`，可选 `?if_version=N`（同 §3.2）：

1. 读目标版正文（缺失 410 / 无元数据 404）
2. **快照当前 head 为新版本**（§3.1 同款捕获）——回滚本身可再回滚
3. 写 head = 目标正文，`INCR seq` → new_seq
4. `publish(Screen{action:"rollback", version:new_seq, actor})`
5. 响应 `{status:"ok", version:new_seq}`

### 3.5 SSE 变更事件

`event: screen`，data：
```json
{ "id":"bi-station", "kind":"screen", "action":"put|delete|rollback",
  "version":8, "actor":"admin", "ts":"2026-10-30T12:00:00Z" }
```
发布点：put（判重命中时**不发**——内容未变不该惊动运行页）、delete、rollback。
`kind` 字段与 `EventPayload` 其他变体的 serde 形态保持一致（tag 字段名以现有变体为准，
实现时以 `event_bus.rs` 既有 `#[serde(tag="kind")]` 属性为准）。

### 3.6 作者提取（handler 改动）

`Claims.sub` 由 `auth.rs` 中间件解析后未注入请求扩展，四个 meta handler 也都不接收。
最小改法：`auth_middleware` 校验通过后 `req.extensions_mut().insert(AuthUser{sub})`，
meta handler 用 `Extension<Option<AuthUser>>` 取（缺失时 author 记 `"unknown"`——
对齐"不因审计缺失而失败写"的可用性取向）。改动面：`auth.rs` ~5 行 + `routes_meta.rs`
四 handler 签名，regression 由现有 auth 测试覆盖。

## 4. 捕获策略（防版本洪水）

### 4.1 三条收敛线

AI 写作频率（一轮对话改数十次）下的三条收敛线，全部在写路径内完成、无后台任务：

1. **内容判重**：PUT 流程天然先 `HGET` 旧 doc（捕获需要）——sha256(新) == sha256(旧)
   → 不建版、不 INCR、直接返回 ok + 当前 version。改一个像素保存十次只产生一条版本。
   **这条同时覆盖了 e2e 播种场景**：`e2e/fixtures.ts` 的 11 个长期资产画面每次 e2e 运行
   都 PUT 全量 upsert（内容由确定性纯函数生成器产出，逐字节相同）→ 判重命中 → 反复
   播种不产生版本垃圾。这是无意的收益，但必须写进测试（§6 第 10 条）防止后人改动判重
   逻辑时破坏它。
2. **正文 TTL**：`SETEX` 挂 `versions_ttl_secs`（默认 30 天）——旧版本正文自动淘汰，
   无删除代码。元数据小（~140B），多活 30 天无害。
3. **元数据容量上限**：vmeta HLEN 超过 `2 × versions_cap` 时触发紧缩（§4.4）。

不做时间窗合并（讨论阶段提案）：判重 + TTL + 容量三线已把最坏情形封顶（§5.3 按实测
体积核算），再叠加合并窗会增加「哪次改动被合并了」的不可预测性，违背版本语义的
可解释性。

**捕获时机**（§4.2）：内容变化成功的 PUT，把**旧内容**存为版本（lua 先例）——语义是
「撤销我刚才的写入」；head 本身永远是最新态，不占版本位。

### 4.2 捕获时机与写入顺序

见 §3.1 的 4 步序列（判重 → 存旧版 → INCR → 写 head）。此处只强调一个不变量：
**head 的 HSET 是整个序列的最后一步**——它成功即代表"新版生效"，此前的任何失败都应
中止并报错（§4.3）。

### 4.3 失败矩阵（embedded 无事务 → 必须显式定义）

§1.1 已取证：`AppState.lux` 是 `EmbeddedClient`，多命令无原子性。捕获是 4~5 个独立
命令，任何一步失败都要有确定行为。**统一原则：head 写入是唯一不可回退的关键动作，
其余失败只影响历史完整性，不影响数据本体。**

| 失败位置 | 后果 | 处理 |
|---|---|---|
| 捕获阶段（SETEX 正文 / HSET 元数据）失败 | 该版历史缺失，head 未写 | **中止并返回 500**，不写 head——宁可这次保存失败让调用方重试，也不产生"内容变了但没历史"的不可追溯写入 |
| 捕获成功、INCR 失败 | 有孤儿正文/元数据，seq 未进 | 返回 500；孤儿键由 TTL 与下次紧缩清理（不影响正确性：元数据指向的 seq 未被 head 使用） |
| INCR 成功、head HSET 失败 | seq 已消耗但 head 仍是旧内容 | 返回 500（如实报错）；版本号出现空洞（seq=8 无 head 对应），列表按 seq 降序展示不受影响（用户看到"某版无正文"标记 expired） |
| head 成功、publish 失败 | 数据已正确落库，仅无实时通知 | 吞掉（publish 本就无返回错误），记 warn 日志。SSE 是通知非事务参与方 |
| 并发两写（毫秒级竞争） | 两方都通过 if_version 校验，后写覆盖先写的**历史记录**（先后两次捕获都成功，但 seq 只有一个生效） | 概率低、后果 = 一次历史记录串号；**head 数据本身不会损坏**（最后一个 HSET 赢）。不做分布式锁（单机嵌入式，收益不抵复杂度） |

阈值判断：以上全部是"返回 500 + 数据本体无损"，没有静默数据丢失路径——这是本设计
对 rs-iot "不吞错"原则（22ee8b0 修复确立）的延续。

### 4.4 紧缩（compact）

`HGETALL sv:...:meta` → 按 seq 降序保留前 `versions_cap` 条且正文仍在（`MGET` 非 Nil）
的 → `DEL` 整键 → 逐条 `HSET` 重写。非原子（读改写窗口内并发的另一次捕获可能丢元数据，
正文仍在但短期不出现在列表）。触发条件是 HLEN > `2 × versions_cap` 且每次 PUT 至多
紧缩一次，窗口概率极低、后果可自愈（下次紧缩收敛）；如实登记为已知限制（§7）。
**可选简化**：lux 增补 `HDEL`（`command.rs` 解析器无该分支，实现面小）即可消除读改写
窗口——作为独立小改动评估，不与本设计绑定。

## 5. 配置、指标与容量核算

### 5.1 配置

`config.toml` 新段（带校验：正整数、上限钳制——对齐深度审查对配置键的护栏要求）：

```toml
[meta]
versions_ttl_secs = 2592000   # 正文 TTL，默认 30 天
versions_cap      = 50        # 每画面保留版本数上限（钳制 1..=500）
```

### 5.2 指标

指标（命名对齐 `rs_iot_sse_lagged_total` 先例，挂 `ProcessMetrics`）：

| 指标 | 类型 | 含义 |
|---|---|---|
| `rs_iot_meta_versions_created_total{kind}` | counter | 版本快照创建数 |
| `rs_iot_meta_version_conflicts_total{kind}` | counter | if_version 409 次数（写者竞争温度计） |
| `rs_iot_meta_rollbacks_total{kind}` | counter | 回滚次数 |
| `rs_iot_meta_compactions_total{kind}` | counter | 紧缩执行次数（频繁 = cap 配置过小或 TTL 过长） |

审计：版本元数据的 author/ts/size/sha 即审计记录（谁、何时、对什么、旧值指纹），
不另设审计日志。

### 5.3 容量核算（按 §1.3 实测体积）

| 情形 | 单画面 | 全库（11 画面） |
|---|---|---|
| 典型（15KB 画面 × cap 50） | ~750 KB | ~8 MB |
| 最坏（40KB 最大画面 × cap 50） | ~2 MB | ~22 MB |

叠加 TTL 后实际远低于上界（AI 高频改动的画面正文很快被淘汰）。与 tsink 历史库的
GB 级体量相比可忽略；且全部在 lux 内存 + 磁盘双写，随 lux SAVE 持久化。

## 6. 测试计划（TDD）

计划新增 `tests/meta_versioning.rs`（或并入 api 测试模块内；二选一，实施时定）：

1. 判重：同内容二次 PUT → version 不变、无新版本条目
2. 捕获：内容变更 PUT → 旧内容可从 versions/{seq} 取回，author 正确
3. 乐观锁：先 PUT A 再带过期 if_version PUT B → 409 + current_version；
   缺省 if_version → 无条件成功（向后兼容）
4. 回滚：rollback 后 head == 目标版内容，且回滚前的 head 已存为新版本（可再回滚）
5. TTL：正文明设短 TTL 过期后列表标 expired、取回 410
6. 紧缩：灌 2×cap+ 条元数据触发紧缩 → 保留最新 cap 条
7. 删除级联：DELETE 后 seq/meta/正文全清，同 slug 重建读不到孤儿版本
8. SSE：put/delete/rollback 各发一帧 `event: screen`，payload 字段齐；判重命中不发帧
   （对齐 `tests/events_e2e.rs` 既有断言风格；该套件已知并发时序 flaky，本用例独立跑）
9. themes 同构：同一套断言跑 theme 前缀（MetaKind 泛化回归）
10. **列表隔离回归**（§1.2 脆弱点守护）：写入带版本历史的画面后，
    `GET /screens` 的返回**不含**任何 `sv:` 派生项、不含 `{id}:v...` 形态；
    并加一条断言：即使人为往 lux 塞一个 `screen:{id}:vseq` 形态的键，列表也被
    `valid_id` 挡下——把这条隐式保护显式化为测试，防将来放宽 id 词法时静默泄漏
11. 失败矩阵（§4.3）：mock/注入 HSET 失败 → 500 且 head 未变（用 lux 层错误注入或
    单元级构造）；head 写失败 → seq 空洞但列表可读

文档：`docs/reference/rest-api.md` §4.19 增补三端点 + if_version + version 字段 +
§4.10 事件表加 `screen` 一行。

## 7. 风险与已知限制

| 风险 | 评估 | 对策 |
|---|---|---|
| 无条件写仍是 last-write-wins | 现状不变（既有契约），只有接入 if_version 的写者受保护 | 编辑器与 hmi-mcp 接入后覆盖主要写者；409 计数器暴露剩余裸写者 |
| 毫秒级并发竞争的串号 | 低（单人/单 agent 场景罕见）；head 数据本体不损坏 | §4.3 失败矩阵登记；不做分布式锁 |
| 紧缩读改写窗口丢元数据 | 概率极低（触发稀疏）、后果可自愈（正文在、列表短期缺）、不丢数据本体 | 登记；若实际发生走 lux 增补 HDEL |
| 存储增长 | 实测核算见 §5.3（典型 8MB / 最坏 22MB 全库） | 配置可调；created/compactions 指标监控 |
| `KEYS sv:screen:{id}:v:*` 删除 | 键空间有界（单画面 ≤cap 量级） | 无需 SCAN |
| 版本正文含敏感数据 | 画面文档本身即业务数据，与 head 同级 | 无新增暴露面（同一 JWT 边界） |
| author 提取依赖 auth 中间件改动 | 低（5 行 + Extension 注入，现有 auth 测试覆盖） | 缺失时记 `"unknown"`，不阻断写 |

## 8. 实施切分

单阶段可完成（预估 1.5~2 天含测试）：存储层（捕获/紧缩/级联删）→ API 三端点 +
if_version → auth Extension 注入 → SSE 枚举扩展 → 指标 → rest-api.md → 上述 11 组
测试红绿。无 schema 迁移（纯增量键族，旧数据零影响——seq 缺失视为 0，首次内容变更起
建历史）。
