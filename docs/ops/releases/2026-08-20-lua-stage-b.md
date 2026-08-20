# Release Note：Lua 脚本系统 · 阶段 B（北向突破）

> 日期：2026-08-20
> **状态**：已发布（worktree `feat/lua-stage-b`，`ded2cb4..` 12 task，待合并 master）
> 范围：演进设计 v0.6 阶段 B 全量——luaNorth（Sink 首个接线者）/ mqtt-north 契约（Q5）/ secret 链路（Q4）/ cloud 配方 EMQX→OneNET（Q8）/ MqttPublisher 无损迁移 / MQTT 透传 backend（Q10）
> 读者：网关运维 + 平台接入脚本作者。升级动作集中在「升级必读」，其余为了解性内容。

## 升级必读（替换二进制即生效）

1. **旧 `[mqtt]` 配置行为等价迁移**：配置了 `[mqtt] host` 的部署，启动时自动生成默认北向实例
   `mqtt-pub-default`（app_code=`luaNorth`，内置兼容脚本落位 `scripts/_builtin-north-compat/main.lua`，
   已存在不覆盖——修改后即为自定义脚本）。**topic/payload/命令处理与旧 MqttPub 逐字段等价**
   （解析后 JSON 结构比对测试锁定）；差异仅在断连重连机制更健壮（重连重走认证序列）与计数
   迁至实例 stat（`publish_ok`/`publish_fail`/`reconnect_count`；`/metrics` 的 mqtt_* 恒 0 保留形状）。
   彻底关闭北向：`[mqtt] host=""`。**注意**：无 `lua` feature 的裁剪构建不再有默认北向（旧代码删除的
   必然结果，启动时显式 warn）。
2. **`app_type:"North"` 实例不再被拒绝**：REST `startinst` 与 lux autoStart 均按方向分发
   （North→Sink 路径）——此前 North 实例被 400 拒绝/静默跳过。
3. **实例配置新增可选键**：`secrets: ["secret:<ref>", ...]`（凭证授权清单——不在清单的引用
   `ctx:secret_get` 返回可感知错误）；mqtt-north 全套配置见指南新章。
4. **无破坏性变更**：collector（luaClient）行为零变化（回归 366 项测试）；脚本 API 仅新增不修改。

## 新能力

- **luaNorth（mqtt-north 角色）**：Lua 脚本化北向平台接入——六回调契约
  （`auth(cfg)`/`up(batch)`/`down(topic,payload)`/`will()`/`on_connack`/`on_disconnect`）、
  混合批量（`batch_max_count` 默认 50 / `batch_max_secs` 默认=period，lux pull 快照批量）、
  **重连必重调 auth()**（token 型认证自动刷新）、宿主逐条投递（qos 按条声明，失败记 stat 不中断）；
  走 InstanceManager 全套（多 broker 并存/启停/stat/热加载/VM 级 reload 失败回退）。
  能力包：无 C1（request/transport 不注入）；send_command/secret_get 可用。
- **secret 链路（凭证不出 lux）**：lux 全局 `secrets` 表 + REST
  `POST /api/v1/secret/set` / `GET /api/v1/secret/list`（**只返掩码**）/ `DELETE /api/v1/secret?ref=`
  （写删均审计 ref 级不落值）；脚本侧 `ctx:secret_get(ref)`（实例 `secrets` 清单显式授权，
  越权可感知报错）；NorthCfg 的 `password` 支持 `secret:` 引用。
- **cloud/ 配方（官方源仓 1.2.0 在线分发）**：`cloud.emqx`（裸 broker 直连 + topic helper）、
  `cloud.onenet`（安全鉴权 token——`et\nmethod\nres\nversion` 排序签名 + key 先 base64 解码 +
  urlencode，Python 参考实现逐字符一致；物模型 topic/下行 helper）；
  `algo.hashings.{md5,sha1,sha256,hmac}`（freeioe vendored + NIST/RFC 向量 selftest）。
  **OneNET 生产提示**：token 含一次纯 Lua hmac_sha1（~70 万 VM 指令），实例
  `hook_instr_limit` 须 ≥2_000_000（默认 1M 会拦截 auth 回调）。
- **MQTT 透传南向（io_engine 第 4 传输）**：`kind:"mqtt"` + `req_topic`/`resp_topic`（+
  可选 `passive_topics`）——collector 脚本 `ctx:request` 一问一答完全不感知链路是 MQTT
  （`ctx:transport()` 返 `"mqtt"`）；被动帧走 on_data。QoS1 断连补发可能迟到——脚本侧帧带序号幂等。
- **manifest 多角色契约**：`-- @role mqtt-north` 强制 `auth`+`up` 存在（collector 强制 collect 不变；
  无 manifest 头的存量脚本豁免不变）。

## 已知限制（上线前关注）

- **OneNET 真平台端到端待现场验收**：token 向量与进程内契约闭环已验；真实平台（认证→上报→下行）
  需现场凭证手动跑（真机 e2e `RSIOT_NORTH_E2E_BROKER` 已就绪）。
- **官方源仓未发布**：cloud/hashings 经 sync 在线分发，默认 upstream 仍为占位 URL（阶段 A 遗留）——
  上线前发布 dist 或配置镜像，然后 `POST /api/v1/lua/stdlib/sync`。
- **mqtt-north 数据面为 lux pull 快照批量**：每周期收拢全部设备最新值（非增量流）；流式升级排
  C 阶段（EventBus 带值）。
- **透传 backend 的 30s 兜底**：连接健康但响应丢失时，单次 request 最长占住 IO task 30s（与 TCP
  同语义，登记架构遗留评估）。
- **mqtt-north 的 send_command 恒注入**（兼容脚本依赖 down()→send_command）：R3 显式授权收紧
  列后续（设计遗留清单）。

## 配置新增汇总

| 键 | 层 | 默认 | 说明 |
|---|---|---|---|
| `secrets` | 实例 app_config | 无 | `ctx:secret_get` 授权清单（ref 数组） |
| `script/host/port/username/password/client_id` | north app_config | — | mqtt-north 必填项（password 支持 `secret:`） |
| `batch_max_count` / `batch_max_secs` / `period` | north | 50 / =period / 5 | 混合批量参数 |
| `subscribe` | north | [] | down() 订阅 topic 列表 |
| `req_topic` / `resp_topic` / `passive_topics` | collector（kind=mqtt） | 必填 | 透传 topic |
| REST | — | — | secret 三端点（见参考文档 §4.11） |

> 工程侧：本阶段另修复存量 flaky `db::stat::instances_are_isolated`（测试 lux 共享 data_dir 并发
> 串数据）；阶段 B 全程子代理驱动 + task 间审查，验收清单逐项实测记录于
> [阶段 B 计划](../../designs/2026-08-20-lua-script-system-stage-b-plan.md)文末。
