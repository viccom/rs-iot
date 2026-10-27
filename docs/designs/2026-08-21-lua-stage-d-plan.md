# Lua 脚本系统演进 · 阶段 D（按 2026-08-21 评估收窄）实施计划

> 日期：2026-08-21
> **状态**：implemented（2026-08-21 执行完毕，worktree `feat/lua-stage-d`（`38a37e4..`，基于 master `c82c86f`），9 task + T3/T4 间 fmt 收口（`187e740`），已合并 master）
> **实施**：T1 `2af6f61`（M3 实例回显遮蔽 + set 回写恢复）/ T2 `5db2fd8`（NorthCfg TLS 三键 + Transport 组装）/ T3 `72b0b94`（EventBus Alarm 变体 + lux alarms Hash + REST）/ T4 `e979173`（ctx:fire_event/clear_event：跨角色 + 20/s 频控 + 同 key 1s 抖动）/ T5 `b74de2a`（SSE alarm 接线 + 前端活跃告警面板 + 指南告警节）/ T6 `60fa416`（清偿批：north P3 四项 + tag helper 收拢 + flaky 加固）/ T7 `f03b144`（serial open spawn_blocking + 5s 限时 + 真机 3 项）/ T8 `682486c`（upstream 发布就绪注记 + 演练留证；源仓配套 commit 3235758）/ T9 本 commit（验收核对 + 文档同步 + 回归）
> **代码**：`src/api/routes_inst.rs`（redact_inst/redact_inst_map/restore_masked_password）+ `src/plugin/builtin/lua_north/{config,contract}.rs`（tls/ca_path/insecure_tls + build_transport/NoVerify）+ `src/event_bus.rs` + `src/db/alarms.rs` + `src/api/routes_alarm.rs` + `src/plugin/lua_core/api/mod.rs`（fire_event/clear_event + AlarmStats）+ `frontend/src/{types.ts,stores/monitor.ts,components/monitor/AlarmPanel.vue,views/Monitor.vue}` + `src/plugin/lua_core/io_engine/serial.rs`（open 走 spawn_blocking+timeout）；源仓 `E:/Rs_Codes/rsiot-lualib`（release.yml 硬化 + docs/PUBLISH.md）
> **测试**：T9 三组合全量回归实测（数字见文末「执行核对」）——default 组合：lib **420 通过 / 9 ignored**[需外部资源] + 30 个测试目标全绿；minimal 组合（`--no-default-features --features lua`）：lib **337 通过 / 5 ignored** + 30 目标全绿（回归中发现并修复 T4 遗留 bug：`ratelimit` 模块 lua-http 门控致 minimal 编译失败 E0432，解除门控——纯 std 通用件本就该随 lua 编译）；clippy（default+显式 lua,lua-http 双查）零警告；fmt 零 diff；check_docs 通过；前端 vitest **101 通过**（7 文件）+ build 过
> **遗留**：按需挂起清单原样保留（Sparkplug Q9/脚本包 Q14/模板安装器 Q18/M4+M5 与 send_command R3 收紧/alarm 角色独立壳/OneNET 现场验收——触发条件见演进设计遗留清单）；TLS 真 broker 真机 e2e 未建（无本机 TLS broker，留证形态为组装断言单测 9 项 + 明文反向锁）；upstream 正式发布属用户检查点（正式钥/Pages/首版 tag，按源仓 PUBLISH.md 执行）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 安全最小集（M3 密码回显遮蔽 + NorthCfg TLS）+ 告警最小闭环（Q12，独立 spec `2026-08-21-alarm-minimal-loop-design.md`）+ 顺带清偿（B 复审 P3 四项 + C 杂项）+ 串口 open 异步安全 + 源仓 upstream 发布准备。

**Architecture:** 告警不建第四角色薄壳（spec §0 简化定案）——`ctx:fire_event/clear_event` 跨角色注入，宿主管 lux `alarms` Hash + EventBus `Alarm` 变体 + REST/SSE；安全最小集是两处小修；串口仅改 open 路径（`spawn_blocking` + 限时，流读写保持 SerialStream 不动）；upstream 发布是源仓（E:/Rs_Codes/rsiot-lualib）流程件，正式钥与 Pages 发布留用户检查点。

