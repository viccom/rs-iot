# lux 上游同步与本地补丁维护策略

> **状态**：活文档（lux 上游同步工作流，随补丁集演进）
> rs-iot 以 `path = "../lux"` 内嵌 lux 作为进程内 RESP 数据库。维护者**非 lux 官方成员**,无法提 PR,
> 仅以"克隆官方源 + 叠加本地小补丁"方式跟随上游。本文记录这套工作流的操作规范、风险点与应急恢复。

## 一、依赖形态

`rs-iot/Cargo.toml:12`:

```toml
lux = { path = "../lux" }
```

- 纯路径依赖,**无 `version` / 无 `features`**
- Cargo.lock 记录 lux 当前版本(`0.37.1`),但实际编译用本地 working tree
- lux 必须位于 rs-iot **同级目录**(任何机器/任何盘符均可,路径是相对的),修改 lux 直接影响 rs-iot 编译与行为

**路径约定**(本文所有命令用变量,按机器实际位置设置):

```bash
RSIOT=<rs-iot 仓库根>     # 例:Git Bash 下 /e/Rs_Codes/rs-iot
LUX=<rs-iot 同级 lux>     # 例:Git Bash 下 /e/Rs_Codes/lux
```

## 一.5、新机器初始化(从零恢复 lux)

lux 是 path 依赖,新机器 clone rs-iot 后必须先恢复同级 lux(否则 `cargo build` 报 `failed to read ../lux/Cargo.toml`):

```bash
# ① 克隆官方源
git clone https://github.com/lux-db/lux.git "$LUX"
cd "$LUX"

# ② 配私人 fork remote 并拉取含本地补丁的历史
git remote add mine https://git.metme.top/viccom/lux.git
git fetch mine
git checkout -B main mine/main

# ③ 若官方 main 有新提交,把补丁 rebase 上去(冲突按 §七 决策树)
git fetch origin
git rebase origin/main

# ④ 验证:lux 自身测试 + rs-iot 集成(见 §八)
```

