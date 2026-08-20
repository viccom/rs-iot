# Lua 脚本系统演进 · 阶段 B（北向突破）实施计划

> 日期：2026-08-20
> **状态**：accepted（执行中）
> **实施**：Task 1-2 已完成（T1：R4 核查 + start_sink 接口适配；T2：spike-2 MQTT 透传 pump 分路模型验证——方案 a 成立，结论落设计 §5.4；均 2026-08-20）
> **代码**：（执行时回填）
> **测试**：（执行时回填）
> **遗留**：全部（本文件即待办）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 落地 `2026-08-18-lua-script-system-evolution-design.md` v0.5 阶段 B——luaNorth 薄壳（Sink 首个接线者）、mqtt-north 契约（Q5 混合批量）、secret 链路（Q4）、cloud/ 配方 EMQX→OneNET（Q8）、MqttPublisher 实例化迁移（还旧账）、spike-2 MQTT 透传 backend（§9 R2/Q10）。

**Architecture:** luaNorth 以 **Sink trait** 接入 InstanceManager 全套（多 broker/启停/stat/热加载）；数据收拢取 **lux pull 快照批量**（复用 MqttPub 已验证模式，流式 tap 依赖 C 阶段 EventBus 带值——Q6 已排期）；mqtt-north 回调契约（auth/up/down/will/on_connack/on_disconnect）在薄壳事件循环组装，沙箱/能力包/loader 全复用 lua_core；secret 走 lux 全局 `secrets` Hash + 实例配置显式授权清单（R3）；cloud/ 配方进官方源仓在线分发（hashings 子集不进种子，D5 预算）。

**Tech Stack:** rumqttc 0.25（已依赖；`local-broker` rumqttd feature 预留可作 e2e）、mlua 0.12、lua_core（阶段 A 全量）、lux EmbeddedClient HSET/HGETALL。

**执行前置：** 本计划跨 ≥5 文件 ≥10 commit，**必须在独立 worktree 执行**（`git worktree add ../rs-iot-lua-b <branch>`）。设计依据：v0.5 §5.0/§5.3/§5.4、Q4/Q5/Q8/Q10、§9 风险 R2/R4/R5；阶段 A 审查遗留（register 参数收敛）在 T3。

**约定：** `cargo test -p rs-iot --features lua,lua-http`（包名 `rs-iot`）；每 Task 末 commit；测试优先单测/函数级（本仓无 broker 级测试基建时），mock broker e2e 走 `local-broker` feature 门控或 `#[ignore]` 手动真机（CLAUDE.md：CI 守协议正确性、现场守物理正确性）。

**B 阶段三项已定取舍（计划级决策，执行不再议）：**

1. **luaNorth 数据收拢 = lux pull 快照批量**（非流式 tap）：每 `batch_max_secs`（默认 = period）从 lux 收拢全部 `rtdata:<devid>` 最新值组成 batch（上限 `batch_max_count` 分片）；Sink trait 的 `rx` 保留并正确消费（收到样本直接入批缓冲——pipeline 未来 tap 时零改动）。流式语义升级排 C 阶段（EventBus Rtdata 带值，§9.1 Q6 行）。
2. **MQTT 命令下行（`<hwid>/command`）语义保持**：兼容脚本 `down()` 解析 CtlMessage 后调 `ctx:send_command`（C3/R3，默认实例显式授权）；宿主不额外订阅命令 topic（报文语义归脚本，链路形态归宿主——§5.4 原则复用）。
3. **hashings 不进种子**：OneNET token 需 sha1/hmac，从 FreeIOE 搬 `algo/hashings` 子集进官方源仓在线分发（种子 ≤500KB 预算不破，D5）。

---

## 组 0：前置核查与 spike（阻塞 T11 与 T6 形态）

### Task 1: R4 核查——Sink 侧管理面覆盖 + start_sink 接口适配

**Files:**
- Test: `tests/instance_failed_e2e.rs` 追加（或新 `tests/lua_north_e2e.rs`）
- Modify: `src/plugin/instance.rs:119`（start_sink 签名可能调整）

