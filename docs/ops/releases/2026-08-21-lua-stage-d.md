# Release Note：Lua 脚本系统 · 阶段 D（安全与告警）

> 日期：2026-08-21
> **状态**：已发布（worktree `feat/lua-stage-d`，`38a37e4..` 9 task，已合并 master）
> 范围：评估收窄后的 D 阶段全量——安全最小集（M3 实例回显遮蔽 + NorthCfg TLS）/ 告警最小闭环（Q12：fire_event/clear_event + lux alarms + REST/SSE + 前端面板）/ 顺带清偿（B 复审 P3 四项 + tag helper + flaky 加固）/ 串口 open 异步安全 / 源仓 upstream 发布就绪（流程件）
> 读者：网关运维 + 脚本作者。升级动作集中在「升级必读」，其余为了解性内容。

## 升级必读（替换二进制即生效）

1. **安全修复（M3）：实例回显不再泄漏明文密码**。此前 `GET /api/v1/insts`、
   `/inst/get`、`/inst/roster` 原样回显 north 实例 `app_config` 中的字面
   `password`（与 `/config` 端点遮蔽行为双标）；现三面统一遮蔽为 `"***"`——
   `secret:xxx` 引用形不遮蔽（非明文），其余字段原样。
2. **M3 回写恢复语义（脚本/工具作者注意）**：把回显的整份配置原样回存
   （`POST /inst/set`）时，`password` 为遮蔽哨兵 `"***"` 会自动用库中原值恢复
   ——不改密码的「复制回存」行为不变；库中原配置本无密码键时哨兵被移除（等价
   无密码声明）；非哨兵值（新明文 / `secret:` 引用）原样透传。**不要**把
   `"***"` 当真实密码参与外部系统对接。
3. **串口 open 行为变化**：串口打开改走阻塞线程 + 5s 限时——对端接错/COM 号
   不存在/端口被占时，不再冻结实例 IO task 或占住 COM 口（此前须杀进程释放）；
   超时后本次 open 失败、下轮 request 自动重试（连接状态置 None，日志可见）。
   流读写路径不变。
4. **告警系统上线**：脚本侧新增 `ctx:fire_event/clear_event`（全角色注入），
   REST/SSE 查询清除 + 前端「数据页 → 活跃告警」面板；活跃清单落 lux `alarms`
   Hash **跨重启保留**——升级后首次启动若见历史告警属预期（清除走面板或
   `DELETE /api/v1/alarms?key=`）。
5. **无破坏性变更**：不配置 TLS 三键的 north 实例、不含告警调用的脚本、既有
   collector/transform 实例行为零变化（明文 TCP 反向锁单测保证）。

## 新能力

- **告警最小闭环（Q12，不建独立角色壳）**：
  - 脚本 API（全角色注入，判定逻辑归脚本——阈值/持续时间/迟滞在 Lua 里写）：
    ```lua
    -- 判定处（如 on_rtdata/on_data 内）
    if temp > 80 then
      ctx:fire_event("dev1.temp_high", "error", "1#窑温度超限 " .. temp)
    elseif temp < 75 then
      ctx:clear_event("dev1.temp_high")   -- 迟滞恢复，幂等
    end
    ```
    `key` ≤128 字符；`level ∈ {info, warn, error}`；`message` ≤512 字符——
    超限返回 `nil, err`。同 key 重复 fire 是**覆盖写 + count 累计**（风暴可见性）。
  - **频控与抖动护栏**（防告警风暴）：每实例 20 次/s token bucket——超限
    `fire_event` 返回 `false`（静默退避，不写清单不推 SSE），计数落 stat
    `alarm_rate_limited`；同 key 1s 窗口内重复 fire 仍计数但**不重复推 SSE**
    （前端不被同键闪屏）。`clear_event` 幂等且恒发 SSE 事件（多客户端视图收敛）。
  - REST（JWT）：`GET /api/v1/alarms`（活跃清单，按 ts 倒序，含 count）/
    `DELETE /api/v1/alarms?key=<key>`（运维兜底清除）。
  - SSE：`event: alarm` 帧（`action: fire|clear`，`source` 由宿主注入——脚本
    不可伪造来源实例）。
  - 前端：数据页「活跃告警」面板（level 徽标/key/来源/次数/时间/手动清除；
    SSE 实时收敛 + 登录/断线重连全量拉取兜底）。
- **NorthCfg TLS（H1 收口——凭证明文过网）**：见下节配置示例。
- **清偿批**：north PubResult 语义注明 / down 回调失败计数 `down_skipped` /
  will() 重连重调契约注明 / lux 读取错误 Fatal 归类核对；`strip_dev_prefix`
  helper 收拢 4 处 tag 剥前缀重复；存量 flaky 两处测试加固。
