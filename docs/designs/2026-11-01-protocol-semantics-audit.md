# 协议插件协议语义审查问题台账（2026-11-01）

> **状态**：implemented（修复已落地并通过门禁；未闭环项与原因见 §9）
> **日期**：2026-11-01
> **实施**：2026-11-01（`a27328b` 框架+四协议插件、`a4b6c1b` selfMonitor、`832c8e2` tokio-s7 fork 接入）
> **代码**：`src/types.rs`、`src/host.rs`、`src/api/routes_command.rs`、`src/db/rtdata.rs`、`src/history/writer.rs`、`src/model/expand.rs`、`src/lib.rs`、`src/plugin/builtin/{modbus_client,opcua_client,s7_client,eip_client,self_monitor}/`、`Cargo.toml`（`[patch.crates-io]`）
> **测试**：各插件新增单测（modbus 64 / opcua 31 / s7 40 / eip 29 / selfMonitor 21）+ `cargo test --all-features` 全绿（1036 通过）+ fork 仓 11 绿 + git 源联调 s7 单测/e2e 复跑
> **残留**：F-3 前端质量位、OPC-9 Subscription（见 §9.2）
> **来源**：协议插件协议业务语义深度审查（Modbus / OPC UA / S7 / EtherNet/IP）；逐行核对插件代码与上游库源码；S7 项另有进程内 mock PLC 探针实测（`/tmp/s7probe`，仓库零改动，未提交）
> **分类口径**：**框架** = rs-iot 跨插件基础设施 / 共享类型 / 跨仓契约；**插件** = `src/plugin/builtin/<protocol>` 自身实现（含对库缺陷的未规避）；**上游库** = 第三方 crate 行为与协议语义冲突，插件无法经正常 API 获得正确信息（需 fork / 升级 / 配置规避）
> **范围说明**：不复述 2026-11-01 内置插件审查已修的 28 项（见 [2026-11-01-plugin-audit-fix-plan.md](2026-11-01-plugin-audit-fix-plan.md)）；本台账只记**协议语义**问题。`target/release/rs-iot`（11-01 11:32 构建）已编入 s7、未编入 eip（`strings` 计数 eipClient=0），EIP 侧问题尚未在生产路径暴露。

本文全部证据均落在 `插件文件:行号` 或 `上游库文件:行号`。上游库源码根以 **`<REG>`** 简写：
`/root/.cargo/registry/src/rsproxy.cn-e3de039b2554c837/`（tokio-modbus-0.17.0 / async-opcua-client-0.19.0 / async-opcua-types-0.19.0 / async-opcua-crypto-0.19.0 / tokio-s7-0.1.7 / rust-ethernet-ip-1.2.0 / rust-ethernet-ip-protocol-1.2.0 / rust-ethernet-ip-tag-path-1.2.0）。

## 1. 统计

| 主责分类 | 条数 | 其中 P0/P1 |
|---|---|---|
| rs-iot 框架（F-1~F-9） | 9 | 6 |
| 插件（MB / OPC / S / E 中标注） | 43 | 14 |
| 上游库（标注 `<REG>` 证据） | 21 | 8 |
| **合计** | **73** | **28** |

> 计数口径：S-16 与 E-8 分别是 F-8 / F-2 在 S7 / EIP 侧的实例引用，已在协议表中保留行位、不重复计数（表格共 75 行，去重后 73 条）。
>
> **处置列**（MB / S / E 三张协议表）：`插件` = 插件侧修；`P` = 插件侧规避库缺陷；`F` = fork 上游打补丁（tokio-s7）；`U` = 提上游 PR；`P+U` = 先规避、同时推动上游；`N` = 记录不修；`框架` = 框架侧修。裁决依据、fork 工程形态与退出条件见 [上游库缺陷处置策略](../decisions/2026-11-01-upstream-library-fork-policy.md)。

严重度口径：**P0** = 静默错数 / 控制不生效 / 可打爆 PLC 或实例资源；**P1** = 现场高概率停采或数据失真；**P2** = 语义风险 / 诊断缺失；**P3** = 能力缺口 / 低概率。

---

## 2. 框架级（F）——影响多个插件或全部写入口