> 若 `mine` 不可达(网络/凭证问题),退路:clone 官方后按 §四 补丁清单**手动重打补丁**(清单记录了每个补丁的改动文件与内容,改动面极小),再 `git push mine main` 重建云端备份。

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
镜像   → https://github.com/viccom/lux.git        私有镜像,只供 CI 检出(§九.5);本地不加 remote
```

- 本地 `main` 是工作分支,叠本地补丁于官方之上;另有 `fix/windows-compat` 历史分支
- `origin` 无写权限(非维护者),只 `fetch`;`mine` 有写权限,`push` 备份
- 命名与 git 惯例(origin=自己 / upstream=官方)相反,但**功能正确**,保持现状不改名(收益小、成本无谓)

查看当前状态:

```bash
cd "$LUX"
git remote -v
git branch -vv          # 看 main 相对 origin/main ahead/behind
```

## 四、当前本地补丁清单

本地 `main` 领先 `origin/main` 3 个提交（2026-11-09 复核；`mine/main` 已同步含 `bf32c25`，官方源不推——无写权限且不接收 Agent 修复）：

| commit | 用途 | 改动文件 | 冲突热点 | 丢弃条件 |
|---|---|---|---|---|
| `fed8471` | mlua 0.10→0.12 对齐 rs-iot | `Cargo.toml` + `Cargo.lock` + `src/lua.rs`（`String`→`LuaString` + `set_hook` 返回 `Result`） | mlua 版本行、lua.rs API 改名 | 官方 lux 升 mlua ≥ 0.12 |
| `f745e5f` | Windows 测试套件修复 | `tests/common/mod.rs` + `tests/public_api.rs` | 测试辅助函数 | 官方修了同问题 |
| `bf32c25` | state 目录 owner-only 创建（与进程 umask 无关） | `src/file_security.rs`（`create_dir_all_private` + 3 个测试） | `ensure_dir` 创建路径 | 官方改用 `DirBuilder::mode(0o700)` 或等价实现 |

**关键**：三个补丁都在 lux **内部**（lua 引擎 / 测试 / 文件安全），**完全不碰 rs-iot 耦合面**（`ServerConfig`/`EmbeddedClient`/`EmbeddedValue` 等）。所以补丁本身不会让 rs-iot 代码失效。

`bf32c25` 背景：`ensure_dir` 先 `create_dir_all`（按 umask 得 0o775/0o755）再 `validate_directory`（`mode & 0o022 != 0` 拒绝）最后才 chmod 0o700——顺序错误使 Linux 首次启动必败（`state/shard_0/journal` 等子目录同病）。补丁把创建改为 `DirBuilder::recursive(true).mode(0o700)`，umask 无法放宽 owner-only 位。

### 四.5、行为契约适配（rs-iot 侧，不改 lux 签名）

lux 升级还可能改变**运行期契约**，`cargo build` 不报错，需按清单核对：

| lux 变更 | 影响 | rs-iot 适配 |
|---|---|---|
| `3f75c17` shutdown 变持久化屏障 | 丢弃 `ServerHandle` → `accepting_work=false`，后续 client 调用报 "SERVER shutting down" | 测试辅助函数取 `handle.client()` 后 `std::mem::forget(handle)`（全仓统一此方案）。注意 `let _keep = handle` 这类绑定保活**无效**——async helper 的栈帧随 future 完成即销毁；生产路径始终持有 handle 到 `shutdown_and_wait` |
| v0.37.1 `jsonwebtoken` feature 由 `rust_crypto` 改 `aws_lc_rs` | 宿主若仍声明 `rust_crypto`，Cargo feature 统一后双 provider 同时启用 → encode/decode panic（jsonwebtoken 10 的二选一约束） | rs-iot 与 gateway 的 `jsonwebtoken` 均声明 `aws_lc_rs`（全工作区单 provider，与 lux 一致）；`api::auth::install_jwt_crypto_provider()` 与 `crates/rsiot-gateway/src/auth.rs` 同名函数保留为幂等兜底（`crypto::aws_lc::DEFAULT_PROVIDER.install_default()`；自行签发 JWT 的测试也调用） |
| `data_dir` 排他锁 | 同名目录并发打开直接 `AlreadyExists` 拒启 | 测试 data_dir 每次调用唯一（`tests/common::test_data_dir`：pid + 纳秒 + 进程内序号）；同一用例内跨 start/stop 重启复用同一个 `PathBuf`，不重复调用 helper |
| `827d1dd` state 目录 owner-only 校验 | 新建 state 子目录按 umask 取权限，0o775 被拒 | 本地补丁 `bf32c25` + rs-iot `prepare_lux_data_dir` 预建根目录并 chmod 0o700（不再改进程 umask，避免全局副作用） |

## 五、同步策略:rebase(非 merge)

非维护者跟上游 + 少量本地补丁,**rebase 远优于 merge**:

- 补丁永远线性叠在 `origin/main` 顶端,历史干净,一眼看出"官方到哪 + 我的补丁"
- 冲突逐个补丁解决,一次一个,定位清晰
- 改写历史**无风险**:本地 main 不推 origin(无权限),且 `mine` 只自己一人用

merge 会产生 merge commit,多次同步后补丁埋进 merge 历史,难追踪、难丢弃。不要用。

## 六、标准同步流程

```bash
cd "$LUX"

# ① 备份补丁到仓库外(rebase 失败时可恢复;目录放仓库外避免污染工作树)
git format-patch origin/main -o "$LUX/../lux-local-patches/"

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
cd "$LUX"
cargo build -j 2

# 2) rs-iot 集成(轻量 check,不编测试 binary,内存占用低)
cd "$RSIOT"
cargo check -j 2

# 3) check 通过后,跑 clippy + 关键集成测(走 lux 读写)
cargo clippy -j 2 --all-targets -- -D warnings
cargo test -j 2 --test dataflow --test api_flow --test sidecar_e2e
```

全量 `cargo test --all-targets --all-features` 留到最后,且**务必 `-j 2`**。

**umask 注意（2026-11-09 实测）**：lux lib 测试大量用 `tempfile::tempdir()` 作 state 根，其权限为 `0o777 & !umask`；在 **umask 0o002** 的机器上根目录为 0o775，会被 state 目录校验拒绝（84 例环境性失败，与代码无关）。跑 lux 自身测试前先 `umask 022`（CI 默认即此）；`bf32c25` 已把 `file_security` 相关 4 例改为显式收紧 tempdir，但仍建议整体在 0o022 下跑。rs-iot 侧不受影响：`prepare_lux_data_dir` 预建根目录并 chmod 0o700。

## 九、push 私人 fork 备份

rebase 成功后,把含补丁的 main 推到 `mine`(云端备份,防本地丢失):

```bash
# 正常推送(rebase 前已 push 过)
git push mine main