- **upstream 发布就绪（源仓 rsiot-lualib 流程件）**：release CI（tag → 签名
  index → Pages/Release attach + 发布后在线验签）+ `docs/PUBLISH.md` 发布
  runbook；本地全链演练与篡改负向（sha256 不符拒装）已留证。**正式发布
  （正式钥生成/Pages 开启/首版 tag）属用户检查点**，完成后按 PUBLISH.md 回填
  主仓公钥与正式 URL。

## NorthCfg TLS 配置示例（三键）

```json
{
  "inst_id": "cloud1",
  "app_code": "luaNorth",
  "app_type": "North",
  "app_config": {
    "script": "cloud1",
    "host": "mqtt.example.com",
    "port": 8883,
    "username": "u",
    "password": "secret:broker_pw",
    "tls": true,
    "ca_path": "C:/rs-iot/certs/ca.pem",
    "insecure_tls": false
  }
}
```

| 键 | 默认 | 说明 |
|---|---|---|
| `tls` | `false` | `true` 走 TLS（rumqttc `Transport::tls`）；不配置零行为变化（明文 TCP） |
| `ca_path` | 无 | 自签/私有 CA 的 PEM 文件路径（文件字节原样透传，读不到启动报错）；缺省用系统根证书库 |
| `insecure_tls` | `false` | **仅内网/自签调试**：跳过证书验证（日志 warn 可见）；与 `ca_path` 并存时跳过优先 |

公网平台（EMQX Cloud 等）通常 `tls:true + ca_path:<平台CA>`；证书 hostname 校验
仍生效——域名不匹配会连接失败（属预期安全行为）。

## 已知限制（上线前关注）

- **TLS 真 broker 未在本机实测**：留证形态为组装断言单测 9 项（transport 三变体
  + 明文反向锁 + CA 读失败 + 跳过验证语义）——无本机 TLS broker（mosquitto-TLS/
  EMQX）；现场部署时按上节配置示例验收，异常反馈日志 `[luaNorth]` 前缀。
- **broker ACL 与 `<hwid>/command` 控制链收紧不在本版**（部署形态决策，安全二期
  挂按需）；M4（secret 授权清单撤销不即时）/M5（secret 引用存在性 oracle）同批
  挂按需。
- 告警**无历史时序表**（活跃清单 only，历史走日志/外部系统）与**通知渠道**
  （邮件/短信/webhook——可用 C5 http 能力脚本自组）——Q12「最小」语义边界。
- 串口 5s open 限时后的重试周期取决于脚本 request 节奏（无独立重连定时器）；
  连续 3 次超时端口 poisoned 退避 30s（复审 P2-1，防 blocking 池被灌满）。
- **M3 密码遮蔽的跨机限制（复审 P3 登记）**：实例配置经 REST/MCP 读回时字面
  `password` 以 `***` 哨兵遮蔽；把 A 机读到的配置原样 set 到 B 机时，B 机库中
  无原值可恢复——**password 键会被移除**（B 机已有该实例则静默保留 B 机旧值）。
  密码不随配置复制跨机：用 `secret:` 引用（推荐）或目标机手动改写。

## 配置新增汇总

| 键 | 层 | 默认 | 说明 |
|---|---|---|---|
| `tls` / `ca_path` / `insecure_tls` | luaNorth app_config | false / 无 / false | TLS 三键（见上节） |
| `ctx:fire_event(key, level, message)` | Lua API（全角色） | — | 告警触发（20/s 频控 + 同 key 1s 抖动） |
| `ctx:clear_event(key)` | Lua API（全角色） | — | 告警清除（幂等恒发 SSE） |
| `GET`/`DELETE /api/v1/alarms` | REST（JWT） | — | 活跃告警查询/清除 |
| SSE `event: alarm` | 事件流 | — | `{action: fire\|clear, key, level, source, message, ts, count}` |
| stat `alarm_rate_limited` / `down_skipped` | 实例 stat | — | 告警频控计数（fire/clear 共桶）/ north 下行丢弃计数 |
| stat `alarm_hash_capped` | 实例 stat | — | 告警基数上限拒绝计数（全局 1000 键，复审 P1-1） |

> 工程侧：阶段 D 全程 worktree `feat/lua-stage-d` 隔离执行 + task 间审查，
> 验收清单逐项实测记录于
> [阶段 D 计划](../../designs/2026-08-21-lua-stage-d-plan.md) Step 1
> （含串口真机 3 项 com0com 实测与 upstream 演练留证）。