| ID | 严重度 | 问题 | 证据 |
|---|---|---|---|
| F-1 | P0 | HMI 写命令契约与四个协议插件全部不匹配 | `hmi-studio/src/lib/command.ts:54`（只发 `{tag,value}`）与 `hmi-studio/src/lib/command.test.ts:60-63`（把 modbus 场景固化为 `{tag,value}`）；`src/plugin/builtin/modbus_client/mod.rs:539`、`s7_client/mod.rs:750`（认 `address`）、`opcua_client/mod.rs:559`（认 `node_id`）、`eip_client/mod.rs:680`（认 `tag` 但语义是 CIP 路径）、`self_monitor/mod.rs:404-460`（只认顶层键）；`src/api/routes_command.rs:31-40`（投递即 200） |
| F-2 | P0 | 写命令无 ACK：插件成败不回流，REST 200 与业务成功解耦 | `src/api/routes_command.rs:30-43`；四个插件 `handle_write` 失败仅 `tracing::warn!`（如 `eip_client/mod.rs:716-722`、`s7_client/mod.rs:782-787`、`opcua_client/mod.rs:592-599`、`modbus_client/mod.rs:640-645`） |
| F-3 | P1 | 点级质量缺失：读失败点保留旧值，rtdata 无质量位 | `src/types.rs:114-123`（SampleValue 无 quality 字段）；`src/db/rtdata.rs:64-93`（cell 仅 `[ts,localTime,value]`）；插件侧仅跳过不标记（见 OPC-1 / S-15 / E-1） |
| F-4 | P2 | NaN 只污染实时面：rtdata 变字符串 `"NaN"`；历史面已有防线 | `src/db/rtdata.rs:88`（`SampleValue::Float.to_string()` → `"NaN"`）；`src/history/writer.rs:321-328`（非有限值 `nonfinite_skipped` 拒收，不进 tsink、不进死区比较） |
| F-5 | P1 | `SampleValue::Int(i64)` 无法承载 u64：opcua/s7 读值超 2^63 静默符号翻转 | `src/types.rs:114-123`；`opcua_client/mod.rs:130`、`s7_client/mod.rs:341-345`（`as i64`）；EIP 已用饱和规避（`eip_client/mod.rs:194-195`） |
| F-6 | P1 | 设备 bind 的 `slave_id` 解析失败静默回退实例默认 | `src/model/expand.rs:60`（`s.bind.get("slave_id").and_then(\|s\| s.parse().ok())`）；`{"slave_id":"300"}` → None → `modbus_client/mod.rs:191` 回退 `cfg.slave_id`（默认 1）→ 读写打错从站 |
| F-7 | P2 | 请求超时策略四家不一，且"一轮读超 period"无告警 | modbus 2s 可配（`modbus_client/config.rs:214-218`）/ s7 5s 可配（`s7_client/mod.rs:153-156,374-376`）/ opcua 60s 不可配（见 OPC-10）/ eip 库硬编码 5~10s（`<REG>rust-ethernet-ip-1.2.0/src/client.rs:667,685,3805`）；`MissedTickBehavior::Skip` 静默丢拍（如 `modbus_client/mod.rs:335`） |
| F-8 | P2 | 未装 `log`→`tracing` 桥，全部用 `log` 的上游库日志丢失 | `src/lib.rs:76-130`（`init_tracing` 只装 tracing layer）；全仓无 `LogTracer` / `tracing_log`（rg 核实）；直接受害者：tokio-s7 全篇 `log::info!/warn!`（见 S-16） |
| F-9 | P1 | 空 `values` 的 Sample 会让 rtdata 写入报错，把"读不到"伪装成"存储失败" | `src/db/rtdata.rs:64-66,84-93`（空 pairs 仍 `HSET` 只带 key）→ `lux/src/cmd/hashes.rs:30-35` 报 `ERR wrong number of arguments`；触发源见 OPC-1（全点坏时仍投递空 Sample） |

### F-1 证据补充（最高优先级）

- 前端链路：`bridge.ts:69-100`（`send(devid, tag, value)`）→ `command.ts:54`（`data: JSON.stringify({ tag: o.tag, value: o.value })`）→ `push-button.ts:23`（默认事件模板 `$rsiot.send(devid,tag,value)`，`tag` prop 标题为"目标点位"，默认值 `uptime_s`）。
- 插件链路：modbus `req.get("address")`（modbus_client/mod.rs:539）、s7 `req.get("address")`（s7_client/mod.rs:750）、opcua `req.get("node_id")`（opcua_client/mod.rs:559）、eip `req.get("tag")`（eip_client/mod.rs:680，但此处 `tag` 是 CIP 标签路径，前端传的是点位显示名）。
- 设备 API 拿不到绝对地址：`src/api/routes_device.rs:56-71` 的 `children.value` 是**未解析的参数化地址模板**（如 `holding:{addr}:u16`），绝对地址在实例装配期才解析（`src/model/expand.rs:37-62`）。
- 结论：对 modbus/s7/opcua 是"字段名不匹配 → 直接丢弃"；对 eip 是"字段名匹配但值语义错 → 写错/写不存在的标签"。两种情况 REST 都返回 `{"status":"ok"}`，前端 `CommandTracker` 随即 `confirm()` 显示"命令已下发"。

### F-9 证据补充

`opcua_client/mod.rs:463-487` 对每个 devid 无条件构造 `Sample` 并投递（无 `values.is_empty()` 判断）。全部节点质量坏时 → 空 values → `db/rtdata.rs` 建空 entry → lux `HSET rtdata:<devid>`（0 个 field）→ `ERR wrong number of arguments` → `failed_devids` → pipeline 写失败计数增长。现场表现为"采集正常但存储一直报错"。

---

## 3. Modbus（tokio-modbus 0.17.0）

| ID | 分类 | 严重度 | 问题 | 证据 | 处置 |
|---|---|---|---|---|---|
| MB-1 | 插件 | P1 | 写数值越界 `as` 回绕，不校验范围 | `src/plugin/builtin/modbus_client/codec.rs:190-200`（`as_i()? as u16` / `as i16 as u16` / `as i32 as u32`）；实证：`-1 as u16 = 65535`、`70000 as u16 = 4464`、`40000 as i16 = -25536`、`-5 as u32 = 4294967291`（独立 rustc 程序输出） | 插件 |
| MB-2 | 插件 | P1 | 布尔字符串只认 `"true"`，`"1"/"on"` 静默写 false | `codec.rs:170-175`（`s.eq_ignore_ascii_case("true")`）；实证 `"1" → false`、`"on" → false`；HMI 推按钮 value 为 number 时无碍，MQTT/外部 API 传字符串触发 | 插件 |
| MB-3 | 插件 | P2 | `slave_id` 允许 0（广播）与 248-255（保留区），无校验 | `modbus_client/config.rs:167-171`（只做 `u8::try_from`）；库对 `Slave(0)` 无广播特殊处理（`<REG>tokio-modbus-0.17.0/src/client/mod.rs:118-150` 的 call 无 is_broadcast 分支）→ 读广播必超时、写广播实际已发出但等响应到超时后拆 session | 插件 |
| MB-4 | 插件 | P3 | 地址 65535 + 多寄存器类型静默丢点 | `modbus_client/merge.rs:34`（`saturating_add` 截断）、`codec.rs:74-76`（`off + w > regs.len()` → None）；`holding:65535:i32` 区间长 1，解码永远 None | 插件 |
| MB-5 | 插件 | P2 | 写命令 `data.slave_id` 可任意覆盖，绕过设备归属 | `modbus_client/mod.rs:576-580`（`req.get("slave_id")` 优先于 devid 映射）；可向任意从站写 | 插件 |
| MB-6 | 插件 | P3 | coil/discrete 合并上限被统一 clamp 125（FC01/02 协议上限 2000） | `modbus_client/config.rs:190`（`.clamp(1,125)`）+ `merge.rs:36-37`；大点表多请求，非错误但浪费 | 插件 |
| MB-7 | 插件 | P3 | `IllegalDataAddress` 无自动降级（文档要求人工调小配置） | `modbus_client/mod.rs:436-448`（exception 只计数 + warn） | 插件 |
| MB-8 | 上游库 | P3 | RTU 主站不实现帧间静默（t3.5）/ 从站周转延迟 | `<REG>tokio-modbus-0.17.0/src/codec/rtu.rs`（无请求间隔逻辑）；慢从站偶发丢帧，靠下一轮重试 | P |