R4（§9 风险表）：`start_sink` 从未接线，REST 实例 CRUD/状态事件/stat 对 sink 实例的覆盖度未验证。**B 阶段首项核查，早于 mqtt-north 主体。**

**Step 1: 写失败测试**——sink 实例经 InstanceManager 全生命周期（用临时 `DummySink` 注册进测试 registry？**不可**：registry 是 `collect!` 静态链接。改为核查现有 REST 启动路径）：
- 读 `src/api/routes_inst.rs`（实例 CRUD）与 `src/host.rs::try_start_from_lux`：确认北向实例（`app_type=North`）创建后走 `mgr.start_source` 还是 `start_sink`；
- 已知缺口预判：`start_sink(inst, sample_rx)` 要求调用方提供 `sample_rx`——REST/lux 自动启动路径无法提供。**修复方向（本 task 定案）**：`start_sink` 签名去掉 `sample_rx` 参数，luaNorth 在 `run()` 内自建闲置 rx 通道（数据走 lux pull，见取舍 1）；pipeline tap 是 C+ 阶段事，届时再加 `start_sink_tapped` 变体，YAGNI。
- 写集成测试：lux 写入一个 `app_type=North` 的 luaNorth 实例配置 → `try_start_from_lux` → 断言 states 含该实例（Running）、`/inst/stats` 可见、`stop` 后 states Stopped。T6 前用任一已注册 sink？**无已注册 sink**——本 task 先做核查 + 签名修正 + 测试留 `#[ignore]`（T6 luaNorth 注册后启用）。

**Step 2-3:** 实现签名修正（`start_sink(&self, inst)`——内部 `mpsc::channel(64)` 自建 rx 传入 run），编译通过；核查结论（REST CRUD 覆盖情况）写入本文件末尾「核查记录」节。
**Step 4:** Commit `fix(plugin): start_sink 签名适配 lux/REST 启动路径（R4 核查）`

### Task 2: spike-2——MQTT 透传 backend 与 io_engine 独占模型的适配

**Files:**
- Test: `src/plugin/lua_core/io_engine/mod.rs` tests 追加（结论落 §5.4）

**Step 1: 写实验测试**（`#[tokio::test]`，用 rumqttc 连本机？**无需 broker**——验证的是模型不是网络：用 `rumqttc::EventLoop` 直驱 + mock transport 不可行，改用最小 AsyncClient + 未启动 server 的连接失败路径 + 内部通道分路逻辑纯测）：

```rust
/// spike-2（§9 R2/Q10）：验证「EventLoop 独立 pump task + 内部 mpsc 分路」
/// 能适配 IoBackend 独占模型——request = publish + 等首帧（reply topic 模板），
/// recv_idle = 被动帧通道。结论决定 T11 走全双工（方案 a）或单向透传（方案 C）。
#[tokio::test]
async fn spike_mqtt_backend_eventloop_pump_fanout() {
    // 1) 起 EventLoop（连 127.0.0.1:1 必失败端口）+ spawn pump task：
    //    poll() 出包按「当前是否有在飞 request」分路到 resp_tx / passive_tx
    // 2) 断言：连接失败经 poll 冒出，pump 不 panic、通道正确传达错误
    // 3) 分路逻辑抽纯函数 route_incoming(in_flight: bool, msg) -> Sink 判定单测
    //    （真网络双向收发需 broker，归 T11 e2e）
}
```

**Step 2:** 跑通并记录：EventLoop 可被独立 task 持有轮询、AsyncClient clone 发 publish 不阻塞、分路模型成立 → 方案 a；若 poll 独占导致 request 无法并发 → 方案 C（单向：publish-only + on_data 被动订阅）。
**Step 3:** 结论一行落设计 §5.4（`spike-2 实测：…取方案 a/C`），commit `docs(spec): spike-2 结论落档`。

---

## 组 1：结构准备（阶段 A 审查遗留）

### Task 3: api::register 9 参收敛为 ApiCtx 组装 struct

