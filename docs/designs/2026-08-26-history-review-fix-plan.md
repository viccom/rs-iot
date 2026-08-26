# 历史存储 Phase 1/2/3 审查修复计划

> **状态**：implemented（2026-08-26 收口；High×5 + Medium×11 全修，Low×17 延后见 §七/backlog）
> **日期**：2026-08-26
> **来源**：Phase 1/2/3 深度代码审查（三路并行审查 + 主会话逐项复核，**16/16 复核成立**，无一推翻；Low 17 项延后见 §七）
> **范围**：仅 High×5 + Medium×11；不动 Low；不动数据面架构（写路径 D1-D11 ADR 不变）

## 验收记录（2026-08-26，全部真实运行输出见各 commit）

| 项 | commit | 红→绿锚点 |
|---|---|---|
| F1 H1+M11 NaN 拒收+dup_ts 计数 | `52b995d` | writer 10 测（新增 2：nonfinite 不毒化死区/跨批同 ts 计数）+render 16→18 family |
| F2 M1 时界钳制+checked_mul | `7f04565` | rows 单测 3 例（溢出/now−8d 拒、now−1h 收） |
| F3 M2 长度预检 | `61d80e9` | rows 单测（70KiB tag 拒） |
| F4 H4 配置校验+饱和乘法 | `9399dae` | config 非法值矩阵 Err + engine_invalid_config_err_not_panic（红：巨值 Ok/panic→绿：Err） |
| F5 M3 spawn Err 通道 | `402b623` | 豁免行为红测（线程创建失败不可注入，登记）；12 测试调用点机械适配 |
| F6 M6 series TTL 缓存 | `b83103c` | telemetry_series_count_ttl_cached（红：直插即见 2→绿：TTL 内 1/过期 2；测试档 TTL 2s） |
| F7 H3 Restart 双 bump | `51492e3` | watch_restart_double_bump_covers_midwindow_cursors（单 bump 盲区对照臂）+watch e2e 4/4 |
| F8 H5 checked_sub | `b8efa37` | 400 矩阵 +1 例（红：`attempt to subtract with overflow`→绿：400）；read e2e 10/10 |
| F9 M4 panic 监督 | `0c7cc6d` | watch_supervise_restarts_on_panic（编译红→绿；panic×2→epoch 恰+2）+subscribe e2e 11/11 |
| F10 M5 投影守门 | `ff7fcca` | 单测×2（含删投影对照臂）+e2e JSON 断言（devB.t1 不误报）；守门测试预期即绿（D-F；夹具首跑红系裸 tag 误用，修正记录于 commit） |
| F11 M10 R13 登记 | `6a43d35` | 文档（phase2 plan §二 R13） |
| F12-F15 前端 H2/M7/M8/M9 | `60c6a21`/`64c27e2`/`7ef2fae`/`9ad8ebc` | 196 测（基线 188+8）+ build 过 |
| 文档同步 | 本 commit | rest-api.md §4.1 十三计数器/§4.10 18 family/§4.18 写侧拒收语义+校验边界+400 溢出+面板迁址（Low-L1 顺手） |

**门禁**：全量四档 cargo test（默认/history/minimal）+ 双档 clippy + fmt + 前端 vitest 196+build——见收口 commit。

**Goal**: 消除审查发现的 5 High / 11 Medium——NaN/Inf 数值域防线、tsink「一行连坐」写入预检、配置边界校验与算术溢出、订阅重建窗口丢帧与 panic 监督、测试守门缺口、前端脏数据防御。

---

## 一、复核结论总表