**已核实正确**：`perm4/perm8` 四种字序与工业记法一致（含 f64 的 CDAB=GHEFCDAB、BADC=BADCFEHG、DCBA 全反）；寄存器大端（`codec.rs:27-51`）；FC01-04/05/06/0F/16 使用正确；CRC、事务 ID、从站号回显、功能码校验由库完成（`<REG>tokio-modbus-0.17.0/src/service/rtu.rs:56-102`、`service/tcp.rs:92-130`）；C-3 的超时拆 session 已缓解 RTU 迟到响应串扰。

---

## 4. OPC UA（async-opcua 0.19.0）

| ID | 分类 | 严重度 | 问题 | 证据 |
|---|---|---|---|---|
| OPC-1 | 插件 | P1 | 坏质量点静默丢弃且 tick 仍成功；全坏时投递空 Sample | `opcua_client/mod.rs:526-538`（status 非 Good → warn + None）、`471-483`（缺失字段直接不插入）、`501-503`（`tick_ok` 照走）；空 Sample 链条见 F-9 |
| OPC-2 | 插件 | P1 | 单请求塞全部节点、失败即拆会话 | `mod.rs:467-469`（每设备一次 `s.read(&nodes,…)`，不查 `MaxNodesPerRead`）、`488-498`（任何 `Err` → `session=None` + abort）；库侧单请求：`<REG>async-opcua-client-0.19.0/src/session/services/attributes.rs:565-586` |
| OPC-3 | 插件 | P2 | 请求了 `TimestampsToReturn::Both` 却不用 SourceTimestamp | `mod.rs:469`（Both）、`478-483`（`chrono::Local::now()`）；`526-538` 只取 `dv.value` |
| OPC-4 | 插件 | P2 | 非标量 Variant 转 `Debug` 字符串（Empty/Array/ByteString/ExtensionObject/StatusCode） | `mod.rs:120-137`（`_ => format!("{v:?}")`）；库提供 `Display`（`<REG>async-opcua-types-0.19.0/src/variant/mod.rs:466-489`）未用 |
| OPC-5 | 插件 | P2 | `UInt64 as i64` 符号翻转 | `mod.rs:130`；根因是框架 `SampleValue` 无 u64（见 F-5） |
| OPC-6 | 插件 | P1 | 写值类型：Bool 串只认 `"true"`、数值 `as` 截断、写状态只 warn 不回传 | `mod.rs:161-220`（`as` 转换与 `as_b`）、`589-599`（每节点 StatusCode 只 warn） |
| OPC-7 | 插件 | P2 | 点表 `ChildSpec.type` 被忽略；`opc_type` 词表与点表词表无别名（`u32` 被拒写） | `mod.rs:362-392`（只读 value/label）、`580`（只用命令里的 opc_type）；点表词表 `src/model/types.rs:45-52` vs `mod.rs:191-217` |
| OPC-8 | 插件 | P2 | NodeId 语法窄：不支持 `nsu=`、大写/空格敏感；不读 NamespaceArray | `mod.rs:113-117`（`s.parse::<NodeId>()`）+ `<REG>async-opcua-types-0.19.0/src/node_id/mod.rs:195-196`（库正则仅 `ns=<数字>;i\|s\|g\|b=`） |
| OPC-9 | 插件 | P3 | 只有轮询 Read，无 Subscription（能力缺口） | `mod.rs:394-399,463-500`；库订阅能力完整（`<REG>async-opcua-client-0.19.0/src/session/services/subscriptions/`） |
| OPC-10 | 插件 | P1 | 读/写无超时预算，库默认 60s，select 分支被阻塞 | `mod.rs:469`（裸 await）、`590`（写裸 await）；`<REG>async-opcua-client-0.19.0/src/config.rs:559-561`（60s）、`src/session/client.rs:306`（`channel.send(request, timeout)`）；D-1 只包了 connect（`mod.rs:298-327`） |
| OPC-11 | 插件 | P2 | 默认 `security_policy=none` + `trust_server_certs(true)`：None 通道密码明文；跳过证书有效期/主机名 | `mod.rs:261-266`（trust_server_certs(true)）、`102-111`（默认 none）；`<REG>async-opcua-crypto-0.19.0/src/certificate_store.rs:386-397`（`skip_verify_certs` 直接 return Ok） |
| OPC-12 | 插件 | P3 | 策略只开 none/Basic256Sha256 且固定 SignAndEncrypt；无 X509 身份；PKI 目录随 cwd | `mod.rs:102-111,279-296`；库支持 6 种策略（`<REG>async-opcua-crypto-0.19.0/src/security_policy.rs:198-214`） |
| OPC-13 | 插件 | P2 | 断连只 abort 事件循环，不发 CloseSession；重连计数把首连算作重连 | `mod.rs:493-496`（abort）、`437-445`（首连也 `reconnects += 1`）；会话超时 60 000ms（`<REG>async-opcua-client-0.19.0/src/config.rs:579-581`） |
| OPC-14 | 插件 | P3 | endpoint 匹配失败一律 `Cannot find matching endpoint`，不打印服务端端点列表 | `mod.rs:271-278`；库错误映射 `<REG>async-opcua-client-0.19.0/src/session/connection.rs:249-256` |
| OPC-15 | 插件 | P3 | 数组/结构既不能按元素读也不能写（`index_range` 恒空） | `mod.rs:236-243,245-255`（两处 `Default::default()`） |
| OPC-16 | 插件 | P2 | 节点级坏状态每拍每点一条 warn，无限流 | `mod.rs:530` |
| OPC-17 | 插件 | P3 | `null` 写成字符串 `"null"`；>i64 整数精度丢失 | `mod.rs:223-234`（`json_to_sample_value`） |