**Tech Stack:** lux EmbeddedClient HSET/HGETALL/HDEL（照 secrets 模式）、tokio broadcast（EventBus）、rumqttc TLS（`set_transport(Transport::tls)` + CA/域名参数）、`tokio::task::spawn_blocking`（serial open）。

**执行前置：** 跨 ≥5 文件 ≥10 commit，**独立 worktree 执行**（`git worktree add ../rs-iot-lua-d -b feat/lua-stage-d`）。

**约定：** `cargo test -p rs-iot --features lua,lua-http -j 4`（**必须 -j 4**，本机页面文件限制）；每 Task 末 commit；TDD 先红后绿。

---

## 组 0：安全最小集（必须做的投产门槛）

### Task 1: M3——实例 REST 回显遮蔽字面 password

**Files:**
- Modify: `src/api/routes_inst.rs`（实例 GET 回显路径——定位 `app_config` 原样序列化点）
- Test: 同文件或 tests/ 追加

**Step 1: 失败测试**——north 实例 app_config 含字面 `password` → GET 实例配置返回中该字段被遮蔽（`"****"` 或移除——**采用遮蔽**，与 `/config` 端点既有遮蔽行为对齐）；secret 引用形 `secret:xxx` 不遮蔽（非明文）；其余字段原样。注意：遮蔽读回的配置**不可直接回写**（保存时若 password 为遮蔽值则保留原值——看 routes_sys /config 遮蔽的既有处理模式照抄；若实例 set 无此保护则一并补）。
**Step 2-4:** 实现 → 全绿 → commit `fix(security): 复审 M3——实例 REST 回显遮蔽字面 password（与 /config 双标消除）`

### Task 2: NorthCfg TLS 选项（凭证明文过网收口）

**Files:**
- Modify: `src/plugin/builtin/lua_north/config.rs`（`tls: bool` 缺省 false、`ca_path: Option<String>`、`insecure_tls: bool` 缺省 false——三键）
- Modify: `src/plugin/builtin/lua_north/contract.rs`（connect 组装处：tls=true → `opts.set_transport(Transport::tls_with_ca(ca_path))` / 无 ca 用 `Transport::tls`；insecure_tls 仅在显式开启时 `set_insecure`——日志 warn）
- Test: config 解析单测 + connect 组装单测（MqttOptions 的 transport 断言可从 `MqttOptions` API 读回）

**Step 1: 失败测试**——`{"tls": true}` 解析 + 组装后 transport 为 tls；`tls+ca_path` 用文件 CA；缺省组合零行为变化（非 tls 实例报文形状不变）。
**Step 2-4:** 实现 → 全绿 → commit `feat(lua): NorthCfg TLS 选项（H1 收口——凭证明文过网，CA/insecure 显式）`
**注记：** broker ACL 与 `<hwid>/command` 控制链收紧**不在本 task**（部署形态决策 + 兼容脚本行为变更，挂按需清单）。

---

## 组 1：告警最小闭环（Q12，依独立 spec）

### Task 3: EventBus Alarm 变体 + lux alarms Hash + REST

**Files:**
- Modify: `src/event_bus.rs`（`AlarmAction` enum + `Alarm` 变体 + kind "alarm"）
- Create: `src/db/alarms.rs`（fire：read-modify-write count++ / clear 幂等 / list 倒序——照 secrets.rs 模式）
- Create: `src/api/routes_alarm.rs`（GET /alarms + DELETE /alarms?key=，JWT，照 routes_secret 模式）
- Modify: `src/api/mod.rs`（路由注册）、`src/db/mod.rs`
- Test: event_bus/db/routes 单测 + e2e（JWT 401/清单形状/DELETE 后 SSE Clear 到）

**Step 1: 失败测试** → **Step 2-4:** 实现 → 全绿 → commit `feat(alarm): EventBus Alarm 变体 + lux alarms Hash + REST 查询/清除（Q12 宿主面）`

### Task 4: ctx:fire_event/clear_event（跨角色 C6 + 频控 + 同 key 抖动）

