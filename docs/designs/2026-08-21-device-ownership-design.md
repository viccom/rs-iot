# 设备归属模型设计（device ownership）+ lux/RESP 收口 + secrets 加密

> 日期：2026-08-21
> **状态**：implemented（2026-08-21/22 于 worktree `feat/device-ownership` 执行完毕，D-OWN-1..4 / D-LUX-1 / D-SEC-1 / D-CLN-1 全落地，已合并 master）
> **实施**：T1 `64635eb` dev_registry → T2 `4eadce3` SampleSender 遮蔽 send → T3 `b5fac22` 级联闭环+预登记 → T4 `dc0d22b` GET /devices 并入 → T5 `7cb6b7b` 非 loopback 强制认证 → T6 `b6d5323` secrets AEAD → T7 本提交（D-CLN-1 + 文档收口）
> **代码**：`src/db/dev_registry.rs`（dev2inst 注册表）+ `src/plugin/sample_sender.rs`（Source 出口归属登记，Deref+遮蔽；Sample/pipeline/lux 零改）+ `src/plugin/instance.rs`（预登记）/`src/api/routes_inst.rs`（级联/purge）/`routes_device.rs`（视图并入）+ `src/config.rs`+`src/db/mod.rs`+`src/lib.rs`（[lux] password + 非 loopback 校验 + 死表 WARN）+ `src/db/secrets.rs`（chacha20poly1305 AEAD + HKDF(jwt_secret) OnceLock 派生键，enc:v1/明文兼容懒迁移）
> **测试**：`cargo test -p rs-iot --features lua,lua-http -j 4` **566+/0**（dev_registry 8 / SampleSender 7 / dev_ownership_e2e 集成 3：预登记·级联闭环·双宿主·purge / devices 视图 6 / lux 校验 6+redact / secrets 加密往返·篡改·nonce 唯一·明文兼容 6+）；clippy `-D warnings` 0；现场冒烟见计划实施块
> **遗留**：跨实例同 devid 共写的**语义**治理（写合并策略，本设计只做归属记账）；rest-api §4.7 device/set|delete body 描述与代码的既有过时（T4 登记）；lux 只读模式/per-connection ACL（用户决策略过）

## 0. 问题回顾（审查定级）

| # | 缺陷 | 级别 |
|---|---|---|
| 1 | 运行时产生的设备/标签（lua 嵌套/set_value、transform 派生、sidecar 上报）删除实例后残留，且经 mqtt-pub-default 持续上北向平台 | 高 |
| 2 | lux RESP 无认证（loopback 全权，`HGETALL secrets` 可读平台凭证明文） | 高 |
| 3 | Sample 无宿主维度，运行时设备不可溯源（#1 根因） | 中 |
| 4 | lux `devices` Hash 死表（已被聚合派生架空） | 低 |

## 1. 决策 D-OWN-1：归属登记制——`SampleSender` 遮蔽 send 拦截（不动 Sample/pipeline）

**候选对比**：

| 方案 | 改动面 | 破坏 ADR D1 | 缺陷 |
|---|---|---|---|
| a. Sample 加 `src_inst` 字段 | Sample + 全部构造点（编译驱动） | ✗ 破 | 外部 sidecar bridge 协议不变但序列化面变；类型污染（传输层字段混入数据结构） |
| b. hub 元素包装 `(InstId, Sample)` | hub/dispatcher/worker + Source trait | ✗ 破（trait 签名） | 包装/解包散落，与 a 波及相当 |
| c. **SampleSender 拦截**（定案） | Source trait 的 tx 参数类型 `mpsc::Sender<Sample>` → `SampleSender`（newtype 包 `Sender` + `Deref` + **自身 `send`/`try_send` 方法遮蔽 Deref 的**）；`InstanceManager` 构造处集中包装 | **不破**（Sample/pipeline/lux 全不动；仅 trait 的 tx 类型具名化，插件调用点 `tx.send(...)` 形态零改） | 登记是旁路写（与 rtdata 写非原子——归属是管理面数据，最终一致可接受，启动登记兜底） |

**机制**：`SampleSender::send(sample)` → `dev_registry::note(lux, &sample.devid, inst_id)`（进程内 `seen: HashSet` 去重，同 devid 仅首次 HSET，防每 tick 写）→ 转发 `inner.send(sample)`。`try_send` 同理。lua_core 的 `send_with_deadline`/`try_send_outcome` 参数类型随之 `&SampleSender`（内部转发，语义不变）。未来新插件自动获得归属登记（不会忘）。

**登记点收敛**：仅 `InstanceManager` 一处包装；`selfMonitor`/sidecar/内置协议插件/lua 全角色统一覆盖（它们都经 mgr 传的 tx）。