| 编号 | 级别 | 位置（master 2786f2e） | 复核 | 处置组 |
|---|---|---|---|---|
| H1 | High | writer.rs:245-256 NaN 毒化 last_val 致死区永久失效 | ✅ | G1 |
| H2 | High | routes_history.rs:201 + HistoryQuery.vue:129-135 NaN→null→整条曲线消失 | ✅ | G1(写侧)+G3(前端) |
| H3 | High | rtdata_watch.rs:249-256 Restart 重建窗口新建连接丢帧 | ✅ | G2 |
| H4 | High | engine.rs:92-95 + config.rs:188-229 配置零校验+乘法溢出 | ✅ | G1 |
| H5 | High | routes_history.rs:135-139 桶护栏 `end-start` 溢出（debug 可远程 panic） | ✅ | G2 |
| M1 | Med | rows.rs:226 `ts*1000` 无 checked/时界，坏 ts 连坐整批 | ✅ | G1 |
| M2 | Med | rows.rs:210-213 tag/devid/hwid 无长度预检（tsink 拒整批） | ✅ | G1 |
| M3 | Med | writer.rs:96-100 spawn_writer 失败 expect panic 违反 P8 | ✅ | G1 |
| M4 | Med | rtdata_watch.rs:210 watcher task panic 无监督静默冻结 | ✅ | G2 |
| M5 | Med | routes_rtdata_subscribe.rs:218-238 project_baseline/dev_segment 零测试锚定 | ✅ | G2 |
| M6 | Med | engine.rs:193-201 series_count O(series) 分配 × sysstatus 5s 轮询 | ✅ | G1 |
| M7 | Med | historyQuery.ts:66-69 agg 有 intervalSec 无 → `interval=undefined` | ✅ | G3 |
| M8 | Med | historyQuery.ts:124-125 sparkline x 按桶序均分，稀疏序列时间轴失真 | ✅ | G3 |
| M9 | Med | HistoryQuery.vue:119-123 响应解析失败误报「网关不可达」 | ✅ | G3 |
| M10 | Med | 双 impl 守门「同断言双档」防不了共同漂移 | ✅ | G2（登记） |
| M11 | Med | writer.rs:238-241 同 ts 跳过无计数，对账黑洞 | ✅ | G1 |

## 二、方向性裁定（记录取舍与回退）

| # | 裁定 | 理由 | 回退 |
|---|---|---|---|
| D-A | **H1/H2 采用写侧拒收**（审查选项①）：Float 非有限值在 writer 剔行+计数，**不落盘**；读侧不改序列化；前端做逐点过滤防御（旧数据/纵深） | 单点根治三处症状；工业 NaN=bad quality 语义上非有效测量值；读侧显式化方案要动 REST 契约且 series 类型仍被污染 | 单 commit revert，NaN 恢复落盘（死区毒化问题随之回来，需连读侧方案一起换） |
| D-B | H3 双 bump：重建序 = 退订→bump1→backfill→退避→重建 ksubscribe→**bump2** | bump1 保退订窗口存量连接，bump2 保重建窗口新建连接（游标 epoch 必 stale → Full 重读）；单末次 bump 亦可，双 bump 让存量连接更早重同步 | 单 commit revert |
| D-C | M1 时界窗 `[now-7d, now+1h]`（ms 口径，fan_out 批首取一次 now）硬编码 | 工业采集正常抖动远小于窗；配置项 YAGNI（现场回补历史是显式运维动作，可走 tsink 工具） | 放宽窗口常量或 revert |
| D-D | M6 series_count 5s TTL 缓存（Mutex<(Instant,u64)>） | sysstatus 5s 轮询与 15s scrape 均命中；引擎口径数值 5s 滞后可接受（水位同量级） | revert 后 sysstatus 恢复每次全量 list_metrics |
| D-E | M9 抽纯函数 `parseHistoryBody`（historyQuery.ts） | 组件零测试是既登记缺口，纯函数化让 M9/H2 前端部分可 vitest | — |
| D-F | M5 为守门测试（预期即绿），非红→绿 | 锁的是现存正确行为防回归；若跑红=发现真缺陷，停下上报 | — |

## 三、任务分组与派发

- **G1（Rust 写路径+引擎+治理）**：H1、H4、M1、M2、M3、M6、M11 + 四个新计数上治理面。文件：`src/history/writer.rs`、`src/history/rows.rs`、`src/history/engine.rs`、`src/config.rs`、`src/api/routes_sys.rs`、`src/api/routes_metrics/render.rs`、`tests/history_write_e2e.rs`（如需）。
- **G2（Rust 读+订阅）**：H3、H5、M4、M5、M10。文件：`src/api/routes_history.rs`、`src/api/rtdata_watch.rs`、`src/api/routes_rtdata_subscribe.rs`、`tests/history_read_e2e.rs`、`tests/rtdata_subscribe_e2e.rs`、`tests/rtdata_watch_e2e.rs`、`docs/designs/2026-08-25-history-read-subscribe-plan.md`（M10 登记）。
- **G3（前端）**：H2 前端、M7、M8、M9。文件：`frontend/src/lib/historyQuery.ts`、`frontend/src/lib/historyQuery.test.ts`、`frontend/src/components/devmeta/HistoryQuery.vue`。
- **G4（hub 收口）**：`docs/reference/rest-api.md` 两条（§4.18 非有限值拒收语义 / §4.18.1 差溢出 400）、提交编排、全量门禁、merge master。
- 派发顺序：**G1 ∥ G3 并行**（文件零交集）→ **G2 串行其后**（同为 Rust 避免 target 锁 thrash；rest-api.md 全部归 G4 避免并行冲突）。