**已核实正确**：NodeId `i/s/g/b`+`ns` 四形态库均支持；`max_age=0.0` 语义为强制回源（`<REG>async-opcua-client-0.19.0/src/session/services/attributes.rs:537-541`）；`status:None` 按规范视为 Good；坏状态不会被误当值写入（先判 status 再取 value）；写请求不带时间戳（规避部分服务端拒收）；用户名密码按服务端 UserTokenPolicy 加密（仅 None 通道明文）；服务端 Advertise 主机名不可达已自动替换为客户端给的地址。

---

## 5. S7（tokio-s7 0.1.7）

| ID | 分类 | 严重度 | 问题 | 证据 | 处置 |
|---|---|---|---|---|---|
| S-1 | 上游库 | P0 | 写应答的 item 级 `return_code` 被整段丢弃，写失败一律当成功 | `<REG>tokio-s7-0.1.7/src/client.rs:1374-1382`（`check_response` 后直接 `Ok(())`，不查 `datum.return_items`）；`src/datum.rs:79-84` 解析了 ReturnItem 却不使用；探针实测 PLC 回 `0x05 InvalidAddress` → `write_int32` 返回 `Ok(())` | F |
| S-2 | 上游库 | P0 | drop 连接让库 actor 进入无间隔热重连，可打爆 PLC 连接资源 | `<REG>tokio-s7-0.1.7/src/client.rs:1221-1226`（只对 `Ok(0)` break）、`:1301`（命令通道关闭返回 `Ok(1)`，无 sleep、不看 auto_reconnect）；插件触发点 `s7_client/mod.rs:693`（`client = None` 直接 drop）；探针实测 drop 后 0.6s 建连 2044 次（≈3400/s）持续 | 插件 |
| S-3 | 插件 | P0 | 不读协商 PDU，默认 20 项在 240B PDU 上必失败并触发 S-2 风暴 | `s7_client/mod.rs:173-177`（默认 20）、`629-632`（只按项数分片，全文无 `pdu_length` 引用）；库发包前校验 `<REG>tokio-s7-0.1.7/src/client.rs:1322-1324`（`bytes.len()-7 > pdu_limit`）；探针实测协商 240 后 20 项 → `Read request exceeds PDU limit` | 插件 |
| S-4 | 上游库 | P1 | 头级错误解析错位（byte11 当 class、byte12 当 code），真实错码丢失 | `<REG>tokio-s7-0.1.7/src/header.rs:163-171`（同名变量 `ec` 覆盖，`S7ErrorClass::from(buf.get_u8())` 读的是 code 字节）；探针实测 → `Unknown error class: 0x85` | F |
| S-5 | 上游库 | P1 | STRING 按 UTF-8 lossy 解码，0x80-0xFF 全变 U+FFFD | `<REG>tokio-s7-0.1.7/src/client.rs:529-530`（`String::from_utf8_lossy`，注释误称 ISO-8859-1）；探针实测 `"°C"`（0xB0 0x43）→ `"�C"`；写侧 `str_to_latin1`（`:1111-1123`）正确，读写不对称 | F |
| S-6 | 插件 | P0 | 写命令省略 `kind` 时推断 I64/F64，静默越界写 8 字节 | `s7_client/mod.rs:412-419`（`Int → Kind::I64`）、`772`（缺 kind 走 infer）、`826-829`（`write_int64` 写 8 字节）；`{"address":"DB1.DBD0","value":42}` 覆盖 DBB4-7 | 插件 |
| S-7 | 插件 | P1 | STRING 写按请求 `max_len` 全长写，不与点位声明交叉校验 | `mod.rs:775-777`（请求可任意给 max_len）、`830-843`；库 `client.rs:590-599`（写 `max_len+2` 字节）→ `string[10]` 点位传缺省 254 覆盖后 244 字节 | 插件 |
| S-8 | 插件 | P2 | `string` 不写 `[N]` 时默认按 254 全长读，240B PDU/小 DB 必失败 | `mod.rs:568-577`（`read_string(addr, 254)`）；库 `client.rs:518-521`（`total_len = max_len+2`） | 插件 |
| S-9 | 上游库 | P2 | `connection_type`（pg/pc/op）是死配置，库不使用 | `<REG>tokio-s7-0.1.7/src/client.rs:34,54,89-92`（仅定义/默认/withter）；`src/client.rs:1178-1183` 的 TSAP 只由 plc_type+rack/slot 算出 | F |
| S-10 | 插件 | P2 | `kind:"byte"` 实为有符号 i8，S7 BYTE（0-255）无法表达，且无无符号字节 kind | `mod.rs:333`（`i8::from_be_bytes`）、`264-278`（kind 全表无 ubyte）；测试锁定 0xFF→-1（`mod.rs:1061-1066`）；库 `read_byte` 返回 u8（`client.rs:251-257`） | 插件 |
| S-11 | 插件 | P1 | 写值转换一律 `as` 截断/回绕，非数字串写 0 | `mod.rs:800-818`（`s.parse().unwrap_or(0)`、`as` 转换） | 插件 |
| S-12 | 上游库 | P2 | 地址解析"过滤所有非数字"，多处静默取错 | `<REG>tokio-s7-0.1.7/src/item.rs:211-222`（`filter(\|c\| c.is_ascii_digit())`）；`DB1.DBX0.5`+数值 kind → 读 `DB1.DBW0`；`%DB1.DBX0.0`（TIA 导出）报错；T/C 区按 `<<2` 编址且值是 BCD（`item.rs:236-240`） | P |
| S-13 | 上游库 | P2 | 区域缺口：P/PIW/PQW、S7-200 的 AI/AQ/SM 不可达 | `<REG>tokio-s7-0.1.7/src/types.rs:179-189`（`from_first_char` 只认 I/Q/M/D/V/T/C）；库虽定义 `DirectPeripheralAccess=0x80`/`AnalogInputsOf200Family=0x06` 但无解析入口（`types.rs:100-124`） | P+U |
| S-14 | 插件 | P2 | 无 `bytes[N]/raw` kind，DT/DTL/S5TIME/BCD 只能硬读成垃圾值 | `mod.rs:206-232`（Kind 全表）、`331-352`（decode 分支） | 插件 |
| S-15 | 插件 | P2 | 单点永久失败无指标、无限流，日志每拍刷 | `mod.rs:313-321`（decode_item warn）、`639-643`（None 即跳过，不进 tick_fail/stat） | 插件 |
| S-16 | 框架 | P2 | 上游库 `log` 日志全丢（见 F-8），tokio-s7 是主要受害者 | 同 F-8 | 框架 |
| S-17 | 插件 | P2 | 读响应项数与请求不校验，`zip` 静默错位 | `mod.rs:639-643`（`chunk_pts.iter().zip(data_items.iter())`）；库会丢弃非 Data 项（`<REG>tokio-s7-0.1.7/src/client.rs:1337-1349`） | 插件 |
| S-18 | 插件 | P1 | `endpoint` 按 UI 提示填 `ip:102` → 库拼成 `ip:102:102`，必连不上 | `mod.rs:125-134`（endpoint 原样）；库 `client.rs:1137-1152`（`format!("{host}:{port}").parse::<SocketAddr>()`）；前端 schema hint 提示 `host:port` | 插件 |
| S-19 | 插件 | P2 | `U64` 解码后 `as i64`，超 2^63 变负数 | `mod.rs:341-345`；根因见 F-5 | 插件 |
| S-20 | 插件 | P2 | 配置边界：单独给 `rack` 被静默忽略；`port/rack/slot` 用 `as` 截断；PDU 不可配 | `mod.rs:140-147`（必须 rack+slot 同时给）、`142-143`（`as u8`）、`133`（`as u16`）、`379-395`（无 max_pdu_size 入口） | 插件 |
| S-21 | 上游库 | P3 | `read_bytes` 家族不检查 item `return_code`，失败时 data 为空（typed 读会 panic） | `<REG>tokio-s7-0.1.7/src/client.rs:270-278`、`:318-321`；插件当前只用 `read_string/read_wstring`（有长度检查，不 panic），但错误信息误导 | P |

