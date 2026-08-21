# 设备归属模型 + lux/RESP 收口 + secrets 加密 实施计划

> 日期：2026-08-21
> **状态**：partially-implemented（Task 1-4 已实施）
> **实施**：T1 `64635eb` / T2 `4eadce3` / T3 `b5fac22` / T4 本提交（GET /devices 并入运行时设备）
> **代码**：`src/db/dev_registry.rs`、`src/plugin/sample_sender.rs`、`src/api/routes_inst.rs`、`src/api/routes_device.rs`
> **遗留**：Task 5（D-LUX-1 非 loopback 强制认证）/ Task 6（D-SEC-1 secrets AEAD）/ Task 7（D-CLN-1 死表 WARN + 文档收口）/ Task 8（全量回归 + 冒烟）
> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> 设计定案见同日 `2026-08-21-device-ownership-design.md`（D-OWN-1..4 / D-LUX-1 / D-SEC-1 / D-CLN-1，本计划不重复论证）。

**Goal:** 运行时设备归属闭环（登记/级联清理/设备视图）、非 loopback RESP 强制认证、secrets AEAD 加密、devices 死表提示。

**Architecture:** `SampleSender` 遮蔽 send 在 mgr 单点拦截登记（Sample/pipeline/lux 零改）；`dev2inst` 多宿主列表驱动级联；`chacha20poly1305` + HKDF(jwt_secret) 加密 secrets。

---

## 已核实契约（子代理不必重查）

- Source trait：`src/plugin/traits.rs`——`run(&self, ctx, inst, tx: mpsc::Sender<Sample>, cmd_rx, token)`；实现方 7 个（data_sim/lua_client/lua_transform/self_monitor/modbus_client/opcua_client/s7_client/eip_client/sidecar_client——以 `impl Source for` 全列为准）
- `InstanceManager`：`src/plugin/instance.rs`——`new(ctx, hub_tx)` 后 `start_source(inst)` 内 clone tx 传 Source；**包装点在此**
- lua_core 出口：`api/mod.rs` 的 `send_with_deadline`/`try_send_outcome`/`push_sample`（参数 `&mpsc::Sender<Sample>`）；lua_client 的 collect 分支直接 `tx.send`（约 327-351 行）
- cascade：`src/api/routes_inst.rs:138`；purge：同文件 205+；`db::rtdata::delete_device`
- `GET /devices` 聚合：`src/api/routes_device.rs:27`（聚合函数 + 测试模式已有）
- lux 配置：`src/db/mod.rs:24` `build_lux_config`（`require_auth=false` 注释处）
- secrets：`src/db/secrets.rs`（set/get/del/list + mask_value；routes_secret.rs 调用面）
- dev2inst 读写用 EmbeddedClient RESP（照 db/ 层既有封装风格）
- Cargo 依赖：jwt 已带 hmac/sha2 栈；新增 `chacha20poly1305` + `hkdf`（RustCrypto，无系统依赖）

## Task 0: worktree

`git worktree add ../rs-iot-own -b feat/device-ownership`

## Task 1: db::dev_registry（TDD）

Create `src/db/dev_registry.rs`（mod.rs 注册）：
- `note(lux, devid, inst_id)`：HGET→列表追加去重→HSET（不存在则 HSETNX 语义初始化）
- `owners(lux, devid) -> Vec<String>`
- `remove_owner(lux, devid, inst_id) -> bool`（移除后空 → HDEL，返回"列表是否已空"供调用方决定 rtdata 删除）
- `devices_of(lux, inst_id) -> Vec<String>`（HGETALL 反查，cascade 用）
- `clear(lux)`（purge 用）
- 测试（照 alarms::tests 的 test_client 模式）：note 去重/多宿主/remove 到空 HDEL/devices_of 反查

## Task 2: SampleSender + trait tx 类型 + mgr 包装（编译驱动 + 单测）