**Files:**
- Modify: `src/plugin/lua_core/api/mod.rs`（两函数注册——fire：校验（key ≤128/level 三档/message ≤512）→ 频控（token bucket 20/s，照 http R2 模式）→ lux 覆盖写 count++ → 同 key 1s 抖动决定是否 publish；clear：HDEL + Clear 事件恒发）
- Modify: `src/plugin/lua_core/ratelimit.rs` 复用或微调（若现 bucket 是 http 专用，抽出通用）
- Test: `src/plugin/lua_core/api/tests.rs`（三角色注入矩阵锁/校验拒绝/频控 false+计数/抖动 count 不重发/clear 幂等恒发事件/source 宿主注入不可伪造）

**Step 1: 失败测试** → **Step 2-4:** 实现 → 全绿 → commit `feat(alarm): ctx:fire_event/clear_event——跨角色注入 + 20/s 频控 + 同 key 抖动（Q12 脚本面）`

### Task 5: SSE 接线 + 前端告警面板 + 指南

**Files:**
- Modify: `frontend/src/types.ts`（AlarmEvent/AlarmItem）、`frontend/src/stores/monitor.ts`（alarm 监听 + alarms ref + loadAlarms）、监控页告警面板（level 徽标/key/来源/次数/时间/手动清除——复用现有面板样式）
- Modify: `docs/guides/lua-script-guide.md`（告警节：API/判定示例片段/频控/与 transform 配合示例）
- Test: monitor.test.ts（fire upsert/clear remove/缺事件兜底拉取）

**Step 1: vitest 红** → **Step 2-4:** 实现（`npm run build` 过）→ 全绿 → commit `feat(alarm): SSE alarm 事件 + 前端活跃告警面板 + 脚本指南（Q12 闭环）`

---

## 组 2：顺带清偿（B 复审 P3 四项 + C 杂项）

### Task 6: north 四项 + lua_core tag helper + 测试杂项

**Files/内容（全部小项，一个 commit）:**
- `lua_north/contract.rs`：① PubResult 语义 doc 注明（ok/fail 计数口径——异步回执不阻塞 up）；② down 回调失败计 stat（`down_skipped` extra，照 rtdata_skipped 模式）；③ will() 重连重调契约 doc 注明（每次 connect 前调，幂等要求）；④ lux 读取错误的 Fatal 归类核对（auth secret 解析处 lux 错误→Fatal 的注释与行为对齐——实读后按需微调）。
- `lua_core`：tag 剥前缀 helper（`strip_dev_prefix(devid, tag) -> &str`）收拢 4 处（transform shell/api self_write_keys/get_device/north pull）。
- `tests/common/mod.rs`：`stop_lux` doc 修正（去「清理 data_dir」的不实表述或补实现——取 doc 修正，不动行为）。
- 存量 flaky 两处加固：`routes_secret` / `lua_client ctl_reload`（读测试定位竞态——轮询窗口放宽/断言改容忍式，不改被测行为）。

**Step 1:** 逐项红/绿或纯文档核对 → **Step 2:** 全量绿 + flaky 项连跑 3 次观察 → commit `fix(lua): 清偿批——north P3 四项 + tag helper 收拢 + 测试杂项加固`

---

## 组 3：串口 open 异步安全（专项收口）

### Task 7: serial open 走 spawn_blocking + 限时

**Files:**
- Modify: `src/plugin/lua_core/io_engine/serial.rs`（`ensure_connected` 内 `builder.open_native_async()` 改为把 builder 参数打包进 `tokio::task::spawn_blocking`（Windows 驱动异常时 open 阻塞在系统调用——异步包装不救系统调用级阻塞）+ 外包 `tokio::time::timeout`（5s——open 超时即 Err 置 None，下轮 request 再试，**不再卡死 IO task/占住 COM 口**）；流读写保持 SerialStream 不动）
- Test: 单测（open 失败路径照旧）+ **真机验证**：本机 com0com 对（COM3/COM4）——① 正常对端收发回归；② open 不存在的 COM 号 → 5s 超时报错不卡死（stop 后无残留句柄——Windows 下可用再次 open 同口成功佐证释放）；③ 对端被第三方进程独占 → 超时路径同②。真机项写成 `#[ignore]` 测试（`RSIOT_SERIAL_E2E=1` 环境变量门控，照 north 真机 e2e 模式），现场手册补验证步骤。