**Files:**
- Modify: `src/plugin/lua_core/api/mod.rs`（register/reload_vm 签名）
- Modify: `src/plugin/builtin/lua_client/mod.rs`（调用点 ×2）
- Modify: `src/plugin/lua_core/api/tests.rs`（调用点）

**Step 1: 重构**（纯签名收敛零行为变化，现有 304 测试即回归锁）：

```rust
/// register 的组装上下文（阶段 A 审查遗留：9 参收敛，B 阶段 north 能力包再加字段不再扩参）。
pub(crate) struct ApiCtx {
    pub(crate) ctx_host: HostContext,
    pub(crate) inst: InstConfig,
    pub(crate) io: IoEngine,
    pub(crate) tx: mpsc::Sender<Sample>,
    pub(crate) runtime_devs: RuntimeDevs,
    pub(crate) dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) transport: &'static str,
    pub(crate) scripts_root: std::sync::Arc<std::path::Path>,
}
pub(crate) fn register(lua: &Lua, ac: &ApiCtx) -> mlua::Result<()>
```

`reload_vm(cfg, lctx, ac: &ApiCtx)` 同步收敛（8 参 → 3）；两处 `#[allow(clippy::too_many_arguments)]` 删除。
**Step 2:** `cargo test -p rs-iot --features lua,lua-http` 全绿（零行为变化准入）。
**Step 3:** Commit `refactor(lua): api::register/reload_vm 参数收敛为 ApiCtx（审查遗留）`

---

## 组 2：secret 链路（Q4/D6）

### Task 4: lux `secrets` 全局表 + REST 管理（写入/掩码读/审计）

**Files:**
- Create: `src/db/secrets.rs`（`HSET secrets <ref> <value>` / `HGETALL` / `HDEL`；照 `db/luakv.rs` 模式）
- Create: `src/api/routes_secret.rs`（`POST /api/v1/secret/set`、`GET /api/v1/secret/list`、`DELETE /api/v1/secret?ref=`）
- Modify: `src/api/mod.rs`（路由注册，JWT 保护）

**Step 1: 失败测试**（routes_secret.rs 函数级 + db/secrets.rs 单测）：
- set 写入 lux 后 `list` 只返 `{ref, masked: "abcd****wxyz"}`（前 4 后 4，≤8 字符全掩码）——**绝不回显明文**（D6）；
- ref 命名校验：`secret:` 前缀（D6 受限前缀）+ `[a-z0-9_-]`；
- 审计：set/delete 各落一条 `tracing::info!("[secret][audit] set ref={ref} by ...")`——**ref 级不落值**（Q4）。

**Step 2-4:** 实现 → 全绿 → commit `feat(secret): lux secrets 全局表 + REST 管理（Q4/D6，掩码读/审计）`

### Task 5: `ctx:secret_get(ref)` + 实例授权清单（R3 显式授权）

**Files:**
- Modify: `src/plugin/lua_core/api/mod.rs`（register 注入 secret_get；ApiCtx 不变——授权清单从 `inst.app_config.secrets` 读）
- Test: `src/plugin/lua_core/api/tests.rs` 追加

**Step 1: 失败测试**：
- 实例配置 `app_config.secrets: ["secret:onenet_key"]` → `ctx:secret_get("secret:onenet_key")` 返回明文；
- 引用不在清单的 ref → `RuntimeError("secret 未授权：secret:xxx（请检查实例配置 secrets 清单）")`——**可感知越权**（R3）；
- ref 无 `secret:` 前缀 → 报错（防脚本读任意 lux key）；
- 无任何 secrets 配置时函数仍注入（Q4「恒注入」，未引用零成本）。

**Step 2-4:** 实现（`lux HGET secrets <ref>`，miss 返 nil）→ 全绿 → commit `feat(lua): ctx:secret_get + 实例授权清单（Q4/R3）`

---

## 组 3：luaNorth 薄壳 + mqtt-north 契约（Q5/Q19）

### Task 6: luaNorth 薄壳骨架——Sink 接线 + InstanceManager 全套

