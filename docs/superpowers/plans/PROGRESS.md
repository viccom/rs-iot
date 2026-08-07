# Lua 脚本采集 spec 实现进度（/loop 驱动）

> **运行时不变量**：CronCreate job `39632fab`（每 10m，session-only，7 天自动过期）。**轮内禁止 ScheduleWakeup**；完成/阻塞 → `CronDelete 39632fab`。每轮 cron 触发时读本文件 + plan checkbox + `git log --oneline -15` 重建上下文。

## 当前状态

- ✅ Plan 1-5（M1-M6：MVP / M4 联动持久 / M5 HTTP / on_data / M6 热加载）— 合并 master @ `cf92eee`（**111 测试全绿**）
- ✅ Plan 6（M7 状态监测 + REST）— 合并 master @ `26897bb`（fast-forward，**125 测试全绿**）
- ✅ Plan 7（M8a serial/udp 传输）plan 就绪 @ `c4ffbd0`（2026-08-07）
- 🔵 下一步：建 `feature/lua-m8a` 分支，执行 Plan 7 Task 1（config serial 参数）

## Plan 路线图（spec milestone + 欠账补全）

| Plan | 内容 | 状态 | plan 文件 |
|---|---|---|---|
| 1 | M1-M3 MVP | ✅ done @ master | `2026-08-06-lua-client-mvp.md` |
| 2 | M4（L3 send_command + L4 kv + I2 重连） | ✅ done @ master（`136e9a9`） | `2026-08-06-lua-client-m4.md` |
| 3 | M5 HTTP（L5 http_get/post + 审计） | ✅ done @ master（`6ff4ff0`） | `2026-08-07-lua-client-m5.md` |
| 4 | on_data 被动帧（M2 欠账） | ✅ done @ master（`797545d`） | `2026-08-07-lua-client-on-data.md` |
| 5 | M6 热加载（notify + VM 重建 + dry-run + 失败回退） | ✅ done @ master（`cf92eee`，2026-08-07 合并） | `2026-08-07-lua-client-m6.md` |
| 6 | M7 状态监测（`stat:<inst>`）+ REST（`routes_lua` stats/reload） | ✅ done @ master（`26897bb`，125 测试绿） | `2026-08-07-lua-client-m7.md` |
| 7 | M8a serial/udp 传输（SerialIo/UdpIo + config serial 参数） | 🔵 plan 就绪 @ `c4ffbd0`，待执行 | `2026-08-07-lua-client-m8.md` |
| 8 | M8b L2 数据 API（set_value/get_value/get_device/list_devices）+ 文档 + 示例 | ⏭ 待写 plan | — |

## Plan 6（M7 状态监测 + REST）完成总结 ✅（合并 master @ `26897bb`，2026-08-07）

- [x] Task 1：`db/luastat.rs`（LuaStat + write/read，lux `stat:<inst>` SET/GET JSON）+ identity 清单（`f5d25bc`，3 单测）
- [x] Task 2：`lua_client/config.rs` `fail_threshold`（连续失败标 degraded，默认 10）（`0363912`，5 单测）
- [x] Task 3：`lua_client/mod.rs` run() stat 采点（启动 running / Tick 计数+耗时 / Reload vm_restart+1 / 退出 stopped）+ `script_version`（main.lua sha256 前 8 hex）（`8ba9b4c`，lua_client 26 单测）
- [x] Task 4：`api/routes_lua.rs` stats/reload/scripts（query 参数，JWT 保护）（`ffde6e1`，2 单测）
- [x] Task 5：集成测 `lua_stat_e2e`（采集+stopped）+ `lua_api_e2e`（REST stats/reload/scripts + JWT，5 测）（`adfb69a`）

**125 测试全绿**（lib 99 = 既有 92 + M7 +7；集成测 26 = 既有 19 + M7 +7）。spec §13 状态监测（`stat:<inst>` JSON）+ §4 routes_lua（stats/reload/scripts）覆盖。
**v1 简化（plan 注明）**：io_connected/last_io_err 后置（io_engine 改造）；REST reload = stop+start（IO 重连）；script_version = main.lua 单文件 sha256；stat 按 collect 周期计。