**Step 1: 失败测试（超时路径单测可 mock：非阻塞验证 timeout 包装存在性 + open 错误传播）** → **Step 2-4:** 实现 → 全绿 + 真机 3 项手动跑通留证 → commit `fix(lua): serial open spawn_blocking+限时——驱动级阻塞不再冻结 IO task/占 COM 口`

---

## 组 4：源仓 upstream 发布准备（流程件，正式钥留用户检查点）

### Task 8: rsiot-lualib 发布就绪（跨仓 E:/Rs_Codes/rsiot-lualib）

**Files（源仓）:**
- Verify/Harden: `.github/workflows/release.yml`（tag push → gen_index → sign → Pages/Release attach；`ED25519_KEY` secret 位）
- Verify: `gen_index.sh`/`sign_index.sh`（含 `min_api_ver`/`min_host_ver` 字段完整性——对照主仓 sync 预检消费的字段）
- Create: `docs/PUBLISH.md`（发布 runbook：tag → CI → Pages 生效 → 主仓 sync 验证；**正式钥生成与保管流程**（Q17：CI secret 保管制 + 组织双审批）+ 首次发布后主仓 `VERIFY_KEYS` 公钥回填步骤）
- 主仓侧：`src/config.rs` upstream 占位注释补「正式发布后删占位标记」（不改行为）

**Step:** 源仓本地全链演练（dev 钥 sign → 本地静态目录 → 主仓 sync 验签通过——复用 A 阶段 T13 的 python http.server 手法）→ 两仓分别 commit（源仓 `chore: 发布就绪——release CI 硬化 + PUBLISH runbook`；主仓 `docs(lua): upstream 发布就绪注记`）。
**用户检查点（不代办）：** 正式 ed25519 钥生成（组织流程）、GitHub 仓公开/Pages 开启、首版 tag 发布。完成后按 PUBLISH.md 回填公钥。

> **执行回填（2026-08-20，Task 8 完成）：**
>
> - **release.yml 硬化**（源仓已有基础版，按计划补全）：新增 `release` job（dist zip + index.json/.sig attach GitHub Release，tag-only，`contents: write`）与 `verify-online` job（发布后 curl 线上 Pages 拉 index → 仓内 pub.key 验签 + 版本核对，Pages 未传播 10×30s 重试，fail closed）；私钥还原 step 补 secret 缺失/空值 fail-fast；`verify_index.sh` 加目录参数（缺省 dist 不变，供在线自检复用）。YAML/脚本语法校验通过。
> - **index 字段核对**：主仓 `stdlib_sync.rs::LocalIndex` 消费 `version`/`min_api_ver`(default 1)/`files{sha256,ver}`——gen_index.sh 三者齐备；`min_host_ver` 演进设计 §3.3 提及但主仓实现**无消费者**，不生成（结论与依据写入 PUBLISH.md §2，将来要加须双侧同步）。
> - **PUBLISH.md**（源仓 `docs/PUBLISH.md` 新建）：用户检查点置顶 + 发布全链 runbook + 正式钥双审批/CI secret 保管制责任边界 + Q17 双钥过渡与 dev 钥 v1 退役步骤 + 镜像（wget --mirror/rclone，§3.6 D2）+ 版本策略（STDLIB_VER MAJOR.MINOR 轴 + CHANGELOG 要求）+ 本地演练手法。
> - **本地全链演练（dev 钥）真实输出**：
>   - 源仓：`./gen_index.sh 1.3.0 && ./sign_index.sh && ./verify_index.sh` → `gen_index: 1.3.0，15 个文件 → dist/`、`sign_index: dist/index.json.sig（64 字节裸 ed25519）`、`Signature Verified Successfully`；index 字段实查 `version=1.3.0 / min_api_ver=1 / files=15`。
>   - 在线自检路径（CI verify-online 等价）：curl 8765 拉 index.json/.sig（200, 2241B/64B）→ `./verify_index.sh /tmp/online-check` → `Signature Verified Successfully`。
>   - 主仓真 HTTP e2e（生产 VERIFY_KEYS）：`RSIOT_E2E_UPSTREAM=http://127.0.0.1:8765 cargo test -p rs-iot --features lua,lua-http -j 4 --lib -- --ignored e2e_sync` → `e2e_sync_official_dist_over_http ... ok (1 passed)`——内置断言：验签通过、1.3.0 全量 15 文件落 `_stdlib/`、清单（stdlib.list 背后 inventory）ver=1.3.0 + crc16 徽标 synced、require 取同步版且 selftest 过。
>   - 篡改场景（fail-closed 证据）：`printf '\n-- tampered' >> dist/algo/crc16.lua` 重跑 → `FAILED ... sha256 不符：index 声明 83bf11cc…，实际 f6308e83…（内容被篡改，拒装）`；随后 dist 已重建自验通过。
> - **主仓侧**：`src/config.rs` `lua_stdlib_upstream` 注释补「正式发布后此处改正式 URL（rsiot-lualib docs/PUBLISH.md runbook）」（一行注释，零行为变化）。