**Files:**
- Create: `src/plugin/builtin/lua_north/mod.rs`（Sink impl + 主循环骨架）
- Create: `src/plugin/builtin/lua_north/config.rs`（NorthCfg 解析）
- Modify: `src/plugin/registry.rs`（SinkEntry 注册 `luaNorth`）、`src/plugin/builtin/mod.rs`
- Test: `tests/lua_north_e2e.rs`（启用 T1 留的 `#[ignore]` 测试）

**Step 1: NorthCfg**（`app_config` 键，照 lua_client/config.rs 模式）：

```rust
pub(crate) struct NorthCfg {
    pub script: String,              // scripts/<name>/main.lua（mqtt-north 角色）
    pub host: String, pub port: u16,
    pub username: Option<String>, pub password: Option<String>,  // 密码支持 secret: 引用（运行前解析）
    pub client_id: Option<String>,   // 缺省 hwid-<inst_id>
    pub batch_max_count: usize,      // 默认 50（Q5）
    pub batch_max_secs: u64,         // 默认 = period
    pub period: u64,                 // 收拢周期秒
    pub subscribe: Vec<String>,      // down() 订阅 topic 列表
    pub secrets: Vec<String>,        // R3 授权清单（Task 5 消费）
}
```

**Step 2: Sink impl 骨架**（生命周期骨架先行，契约回调 T7 填肉）：

```rust
#[async_trait]
impl Sink for LuaNorth {
    fn name(&self) -> &'static str { "luaNorth" }
    async fn run(&self, ctx, inst, mut rx, mut cmd_rx, token) {
        // select! 主循环：token / cmd_rx（"reload" → Evt::Reload 复用 VM 级热加载）
        //   / rx（样本入批缓冲——取舍 1：pipeline tap 未来接入零改动）
        //   / batch ticker（lux pull 收拢 rtdata → 组 batch → T7 up()）
        //   / mqtt 事件（connack/disconnect → T7 回调）
        // VM 经 lua_core::engine/script/api 组装；manifest dry-run 校验 @role mqtt-north
    }
}
```

**Step 3: 集成测试**（T1 的 ignore 测试启用）：lux 写 north 实例 → `try_start_from_lux` → Running → `/inst/stats` → stop。加能力包断言：`ctx` 表上**无** `request`/`transport`（C1 不注入 north，§5.2 矩阵）、有 `set_value/kv_get/secret_get`。
**Step 4:** Commit `feat(lua): luaNorth 薄壳——Sink 首个接线 + 实例全套（Q19/R4）`

### Task 7: mqtt-north 回调契约（auth/up/down/will/on_connack/on_disconnect + 混合批量）

**Files:**
- Modify: `src/plugin/builtin/lua_north/mod.rs`（契约填肉）
- Create: `src/plugin/builtin/lua_north/contract.rs`（回调调用薄包装：`call_auth/call_up/call_down/...`，全部包 `with_budget` 等价预算——照 lua_client 模式）

**Step 1: 失败测试**（contract.rs 单测 + mod.rs 集成，无需 broker：auth/batch/up 组装用 mock client 层断言调用序列）：
- **连接序列**：connect 前 `will()`（可选，出 LWT payload）→ `auth(cfg)` → 用返回值组 ConnectInfo（client_id/username/password/keepalive）；**重连必重调 auth()**（Q5：token 型刷新；断言断连重连序列中 auth 恰再调一次）；
- **up(batch)**：batch 元素 `{devid, values={tag=v}, ts}`（lux pull 组装）；返回数组逐条 `{topic,payload,qos}` → 逐条 publish，qos1 失败记 stat `publish_fail` 不中断（Q5）；返回非数组/空 → 记 warn 跳过本轮；
- **down(topic,payload)**：订阅 `subscribe` 列表，消息路由到 down()；down 内可调 `ctx:send_command`（兼容脚本依赖，取舍 2）；
- **on_connack/on_disconnect** 可选回调（globals 有则调）；
- **cfg 全局表**：NorthCfg 解析产物注入 globals（`auth(cfg)` 参数形态保留，Q5）；
- **预算**：任一回调挂起被 `callback_timeout_ms` 切断（复用 lua_core with_budget 语义——north 自带同名实现或下沉共用）。

