# 内置插件审查修复跟踪计划（2026-11-01）

> **状态**：implemented（28 项全部落地：A~H 八批 + 门禁全绿；§3 为定性不修清单）
> **日期**：2026-11-01
> **实施**：2026-11-01（批次 A~H，见 §4 进度）
> **代码**：`src/plugin/builtin/{eip_client,s7_client,modbus_client,opcua_client,lua_transform,lua_north,data_sim,self_monitor,sidecar_client}`、`src/plugin/lua_core/config.rs`、`src/host.rs`、`src/config.rs`
> **测试**：各批内新增单测（TDD）+ `cargo test --features "s7,eip" --lib` + `just check`
> **遗留**：见 §3「定性为不修」清单

来源：2026-11-01 内置插件 10 插件全量审查（5 路并行深查 + 主会话逐条源码实证）。
本文是修复跟踪表：§1 待修项（定性=真 BUG）、§2 验证策略、§3 不修项（定性+理由）、§4 批次进度。

## 1. 修复项（定性均为真 BUG，证据已源码实证）

### 批 A — eip_client（feature s7,eip）

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| A-1 | P1 | 写命令 `plc_type:"string"`+非 Str 值 → `unreachable!` panic，实例 Failed 无自愈 | mod.rs:596-604 | 非 Str 值改 `format!("{:?}")` 兜底（对齐 plc_with_type string 分支） | TDD：纯函数抽取 + 单测 |
| A-2 | P1 | 批量读对位寻址 tag（`MyDint.5`）返回父字整值（库批量路径位在客户端解析，编码不含位） | mod.rs:252,467-480 | 识别 `.N` 位寻址 tag，分流到单点 `read_tag`（有位处理） | TDD：分流分类器单测 |
| A-3 | P1 | `EipClient::new` 内裸 TCP connect，黑洞地址挂 ~2min，token 不可退出 | mod.rs:127-132 | 整体包 `timeout(10s)`，超时 drop future | 编译+clippy（网络行为无法单测） |
| A-4 | P2 | 健康检查 `Ok(false)` 被忽略（Err 分支死代码），会话失效 fail-fast 丢失 | mod.rs:406 | `Ok(false)` 同样视为失效拆连接 | 编译+推理 |
| A-5 | P2 | `by_tag` 单值 HashMap，同 tag 挂两 devid 后者静默无数据 | mod.rs:467-474 | 值改 `Vec<(devid,field)>` 全量回填 | TDD：映射构造单测 |
| A-6 | P2 | 未知 `plc_type` 静默回退默认推断（"dnit" 按 Dint 写） | mod.rs:245 | 未知值 warn + 拒写（对齐 s7 parse_kind 报错口径） | TDD |

### 批 B — s7_client（feature s7,eip）

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| B-1 | P1 | 会话期 read/write 裸 await（库 timeout 只包 connect），半开连接永久挂死 tick 分支 | mod.rs:591,541-543,740-749 | 每次 read/write 包 `timeout(cfg.timeout_ms)` | 编译+clippy |
| B-2 | P1 | `is_connection_error()` 只认四变体而会话期错误实为 IoErr/Error → `conn_lost` 恒 false，重连逻辑死代码 | mod.rs:550,607 | 读/写任何 Err 一律 `conn_lost=true` | 编译+推理（保留注释说明库行为） |
| B-3 | P2 | 停机 `disconnect()` 可无限期挂起（库重连循环中 Disconnect 等待） | mod.rs:647-649 | 包 `timeout(1s)`，超时放弃（drop client） | 编译 |
| B-4 | P2 | `string[65535]` 经库 `max_len+2` u16 回绕 → 写路径 `buf[1]` 越界 panic | mod.rs:252-260 | `parse_kind` 对 N clamp ≤254 | TDD：parse_kind 单测 |