---

## 组 5：收口

### Task 9: D 阶段验收核对 + 文档同步 + 回归

**Step 1: 验收核对（对照本计划逐项实测留证，2026-08-21）：**
- [x] M3：实例回显不再泄漏字面 password——单测 7 项全过（`redact_inst_masks_literal_password`（get 面）/ `redact_inst_keeps_secret_ref` / `redact_inst_noop_without_password` / `redact_inst_map_masks_each_entry`（list+roster 两端点共用的 map 面）/ `restore_masked_password_recovers_from_store` / `_removes_when_no_original` / `_passes_non_sentinel_through`（set 回写恢复三态）——`cargo test --lib -- routes_inst::tests::redact_inst routes_inst::tests::restore_masked` → **7 passed, 0.00s**）
- [x] TLS：真 TLS broker 无本机条件（无 mosquitto-TLS/EMQX）——按计划以组装断言留证：解析 3 项（`parse_tls_defaults_off`/`parse_tls_overrides`/`parse_tls_rejects_bad_types`）+ 组装 5 项（`transport_default_tcp_zero_behavior_change`（**明文反向锁**：缺省仍 Tcp 且经 set_transport 读回）/ `transport_tls_with_ca_file_bytes` / `transport_tls_without_ca_uses_system_roots` / `transport_tls_insecure_shape` / `transport_ca_read_failure_errors`）+ `no_verify_accepts_everything`（跳过验证语义锁）→ **9 passed, 0.00s**。注记：T2 未另建 `#[ignore]` 真机 TLS 测试（组装锁已覆盖 transport 形态三变体 + 错误路径，真机项需先具备本机 TLS broker，挂按需——部署时按 release note TLS 配置示例现场验收）
- [x] 告警全链：①e2e `tests/alarm_e2e.rs` 4 项（`alarms_without_jwt_401`/`alarms_list_empty_returns_empty_array`/`alarms_list_shape_and_ts_desc`/`alarm_delete_clears_list_and_publishes_sse_clear`）→ **4 passed, 2.02s**（REST/SSE/幂等）；②脚本面 `lua_core/api/tests.rs` 5 项（`alarm_api_injected_for_all_roles`（三角色注入矩阵）/`fire_event_validates_args`（校验拒绝）/`fire_event_rate_limited_returns_false`（频控 20/s）/`fire_event_dedupes_same_key_sse`（同 key 1s 抖动 count 不重发）/`clear_event_idempotent_always_publishes`（幂等恒发））→ **5 passed, 0.24s**；③前端 vitest `monitor.test.ts` 5 项（`start 订 alarm kind，fire 帧直入 alarms`/`onAlarm 语义：同键覆盖保 count 置顶/clear 移除/未知 key 无害`/`loadAlarms 全量替换`/`SSE 重连 onopen 补拉 alarms`/`clearAlarm DELETE 不本地移除`）→ **5 passed**（全量 28 项同文件）
- [x] 清偿批：north P3 四项（PubResult 语义 doc/down 失败计数 will 重调契约/lux Fatal 归类）+ tag helper `strip_dev_prefix` 收拢 4 处随 `60fa416` 全绿；flaky 两项 5 连跑实测——`routes_secret::tests::list_masked_never_echoes_plaintext` → **5×1 passed**（0.02-0.03s）；`lua_client::tests::ctl_reload_message_triggers_vm_reload` → **5×1 passed**（1.28-1.29s）
- [x] 串口：真机 3 项（com0com COM3/COM4，`RSIOT_SERIAL_E2E=COM3,COM4 ... io_engine::serial -- --ignored`）→ `serial_e2e_pair_roundtrip_and_handle_release`（正常收发+句柄释放）/ `serial_e2e_missing_port_errors_and_io_task_alive`（不存在口）/ `serial_e2e_occupied_port_no_wedge_and_no_handle_leak`（被占口）**3 passed, 0.40s**
- [x] upstream：本地全链演练 + 篡改负向已留证（Task 8 执行回填块，commit `682486c`）——dev 钥 gen_index 1.3.0/15 文件 + sign 64B + verify 通过；主仓真 HTTP e2e `e2e_sync_official_dist_over_http` 1 passed（验签/落盘/清单 synced/require 同步版）；篡改 crc16.lua 1 字节 → sha256 不符拒装（fail-closed）
**Step 2: 文档**——演进设计头部实施块 + v0.8 行 + 遗留清单勾销（M3/H1-TLS/Q12/串口/upstream 准备；按需清单保留触发条件）；docs/README 索引；release note `2026-08-21-lua-stage-d.md`（含 TLS 配置示例与告警 API 告示）。
**Step 3:** 三组合全量回归（default + minimal + clippy/fmt + check_docs + 前端 build）→ commit `docs(lua): 阶段 D 收口——验收核对/发布说明/遗留勾销`