## Plan 5（M6 热加载）完成总结 ✅（合并 master @ `cf92eee`）

- [x] Task 1：Cargo notify 8 + notify-debouncer-mini 0.7（`c079aca`）
- [x] Task 2：Evt 枚举 + 主循环 Evt 重构 + `reload_vm`（new_vm+load+register+dry-run on_init）+ Evt::Reload 替换 lua（`25efc3b`）
- [x] Task 3：`hotload.rs` notify watcher（去抖 200ms → reload_tx）（`4d2fb62`；**修复** std::thread 替 spawn_blocking 避免 runtime shutdown 卡）
- [x] Task 4：热加载集成测（改脚本 v1→v2 → reload → 新 collect 值）（`cf92eee`）

111 测试全绿（lib 92 + 集成测 19）。spec §5.3 热加载覆盖（notify + VM 重建 + dry-run + 失败回退「永不半生效」）。

## 下一步（每轮 cron 触发执行）

**执行 Plan 7（M8a serial/udp 传输）**：建 `feature/lua-m8a` 分支（从 master `c4ffbd0`），subagent-driven 逐 task（API 503 时控制器接手）。Plan `2026-08-07-lua-client-m8.md` @ `c4ffbd0`，5 task：
- 🔵 Task 1：`config.rs` serial 参数（baud_rate/data_bits/parity/stop_bits/flow_control + Parity/FlowControl enum）。（下一步）
- ⏳ Task 2：`io_engine.rs` SerialIo（tokio-serial，IoBackend impl，单测 `#[ignore]` 需真串口）。
- ⏳ Task 3：`io_engine.rs` UdpIo（connected UdpSocket，datagram 边界）+ mock udp 单测。
- ⏳ Task 4：`mod.rs` backend 选择接 Serial/Udp（去 MockIoTask 占位）。
- ⏳ Task 5：`tests/lua_udp_e2e.rs`（mock udp server → collect → rtdata）。

**M8 拆分**：Plan 7（M8a serial/udp，本计划）+ Plan 8（M8b L2 数据 API + 文档 + 示例）。两者独立可测。Plan 8 完成后 spec 全 milestone 实现 → CronDelete `39632fab` 收尾。

## 已知简化 / 风险（累计，注明）

- Plan 5 dry-run = on_init only；notify Windows 事件丢失（去抖 + M7 REST reload 兜底）；VM 重建不重启 IO；watcher std::thread
- Plan 4 on_data 混合协议竞争丢帧（纯被动不受影响）
- Plan 3 http：reqwest Client per-call / bytes() 后 check 上限 / http_timeout_secs config 未接
- mod.rs reload channel 注释 _reload_tx 过时待清（Plan 5 Task 3 遗留）

## 接手模式说明（API 503 期间）

API 503 间歇故障致 subagent dispatch 不稳。Plan 2 起控制器接手机械 task（plan 含完整代码 + cargo 不依赖 LLM API）。独立 reviewer 暂跳过。API 恢复后恢复 subagent-driven 双 review。

## 后续清理项（跨模块，非阻断）

- `luakv::get` 空 JSON 值误判 missing（与 `ext_token::get` 同根）
- `RsIotError::Config` 用于运行时校验语义勉强（待 `Validation` 变体）
- in-crate 单测 data_dir 按 pid 共享
- M5 http 简化（per-call Client / 流式 limit / config 字段）
- on_data 混合协议竞争丢帧
- mod.rs reload channel 注释 _reload_tx 过时待清

## CI 命令（Windows，规避 link.exe 并行 OOM）

```bash
CARGO_BUILD_JOBS=1 cargo test --all-targets --all-features
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
```

## 阻塞点

- ⚠️ API 503 间歇故障 → subagent dispatch 不稳，控制器接手中。无逻辑阻塞。