- Create `src/plugin/sample_sender.rs`：`pub struct SampleSender { inner: mpsc::Sender<Sample>, lux, inst_id, seen: std::sync::Mutex<HashSet<String>> }`；`Deref<Target=Sender<Sample>>`；**自身 `send`/`try_send` 遮蔽**（先 `note`（seen 去重，注册失败仅 warn 不阻断数据面），再转发；返回类型与 mpsc 对齐）
- Source trait tx 参数类型 `mpsc::Sender<Sample>` → `SampleSender`；`instance.rs` 构造处包装（`SampleSender::new(hub_tx.clone(), lux.clone(), inst_id)`）；lua_core 三个 helper 参数类型同步
- 单测：send 后 dev2inst 有归属；同 devid 二次 send 不重复 HSET（seen 生效——可用 lux 查或 mock 计数）；try_send 同
- 全量编译绿（实现方调用点应零改动——`tx.send(...)` 形态不变；若有编译点暴露差异逐一适配并汇报）

## Task 3: 级联闭环 + 启动预登记

- `cascade_delete`：配置 devid 清理改为经 `dev_registry::remove_owner` 路径（配置设备启动时已预登记，统一走归属表）；再 `devices_of(inst_id)` 兜底清运行时残留归属
- `start_source`：启动即对 `inst.devices[].value` 预登记 `note`
- `purge`：`dev_registry::clear` + rtdata 兜底清不变
- 测试：e2e 形态（照 tests/ 既有集成测试）——建实例（带配置设备）→ 模拟运行时 devid note → cascade → rtdata 与 dev2inst 全清；双宿主删一留一

## Task 4: GET /devices 并入运行时设备

`routes_device.rs` 聚合函数：配置派生 ∪ dev2inst 补集（`{"value":devid,"children":[],"runtime":true,"owner_inst":...}`）；DeviceSpec 聚合输出结构适配；既有聚合测试更新 + 新用例（运行时设备并入/与配置重叠时以配置为准）

## Task 5: D-LUX-1 非 loopback 强制认证（TDD）

- `config.rs`：`LuxConfig` 增 `password: String`（serde default 空）
- `run_app` 启动校验（或 build_lux_config 内 panic 前置校验函数返回 Err）：非 loopback（127.0.0.1/::1/localhost 白名单）且 password 空 → `Err` 退出，错误信息含两条出路；非空 → `require_auth=true` + password 传入
- 测试：loopback 无密 OK / 非 loopback 无密拒 / 非 loopback 带密 require_auth=true

## Task 6: D-SEC-1 secrets AEAD（TDD）

- Cargo 增 `chacha20poly1305`、`hkdf`（workspace 版本对齐 RustCrypto 现行）
- `db/secrets.rs`：`encrypt/decrypt`（key = HKDF-SHA256(jwt_secret)；格式 `enc:v1:<n>:<c>`；`enc:v1:` 前缀判定）；`set` 加密写、`get`/`resolve_password_opt` 兼容明文（无前缀原样返）；`list` 掩码基于解密值
- 密钥构造：`OnceLock<ChaChaKey>`（jwt_secret 进程级恒定）
- 测试：加解密往返/前缀判定/明文兼容/篡改 ct 解密失败返错/跨 secret 值唯一 nonce

## Task 7: D-CLN-1 + 文档收口

- 启动检测 `HLEN devices > 0` → 一条 WARN
- rest-api.md：§4.3 devices 节补运行时设备说明；§2/config 补 `[lux] password`；secret 节补加密存储一句
- release note `docs/ops/releases/2026-08-22-device-ownership.md`（行为变更：非 loopback 启动校验/旧明文 secret 懒迁移/devices 死表提示）
- 计划/spec 实施块回填 + docs/README.md 索引登记

## Task 8: 全量回归 + 冒烟（主会话亲自）

- `cargo test -p rs-iot --features lua,lua-http -j 4` 全绿 + clippy 0 + fmt + check_docs
- 冒烟（e2e-lua-d 环境）：lua 脚本 set_value 运行时设备 → `HGET dev2inst` 归属 → 删实例全清；双宿主场景；`GET /devices` 含 runtime；`secret/set` 后 RESP 原始读见 `enc:v1:`；非 loopback 无密码启动被拒（临时 config 试）
- 合并 master（用户确认）→ worktree 清理

## 验收清单（照抄 spec §8）

- [ ] 运行时设备归属/级联/多宿主三态
- [ ] GET /devices runtime 并入
- [ ] 非 loopback 拒启/带密 AUTH
- [ ] secrets enc:v1 + 明文兼容
- [ ] devices 死表 WARN
- [ ] 全量 + e2e 绿