### 批 C — modbus_client

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| C-1 | P1 | 写命令路径（RMW 读+写）裸 await 无超时，slave 无响应 → 实例永久挂死 | mod.rs:557-594 | 每个请求包 `timeout(req_timeout)`（与读路径同款） | 编译 |
| C-2 | P1 | `TcpStream::connect` 裸调用，SYN 黑洞阻塞 ~130s | transport.rs:32-44 | 包 `timeout(req_timeout)` | 编译 |
| C-3 | P1 | RTU 无事务 ID：读超时后迟到响应可串到下个同功能码请求 → 静默错值 | mod.rs:426-435 | per-plan 连续 Timeout 计数 ≥2 → 拆 session（保单 slave 隔离：计数按 plan 归零） | TDD：计数逻辑单测（纯函数抽取） |
| C-4 | P2 | 写值 `Str.parse().unwrap_or(0)` 静默写 0 | codec.rs:150-157 | 解析失败返回 Err 拒写 | TDD：encode_value 单测 |
| C-5 | P2 | `slave_id as u8`（300→44 打到错误从站）/`gap_threshold as u16` 静默截断 | config.rs:163-176 | `u8::try_from`/范围校验，越界 Err | TDD |
| C-6 | P2 | 重连 sleep 不与 token 竞争 + `reconnect_secs` 无上界（可配 86400） | mod.rs:373 | sleep 换 select{token,sleep}；parse 处 clamp [1,3600] | TDD（clamp）+编译 |

### 批 D — opcua_client

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| D-1 | P1 | connect 全流程（含 `wait_for_connection`）无外层超时，分钟级阻塞 | mod.rs:261-266 | connect 序列整体包 `timeout(15s)` | 编译 |
| D-2 | P2 | `cmd_rx.recv()=None` 空分支不退出（与 modbus 不一致，潜伏热循环） | mod.rs:359-367 | None → break | 编译 |
| D-3 | P2 | username/password 半配置静默降级 Anonymous | mod.rs:256-259 | 半配置 warn（保持兼容不拒连） | 编译 |
| D-4 | P2 | 写值 Str.parse 失败静默写 0/默认 Variant | mod.rs:164-171 | 解析失败 Err 拒写 | TDD |

### 批 E — lua_transform

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| E-1 | P1 | `biased` 臂序 event 在 ticker 前：持续 Rtdata 压力下 on_timer/flush 确定性饿死且不可观测 | mod.rs:208-249 | ticker/flush_ticker 臂移到 event 臂之前（token/cmd/reload 保持前位） | 既有测试回归 + 结构推理（select 宏结构无法单测） |

### 批 F — lua_north + lua_core config

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| F-1 | P1 | pump 状态事件阻塞 send ↔ 主循环 flush 阻塞 send ↔ handler publish ↔ flume 构成等待环，断连+evt 满时实例永久冻结 | contract.rs:124,133-137 + mod.rs:222-223,826-829 | Down 事件分流独立通道（try_send+丢弃计数），状态通道只剩 ConnAck/Disconnected（≤1 在飞不再填满）——切断环的关键边 | TDD：config/计数单测 + 既有 80 条 contract e2e 回归 |
| F-2 | P2 | `mem_limit_mb=0` 关限额（mlua 语义）、`hook_instr_limit` u64→u32 截断 0=VM 报废 | lua_north/config.rs:105-112 + lua_core/config.rs:240,244 | 0/越界 → Err（坏声明可见）；同修 transform config.rs:75,83 | TDD：parse 单测 |
| F-3 | P2 | Down evt 满丢弃无计数；满批丢最旧无计数 | contract.rs:128-131 + mod.rs:347-349 | 两处 AtomicU64/local 计数 → stat extra（down_dropped/batch_evicted） | TDD（计数接线） |
| F-4 | P2 | port/username/password/client_id 类型不符静默缺省 | lua_north/config.rs:84,151-162 | 类型不符 → Err（对齐 subscribe/secrets 口径） | TDD |

