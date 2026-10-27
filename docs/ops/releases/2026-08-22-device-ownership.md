# 2026-08-22 · 设备归属模型 + lux/RESP 收口 + secrets 加密

> 设计/计划：`docs/designs/2026-08-21-device-ownership-design.md`（D-OWN-1..4 / D-LUX-1 / D-SEC-1 / D-CLN-1）
> 实施：worktree `feat/device-ownership`（T1 `64635eb` → T7）

## 变更摘要

修复"运行时设备无归属、删除残留、孤儿数据上北向"缺陷族（2026-08-21 深度审查定级高×2）：

- **设备归属注册表 `dev2inst`**（D-OWN-1/2）：lux Hash，field=devid，value=宿主实例列表（多宿主并记）。登记点=`SampleSender`——Source trait 的 tx 从裸 `mpsc::Sender` 换为包装类型，**遮蔽 send/try_send 在转发前登记 devid 归属**（进程内 seen 去重）；Sample/pipeline/lux 结构零改动（ADR D1 保全），9 个 Source 插件调用点零改
- **删除级联闭环**（D-OWN-2）：`inst/delete` 反查归属表清理——多宿主共写设备**删一宿主不清数据，最后宿主移除才清 rtdata**；配置设备启动预登记（防"启动未出数即删"漏归属）；`purge` 同步清归属表
- **设备视图并入运行时设备**（D-OWN-3）：`GET /devices` = 实例配置派生 ∪ dev2inst 补集（运行时设备带 `runtime:true` + `owner_inst`；配置优先不重复）
- **北向不做过滤**（D-OWN-4）：归属清理闭环后无孤儿 rtdata，`mqtt-pub-default` 维持全量抽取（过滤引入漏发风险，治本不治标）
- **lux RESP 非 loopback 强制认证**（D-LUX-1）：`[lux]` 增 `password` 键；`bind_host` 非 loopback 且无密码 → **启动失败**（错误信息含两条出路）；`GET /api/v1/config` 对 `lux.password` 脱敏回显
- **secrets AEAD 加密存储**（D-SEC-1）：值经 chacha20poly1305 加密（密钥 HKDF(jwt_secret) 派生），存储格式 `enc:v1:<nonce>:<ct>`——**RESP 直读只见密文**；进程内透明解密；旧明文读兼容（懒迁移：下次 set 才加密）
- **devices 死表提示**（D-CLN-1）：启动检测 lux `devices` Hash 存量非空 → WARN 指引（已废弃，权威源=实例配置）

## 行为变更（升级注意）

- **非 loopback 部署**：`[lux] bind_host` 指向非 loopback 且未设 `[lux] password` 的既有部署，升级后**无法启动**——设密码或改回 loopback
- **旧明文 secret**：读兼容；下次 `secret/set` 覆盖时自动转加密存储。RESP 直读历史明文值在未重写前仍可见明文（介意可逐个 set 重写触发迁移）
- **删除语义增强**：运行时设备（lua `set_value`/transform 派生/sidecar 上报产生的 devid）随宿主实例删除一并清理（含 rtdata 与归属）；多宿主共写设备行为变化——此前删任一实例不触发任何清理，现按归属表决策
- 新增 lux 键：`dev2inst`（归属表）；`devices` 死表如存量非空启动有 WARN（不自动删）

## 验证

- 单测 566+ 通过（dev_registry 8 / SampleSender 7 / dev_ownership_e2e 集成 3 / secrets 加密 6+ / lux 校验 6 等）；clippy `-D warnings` 0；`check_docs.py` OK
- 现场冒烟：lua 运行时设备归属→删除全清、双宿主删一留一、`GET /devices` runtime 徽标、`secret/set` 后 RESP 原始读见 `enc:v1:`、非 loopback 无密码启动被拒（见计划实施块冒烟记录）