## 2. 决策 D-OWN-2：`dev2inst` 多宿主列表 + 最后宿主删除才清 rtdata

lux Hash `dev2inst`：field = devid，value = 宿主 inst_id 逗号分隔列表。

- **多宿主并记**（跨实例同 devid 共写是既有遗留，C 阶段已登记）：`note` 追加去重；同一 devid 被多实例写时全部归属在册。
- **cascade_delete 闭环**：删实例时反查 `dev2inst` 中含该 inst 的全部 devid → 从各列表移除该宿主 → **列表空才 `DEL rtdata:<devid>`**（共写设备删一个宿主不清数据）→ field 空则删。原"配置声明 devid 清理"逻辑保留（配置声明 + 运行时产生两类都经归属表闭环，配置路径成为子集）。
- **purge**：`DEL dev2inst` 一并清（scope=all/rtdata 都清归属——rtdata 没了归属无意义）。
- 兜底：`start_source` 启动时对配置 `devices[].value` 预登记（防"只启动未出数即删"漏归属）。

## 3. 决策 D-OWN-3：`GET /devices` 并入运行时设备

聚合 = 实例配置派生（现状）∪ `dev2inst` 补集（运行时设备，带 `runtime: true` + `owner_inst`）。前端设备树/roster 自动获得运行时设备宿主（替换现"stat runtime_devs"观测源——后者保留于 stat 不动）。管理 API `device/set|delete` 语义不变（仍写实例配置）。

## 4. 决策 D-OWN-4：北向不过滤

归属清理闭环后不存在孤儿 rtdata，`mqtt-pub-default` 维持全量抽取。理由：过滤引入"漏发"风险（登记延迟窗口内正常数据被误滤），清理闭环已消除问题本体。

## 5. 决策 D-LUX-1：非 loopback 强制认证

- `AppConfig.lux` 增 `password` 键（缺省空）。
- 启动校验：`bind_host` 非 `127.0.0.1`/`::1`/`localhost` 且 `password` 空 → **启动失败退出**（明确错误信息指引两选一：设密码或回 loopback）。非空 → `require_auth=true` + 传入 password（lux 自身对非 loopback 无认证另有 `allow_insecure_no_auth` 拒绝兜底，双保险）。
- loopback 维持现状（无密码）——单机信任模型文档化。

## 6. 决策 D-SEC-1：secrets 值 AEAD 加密存储

- 算法 `chacha20poly1305`（RustCrypto 纯 Rust，无系统依赖）；密钥 = `HKDF-SHA256(jwt_secret, salt="rs-iot-secrets-v1", info="lux-secrets")` 派生 32B（jwt_secret 已是部署期秘密，不新增配置面）。
- 存储格式 `enc:v1:<nonce_b64>:<ct_b64>`；**无前缀 = 旧明文，读兼容**（`ctx:secret_get`/北向 resolve 命中明文直接用），`secret/set` 一律写加密；懒迁移（读到明文不回写，下次 set 才加密）。
- 效果：RESP 直读 `HGETALL secrets` 只见密文；进程内 `EmbeddedClient` 路径经 `db::secrets` 透明解密。
- `secret/list` 掩码语义不变（掩码基于明文，加密后仍基于解密值）。

## 7. 决策 D-CLN-1：devices 死表提示废弃

启动时检测 lux `devices` Hash 非空 → 一条 WARN 指引（历史遗留表已废弃，权威源为实例配置；可 `DEL devices` 清理）。不自动删数据（保守）。

## 8. 验收标准

- [ ] lua 脚本 `set_value("rtdev1", ...)` 产生运行时设备 → `HGET dev2inst rtdev1` 归属正确；删实例 → `rtdata:rtdev1` 与归属项同清
- [ ] 跨实例共写同 devid：删其一数据保留，删最后一个才清
- [ ] `GET /devices` 含运行时设备（runtime 标记 + 宿主）
- [ ] 非 loopback 无密码启动被拒（错误信息可操作）；带密码启动 lux 要求 AUTH
- [ ] REST 写 secret → lux 原始值 `enc:v1:` 前缀；脚本 `secret_get` 与北向 resolve 正常；旧明文值读兼容
- [ ] 旧 `devices` Hash 非空启动出 WARN
- [ ] 全量测试 + e2e（归属登记/级联/多宿主/加密往返/RESP 拒写面回归）绿

## 9. 明确不做

- lux 只读模式（用户决策：略过）
- per-connection ACL / 只读用户分权（后续演进）
- 跨实例同 devid 共写的**语义**治理（写合并策略仍是既有遗留，本设计只做归属记账）
- sidecar bridge 协议变更（零改动——登记在 rs-iot 侧拦截完成）