### 批 G — data_sim / self_monitor

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| G-1 | P2 | hub 关闭只 break 设备 for 循环，task 静默空转（与其他插件不一致） | data_sim/mod.rs:113-116 | break 整个主循环 | 编译 |
| G-2 | P2 | `cmd_rx.recv()=None` 空过（潜伏忙转） | data_sim/mod.rs:85-89 + self_monitor/mod.rs:274-275 | None → break | 编译 |
| G-3 | nit | `instance_stats` 死局部变量 | self_monitor/mod.rs:585 | 删除 | 编译 |

### 批 H — sidecar / host / config

| ID | 严重度 | 问题 | 位置 | 修复 | 验证 |
|---|---|---|---|---|---|
| H-1 | P2 | 未认证阶段把攻击者可控 Hello 字段（≤4MiB）整段写日志 | sidecar/mod.rs:746-766 | inst_id/ver 日志截 64 字符 | TDD：截断 helper 单测 |
| H-2 | P2 | accept 错误无退避，EMFILE 热转+warn 风暴 | sidecar/mod.rs:716 | 错误分支 sleep 100ms | 编译 |
| H-3 | P2 | task 退出无条件 unregister，stop 超时重叠时抹掉后继 task 注册 → 实例永久 NoSlot | sidecar/mod.rs:667 + host.rs:146-148 | `BridgeRegistry` 增按句柄比对删除（task 只删自己的 slot） | TDD：registry 单测 |
| H-4 | P2 | `[bridge]` 零值无钳制：`max_connections=0` 全握手永久等待、`accept_hello_timeout_secs=0` 全拒 | config.rs:186-198 | parse 处 `.max(1)` | TDD |

## 2. 验证策略

- **TDD 项**：先写红测（复现错误行为/锁定新契约）再修，转绿后保留为回归。
- **结构项（超时包裹/臂序/break 语句）**：select!/timeout 为宏结构，无法在不 mock 网络的前提下单测——以编译 + clippy 三档 + 既有 e2e（modbus/eip/s7 feature 档、lua_north 80 条 contract e2e）回归佐证，并在此表如实标注"编译+推理"。
- **收口**：`just check` 全量 + `cargo test --features "s7,eip" --lib`（协议文件测试仅该档编译）。

## 3. 定性为不修（非 BUG / 取舍 / 待实测 / 需独立设计）

| 项 | 定性 | 理由 |
|---|---|---|
| luaNorth pull 无分页/全量 KEYS+HGETALL | 规模优化 | 现网规模（7 devid）无压力；分页+水位是独立设计 |
| QoS 1/2 跨重连无投递保障 | 语义文档化 | fire-and-forget 为注释明示取舍；补文档即可（随下次触碰） |
| 断连窗口入队计 publish_ok | 口径问题 | 与上一条同源 |
| 自环防护固定 10s 窗口 | 待设计 | 代际/序号校验是独立改造 |
| s7 wstring 缺省 254 超 PDU | 待硬件实测 | PDU 上限依 CPU 型号（1200/1500 不同），不可凭源码定论 |
| s7 位写"整字节覆盖"（库注释自述） | 待硬件实测 | 需真机定性；属实再改 RMW |
| `[mqtt].password` 明文落 lux instances | 需迁移设计 | 转 `secret:<ref>` 涉存量数据迁移与 compat 幂等路径，独立任务（已登记） |
| luaClient on_write 不传 devid | 契约变更 | 改签名牵动脚本生态与 manifest 文档，独立决策 |
| luaClient 闸拒 warn 无频控 + devid="" 归口 | P3 顺手项 | 本批不动 lua_client 数据面，避免与 F 批冲突；下次触碰 |
| json.encode 深嵌套递归栈溢出 | 信任域 | 脚本作者可控；decode 侧已有防线，encode 侧补深度上限随 lua_core 下次触碰 |
| secrets 清单 filter_map 静默丢非 str | P3 | 与 F-4 同类但低危，随 lua_core config 下次触碰 |
| 四协议骨架复制 / stat 键名漂移 | 已登记债务 | 等第 6 协议抽 client_driver |
| luaClient 主循环裸 await lux（note_rejected/flush） | P3 | 与 stat flush 语义耦合，flush 已有保脏重试；下次触碰加 timeout |
| eip/s7 重连计数从首连 +1 | 口径 | 监控解读问题，非行为缺陷 |

