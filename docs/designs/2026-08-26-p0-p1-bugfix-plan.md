# P0+P1 九项 BUG 修复计划

> **状态**：implemented（2026-08-26 收口；P0×1 + P1×8 全修）
> **日期**：2026-08-26
> **来源**：审查修复包收口后的遗留评估（会话裁决：P0×1 + P1×8）；Low 其余项维持 backlog 登记
> **执行方式**：主会话直做——子代理额度两次中断先例 + 单项均 ≤60 行；TDD 除登记豁免项外全红→绿

## 验收记录（2026-08-26，红→绿证据见各 commit）

| 项 | commit | 锚点 |
|---|---|---|
| T1 P0 lua minimal 门控 | `d6740a3` | minimal 18+2红→20/20；默认 23/23（门控断言仍跑） |
| T2 cache 毒化恢复 | `638a308` | 红（read().unwrap 毒化 panic）→绿；cache 5/5 |
| T3 回卷双点（复核证实→修） | `31a803f` | 复核测试实证 t5→t3→t6→t3 盘上 t3 **双点**（max 方案证伪）→ 严格更旧丢弃 stale_ts_dropped；新语义锁定测试 + 治理面十四计数器/19 family + rest-api.md 声明 |
| T4+T5 e2e 断言强化 | `ea8d0bd` | 恒真→禁数据帧；反压 contains→JSON 统计 ≥9 台；init_failed 补 /metrics 省略断言；subscribe 11/11 + init_failed 1/1 |
| T8 hgetall warn | `6b5286f` | 测试豁免登记（需 mock lux client） |
| T6 前端 500 透传 | `e48235d` | 红（error 串不含）→绿；197 测 + build |
| T7 index 派生 | `c340dea` | 等价守门测试（D-F 预期即绿）；rows 9/9 |

**门禁**（全部实测）：fmt --check 过；双档 clippy 零警告；默认档全绿；minimal 档 **357/0 恢复全绿**（T1 兑现）；history 档全量两轮——首轮 `model_registry_e2e::discovery_list_sorted_by_count_and_filtered_by_dev` 1 失败，**复跑零失败 + 单测隔离 5 连绿**：计时型 count 断言的全量并发 flake（model/discovery 本包零触碰，归因成立登记）；前端 197 测 + build 过。

**Goal**: 修复 minimal 档门禁红（P0）与 8 项高确定性小修（P1），一个分支收口合并。

## 〇、复核中发现的关键事实（影响 T3 设计，必须先读）

1. **dispatcher 按 devid hash 路由**（pipeline.rs:1 头注）：同 series 样本固定走同一 worker——审查 L-1 的"乱序流"窗口实际只剩 Source 端乱序（重连重传/时钟跳变），比审查评估窄。
2. **审查建议的 `last_ts` 取 max 修法经推演不能闭合所述双点**：场景 t5→t3→t6→t3（跨批回卷），max 后 last_ts=t6，末笔 t3 仍 ≠ t6 照样双写；且对"重复 t3"场景 max 反而比现状更差（现状等值捕获第二笔，max 后两笔都过）。**唯一轻量闭合方案 = 严格更旧丢弃**（语义变更，见 D-T3）。

## 一、任务清单

### T1（P0）routes_lua minimal 档 2 测门控

**文件**：`src/api/routes_lua.rs`（仅测试，:802 与 :1070-1075）
**根因**：`is_reserved_lib_top` 在 lua feature 关闭时恒 false（routes_lua.rs:346 注释明示设计——require 路由层缺席，保留段语义无从谈起），而测试的 `"algo/evil.lua"`（撞保留段 `algo`）拒绝断言未按 feature 门控。`foo.lua`（单段）与 `../evil.lua`（穿越）两拒绝与 feature 无关（minimal 档已过）。
**修法**：
- `asset_read_path_script_and_lib` 的 `assert!(asset_read_path(Some("lib"), "algo/evil.lua").is_err());` 前置语句级 `#[cfg(feature = "lua")]` + 行注释「保留段拒绝仅 lua feature 开（is_reserved_lib_top 关档 false 设计，routes_lua.rs 注释）」。
- `lib_save_rejects_invalid_names` 的 bad 列表拆为 `["foo.lua", "../evil.lua"]` 循环 + `#[cfg(feature = "lua")]` 单独断言 `save_lib_under(&tmp, "algo/evil.lua", "x").is_err()`。
**红**（已有实测）：`cargo test -j 2 --no-default-features --lib routes_lua::` → `asset_read_path_script_and_lib`/`lib_save_rejects_invalid_names` 2 FAILED（`assertion failed: asset_read_path(Some("lib"), "algo/evil.lua").is_err()`）。
**绿**：同命令 20/20 过 + `cargo test -j 2 --lib routes_lua::`（默认档）零回归。
**Commit**: `fix(lua): minimal 档保留段拒绝断言按 feature 门控——is_reserved_lib_top 关档恒 false 系设计(注释明示),algo/evil.lua 两断言 cfg(lua)(P0)`

