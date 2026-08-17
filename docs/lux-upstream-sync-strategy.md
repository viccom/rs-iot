# lux 上游同步与本地补丁维护策略

> rs-iot 以 `path = "../lux"` 内嵌 lux 作为进程内 RESP 数据库。维护者**非 lux 官方成员**,无法提 PR,
> 仅以"克隆官方源 + 叠加本地小补丁"方式跟随上游。本文记录这套工作流的操作规范、风险点与应急恢复。

## 一、依赖形态

`rs-iot/Cargo.toml:12`:

```toml
lux = { path = "../lux" }
```

- 纯路径依赖,**无 `version` / 无 `features`**
- Cargo.lock 记录 lux 当前版本(`0.37.0`),但实际编译用本地 working tree
- lux 必须位于 rs-iot 同级 `E:\github.com\lux`,修改 lux 直接影响 rs-iot 编译与行为

## 二、是否只能 git clone lux 到本地?

**是的——对于"跟随官方 + 本地小修改"场景,git clone 本地完整仓库是唯一合理方案。**

| 方案 | 跟随官方 | 本地改 | 配 path 依赖 | 结论 |
|---|---|---|---|---|
| **git clone 本地完整仓库** | ✅ fetch + rebase | ✅ commit 保留 | ✅ | ✅ **当前方案** |
| git submodule | △ 指针式,不利于 rebase 本地补丁 | ✗ 本地改无法提交进指针 | △ 需 init | ✗ 补丁管理复杂 |
| cargo vendor | ✗ 失去 git 同步能力 | △ 手动覆盖 diff | ✅ | ✗ 违背"跟随官方"诉求 |
| crates.io `[patch]` | △ 仍需维护含补丁的 git 源 | △ | △ | ✗ 本质回到 clone |

根因:本地补丁要 commit 进 lux 历史(才能 rebase/cherry-pick/丢弃),这要求 lux 是一个**完整可写的 git 工作树**——vendor 是文件快照(无历史)、submodule 是只读指针(本地改游离于指针外)、crates.io 是版本快照。只有 clone 本地仓库能同时满足"fetch 官方 + 保留本地提交 + path 编译"三件事。

## 三、仓库配置(双 remote)

```
origin → https://github.com/lux-db/lux.git        官方上游,fetch 只读同步
mine   → https://git.metme.top/viccom/lux.git     私人 fork,push 备份
```

- 本地 `main` 是工作分支,叠本地补丁于官方之上;另有 `fix/windows-compat` 历史分支
- `origin` 无写权限(非维护者),只 `fetch`;`mine` 有写权限,`push` 备份
- 命名与 git 惯例(origin=自己 / upstream=官方)相反,但**功能正确**,保持现状不改名(收益小、成本无谓)

查看当前状态:

```bash
cd /e/github.com/lux
git remote -v
git branch -vv          # 看 main 相对 origin/main ahead/behind
```

## 四、当前本地补丁清单

本地 `main` 领先 `origin/main` 2 个提交:

| commit | 用途 | 改动文件 | 冲突热点 | 丢弃条件 |
|---|---|---|---|---|
| `3068806` | mlua 0.10→0.12 对齐 rs-iot | `Cargo.toml`(mlua 版本 1 行) + `Cargo.lock` + `src/lua.rs`(3 处 `String`→`LuaString` + `set_hook` 返回 `Result`) | mlua 版本行、lua.rs API 改名 | 官方 lux 升 mlua ≥ 0.12 |
| `206bc22` | Windows 测试套件 + WAL truncate 修复 | `src/disk.rs`(`Wal` 加 `path` 字段 + `truncate` 两阶段 reopen) + `tests/*` | `Wal` 结构体字段、`truncate` 方法体 | 官方修了同 bug |

**关键**:两个补丁都在 lux **内部**(lua 引擎 / WAL 存储),**完全不碰 rs-iot 耦合面**(`ServerConfig`/`EmbeddedClient`/`EmbeddedValue` 等)。所以补丁本身不会让 rs-iot 代码失效。

## 五、同步策略:rebase(非 merge)

非维护者跟上游 + 少量本地补丁,**rebase 远优于 merge**:

- 补丁永远线性叠在 `origin/main` 顶端,历史干净,一眼看出"官方到哪 + 我的补丁"
- 冲突逐个补丁解决,一次一个,定位清晰
- 改写历史**无风险**:本地 main 不推 origin(无权限),且 `mine` 只自己一人用

merge 会产生 merge commit,多次同步后补丁埋进 merge 历史,难追踪、难丢弃。不要用。

## 六、标准同步流程

```bash
cd /e/github.com/lux

# ① 备份补丁到仓库外(rebase 失败时可恢复;目录放仓库外避免污染工作树)
git format-patch origin/main -o /e/github.com/lux-local-patches/

# ② fetch 官方
git fetch origin

# ③ rebase:把本地 2 个补丁重新叠到 origin/main 最新
git rebase origin/main
```

- 无冲突 → 直接进 §八 验证
- 有冲突 → 进 §七 决策树

## 七、冲突解决决策树

**核心原则:优先判断"补丁能否丢弃",而非"怎么解冲突"。** 官方已修的,直接跳过。

补丁1(mlua 对齐)冲突:

```
官方 lux 的 mlua 版本现在多少?
├─ ≥ 0.12 → 本地补丁目的达成,丢弃:
│          git rebase --skip
└─ 仍 0.10 → 保留本地,解冲突:
            git add Cargo.toml src/lua.rs Cargo.lock
            git rebase --continue
```

补丁2(Windows WAL fix)冲突:

