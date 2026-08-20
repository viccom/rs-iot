# Release Note：Lua 脚本系统 · 阶段 A（机制地基）

> 日期：2026-08-20
> **状态**：已发布（master `599fc69..ad62742`，16 commit，49 文件 +5870/-540）
> 范围：Lua 脚本系统演进设计（[2026-08-18 v0.5](../../designs/2026-08-18-lua-script-system-evolution-design.md)）阶段 A 全量 + 四轮独立审查修复（16 项）+ CI minimal 组合存量修复
> 读者：网关运维 + 现场脚本作者。升级动作集中在「升级必读」一节，其余为了解性内容。

## 升级必读（行为变更，替换二进制即生效，无数据迁移）

1. **多段私有 require 语义变更（可能报错的存量脚本）**：旧加载器把任意点分名
   （`require("vendor.helper")`）映射到私有 `scripts/<名>/lib/vendor/helper.lua`；现在多段名
   改判站点共享层 `scripts/_lib/vendor/helper.lua`。**修复**：私有库改单段名（`require("helper")`）
   或旧 `lib.` 前缀（`require("lib.helper")`，语义不变）。单段与 `lib.` 前缀用法零影响。
   详见 [lua-script-guide 迁移告示](../../guides/lua-script-guide.md)。
2. **回调时长预算 10s（默认）**：collect/on_write/on_data/on_init 单次超 10s 即被切断
   （跳过本轮 + 计数，连续超限走 fail_threshold → degraded）。**升级前建议盘查存量脚本**
   是否有单次 collect 超 10s 的用法；有则调实例配置 `callback_timeout_ms` 或优化脚本。
3. **超时 clamp ≤30s**：`ctx:request(frame, timeout_ms)` 与 `ctx:http_*(timeout)` 的脚本侧
   超时一律封顶 30s（防挂死）。
4. **背压丢样（语义从"必达"变"尽力"）**：hub 持续满时样本上送 2s 超时丢弃并计数；
   on_data 被动帧通道（容量 64）满时即时丢帧计数。可观测：实例 stat 扩展字段
   `dropped_samples` / `dropped_frames`（`GET /api/v1/inst/stats`）。
5. **HTTP 出站频控 20 次/s**：超限向脚本返回 `rate limited（20/s）` 错误（可退避重试）。
6. **命名收紧**：require 拒绝含 `:` 段（Windows 路径逃逸防护）与 builtin 多段名
   （`require("json.x")` 报错）；REST 脚本名新增拒绝保留目录 `_lib` / `_stdlib`。
7. **manifest 校验（新增）**：main.lua 头部 `-- @role/@api_ver/@stdlib` 声明会被解析校验。
   无 manifest 头的存量脚本全豁免（零影响）；显式声明 `@role collector` 则强制存在 `collect()`。

## 新能力

- **官方 stdlib 体系**：内嵌种子 4 库（algo.crc16 / codec.basexx / util.retry / util.tablex，
  离线冷启动兜底）；`GET /api/v1/lua/stdlib` 本地清单；`POST /api/v1/lua/stdlib/sync` 验签同步
  （ed25519 + 逐文件 sha256 + 原子替换 + 审计日志）；脚本侧 `stdlib.list()/info()`。
  **pin 策略（默认锁定）**：sync 不带 pin = 锁定本地已装版本（上游发新版会拒装，须显式升级）；
  `"pin":"auto"` 显式跟随 latest；持久化锁定走配置键 `lua_stdlib_pin`。
- **站点共享库 `scripts/_lib/`**：REST 全套管理（`kind=lib` 的列表/内容读回/保存/版本/回滚），
  `require("作者.库名")` 引用；任一库变更 → 全部运行中实例 **VM 级热加载**（失败自动回退旧 VM，
  已停实例不复活）。
- **远程 reload 新入口**：MQTT `<hwid>/command` 主题发布
  `{"instance":"<lua实例>","cmd":"reload"}` 可触发该实例 VM 热加载（与 JWT 保护的
  `POST /api/v1/lua/reload` 等价能力；失败自动回退旧 VM）。
- **MCP**：`rsiot_save_lua_script` 新增可选 `kind=lib` 参数。
- **观测增强**：实例 stat 新增 `dropped_samples` / `dropped_frames` / `vm_restart_count` /
  `script_version`。

## 已知限制（上线前关注）

- **默认 upstream 为占位 URL**（`https://rsiot.github.io/lualib/`，官方源尚未发布）——
  首次 `POST /lua/stdlib/sync` 会 502。上线前发布官方 dist，或配置 `lua_stdlib_upstream`
  指向内网镜像（镜像 = 官方静态目录完整拷贝，验签防篡改）。
- 官方脚本模板随 sync 分发到 `_stdlib/scripts/` 仅供查阅，安装到 `scripts/<name>/` 的
  安装器为 B 阶段项；模板条目不列入 `stdlib.list()`。

## 配置新增

| 键 | 默认 | 说明 |
|---|---|---|
| `lua_stdlib_upstream` | 占位官方 URL | stdlib 同步源（内网镜像改此键） |
| `lua_stdlib_pin` | 无 | stdlib 持久化锁定版本（无则锁定本地已装版本） |
| 实例 `callback_timeout_ms` | 10000 | 单回调时长预算（毫秒） |

## 升级步骤建议

1. 测试环境替换二进制 → 观察实例 stat（`tick_fail` / `consecutive_fail` /
   `dropped_samples` / `last_err`）一个采集周期以上；
2. 用 `GET /api/v1/lua/scripts` 核对脚本清单；对用了多段私有 require 的脚本按迁移告示调整；
3. 现网上线后按需配置 `lua_stdlib_upstream` 镜像并执行首次 sync（审计日志
   `[lua][audit] stdlib sync` 可追溯）。

> 工程侧：本次同时修复 CI minimal 组合存量红（reqwest dev-deps + 协议 e2e feature
> 门控），`cargo test --no-default-features --features lua` 恢复全绿。