**Step 2-4:** 实现 → 全绿 → commit `feat(lua): mqtt-north 回调契约——auth/up/down/will + 混合批量 + 重连重调 auth（Q5）`

### Task 8: MqttPublisher 实例化迁移（还旧账 + 无损迁移 + R5 单通道锁）

**Files:**
- Modify: `src/lib.rs:231-234`（删 MqttPub 直启；`cfg.mqtt` 显式配置时生成默认 luaNorth 实例）
- Create: `src/plugin/builtin/lua_north/compat.rs`（内置兼容脚本常量 + 默认实例构造）
- Delete: `src/plugin/builtin/mqtt_pub/`（整目录）
- Modify: `src/config.rs`（MqttConfig 文档注明「等价生成默认 luaNorth 实例」）

**Step 1: 内置兼容脚本**（`const COMPAT_MAIN_LUA: &str`，**行为逐字段对标 MqttPub**：topic `<hwid>/<devid>/datas`、payload JSON 形状、订阅 `<hwid>/command` → down() 解析 CtlMessage → `ctx:send_command`）：

```lua
-- @role mqtt-north
function up(batch)
    local out = {}
    for _, s in ipairs(batch) do
        for tag, v in pairs(s.values) do
            out[#out+1] = { topic = hwid.."/"..s.devid.."/datas",
                payload = json.encode({devid=s.devid, tag=tag, value=v, ts=s.ts}), qos = 0 }
        end
    end
    return out
end
function down(topic, payload)
    local m = json.decode(payload)
    if m.instance then ctx:send_command(m.instance, m.devid or "", m.cmd or "write", m.data or "") end
end
```

（`hwid` 经 cfg 表注入；MqttPub 的 payload 字段以现网实际为准——执行时逐字段核对 `mqtt_pub/mod.rs:116-122` 再写死，**兼容性是本 task 的验收线**。）

**Step 2: 失败测试**：
- `cfg.mqtt` 缺省（host 空）→ 不生成默认实例（零行为）；显式配置 → `try_start_from_lux` 前编排层生成 `inst_id="mqtt-pub-default"` 的 luaNorth 实例且 app_config 逐键等价（host/port/username/password/publish_interval_secs→batch_max_secs/period）；
- **R5 单通道锁**：同一配置只起一个发布通道——断言进程内 luaNorth sink 实例对同一 broker 配置唯一（重复启动被 InstanceManager 同 id 覆盖语义保护）；
- 删除 mqtt_pub 后 `rg mqtt_pub src/` 零残留、全量测试绿（metrics `mqtt_publish` 计数迁到 luaNorth stat extra）。

**Step 3-4:** 实现 → 全绿 → commit `feat(lua): MqttPublisher 实例化迁移——默认 luaNorth 实例无损迁移（R5 单通道锁，删旧代码）`

---

## 组 4：cloud/ 配方（Q8 双首发）

### Task 9: hashings 子集入官方源仓 + emqx 配方

**Files:**（源仓 `E:/Rs_Codes/rsiot-lualib`，非本仓；产物经 sync 在线分发）
- Create: `stdlib/algo/hashings/`（sha1/sha256/md5/hmac + 依赖闭包，源 `I:\code\freeioe\lualib\hashings/`，vendored 头 + selftest 已知向量）
- Create: `stdlib/cloud/emqx.lua`（薄配方：auth 返回 nil 直连 + topic 规划 helper）
- Modify: `dist/` 重建（`./gen_index.sh && ./sign_index.sh`）