```
官方 src/disk.rs 的 Wal::truncate 改了吗?
├─ 已修同 bug → 丢弃:
│             git rebase --skip
└─ 未改 / 改别处 → 保留本地两阶段 reopen,解冲突:
                 git add src/disk.rs
                 git rebase --continue
```

## 八、验证(同步后必做,`-j 2` 防 OOM)

> ⚠️ 本机**禁用页面文件**,rustc 并行编译大型 crate 会撑爆 48GB commit(曾导致 Everything 服务被挤崩溃)。
> **所有 cargo 命令必须 `-j 2` 限并行。**

```bash
# 1) lux 自身能编译
cd /e/github.com/lux
cargo build -j 2

# 2) rs-iot 集成(轻量 check,不编测试 binary,内存占用低)
cd /e/github.com/rs-iot
cargo check -j 2

# 3) check 通过后,跑 clippy + 关键集成测(走 lux 读写)
cargo clippy -j 2 --all-targets -- -D warnings
cargo test -j 2 --test dataflow --test api_flow --test sidecar_e2e
```

全量 `cargo test --all-targets --all-features` 留到最后,且**务必 `-j 2`**。

## 九、push 私人 fork 备份

rebase 成功后,把含补丁的 main 推到 `mine`(云端备份,防本地丢失):

```bash
# 正常推送(rebase 前已 push 过)
git push mine main

# rebase 后历史改写,需强推(mine 只自己一人用,安全)
git push mine main --force-with-lease
```

## 十、应急恢复

rebase 搞砸时,三级兜底:

```bash
# 级别1:回退到 rebase 前(rebase 未完成)
git rebase --abort

# 级别2:从干净官方重新应用全部补丁
git reset --hard origin/main
git am /e/github.com/lux-local-patches/*.patch

# 级别3:am 也冲突 → 单独 cherry-pick 或手动重打补丁
git cherry-pick 3068806       # 一个一个来
```

## 十一、风险点总览

| 风险 | 频率 | 危害 | 缓解 |
|---|---|---|---|
| **本地补丁 rebase 冲突** | 中(官方动 mlua/disk 时) | 低 | 冲突面极小;补丁是官方迟早会做的修复 |
| **lux 自身 breaking change 碰 rs-iot 耦合面** | 低 | 高 | 见附录 A,`cargo check` 直接定位 |
| **编译 OOM** | 高(若不限并行) | 中(进程崩溃) | 所有 cargo 命令 `-j 2` |
| **本地补丁丢失** | 低 | 中 | `format-patch` 备份 + `mine` 云备份 |

## 十二、补丁生命周期管理

两个补丁都是"**临时桥接**",会被官方吸收:

- 每次同步官方,**第一步判断"补丁还要不要"**(而非"怎么解冲突")
- 官方已等效实现 → 丢弃(`rebase --skip`),补丁从本地历史移除
- 官方未修 → 继续背,rebase 到新 base

预期:mlua 升级是时间问题(社区已到 0.12),Windows WAL truncate 是真 bug(官方迟早修)。**这俩补丁不会永久背在本地**。

## 十三、速查(下次同步 3 步)

```bash
cd /e/github.com/lux
git fetch origin && git rebase origin/main     # 同步 + 重叠补丁(冲突按 §7)
cargo build -j 2                                # lux 编译
cd /e/github.com/rs-iot && cargo check -j 2     # rs-iot 集成
# 全绿后:git push mine main --force-with-lease  # 云备份
```

---

## 附录 A:rs-iot → lux 耦合面(lux 升级时检查清单)

lux 若改以下符号的**字段 / 变体 / 方法签名**,rs-iot 需跟着改。这与本地补丁无关(补丁不碰这些面)。

| lux 符号 | lux 定义位置 | rs-iot 调用点 | 检查方法 |
|---|---|---|---|
| `ServerConfig` 字段 | `lib.rs` | `src/db/mod.rs:22` `build_lux_config`(逐字段赋值,最脆) | `cargo build` 直接报错 |
| `StorageConfig` / `StorageMode` | `disk.rs` re-export | `src/db/mod.rs:8,32` | `cargo build` |
| `AuthConfig` | `lib.rs` | `src/db/mod.rs:30` | `cargo build` |
| `EmbeddedValue` 变体 | `lib.rs:531`(**无 `non_exhaustive`**) | match 解构 ~15 处 | `rg 'EmbeddedValue::'` |
| `EmbeddedClient` 方法 | `lib.rs:525`(**无 `Debug`**) | `host.rs`/`api/*`/`db/*`/`plugin/*` ~12 处 | `cargo build` |
| `LuxError` 变体 | `lib.rs:58` | `error.rs:18`(`#[from]`,不 match 变体) | 通常无需动 |
| `run_with_config` 签名 | `lib.rs:2919` | `lib.rs:147` + 6 处测试 | `cargo build` |
| `ServerHandle` 方法(`.client()`/`.shutdown_and_wait()`) | `lib.rs:505` | `db/mod.rs:46` | `cargo build` |

排查命令:

```bash
cd /e/github.com/rs-iot
rg 'lux::' src/ tests/         # 全部 lux 引用一览
cargo check -j 2               # 编译器精确定位 break 点
```

> `EmbeddedValue` 无 `non_exhaustive`:lux **加**新变体时 rs-iot 的 match 不会自动报错(若 match 无 `_` 兜底会静默漏值);lux **改/删**现有变体名时 `cargo build` 必报错。升级时务必 `rg 'EmbeddedValue::'` 逐处确认有 `_ =>` 兜底分支。