**已核实正确**：REAL/LREAL 大端 IEEE754（`client.rs:466-502`）；WORD/DWORD/INT/DINT 大端（`client.rs:334-345`）；WSTRING 读写（UTF-16BE、4B 头、钳位，`client.rs:546-565,612-633`）；STRING 读偏移与按 `bytes[1]` 截断正确（`client.rs:521-529`，错的是编码见 S-5）；位读 PLC 回 1 字节、`d[0]!=0` 判定成立；DB 号 >65535 报错（`item.rs:219-221`）；单连接 actor 一问一答不串包（`client.rs:1246-1300`）；`item_count` u8 上限与 clamp 1-20 无溢出；string 超长报错不截断（`client.rs:583-588`）。

**结论修正（替代上一轮"待硬件实测"登记）**：`write_bool` 的"整字节覆盖"注释与实现不符——探针抓包为 `… 00 03 00 01 01`（传输尺寸 0x03 BIT、长度 1 bit、地址 = byte×8+bit），是标准位写请求；snap7 维护者亦确认 CP 只改目标位。插件无需 RMW，仍建议真机做一次"置 0xFF → 写 bit0 → 读回 0x7F"回归覆盖固件差异。

---

## 6. EtherNet/IP（rust-ethernet-ip 1.2.0）

