# 2026-08-21 · rs-iot 自监控设备（selfMonitor）

> 设计/计划：`docs/designs/2026-08-21-self-monitor-device-plan.md`（含完整标签表）
> 实施：worktree `feat/self-monitor`（T1 `42af12a` → T3 `8bc8e2e`）

## 变更摘要

rs-iot 自身成为一个常驻设备（`devid="rsiot"`，实例 `self`，随进程无条件自启——不落 lux `instances`，重启恒在，`stopinst` 可临时停）：

- **17 只读标签**周期上报（默认 5s，`rtdata:rsiot`）：识别（id/version/platform，Str）、资源（cpu_percent Float、mem_bytes/uptime_s Int）、统计（collector_count/device_count/tag_count/inst_running/inst_degraded/inst_failed/alarm_active/dbsize Int）、自身健康（healthy Bool——无降级无失败业务实例）、吞吐（pipeline_processed_count/hub_depth_last Int）
- **3 可写标签**（`POST /api/v1/command/send`，instance=`"self"`）：`watchdog`（写→下周期回读，写通路自检）、`note`（现场备注 Str ≤256）、`report_period`（上报周期秒，钳 [1,3600]，下周期生效）；多键可一命令同写
- **hwid 部署期覆盖键**：config 顶层 `hwid`（空=硬件自动生成 `eiot*`；非空=指定——多网关接同一平台时规避同型号 CPU 撞号）
- 健康统计口径：只认 lux `instances` 配置集内的 `stat:*`——残留 stat 键（旧实例被杀进程未落 stopped）不计入

## 行为变更（升级注意）

- **`sim1` demo 设备不再产生**（原"lux 无自启实例时拉起 dataSim demo"分支删除）：空配置启动后 `rtdata:devA`/`stat:sim1` 不再出现，取而代之的是 `rtdata:rsiot`
- 从旧版升级的数据目录可能残留 `stat:sim1`（status 永久 running）——不影响 selfMonitor 统计（配置白名单口径），介意可 `DEL stat:sim1`
- `dataSim` 插件本身保留，仍可手动建实例做模拟数据

## 验证

- 单测 442/0（self_monitor 17 用例：标签集精确断言/四型覆盖/配置白名单口径/apply_write 全分支）；clippy `-D warnings` 0
- 现场冒烟（e2e-lua-d 环境）：17 标签四型出数（id==hwid）、watchdog+note 一命令双写回环（中文原样）、report_period 5→2 动态生效、dataSim 实例上线计数联动（collector/device/tag/inst_running=1, healthy=true）、stopinst self 停后标签冻结

## 面向平台的价值

北向视角下网关自身即设备：MQTT topic `<hwid>/rsiot/datas` 携带上述标签——平台可订阅每一台网关的心跳/资源/告警计数，写 `watchdog`/`note` 实现平台↔网关双向通路自检与运维标注。