---

## 验收清单（2026-08-21 逐项实测勾销）

- [x] 安全最小集：M3 遮蔽 + TLS 选项（T1/T2）——单测 7+9 项实测全过（见 Step 1）；真 TLS broker 留证形态注记如上
- [x] 告警最小闭环：宿主面/脚本面/前端面（T3/T4/T5）——e2e 4 + 脚本面 5 + vitest 5 实测全过（见 Step 1）
- [x] 清偿批：north P3 四项 + tag helper + flaky 加固（T6）——`60fa416` 全绿 + flaky 2 项 5 连跑实测（见 Step 1）
- [x] 串口 open 异步安全 + 真机 3 项（T7）——`f03b144` + com0com 实测 3 passed 0.40s（见 Step 1）
- [x] 源仓发布就绪 + 本地演练（T8；正式发布属用户检查点）——演练+篡改负向留证（Task 8 回填块）
- [x] 三组合全绿（T9）——实测数字见文末「执行核对」

> **按需挂起清单（2026-08-21 评估定案，D 收口复核：原样保留未动）**：Sparkplug Q9 / 脚本包 Q14 / 模板安装器 Q18（绑定 upstream 正式发布时收尾）/ M4+M5 与 send_command R3 收紧（安全二期，部署形态决策）/ alarm 角色独立壳 / OneNET 真平台验收（等现场凭证）。

---

## 执行核对（T9，2026-08-21）

三组合全量回归真实输出（check_docs 在文档状态字全部翻面后执行）：

- 组合一 `cargo test -p rs-iot --features lua,lua-http -j 4`：**全绿**——lib `420 passed; 0 failed; 9 ignored`（5 存量[需外部资源] + 3 串口真机 + 1 upstream e2e）+ main 0 + 28 个集成测试目标 + Doc-tests 全部 `test result: ok`（合计 31 行，0 failed）
- 组合二 `cargo test -p rs-iot --no-default-features --features lua -j 4`：**全绿**——lib `337 passed; 0 failed; 5 ignored` + 30 个运行目标全部 `test result: ok`（0 failed）。**回归产出**：首跑编译失败 `E0432`（`api/alarm.rs` 引用 `ratelimit`，该模块被 `#[cfg(feature = "lua-http")]` 门控——T4 复用 http 频控件时漏查 feature 面）；修复=解除门控（`lua_core/mod.rs`，ratelimit 纯 std 无 http 依赖，注释同步改「双消费者」）后全绿
- `cargo clippy --all-targets`（默认 feature）与 `cargo clippy --all-targets --features lua,lua-http`：**均零警告**；`cargo fmt --all --check`：**零 diff**
- `python scripts/check_docs.py`：**通过**（0 违规；首轮报 alarm spec 缺 `**实施**` 行，补齐后过）
- `cd frontend && npx vitest run`：**101 passed / 7 文件，0 failed**（2.42s）；`npm run build`：**通过**（vite built in 3.48s，仅存量 chunk>500kB 提示非错误）