# rebase 后历史改写,需强推(mine 只自己一人用,安全)
git push mine main --force-with-lease
```

### 九.5、CI 可达性(GitHub Actions 私有镜像)

rs-iot 的 CI（GitHub `viccom/rs-iot`，`.github/workflows/ci.yml`）要把 lux 检出到 rs-iot 的同级目录。
lux fork 含本地补丁、不公开，官方源上也没有，所以 GitHub 上另有一份**私有镜像**（2026-11-25 建）：
`viccom/lux`，私有，只有 `main` 分支。

- **建立与同步**：从本地克隆只推 `main`，不带标签、其他分支和工作区改动；本地克隆**不新增 remote**。

  ```bash
  cd "$LUX"
  git push https://github.com/viccom/lux.git main                       # 快进
  git push https://github.com/viccom/lux.git main --force-with-lease    # rebase 改写历史后
  ```

- **同步义务**：本地 `main` 一变（rebase、新增补丁）就要重推。镜像落后时 CI 测的是旧 lux——
  rs-iot 依赖了新的 lux 行为就会误红或误绿。速查见 §十三。
- **CI 布局**：`actions/checkout` 不允许检出到工作区之外，所以 Rust job 的布局是
  `$GITHUB_WORKSPACE/{rs-iot, lux, tsink_fork}`——rs-iot 检出到子目录 `rs-iot/`，lux、tsink 在
  它的同级，各 job 以 `defaults.run.working-directory` 进 `rs-iot/` 执行 cargo。
  2026-11-25 之前写成 `path: ../lux`，被 checkout 拒绝，Rust job 自 CI 建立以来没有跑通过。
- **token**：repo secret `RUST_DEPS_TOKEN`（仓库 Settings → Secrets and variables → Actions）。
  用 fine-grained PAT：Repository access 只选 `viccom/lux`，权限只给 Contents: Read。
  它**不能**用来检出别的仓库，所以 tsink（`viccom/tsink`，公开）的检出步骤不传 token。
  secret 缺失或无权时，Rust job 在「Checkout lux」一步失败；换 token 只改 secret，不动工作流，
  然后在 Actions 页面重跑失败的 job。PAT 有有效期：到期后 Rust job 回到同一处失败，需换新 token
  （`docs` / `frontend` 不受影响，所以整体不会全红，容易漏看）。

## 十、应急恢复

rebase 搞砸时,三级兜底:

```bash
# 级别1:回退到 rebase 前(rebase 未完成)
git rebase --abort

# 级别2:从干净官方重新应用全部补丁
git reset --hard origin/main
git am "$LUX/../lux-local-patches/"*.patch

# 级别3:am 也冲突 → 单独 cherry-pick 或手动重打补丁
git cherry-pick 206bc22       # 一个一个来(补丁 hash 见 §四)
```

## 十一、风险点总览

| 风险 | 频率 | 危害 | 缓解 |
|---|---|---|---|
| **本地补丁 rebase 冲突** | 中(官方动 mlua/disk 时) | 低 | 冲突面极小;补丁是官方迟早会做的修复 |
| **lux 自身 breaking change 碰 rs-iot 耦合面** | 低 | 高 | 见附录 A,`cargo check` 直接定位 |
| **编译 OOM** | 高(若不限并行) | 中(进程崩溃) | 所有 cargo 命令 `-j 2` |
| **本地补丁丢失** | 低 | 中 | `format-patch` 备份 + `mine` 云备份 |

## 十二、补丁生命周期管理

本地补丁都是"**临时桥接**",会被官方吸收:

- 每次同步官方,**第一步判断"补丁还要不要"**(而非"怎么解冲突")
- 官方已等效实现 → 丢弃(`rebase --skip`),补丁从本地历史移除
- 官方未修 → 继续背,rebase 到新 base

预期:mlua 升级是时间问题(社区已到 0.12),Windows 测试修复与 state 目录 owner-only 创建同样是上游迟早要修的问题。**补丁不会永久背在本地**。

## 十三、速查(下次同步 3 步)

```bash
cd "$LUX"
git fetch origin && git rebase origin/main     # 同步 + 重叠补丁(冲突按 §7)
cargo build -j 2                                # lux 编译
cd "$RSIOT" && cargo check -j 2                # rs-iot 集成
# 全绿后:git push mine main --force-with-lease  # 云备份
#          git push https://github.com/viccom/lux.git main --force-with-lease  # 同步 CI 镜像(§九.5)
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
cd "$RSIOT"
rg 'lux::' src/ tests/         # 全部 lux 引用一览
cargo check -j 2               # 编译器精确定位 break 点
```

> `EmbeddedValue` 无 `non_exhaustive`:lux **加**新变体时 rs-iot 的 match 不会自动报错(若 match 无 `_` 兜底会静默漏值);lux **改/删**现有变体名时 `cargo build` 必报错。升级时务必 `rg 'EmbeddedValue::'` 逐处确认有 `_ =>` 兜底分支。