| ID | 分类 | 严重度 | 问题 | 证据 | 处置 |
|---|---|---|---|---|---|
| E-1 | 插件 | P0 | 批量组级失败被库降级成点级 `NetworkError`，插件只 warn 不拆连接 → 断链后永久停采 | `eip_client/mod.rs:552-604`（尤其 `567-571` 点级 Err 只 warn）；`<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:106-125`（continue_on_error=true 时逐 op 转 `BatchError::NetworkError`，`execute_batch` 返回 Ok）、`src/batch.rs:118-134`（默认 true）；`src/client.rs:3798`（发送前即置 `stream_poisoned=true`，只有成功响应才清）→ 毒化后所有请求在 `ensure_stream_usable()` 直接失败 | 插件 |
| E-2 | 上游库 | P0 | `0xD3` 同时表示 `BOOL_ARRAY_DWORD` 与 DWORD 类型码，DWORD 数组元素被当 BOOL 数组取位 | `<REG>rust-ethernet-ip-protocol-1.2.0/src/values.rs:19`（`BOOL_ARRAY_DWORD=0x00D3`）、`:156-169`（0xD3 解 Udint）；`src/client.rs:1472-1480`（数组元素探测到 0xD3 → 取位）；`src/client/batch_exec.rs:562-566,825-853`（批量同款）；AB 类型表 0xD3=DWORD | P+U |
| E-3 | 上游库 | P1 | WORD/LWORD/BYTE/STRING2 等类型码无解码分支，该类点位永久无值 | `<REG>rust-ethernet-ip-protocol-1.2.0/src/values.rs:74-174`（match 仅 C1-CB、CE、DA、A0/2A0、D3）；0xD2=WORD、0xD4=LWORD、0xD1=BYTE、0xD5=STRING2 全落 `Unsupported data type` | P+U |
| E-4 | 上游库 | P1 | 位读只接受 Bool/Dint；`bit≥32` 被 TagPath 编成成员名 | `<REG>rust-ethernet-ip-1.2.0/src/client.rs:1252-1266`（其余 `DataTypeMismatch`）、`:1295-1322`（写同样限制）；`<REG>rust-ethernet-ip-tag-path-1.2.0/src/lib.rs:496-514`（<32 位/≥32 成员）→ `AlarmWord.3`（INT）永久失败、`MyLint.40` 得 `0x04 Path segment error` | P |
| E-5 | 插件 | P2 | 分流口径用"字符串含 `.`"，`Program:MainProgram.Tag` 与 UDT 成员被踢出批量 | `eip_client/mod.rs:324-326`（`c.value.contains('.')`）；库批量对成员/程序路径合法（`<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:556-569`、`src/client.rs:4041-4081`），真正必须单点的只有 `TagPath::Bit` | 插件 |
| E-6 | 上游库 | P1 | STRING 数组元素写走库的数组 workaround，缺结构 handle，写失败或写坏 | `<REG>rust-ethernet-ip-1.2.0/src/client.rs:3063-3076`（数组元素先于 STRING 特判）、`:1823-1894`（用测试读到的元素类型 + `to_bytes()`，不含 2B handle）；正确结构形式 `values.rs:36-45` | P |
| E-7 | 插件 | P1 | 写值窄化回绕 + 非数字串写 0 | `eip_client/mod.rs:248-284`（`s.parse().unwrap_or(0)`、`as i8/i16/i32/u8/u16/u32`）；`as_u()` 对负数 `max(0)` 静默写 0（`:260`） | 插件 |
| E-8 | 框架 | P0 | 写命令无回执（F-2 的 EIP 实例）；未连接/类型不符/CIP 拒绝只落一条 warn | `eip_client/mod.rs:668-724`；见 F-2 | 框架 |
| E-9 | 上游库 | P2 | 位写 / BOOL 数组元素写是读-改-写，非原子，与 PLC 程序并发会回滚同字其它位 | `<REG>rust-ethernet-ip-1.2.0/src/client.rs:1300-1313`（注释自述 "not atomic across concurrent writers"）、`:1896-1967` | N |
| E-10 | 上游库 | P2 | 健康检查只发 24 字节全零 NOP 且不等应答，无法证明 PLC 存活 | `<REG>rust-ethernet-ip-1.2.0/src/client.rs:3609-3623`（"NOP requires no response, so we don't read one"）；插件注释宣称"应对 PLC 150s 会话超时"（`eip_client/mod.rs:459-483`） | P |
| E-11 | 上游库 | P2 | 批量只按请求字节打包、不算应答；MSP 单条 `0x06 Partial Transfer` 不重组 | `<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:393-420,422-457`（只估请求长度）、`:783-789`（status≠0 直接 BatchError，无 0x06 分支）；单点路径有重组（`src/client.rs:1197-1230`） | P |
| E-12 | 上游库 | P2 | 封装层状态码（0x64/0x65/0x69）映射为非 retriable 的 Protocol，不触发重连 | `<REG>rust-ethernet-ip-1.2.0/src/error.rs:102-114`（is_retriable 只覆盖 Timeout/Connection/ConnectionLost/Io）、`src/client.rs:3823-3841`（封装状态 → `Protocol`） | P |
| E-13 | 上游库 | P2 | 数组元素每 tick 一次"BOOL 数组探测"往返，无缓存 | `<REG>rust-ethernet-ip-1.2.0/src/client.rs:1374-1386`（每次调用都发一次 `send_cip_request`）；批量构造阶段对每个数组元素触发（`src/client/batch_exec.rs:556-569`） | P+U |
| E-14 | 插件 | P3 | 无"一次读 N 元素"；多维数组只能逐元素；UDT 落成 `{"symbol_id":0,"data":[...]}` 大文本 | `eip_client/mod.rs:305-346`（Targets 只有单 tag）、`:199-201`（UDT → JSON 串）；`src/types.rs:290-305`（ChildSpec 无 count）；库 `read_array_range` 未接（`<REG>rust-ethernet-ip-1.2.0/src/client.rs:1756-1770`） | 插件 |
| E-15 | 插件 | P1 | `endpoint` 必须 `ip:port`，主机名/省略端口必失败，与设计文档不符 | `eip_client/mod.rs:74-79`（只校验字段存在）、`:130-156`（原样透传）；`<REG>rust-ethernet-ip-1.2.0/src/client.rs:592-598`（`addr.parse::<SocketAddr>()`） | 插件 |
| E-16 | 上游库 | P2 | 含 `:` 的 I/O tag（`Local:1:I.Data.0`）编码失败，退化成单符号段 → `0x04` | `<REG>rust-ethernet-ip-tag-path-1.2.0/src/lib.rs:414-435`（只认 `Program:` 前缀）、`:517-534`（兜底整串当符号名） | P+U |
| E-17 | 插件 | P2 | 单连接全串行、无整轮预算；一次 retriable 错误丢整轮已采数据 | `eip_client/mod.rs:404-504`（select 内串行 await）、`:626-644`（`return false` 丢弃 `per_dev`）；库每请求硬编码 10s 超时（`<REG>rust-ethernet-ip-1.2.0/src/client.rs:3805-3809`） | 插件 |
| E-18 | 插件 | P2 | 配置项 `max_packet_size` 对批量打包无效（批量只读 BatchConfig） | `eip_client/mod.rs:104,157-159`（`set_max_packet_size`）；`<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:398`（用 `batch_config.max_packet_size`）、`src/client.rs:720-727`（该字段仅 FFI 读取） | 插件 |
| E-19 | 上游库 | P3 | 打包长度估算少算 16B SendRRData/CPF 固定头 | `<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:422-457` vs `src/client.rs:3641-3698` 与 `rust-ethernet-ip-protocol-1.2.0/src/cip.rs:155-184`（实际 4+2+2+4+4=16B） | P |
| E-20 | 上游库 | P3 | 批量结果按"位置"回填，依赖设备保序；非 AB 设备/网关可能出现整体错位 | `<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:703-744`（`operations[i] ↔ reply_offsets[i]` 按序配对） | N |