### T2（P1）cache RwLock 毒化读不崩

**文件**：`src/model/cache.rs:25`（effective 读）与 :34（reload 写）
**修法**：两处 `unwrap()` → `unwrap_or_else(|p| p.into_inner())`（毒化快照仍是完整旧表，读比崩好；写侧恢复同理由）。`cell()` 同文件顶部。
**红测**（cache.rs 新增内嵌测试）：
```rust
/// 毒化恢复（L-4）：reload 临界区 panic 毒化后，effective() 读旧快照不 panic。
/// 全局静态锁毒化对后续测试无害——修复后读写均走 into_inner。
#[test]
fn effective_read_survives_lock_poison() {
    let join = std::thread::Builder::new()
        .spawn(|| {
            let _g = cell().write().unwrap();
            panic!("注入毒化");
        })
        .unwrap();
    let _ = join.join(); // Err(panic) → 锁毒化
    let _ = effective(); // 当前实现 panic（红）；修复后返回旧快照（空表亦合法）
}
```
**绿**：`cargo test -j 2 --lib effective_read_survives` 过 + `cargo test -j 2 --lib model::` 零回归。
**Commit**: `fix(model): effective 缓存锁毒化读恢复——reload 临界区 panic 后 flush 热路径不再连环崩(审查L-4)`

### T3（P1）writer 跨批回卷双点——复核驱动，证实则修

**文件**：`src/history/writer.rs`（flush step 2）、`src/history/rows.rs`（计数）
**步骤**：
1. **复核测试**（新增 writer 单测）：同 series 跨批发 t5 → t3 → t6 → t3（批间 poll written 推进确保分批）：断言盘上 t3 点数。当前实现 = 2（双点证实）或 1（证伪——若批内路径已消化）。
2. **证实则修（裁定 D-T3）**：step 2 等值判定前加严格更旧判定 `row.ts_ms < st.last_ts` → `stale_ts_dropped++` 剔行（lane/last_val 不动）；`HistoryStats` 增 `stale_ts_dropped`（sysstatus/metrics 接线循 F1 模式——十三计数器扩为十四）；复核测试改断言 t3 恰 1 点 + stale_ts_dropped==1 转绿；乱序单点场景（t5→t3 无回卷）补锁定测试（t3 被 drop——**语义变更**：迟到样本不再落盘）。
3. **证伪则登记**：测试断言盘上 t3 恰 1 点（现状已对）→ L-1 推翻，登记总方案 backlog 复核结论，不改实现。
**D-T3 语义变更明示（用户已批准修此项；此处留回退）**：严格更旧样本从"落盘"变为"丢弃+计数"。收益=闭合双点（Count/Avg 双计）；代价=Source 端乱序/重传的合法迟到样本被丢（窗口内乱序；>7d 已被 F2 时界钳制）。回退=revert 单 commit。
**Commit（若修）**: `fix(history): 跨批回卷 ts 双点闭合——严格更旧样本丢弃计 stale_ts_dropped(语义变更:迟到样本不落盘,回退revert;max 方案推演不成立见plan §〇);乱序窗口实测证实 t5→t3→t6→t3 双点`

### T4（P1）subscribe e2e 两断言强化

**文件**：`tests/rtdata_subscribe_e2e.rs:285`（恒真）与 :560-565（反压判别弱）
**修法**：
- 恒真 `assert!(buf.len() < 1 << 20)` → `assert!(!buf.contains("event: datachange") && !buf.contains("event: snapshot"), "空库连接不得推数据帧: {buf}")`（空 lux 骨架——若未来破例推帧即红；"不卡死"意图保留在 collect_window 超时本身）。
- 反压判别 `combined.contains("event: datachange")` → 解析全部 datachange 帧 JSON，统计 changes 中 cell 第三元素 == "9"（新值）的 field 数，断言 ≥9（20 台减 channel 容量 8 的下界；构造注释里"≥9 台已推走旧值"的精确兑现）。
**性质**：守门强化（D-F 预期即绿），非红→绿；断言强化记录于 commit。
**绿**：`cargo test -j 2 --features history --test rtdata_subscribe_e2e` 11/11。
**Commit**: `test(subscribe): 两弱断言强化——空流骨架禁数据帧(原<1MB恒真)+反压窗口统计≥9台新值(原contains单帧即绿)`