## 4. 批次进度

| 批 | 范围 | 状态 |
|---|---|---|
| A | eip ×6（unreachable!/位寻址分流/connect 超时/health/by_tag/未知类型） | ✅ 已修（单测 8：string 串化/分流分类器/Result 化） |
| B | s7 ×4（I/O 超时/断线分类+连续失败兜底/disconnect 超时/string clamp） | ✅ 已修（clamp 红测；超时/分类为结构改动，编译+推理） |
| C | modbus ×6（写路径超时/connect 超时/连续 Timeout 拆 session/拒写静默 0/try_from/sleep 竞争+clamp） | ✅ 已修（3 新测试：拒写/越界 Err/reconnect clamp） |
| D | opcua ×4（connect 超时+handle.abort/cmd None/半凭据 warn/拒写静默 0+未知类型） | ✅ 已修（variant_with_type Result 化，断言更新+新断言） |
| E | luaTransform ×1（biased 臂序：ticker 前置） | ✅ 已修（结构改动，12 既有测试回归） |
| F | luaNorth ×4（Down 分流独立通道解等待环/沙箱域校验/两计数落 extra/标量类型严格化）+ lua_core/transform config 同步 | ✅ 已修（2 新测试：类型/沙箱域；232 lua 测试回归） |
| G | dataSim/selfMonitor ×3（break 'run/cmd None/死变量） | ✅ 已修（编译+19/4 既有回归） |
| H | sidecar/host/config ×4（日志截断/accept 退避/unregister_if_same/bridge 钳制） | ✅ 已修（2 新测试：后继注册保留/截断） |
| 门禁 | fmt / clippy 四档（默认·minimal·s7,eip·workspace）/ lib 双档（686+703）/ e2e 全量 / s7,eip e2e / layers / docs | ✅ 全绿 |

## 5. 复核结论（全部 28 项逐一回验）

- **定性复核**：28 项修复项的原始证据（file:line + 代码形态）在审查阶段已由主会话
  逐条直读源码确认（非子代理转述）；三个"待实测"项（s7 wstring PDU / s7 位写
  整字节覆盖 / P1-8 等待环的时序窗口）按 §3 定性处理，未凭推断修改。
- **行为变更点（有意为之，测试同步更新）**：①eip/opcua 未知 `plc_type`/`opc_type`
  从静默回退改为 Err 拒写（防拼写错误写错类型）；②数值→string 写从 Debug 字面
  （`Int(5)`）改为可读串（`5`）；③luaNorth 标量键类型错从静默缺省改为 Err；
  ④mem_limit_mb=0 / hook_instr_limit 越界从静默失效改为 Err。均有对应断言。
- **无法单测项的验证边界（如实声明）**：网络超时包裹（A-3/B-1/B-3/C-1/C-2/D-1）、
  biased 臂序（E-1）、Down 分流（F-1 环切断）、accept 退避（H-2）为 select!/timeout
  宏结构，无 mock 网络层不可单测——以编译 + clippy 四档 + 全量 e2e（含 lua_north
  80 条 contract e2e、modbus 全链 e2e、s7/eip e2e 档）回归佐证，未声称单测覆盖。
- **门禁证据**：`cargo test --lib` 686 passed / 0 failed；`--features s7,eip --lib`
  703 passed / 0 failed；`cargo test --tests --no-default-features --features lua`
  全部二进制 0 failed；clippy 四档 `-D warnings` 0 error；`check_layers.py` OK
  （22 模块 / 62 边，无环无越层）；`check_docs.py` OK。