**已核实正确**：全链路小端（`<REG>rust-ethernet-ip-protocol-1.2.0/src/values.rs:47-63,74-174` 全 `_le`/`from_le_bytes`）；AB 有符号/无符号按类型码精确解码；标准 STRING 读写偏移与句柄正确（`values.rs:22-26,36-45,180-223`）；TagPath 的程序作用域/数组元素/多维/UDT 成员/位段编码验证正确；`Ulint` 超 i64 显式饱和（`eip_client/mod.rs:194-195`）；批量 per-tag 容错与部分成功语义正确（`<REG>rust-ethernet-ip-1.2.0/src/client/batch_exec.rs:750-860`）；多 devid 同 tag fan-out 正确（`eip_client/mod.rs:541-573`）；`sender_context` 校验防错配（`<REG>rust-ethernet-ip-1.2.0/src/client.rs:3814-3819`）。

---

## 7. 证据与复现

1. **数值/布尔转换实证**（MB-1/MB-2 同源）：独立 `rustc` 程序输出 `-1 as u16=65535`、`70000 as u16=4464`、`40000 as i16=-25536`、`-5 as u32=4294967291`、`"1".eq_ignore_ascii_case("true")=false`；serde_json 对 NaN/Infinity 序列化为 `null`（`{"x":NaN}` → `{"x":null}`）。
2. **S7 探针**（S-1/S-2/S-3/S-4/S-5）：`/tmp/s7probe` 内五个 bin，含进程内 mock PLC（S7 握手/读写应答），输出：
   - `s7probe`：正常 disconnect → accepts=1；drop 后 0.6s → accepts=2044（累计 5422/1.6s）。
   - `pdu`：协商 PDU=240 + 20 项读 → `Error: Read request exceeds PDU limit`。
   - `err`：PLC 回头级 0x85/0x04 → `Unknown error class: 0x85`；同连接第二条 → `Actor disconnected`。
   - `wr`：写应答 `return_code=0x05(InvalidAddress)` → 库返回 `Ok(())`。
   - `str`：PLC 侧 `"°C"`（Latin-1 0xB0 0x43）→ `"�C"`（bytes `[239,191,189,67]`）。
   - `bit`：`write_bool` 线上字节 `… 00 03 00 01 01`（BIT/1 bit/值 1）。
3. **库源码行号**：均以 `<REG>` 为根，按文件:行号给出（见各表"证据"列）。
4. **未覆盖声明**：无真机 PLC；S7-2 的 PLC 侧连接资源耗尽是协议常识推断（探针只证明客户端侧 3400 conn/s）；E-2/E-3/E-16 依赖 AB 类型码表与库实现，未在真机抓包；opcua/eip 结论均来自库源码逐行核对。`target/release/rs-iot` 未编入 eip，故 EIP 条目无生产复现。

---

## 8. 建议修复顺序（未排期，供决策）

| 批次 | 条目 | 理由 |
|---|---|---|
| 1 | F-1、F-2（+ F-9） | 写通道是唯一让"写"可用的前提，也是其余写修复的验证基础 |
| 2 | S-2、S-3 | 改动小、直接消除 PLC 连接资源级故障（先 disconnect 再 drop + 按 PDU 分片） |
| 3 | S-1、S-6、S-7、MB-1、MB-2、S-11、E-7、OPC-6 | 写失败可见化 + 越界拒写（四家统一） |
| 4 | E-1、E-2、OPC-2、OPC-10、OPC-1 | 停采 / 静默错数 / 假健康 |
| 5 | S-4、S-16（F-8）、S-18、OPC-14、E-15 | 低成本诊断与配置契约 |
| 6 | 其余 P2/P3 与能力缺口 | 按现场点表形态排期 |

---

## 9. 修复落地状态（2026-11-01 第二批）

> 实现证据 = 本批提交 diff + 各插件内新增单测；门禁见提交说明（`cargo fmt --check`、`cargo clippy --all-targets --all-features`、`cargo test --all-features`）。

### 9.1 已闭环（代码 + 单测）

