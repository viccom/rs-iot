# C1-C3：schema 默认值单源 / 默认编译面收敛 / tag_key 串扰修复

> **状态**：implemented（2026-08-26 收口；C1-C3 全修，四档门禁全绿）

## 验收记录

| 项 | commit | 锚点 |
|---|---|---|
| C1 默认值单源 | `7d88344` | 四模块 DEFAULT_* 常量；**漂移实证 5 处修正**（period 前端全系误写 1，后端 3/3/3/2；modbus reconnect 2≠3；补 gap 16/s7 timeout 5000）；schema-values.test.ts 前端镜像锁 5 测 + modbus 后端锁定测；前端 202+build |
| C2 编译面+ticker | `cfd6ded` | default=[lua,lua-http,opcua,modbus]；ticker→src/ticker.rs 12 消费点；decisions 登记；s7 2/2+eip 2/2+feature_gate 4/4 |
| C3 tag_key fail-closed | `7f7cbc0` | effective_all 碰撞剔除+warn；红（后写胜）→绿；前端最长前缀精确剥离；collision 2/2+model 101/101 |

**门禁**（实测）：fmt；clippy 三档（默认/history/s7,eip）零警告；test 四档（默认 614 lib 全绿/history 全绿/minimal 360/s7,eip 全绿）；前端 202+build。
> **日期**：2026-08-26
> **来源**：三项 backlog 的用户裁决——①pluginSchemas 短期修法；②插件编译面收敛为 datasim/lua/modbus/opcua + 最小风险拆分方案；③tag_key 串扰按最优方案修复

## 裁决记录

| 项 | 裁决 | 依据 |
|---|---|---|
| C1 schema 双源 | 后端 `unwrap_or` 收敛 `DEFAULT_*` 常量 + 前端镜像同步（含已发现的 modbus period 1↔3 漂移修正，display-only） | 短期修法；机械对齐靠命名常量+注释契约+后端锁定测试；schema 下发为长期方案另立项 |
| C2 编译面 | `default = ["lua","lua-http","opcua","modbus"]`（移出 transform/s7/eip） | 用户明示默认只需 datasim/lua/modbus/opcua；transform 系 Q7 冻结不接线；s7/eip 已有 cfg 门控+优雅降级（find_source not found），decisions/2026-08-07 先例 |
| C2 拆分 | ticker 提顶级（53 行机械搬迁）；traits 留守 plugin::（契约归属正当）；crate 级拆分不做 | 最小风险：pipeline→plugin 的边因 ticker 搬迁即断；crate 化收益（并行编译）需先实测编译时长 |
| C3 tag_key | effective 构建碰撞检测 **fail-closed**（歧义 key 剔除+warn），前端 datalist 用已知设备目录精确反解析 | 真正的结构修复（二元组 key）牵动 D11 接口全员，为病态命名场景服务不成比例；fail-closed 把"静默越权"变"显式拒绝"，与 L3 默认零归档的安全姿态一致 |

## 任务

### C1：后端默认值常量化 + 前端镜像同步
- modbus_client/config.rs、lua_core/config.rs（luaClient 表单镜像值）、lua_transform/config.rs：前端 pluginSchemas.ts 镜像的每个 `unwrap_or(X)` 提为 `pub const DEFAULT_<FIELD>`（或复用已有 DEFAULT_MEM_LIMIT_MB 等），常量注释声明「前端 pluginSchemas.ts 镜像此值，改动须双侧同步」。
- 每模块内嵌测试：空/缺字段配置 parse → 断言全部默认值等于常量（后端侧防静默改）。
- 前端 pluginSchemas.ts：modbus period `1→3`（对齐后端 unwrap_or(3)，修漂移）；其余逐值核对。
- 红测：modbus parse 测试先断言 period==3（现状即 3，测试即绿——锁定型）；漂移修正在前端 vitest 侧无既有断言则不新增组件测试，人工核对+commit 记录。

### C2：默认编译面 + ticker 提顶级
- Cargo.toml：default 改 `["lua", "lua-http", "opcua", "modbus"]`；s7/eip 注释更新（非默认，显式 `--features s7,eip`）；transform 注释补 Q7 冻结+默认移出。
- src/plugin/ticker.rs → src/ticker.rs（crate::ticker）；消费点 use 改（pipeline.rs、各 source、transform）。
- 门禁矩阵扩：新增 `cargo test -j 2 --features "s7,eip"` 档跑 s7_e2e/eip_client_e2e/feature_gate_e2e（防裁剪后测试失明）；minimal 档保持。
- docs/decisions.md 追加默认编译面变更记录（用户裁决）。

### C3：tag_key 碰撞 fail-closed + 前端精确反解析
- model/effective.rs `effective_all`：构建时记录每个 key 的首个 (devid,tag) 源；后续插入遇同 key 不同源 → **剔除该 key** + 收集冲突对，构建完成 warn 列出（join 碰撞与 contains 直通碰撞同检）。
- 红测（effective 单测）：① ("a","b.c") vs ("a.b","c") 两源 history=true → effective 不含 "a.b.c"；② passthrough 碰撞 ("m","m.n") vs ("m.n","m.n") → 同剔；③ 无碰撞用例零影响。
- fan_out 侧无需改（effective 不含即拒）。
- 前端 Telemetry.vue hqTagOptions：startsWith 盲切改为已知设备目录最长前缀精确反解析。
- rest-api.md §4.14：约束说明升级（碰撞 fail-closed 剔除+日志）。

## 门禁
fmt / 双档 clippy（默认+history）/ 三档 test（默认/history/minimal）+ `--features "s7,eip"` 档 / 前端 vitest+build。

## 风险与回退
- C2 默认面变更：s7/eip 用户显式启用即可恢复；find_source 优雅降级有决策先例；revert 单 commit。
- C3 fail-closed：只影响"碰撞的病态命名"（约束本就声明不含点），正常命名零影响；revert 单 commit。