## 四、逐项修复规格

### F1（H1+M11+新计数，G1）NaN/Inf 写侧拒收 + 同 ts 计数

- writer.rs `flush` 状态机**新增 step 0**（先于 lane 判定）：`SampleValue::Float(f)` 且 `!f.is_finite()` → `stats.nonfinite_skipped.fetch_add(1)` + `continue`（lane/last_ts/last_val 均不动）。
- step 2（`st.last_ts == row.ts_ms` 跳过处）：补 `stats.dup_ts_skipped.fetch_add(1)`。
- rows.rs `HistoryStats` 增字段 `nonfinite_skipped`、`dup_ts_skipped`（AtomicU64，全 pub，对齐既有字段注释风格）；`fresh_stats()` 测试构造同步扩字段（该测试注释即声明此扩展模式）。
- 治理接线：routes_sys.rs history 块 JSON 增两键；render.rs `render_history` 增两行（命名对齐既有 family 风格）。
- **红测**（writer 单测）：① 同 series `[NaN, 5.0, 5.0]` deadband=1.0 → NaN 计 nonfinite_skipped==1 且 written 不含 NaN，两笔 5.0 中第二笔 deadbanded==1（死区未被毒化——当前实现红：NaN 落盘且 last_val=NaN 后死区失效，第二笔也 written）；② 跨批同 ts 两行 → written==1 && dup_ts_skipped==1（当前无计数编译红）。

### F2（M1，G1）fan_out 时界钳制 + checked_mul

- rows.rs `fan_out`：批首一次 `now_ms`（SystemTime::now，毫秒）；行构造处 `sample.timestamp.checked_mul(1000)`：`None` → `ts_rejected++` continue；`Some(ts_ms)` 不在 `[now_ms-7*86400_000, now_ms+3600_000]` → `ts_rejected++` continue。
- `HistoryStats` 增 `ts_rejected`；fan_out 尾部沉积处同构。
- **红测**（rows 单测）：timestamp=i64::MAX（溢出）→ 不发送 ts_rejected==1；now-8d → 拒；now-1h → 收（当前实现对前两者照发——红）。
- 注意：既有 e2e 若有回填历史 ts 走 fan_out 的用例需改当前时间（直插引擎的读侧用例不受影响）；跑 `cargo test -j 2 --features history` 全量确认。

### F3（M2，G1）长度预检

- rows.rs `fan_out` 白名单命中后：`tag.len() > 65535 || sample.devid.len() > 16*1024 || sample.hwid 字节长 > 16*1024` → `len_rejected++` continue（String::len 即 UTF-8 字节）。
- `HistoryStats` 增 `len_rejected`。
- **红测**：70KiB tag（effective 白名单先命中）→ len_rejected==1 且未发送（当前照发——红）。

### F4（H4，G1）配置校验 + 饱和乘法

- `HistoryConfig::validate(&self) -> Result<(), String>`（config.rs，随 struct 注释）：`memory_limit_mb/wal_size_limit_mb ∈ [1, 1<<20]`、`retention_days ∈ [1, 36500]`、`channel_capacity ∈ [1, 1<<24]`、`batch_max_rows ∈ [1, 1<<20]`、`cardinality_limit ∈ [0, 1<<24]`、`data_dir` 非空。`wal_sync_ms`/`close_timeout_secs` 不设限（0 有合法语义：PerAppend / 立即放弃）。
- engine.rs `maybe_init` 入口（spawn_scoped **之前**）调用 validate，Err → 返回 TResult Err（既有 init_failed 通道：error! + 引擎 None + 503，主程序继续）。
- build_storage 三处乘法换 `saturating_mul`；cardinality 自动校准 `count*12/10` 换 `saturating_mul/div`（双保险，校验后理论不可达）。
- **红测**：config 单测 ≥4 非法值（channel_capacity=0、batch_max_rows=0、memory_limit_mb=1<<40、retention_days=0）→ validate Err；engine 单测 maybe_init 非法 cfg 返回 Err 不 panic（debug 档下 1<<40 当前必 panic——红）。

