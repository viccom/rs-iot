# eipClient 设计文档（EtherNet/IP 南向采集插件）

> **状态**：implemented（协议链路 CI 覆盖；真机专项待验）
> **日期**：2026-08-17（定稿）
> **实施**：2026-08-17 `1f522e8`
> **代码**：`src/plugin/builtin/eip_client/`
> **测试**：`tests/eip_client_e2e.rs`（mock TCP EIP server）
> **遗留**：真机专项（route path 多跳 / 拔网线重连 / 自定义 STRING / UDT，见本文 §六）
>
> 2026-08-17 brainstorming 定稿。依赖库选型结论见 `E:\MyCode\RUST生态-工业协议研究报告\rust-eip-ethernet-ip-library-research.md`：
> `rust-ethernet-ip` 1.2.0（MIT，production-ready，真实 CompactLogix/ControlLogix 硬件验证）。
> 本文档是实现的唯一权威设计；与代码冲突时以代码为准并回改本文。

## 一、定位与边界

罗克韦尔 AB PLC（CompactLogix / ControlLogix）南向采集插件，`app_code = "eipClient"`。
与其他南向插件同构：Source trait + hub 上行 + `cmd_rx` 写命令下行 + supervisor 监护。
不改 hub/pipeline/lux/北向（架构原则：加南向协议只动边界）。

## 二、配置（尽量透出库能力）

`InstConfig.app_config`（serde，缺失字段走 Default）：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `endpoint` | String | 必填 | PLC 地址 `host[:port]`（44818 可省） |
| `period` | u64 | 3 | 采集周期（秒） |
| `slot` | Option<u8> | None | ControlLogix 背板槽号；Some → `with_route_path`，None → `connect` |
| `reconnect_secs` | u64 | 5 | 断线重连节流 |
| `max_packet_size` | Option<u32> | None | 透传 `set_max_packet_size`（批量打包上限） |
| `health_check_secs` | u64 | 0 | >0 时周期 `check_health_detailed()`（主动 keep-alive + 重注册 session） |
| batch 相关字段 | — | — | 实现时按 `BatchConfig` 实际结构透传（若为非_exhaustive builder 则暴露安全子集） |

点表：`devices[].children[].value` = tag 全名（`Program:Main.Tag` / `MyTag` / `MyArray[3]`，
高级寻址由库解析）。`children[].type` 可选：
- 缺省：走 `read_tags_batch`（标量 12 类）
- `"string"`：单独走 `read_string_tag`（自定义 STRING 类型如 Str82 需专用解码路径）
- `"bit"`：**位寻址写进 tag 路径字符串**（如 `MyDint.5`），由 rust-ethernet-ip 库在读写两侧自动识别。
  > 勘误（2026-10-27 审查）：本文早期版本写"`children[].bit`（0-31）→ `read_bit`/`write_bit`"，
  > 但实现中既无 `bit` 字段（`src/types.rs` 的 `ChildSpec` 仅 label/value/type/props）也无
  > `read_bit`/`write_bit`（grep 零命中）；位寻址一律走 tag 路径，见 `eip_client/mod.rs:24` 注释。

## 三、数据流（每 tick）

```
ticker(phase_ms 错峰)
  → read_tags_batch(标量 tags)：一次网络往返，Vec<(tag, Result<PlcValue, BatchError>)>
    per-tag 容错：单 tag 失败仅 warn + 计 fail，不丢整批（对标 write_batch 单 devid 容错）
  → "string" tags 逐个 read_string_tag
  → "bit" tags（路径含 `.N` 后缀）随标量批量读，库自动位寻址
  → PlcValue → SampleValue 映射 → 按 devid 分组 → hub.send(Sample)
```

类型映射（纯函数，单测锁定）：

| PlcValue | SampleValue |
|---|---|
| Bool | Bool |
| Sint/Int/Dint/Lint/Usint/Uint/Udint/Ulint | Int（u64 > i64::MAX 饱和） |
| Real/Lreal | Float |
| String | Str |
| Udt | Str（`"<raw N bytes hex>"`；UDT 成员级展开 v1.1 后置，库有 discover_udt_members） |

## 四、写命令（cmd_rx → handle_write）

REST `POST /api/v1/command/send` 或 MQTT `<hwid>/command`，`data` JSON：

```json
{ "tag": "Program:Main.SetPoint", "value": 1500, "plc_type": "dint" }
```

- `plc_type` 可选：`bool/sint/int/dint/lint/usint/uint/udint/ulint/real/lreal/string`
  （CIP 类型化写入，**强烈建议带**——写 DINT tag 传 REAL 会类型不匹配，对标 opcua 的 opc_type）
- 缺省推断：bool→Bool、整数→Dint（AB 最常用）、浮点→Real、字符串→String
- bit 写：`{"tag":"TagBase.3","value":true}` → 走 `write_tag`（库识别 `.3` 位后缀）
- STRING 写走 `write_string_tag`（库处理 unconnected messaging + AB STRING 格式）

## 五、连接健康管理（对标 opcua session=None 模式）

- `client: Option<EipClient>`，None 即断线态
- tick 时 None → 重连（reconnect_secs 节流）；读写失败且 `is_retriable()` → 置 None 下 tick 重连；
  非 retriable（tag 不存在等 BatchError）不拆连接，单 tag 仅 warn
- 可选 health_check_secs：周期 `check_health_detailed()`（keep-alive + 自动重注册）
- 实例停止时 `unregister_session()` 优雅注销（best-effort，失败仅 warn）

## 六、测试

1. 单测（模块内）：映射全 13 变体 + 饱和、JSON→PlcValue 推断/显式、配置解析容错
2. 集成测试 `tests/eip_client_e2e.rs`（CI 可跑）：进程内 mock TCP EIP server（端口 0，
   照 modbus_multi_slave_e2e 模式）——RegisterSession(0x65) + SendRRData(0x6F) 承载
   CIP ReadTag(0x4C)/WriteTag(0x4D) 最小应答。闭环：建实例 → startinst → 验 rtdata 入库 →
   send_ctl → 验 mock 收到 WriteTag 帧
3. 真机验证清单（CI 不跑，文档后置）：route path 多跳、自定义 STRING、UDT raw、
   拔网线重连、PLC 侧 150s 会话超时踢连接后 health_check 自愈

## 七、交付与验证门槛

- `Cargo.toml`：`rust-ethernet-ip = "1.2"` optional + `eip` feature
  > 勘误（2026-10-27 审查）：**eip 已不在 default**。2026-08-26 用户裁决收敛默认编译面为
  > `lua,lua-http,opcua,modbus`（见 [默认编译面决策](../decisions/2026-08-26-default-compile-surface.md)），
  > eip 需显式 `--features eip`；未编译时 `find_source` 返回 "source plugin not found" 优雅降级。
- `src/plugin/builtin/eip_client/mod.rs`（~450 行）+ `builtin/mod.rs` 2 行注册
- `tests/eip_client_e2e.rs`（~300 行）
- CLAUDE.md：feature 说明 + eipClient 端到端验证节
- 门槛：fmt/clippy(-D warnings)/test(--all-targets) 全绿 +
  `--no-default-features --features lua` 裁剪编译仍过

## 八、后置项（v1.1+）

- UDT 成员级展开（discover_udt_members + write_udt_member）
- read_array_range 批量数组读
- tag-group 订阅模式（subscribe_tag_group，替代轮询的可选路径）
- RetryClient/Backoff 策略透传