**Step 1:** 搬运 hashings 子集——逐文件核对依赖闭包（hmac 依赖对应 hash 实现；不搬 whirlpool/blake2 等重型件）；每件 `selftest()`（NIST/RFC 已知向量：sha256("abc")=ba7816bf…、hmac_sha1 RFC2202 case 1…）。
**Step 2:** `cloud/emqx.lua`：EMQX 为裸 broker——`M.auth(cfg) return nil`（直连）+ `M.topic_data(hw,devid)` helper；selftest。
**Step 3:** 本仓集成测试（`lua_core/stdlib_sync.rs` tests 用注入 fetcher + dev key 加载源仓新 index）断言 `require("algo.hashings.sha256").selftest()` 过、`require("cloud.emqx")` 可加载。
**Step 4:** 两仓分别 commit（源仓 `feat: hashings 子集 + cloud/emqx 1.1.0`；主仓测试 commit）。

### Task 10: onenet 配方（token 签名）+ 端到端 + 文档

**Files:**
- Create: 源仓 `stdlib/cloud/onenet.lua`（token = urlsafe_base64(hmac_sha1(access_key, res.."\n"..et.."\n"..method))，version 2018-10-31 规范）
- Modify: `docs/guides/lua-script-guide.md`（cloud/ 配方节 + 兼容矩阵 STDLIB 轴 1.1.0）、`docs/reference/rest-api.md`（secret 端点补 T4）

**Step 1: 失败测试**——token 已知向量（固定 key/res/et，参考值用 Python `hmac` 库预生成写死在测试）；`up` 报文模板 topic/payload 形状断言。
**Step 2:** 实现配方 + 源仓 reindex/sign。
**Step 3: 端到端**（`#[ignore]` 手动，照 e2e_sync 模式）：本机 mosquitto/EMQX 容器 + OneNET 真实产品凭证（环境变量注入，**不落仓**）→ 默认脚本换 `cloud.onenet` 配方 → 断言平台上设备在线 + 数据点到达 + 下行命令闭环。CI 侧：mock broker（`local-broker` feature 起 rumqttd 进程内 broker）跑契约闭环（auth/up/down/LWT/重连全链路，不含 OneNET 平台）。
**Step 4:** Commit `feat(lua): cloud/onenet 配方 + EMQX/OneNET 端到端（Q8）`

---

## 组 5：MQTT 透传 backend（Q10，依 spike-2 结论）

### Task 11: io_engine mqtt backend

**Files:**
- Create: `src/plugin/lua_core/io_engine/mqtt.rs`（MqttIo backend）
- Modify: `src/plugin/lua_core/io_engine/mod.rs`（backend 构造分支）、`config.rs`（transport kind="mqtt" + req_topic/resp_topic）
- Test: `io_engine/mod.rs` tests + `tests/lua_udp_e2e.rs` 同级新 `lua_mqtt_pass_e2e.rs`

**Step 1: 失败测试**（形态依 Task 2 结论；方案 a 全双工为主干描述）：
- `request(frame)` = publish `req_topic(frame)` + 等待 `resp_topic` 首帧（EventLoop pump 分路到在飞 request 的 oneshot；超时弃订——上层 clamp 30s 已有）；
- `recv_idle()` = 无在飞 request 时 pump 分路到被动帧通道 → `on_data`；
- 断连重连后 request 自动恢复（backend 置 None → 懒重连模式照抄 TcpIo）；
- `ctx:transport()` 返回 `"mqtt"`（脚本帧格式无感，§5.4）。

**Step 2-4:** 实现（spike-2 的 pump/分路骨架直接转正）→ 全绿 + mock broker e2e（collector 脚本在 MQTT 链路上行为与 TCP 一致——验收标准）→ commit `feat(lua): io_engine mqtt 透传 backend（Q10，spike-2 方案 a）`

### Task 12: 阶段 B 收口——验收核对 + 文档同步

**Step 1: 验收清单核对**（对照设计路线图 B 行，逐项实测留证）：
- [ ] EMQX 环境契约闭环（auth/up/down/LWT/重连）——mock broker e2e + 真实 EMQX 手动
- [ ] OneNET 端到端（认证→上报→下行命令）
- [ ] 凭证不出 lux（secret 链路：掩码读/审计/越权报错测试全绿）
- [ ] 旧 mqtt 配置无损迁移（兼容脚本 payload 逐字段比对 MqttPub——T8 测试）
- [ ] `cargo test -p rs-iot --features lua,lua-http` 全绿 + clippy 零警告 + `--no-default-features --features lua` 全绿（CI minimal）
- [ ] `python scripts/check_docs.py` OK