### F5（M3，G1）spawn_writer 走 Err 通道

- writer.rs `spawn_writer` 签名 → `Result<(HistoryHandle, JoinHandle), String>`；`.spawn(...).map_err(|e| format!("spawn history writer thread: {e}"))?`。
- engine.rs `maybe_init_impl` 消费处 `?` 映射既有 TResult Err 变体（对齐 RsIotError 惯例）。
- 无行为红测可行（线程创建失败不可注入）——**豁免声明**：编译级验证 + 既有测试零回归；提交信息记录豁免理由。

### F6（M6，G1）series_count TTL 缓存

- engine.rs `EngineTelemetry` 增 `series_cache: Mutex<(Instant, u64)>`（初值 `(Instant::now(), 0)` + 首次必然过期策略或 `Option` 语义，任选实现简单的）；`series_count()`：缓存命中（last + TTL ≥ now）直接返回，过期才 `list_metrics()` 并写缓存。TTL=5s 常量。
- **红测**：真 storage 插 1 series → series_count()==1；直插第 2 series 绕过 → 立即再调仍 1（缓存证据，当前实现返回 2——红）；TTL 过期（测试档缩短 TTL 或注入时刻）后 ==2。

### F7（H3，G2）Restart 双 bump

- rtdata_watch.rs：`'subscribe` 循环顶部 `ksubscribe` 创建后消费 `need_rebump`：`if need_rebump { svc.bump_epoch(); need_rebump = false; }`；Restart 块尾部置 `need_rebump = true`（bump1 保留）。注释更新：bump1=退订窗口存量连接，bump2=重建窗口新建连接（游标 epoch stale → Full）。
- **红测**（单元层，循 phase2「e2e 红不可达落单元层」先例）：真 lux + service——写 devA（版本 v1）→ 取 cursor → 再写 devA（v2）→ `dirty_since` 报 Delta（基线行为）；双 bump 后任意 cursor（含 bump1 后取的）→ dirty_since 必 Full。对照单测：单 bump 后新取 cursor 不触发 Full（论证双 bump 必要性，当前实现红）。既有 respawn e2e 零回归。

### F8（H5，G2）桶护栏 checked_sub

- routes_history.rs：`let span = end.checked_sub(start)` → `None` 返回 400「时间窗口过大（start/end 差溢出）」；Some(d) 后 `d as u64 / iv > MAX_BUCKETS` 判定不变（start<end 已证 d≥0）。
- **红测**：400 矩阵增 `start=-9223372036854775808&end=9223372036854775807&interval=1` → 400（当前 debug 档 handler panic → 测试以连接错误/panic 失败——红）。

### F9（M4，G2）watcher panic 监督

- rtdata_watch.rs：现 `'subscribe` 循环体提为私有 `async fn run(...)`（token 取消 → 正常返回）；`spawn()` 改为监督循环：`loop { let inner = tokio::spawn(run(...)); match inner.await { Ok(()) => break, Err(e) => { error!("[rtdataWatch] watcher panic: {e}; 退避 1s 重启+epoch 重同步"); svc.bump_epoch(); sleep(1s); } } }`。lib.rs 编排不变（`_watcher` 语义不破坏）。
- 监督循环抽自由函数 `supervise(factory)`（工厂返 JoinHandle），`spawn()` 传真实工厂。
- **红测**：注入 panic 两次后正常的工厂 → 断言 epoch 恰增 2（cursor epoch 可观察）且最终正常退出；取消路径（token）→ 直接退出不 bump。当前无监督——编译红起。

### F10（M5，G2）project_baseline/dev_segment 守门（预期即绿，D-F）

- routes_rtdata_subscribe.rs 单测：① 3 设备基线 + scope 仅 devA → 投影后不含 devB/devC 域 → diff 无 removed 误报；② `dev_segment("a.b.c")=="a.b"`、无点串原样返回。
- e2e `removed_on_device_delete_case`：改 JSON 解析断言 devB.t1 不在 removed 数组（替换 contains 串断言）。

### F11（M10，G2）双 impl 守门边界登记

- 2026-08-25-history-read-subscribe-plan.md 风险/遗留节追加 R13：「双 impl 等价守门 = 同断言双档各跑，非 poll vs ksub 输出逐字 diff——两档共同漂移不设防；知情接受（逐字 diff 需固定时间轴的确定性注入，成本高于收益）」。