| 组 | 条目 | 落地位置 |
|---|---|---|
| 框架 | F-1、F-2、F-4、F-5、F-6、F-8、F-9 | `src/types.rs`（`SampleValue::{UInt,Null}`、`CtlMessage.request_id`）、`src/host.rs`（`WriteAcks`）、`src/api/routes_command.rs`（12s 回执 / 422 / 504）、`src/db/rtdata.rs`（空批跳过、坏值不覆盖）、`src/history/writer.rs`（U64 车道、Null 不归档）、`src/model/expand.rs`（`slave_id` 装配期校验）、`src/lib.rs`（`LogTracer`） |
| Modbus | MB-1~MB-8 | `modbus_client/{codec,config,merge,address,mod}.rs`：越界拒写、布尔词表、slave 范围、地址溢出、写归属校验、coil 2000 上限、IllegalDataAddress 对半拆分、RTU `request_gap_ms` |
| OPC UA | OPC-1~OPC-8、OPC-10~OPC-17 | `opcua_client/{convert,node,mod}.rs`：坏质量 Null、`MaxNodesPerRead` 分批、Display 文本、写值 checked、点表类型默认、`nsu=`、请求超时、6 策略、CloseSession/重连计数、端点列表、index_range、日志限流、JSON null/u64 |
| S7 | S-1~S-21（S-4/S-5/S-9 经 tokio-s7 fork 根治，`832c8e2` 接入） | `s7_client/{mod,address}.rs`：先 disconnect 再放下、按协商 PDU 分片、kind 从点表推断、STRING 长度交叉校验、`ubyte`/`raw:N`、写值 checked、地址严格校验、区域缺口显式拒绝、单点失败 Null+限流、响应项数校验、endpoint 拆分、配置 checked；`verify_write` 读回校验保留为可选防线 |
| EIP | E-1~E-8、E-10~E-19（上游项为规避/拒绝） | `eip_client/{mod,route,failure,value,write}.rs`：传输错误重建连接、类型码/位路径/STRING 数组元素配置期拒绝、分流精确化、写值 checked、真实健康读、字节预算切包、0x06 降半重试、探测缓存、endpoint DNS、整轮预算、BatchConfig、16B 估算 |
| 前端契约 | F-1/F-2 用户可见语义 | hmi-studio `src/lib/{command,bridge}.ts`：等待 15s（后端 12s 窗口）、422 透传后端 `error` 文本、504 显示"结果未知（超时）" |

### 9.2 部分闭环 / 未闭环（显式）

| 条目 | 状态 | 说明 |
|---|---|---|
| F-3 | 部分 | 数据面已显式化（Null 标记、rtdata 不覆盖、history 不归档）；**前端质量位渲染未做**——需要 rtdata cell 扩列或新质量 API 的跨仓契约 |
| F-7 | 部分 | 各插件已加 tick 超 period 限流告警与可配超时；EIP 库硬编码 10s 请求超时不可配（上游） |
| OPC-9 | 未做 | Subscription 属能力缺口（P3），轮询 Read 功能可用；需 monitored item 生命周期/keepalive 独立设计 |
| OPC-12 | 部分 | 6 种安全策略映射完成；X509 身份与 PKI 目录未做 |
| OPC-15 | 部分 | `[n]`/`[a:b]` index_range 读写完成；按结构字段名读写未做 |
| E-13 | 部分 | 探测降为每 base 每 tick 一次；库内 `read_array_range` 自带探测无法经公开 API 消除 |
| E-17 | 部分 | 步骤边界预算（非硬超时；库内单次调用仍可能 10s） |
| S-1 | 已闭环 | tokio-s7 fork 补丁逐项校验写应答 return code（`832c8e2` 接入，git 源 `#b04826c`）；插件 `verify_write` 保留为可选第二防线（现场可关） |
| S-4 / S-5 / S-9 | 已闭环 | fork `rs-iot/fixes` 分支根治（GitHub 公开镜像 `github.com/viccom/tokio-s7` 为 `[patch]` 源 + `git.metme.top` 内部镜像，同 rev `#b04826c`），fork 内各带单测；`cargo tree` 实证 git 源生效 |
| E-2 / E-3 / E-6 / E-16、S-13 | 规避 | 配置期拒绝 / 运行期 Null，未根治；上游 issue/PR 待提 |
| E-9 / E-20 | 记录不修 | 台账原裁决 `N`（上游注释自述非原子 / 设备保序假设） |

### 9.3 现场可见行为变更

- **写命令**：声明回执能力的实例，`POST /api/v1/command/send` 最多等待 12s——成功 200、业务失败 422（`error` 为插件原因）、结果未知 504；未声明实例保持"投递即 200"。
- **坏质量点**：插件投 `SampleValue::Null`（不再静默丢点）；rtdata 跳过该 field（保留上次有效值与旧 ts），历史不归档。
- **Modbus**：实例级 `slave_id` 仅 1-247；写值越界/非法布尔串/非数字串一律拒写；coil/discrete 合并上限 2000；RTU 新增 `request_gap_ms`。
- **S7**：`verify_write` 默认 true（每写多一次读回，PLC 会改写写入值的点位需显式关）；string 写长度必须与点位声明一致；写命令缺 `kind` 且点表查不到 → 拒写；`raw:N` 只读。
- **EIP**：DWORD 数组 / WORD / LWORD / BYTE / STRING2、含 `:` 的 I/O tag、STRING 数组元素写在配置期或写入口拒绝（库缺陷，见 E-2/E-3/E-6/E-16）。
- **测试契约**：`tests/modbus_multi_slave_e2e.rs` 两条故障隔离用例改为断言 `Null`（原断言"点位不存在"固化的正是 F-3 要修的旧行为）。