**Step 2: 文档**——guide（mqtt-north 角色契约/cloud 配方/透传 backend/secret 使用）、rest-api.md（secret 三端点 + north 实例类型）、设计文档头部实施块与遗留清单更新、README 索引；release note（`ops/releases/`，含迁移告示：旧 `[mqtt]` 配置行为等价说明）。
**Step 3:** Commit `docs(lua): 阶段 B 收口——指南/参考/矩阵/发布说明`

---

## 验收清单（对照路线图阶段 B）

- [ ] luaNorth 走 InstanceManager 全套（多实例并存/启停/stat/热加载/VM 级 reload 回退）
- [ ] mqtt-north 契约六回调 + 混合批量 + 重连必重调 auth（Q5 全项）
- [ ] secret：写入/掩码读/审计/授权清单/越权报错（Q4 全项）
- [ ] EMQX 契约闭环 + OneNET 端到端（Q8）
- [ ] 旧 `[mqtt]` 配置无损迁移 + R5 单通道锁 + 旧代码删除
- [ ] MQTT 透传脚本行为与 TCP 一致（Q10，依 spike-2）
- [ ] CI 三组合全绿（default / minimal / clippy）

> C/D 阶段计划待 B 落地后按同法拆分（C：luaTransform/EventBus 带值/自环防护/on_timer/rhai 冻结；D：Sparkplug/alarm/脚本包）。

## 核查记录（Task 1 执行时回填）

**核查结论（2026-08-20，代码实读；file:line 以本 worktree 提交前为准）：**

- `start_sink`（`src/plugin/instance.rs:122`）全仓零调用者；签名要求调用方提供 `sample_rx`，lux/REST 启动路径均无法提供——缺口 A，已修复：去参 `start_sink(&self, inst)`，内部自建闲置 `mpsc::channel::<Sample>(64)`（tx 即 drop，rx 是 pipeline 未来 tap 的预留位，见取舍 1）。
- lux 自动启动（`src/lib.rs:363` `try_start_from_lux`，在 lib.rs 非 host.rs）：原条件 `inst.auto_start && inst.app_type == South && mgr.start_source(..)`——North autoStart 实例被**静默跳过**（不启动不报错不计数）。已改为按方向分发（South→`start_source` / North→`start_sink`）。
- REST 启动（`src/api/routes_sys.rs:101` `startinst`）：North 实例原被 400 "only south(source) instances can be started via REST" 显式拒绝，到不了 `start_source`。已改为同构分发；无插件时经统一错误路径返回 500 + `config error: sink plugin not found: ...`。
- REST 实例 CRUD 其余面（`src/api/routes_inst.rs`：set/get/list/roster/delete/purge）与运行态（stop/remove_state/ctl_send/inststatus、`/inst/stats`、状态 SSE）均按 inst_id 操作、不区分 app_type，`set` 可正常写入 North 配置（InstConfig 反序列化即通过）——**CRUD/状态/stat 对 Sink 实例天然覆盖，R4 唯一缺口在启动接线**（本 task 已补齐）。
- registry `SinkEntry`（`src/plugin/registry.rs:25`）当前无任何 `inventory::submit!` 注册（首个 sink = luaNorth，T6）。负向测试（`tests/lua_north_e2e.rs`，2 例）：InstanceManager 层 `start_sink` 无插件返回 `Config` 错误且运行态无残留；REST 层 startinst 对 North 分发到 `start_sink`（修复前 400 / 修复后 500 Config 错误，含 CRUD set 写入 North 配置放行）。lux 自动启动路径的 North 分发与 REST 同构，无插件时外部行为与"跳过"不可区分（不计数无残留），无法写行为区分断言——正向 Running/生命周期断言留 T6 luaNorth 注册后。