### F12（H2 前端，G3）无效点过滤

- historyQuery.ts 新纯函数 `numericPoints(points): { numeric: Array<{ts:number;value:number}>; skipped: number }`。
- HistoryQuery.vue：numPoints 改用之；`skipped > 0` 时结果区提示「N 个无效点未绘制曲线」（替代整条消失+误报类型问题）。
- **红测**：混合 `[数值, null, 字符串]` → numeric 仅数值点、skipped==2；现有曲线断言不回归。

### F13（M7，G3）buildHistoryUrl 半配置拒绝

- `if (p.agg !== undefined && p.intervalSec === undefined) throw new Error('agg 与 intervalSec 必须成对提供')`。
- **红测**：vitest 期望 throw（当前产出 `interval=undefined`——红）。

### F14（M8，G3）sparkline x 轴 ts 线性

- `ts0=d[0].ts, ts1=d[n-1].ts`（decimate 输出升序）；`x = ts1>ts0 ? round((b.ts-ts0)/(ts1-ts0)*w) : (n===1 ? 0 : round(k*w/(n-1)))`（全同 ts 回退均分）。doc 注释同步。
- **红测**：非均匀 ts 间隔 → x 按 ts 比例（当前按桶序——红）；既有 x 断言随行为更新（提交信息记录「断言随行为变更，非弱化」）。

### F15（M9，G3）响应解析失败区分

- historyQuery.ts 纯函数 `parseHistoryBody(text): { ok:true; data } | { ok:false; message:'响应解析失败（网关返回非 JSON）' }`（D-E）。
- HistoryQuery.vue ok 分支：`r.text()` + parseHistoryBody；catch 分支仅剩网络错误文案。
- **红测**：合法 JSON / 非法 JSON 两 case。

## 五、TDD 与信息隔离约定

- 每项先写红测并**回传红证据**（真实命令输出摘录）再实现；红测断言一旦确立**禁改**——若断言本身有误，停下上报由主会话裁决，不得自行弱化。
- F5（线程创建失败不可注入）、F10（守门测试）为登记过的豁免/即绿项。
- 实现者匹配现有代码风格（注释密度、中文注释惯例、计数器注释口径）。

## 六、门禁（G4 全量收口）

```bash
cargo fmt --all --check
cargo clippy -j 2 --workspace --all-targets -- -D warnings            # 默认档
cargo clippy -j 2 --workspace --all-targets --features history -- -D warnings
cargo test -j 2                                                        # 默认全量
cargo test -j 2 --features history                                     # history 档（含全部 e2e）
cargo test -j 2 --no-default-features --lib                            # minimal 档
cd frontend && npm ci && npm test && npm run build                     # 前端 188+新增
```

既有 e2e 零回归清单：history_write / kill_recovery / dataflow / visibility / init_failed / init_stack / read / subscribe / watch。

## 七、Low 延后清单（17 项，本轮不做）

Phase 1：L-1 last_ts max 语义、L-3 tag_key contains 串扰登记、L-4 cache 毒化 unwrap；Phase 2：L-1 恒真断言、L-2 反压判别弱化、L-3 dev_id/tag 长度上限（读侧）、L-4 hgetall 静默丢尾；Phase 3：L1 rest-api 面板位置（**G4 顺手修**，见下）、L2 hqTagOptions 前缀歧义、L3 archived_tags 重复、L4 index/ALL 硬编码、L5 死区分母口径文案、L6 init_failed metrics 断言、L7 datalist 固定 id、L8 零值水位防御分支、L9 500 body 细节。→ 统一登记到总方案 backlog（G4 收口时追加一节）。

## 八、风险与回退

| 风险 | 缓解 | 回退 |
|---|---|---|
| D-A 拒收改变 NaN 数据可得性（原落盘→拒收+计数） | 计数器可观测；工业语义 NaN=无效测量 | revert F1（连同前端 F12 保留亦无冲突） |
| D-C 时界窗误伤合法回补 | 窗口宽松（7d/1h）；误伤可由 ts_rejected 计数暴露 | 放宽常量 |
| F7 双 bump 期间连接全量重读风暴 | Restart 本身罕见（订阅异常才触发）；Full 重读是既有 epoch 机制 | revert F7 |
| e2e 端口/进程竞争 | G1、G2 串行派发；cargo target 锁自然串行化 | — |
| M8 曲线视觉变化 | 更真实（时间比例）；提交信息向用户说明 | revert F14 |