### T5（P1）init_failed e2e 补 /metrics family 省略断言

**文件**：`tests/history_init_failed_e2e.rs`（尾部，退出断言前）
**修法**：init_failed 真实态下 `GET /metrics`（免 JWT，同 app 实例）断言 `!body.contains("rs_iot_history_")`——「仅引擎 ok 输出」契约在真实 init_failed 态的锁点（现仅 visibility e2e 注入态覆盖）。
**性质**：守门断言（预期即绿，D-F）。
**Commit**: `test(history): init_failed 真实态 /metrics family 省略断言——契约锁点补真实路径(原仅注入态覆盖)`

### T6（P1）前端 500 响应 error 细节透传

**文件**：`frontend/src/lib/historyQuery.ts:193`（historyErrorMessage 500 分支）+ 测试
**修法**：末尾兜底前插：
```ts
if (status === 500) {
  const err = typeof obj?.error === 'string' ? obj.error : ''
  return `查询失败（500）：${err || body}`
}
```
doc 注释同步（"其余状态码（含 500）透传"改为"500 提取 JSON error，其余透传"）。
**红测**：`historyErrorMessage(500, '{"error":"history engine: lock"}')` 期望含 `history engine: lock`（当前不含——红）。
**绿**：`npx vitest run`（196+1）+ `npm run build`。
**Commit**: `fix(frontend): 500 响应提取 JSON error 细节——引擎错误串不再被吞(审查L9),红(不含)→绿`

### T7（P1）WriteErrorKind::index 派生自 ALL

**文件**：`src/history/rows.rs:130-138`
**修法**：手写 match → `Self::ALL.iter().position(|k| *k == self).expect("ALL 覆盖全部变体")`；补等价守门测试 `for (i, k) in WriteErrorKind::ALL.iter().enumerate() { assert_eq!(k.index(), i) }`（预期即绿——重构防漂移，D-F 登记）。
**Commit**: `refactor(history): WriteErrorKind::index 派生自 ALL.position——消除双硬编码错位静默自洽(审查L4);等价守门测试锚定下标序`

### T8（P1）hgetall 奇数长度 warn

**文件**：`src/api/mod.rs:125-134`
**修法**：`while` 循环前加 `if arr.len() % 2 != 0 { tracing::warn!("[api] HGETALL {key} 返回奇数长度 {}，尾部元素丢弃", arr.len()); }`。
**性质**：豁免行为测试（hgetall 直调 lux execute_value，奇数 RESP 数组需 mock 整个 client，成本高于收益）——编译 + 既有调用零回归验证，commit 登记。
**Commit**: `fix(api): hgetall 奇数长度返回 warn 日志——静默丢尾可观测化(审查Phase2-L4;测试豁免:需mock client,登记)`

## 二、顺序与门禁

顺序：T1 → T2 → T3（复核驱动）→ T4+T5+T8（测试/可观测强化，可合 1-2 commit）→ T6 → T7。全程主会话直做。

门禁（全部完成后）：
```bash
cargo fmt --all --check
cargo clippy -j 2 --workspace --all-targets -- -D warnings
cargo clippy -j 2 --workspace --all-targets --features history -- -D warnings
cargo test -j 2                                    # 默认档
cargo test -j 2 --features history                 # history 档（含 e2e）
cargo test -j 2 --no-default-features --lib        # minimal 档——本包 T1 后应全绿
cd frontend && npx vitest run && npm run build
```
验收线：minimal 档恢复全绿（T1 兑现）；其余档零回归；前端 197+build。

## 三、风险与回退

| 项 | 风险 | 回退 |
|---|---|---|
| T3 D-T3 语义变更（迟到样本丢弃） | 合法迟到样本不落盘（Source 乱序/重传场景）；>7d 已有 F2 钳制，本变更影响窗内 | revert 单 commit（计数器与接线随之回退） |
| T3 复核证伪路径 | 若现状无双点则不修（登记复核结论） | — |
| T1 语义 | 无——测试对齐代码注释明示的设计 | — |
| 其余 | 测试强化/日志/派生重构，行为面零变化 | 各自单 commit revert |
