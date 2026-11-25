-- @role transform
-- iot-demo 限值报警（ISA-18.2 简化：HH/H/L/LL + 布尔故障），带 2% 回差防临界抖动。
--
-- 数据面：subscribe=["*"]，宿主把每个采样点逐点投递到 on_rtdata；不在 RULES 里的
-- 设备/点位直接返回（rsiot 系统点每秒都在，脚本必须廉价）。
-- 出口：ctx:fire_event/clear_event → lux alarms 表 → SSE alarm 帧 → HMI AlarmStore
--      → 合成点位 __prio/__alarms → 画面报警徽标（hmi-studio lib/synthetic.ts）。
--
-- **阈值与 hmi-studio/src/screen-assets/iot-catalog.ts 的 LIMITS 必须一致**：
-- 两处是同一套工程限值的"判定端"与"显示端"，改一处必须同步另一处（计划文档 §6 已知取舍）。
-- key 约定：<devid>.<tag>.<hh|hi|lo|ll|alarm>（合成点位用第一个 '.' 之前的 devid 聚合）。
-- level 映射：HH/LL/故障 → error（HMI 映射 P1）；H/L → warn（P3）；仅提示类 → info（P4）。

-- 阈值表：h/hh = 高限/高高限，l/ll = 低限/低低限；alarm = 布尔故障点
local RULES = {
  dev_wm1 = {
    flow_rate   = { h = 31.5, hh = 31.9, unit = "m³/h" },
    pressure    = { h = 0.428, unit = "MPa" },
    leak_alarm  = { alarm = true, text = "泄漏报警" },
    reverse_flow = { alarm = true, text = "逆向流", level = "info" },
  },
  dev_wm2 = {
    flow_rate   = { h = 36.5, hh = 37, unit = "m³/h" },
    pressure    = { h = 0.468, unit = "MPa" },
    leak_alarm  = { alarm = true, text = "泄漏报警" },
    reverse_flow = { alarm = true, text = "逆向流", level = "info" },
  },
  dev_wm3 = {
    flow_rate   = { h = 41.5, hh = 42, unit = "m³/h" },
    pressure    = { h = 0.508, unit = "MPa" },
    leak_alarm  = { alarm = true, text = "泄漏报警" },
    reverse_flow = { alarm = true, text = "逆向流", level = "info" },
  },
  dev_gm1 = {
    flow_rate   = { h = 21.7, hh = 22, unit = "m³/h" },
    pressure    = { l = 1.75, h = 3.05, unit = "kPa" },
    leak_alarm  = { alarm = true, text = "泄漏报警" },
    overpressure = { alarm = true, text = "超压" },
  },
  dev_gm2 = {
    flow_rate   = { h = 26.5, hh = 27, unit = "m³/h" },
    pressure    = { l = 1.85, h = 3.55, unit = "kPa" },
    leak_alarm  = { alarm = true, text = "泄漏报警" },
    overpressure = { alarm = true, text = "超压" },
  },
  dev_em1 = {
    active_power  = { h = 51.8, hh = 52, unit = "kW" },
    thd_voltage   = { h = 2.95, unit = "%" },
    overload_alarm = { alarm = true, text = "过载报警" },
  },
  dev_em2 = {
    active_power  = { h = 57.8, hh = 58, unit = "kW" },
    thd_voltage   = { h = 3.45, unit = "%" },
    overload_alarm = { alarm = true, text = "过载报警" },
  },
  dev_em3 = {
    active_power  = { h = 63.5, hh = 64, unit = "kW" },
    thd_voltage   = { h = 3.95, unit = "%" },
    overload_alarm = { alarm = true, text = "过载报警" },
  },
  dev_env1 = {
    co2         = { h = 810, hh = 830, unit = "ppm" },
    temperature = { h = 28.8, unit = "°C" },
    alarm       = { alarm = true, text = "环境报警" },
  },
  dev_pt1 = {
    pressure_1 = { h = 1.69, hh = 1.695, unit = "MPa" },
    vibration  = { h = 3.8, unit = "mm/s" },
    fault      = { alarm = true, text = "变送器故障" },
  },
}

-- 回差：默认取阈值的 2%（rel）；规则可写 hys 覆盖为绝对值
local HYS_REL = 0.02

-- fired[key] = true：已经在报（避免每采样重复 fire 刷 count）
local fired = {}

-- 启动对账：transform 重启会丢失 fired 表，上一轮已 fire 的告警会变成孤儿
-- （值已回落但 alarms 表里还留着，永远不会被 clear）。策略：on_timer 分批 clear
-- 全部规则 key（15 条/秒，避开 20/s 频控），**跳过本轮已 fire 的 key**；对账完成后停。
-- 注：ctx:clear_event 对不存在的 key 也恒发 Clear 事件（幂等），所以只清 fired 之外的。
local RECON_KEYS = {}
for devid, dev in pairs(RULES) do
  for tag, rule in pairs(dev) do
    if rule.alarm then
      RECON_KEYS[#RECON_KEYS + 1] = devid .. "." .. tag .. ".alarm"
    else
      for _, kind in ipairs({ "hh", "hi", "lo", "ll" }) do
        RECON_KEYS[#RECON_KEYS + 1] = devid .. "." .. tag .. "." .. kind
      end
    end
  end
end
table.sort(RECON_KEYS)
local recon_idx = 1

function on_init()
  fired = {}
end

function on_timer()
  if recon_idx > #RECON_KEYS then return end
  for _ = 1, 15 do
    local key = RECON_KEYS[recon_idx]
    if not key then return end
    recon_idx = recon_idx + 1
    if not fired[key] then ctx:clear_event(key) end
  end
end

local function key_of(devid, tag, kind)
  return devid .. "." .. tag .. "." .. kind
end

local function hys_of(limit, rule)
  if rule.hys then return rule.hys end
  return math.abs(limit) * HYS_REL
end

local function fire(key, level, msg)
  local ok = ctx:fire_event(key, level, msg)
  if ok then fired[key] = true end
end

local function clear(key)
  ctx:clear_event(key)
  fired[key] = nil
end

-- 高限：v >= limit 触发；v < limit - hys 恢复
local function eval_hi(devid, tag, v, rule, limit, kind, level)
  local key = key_of(devid, tag, kind)
  if not fired[key] and v >= limit then
    fire(key, level, string.format("%s %s 高限越限 %.3f %s（%s≥%.3f）",
      devid, tag, v, rule.unit or "", kind, limit))
  elseif fired[key] and v < limit - hys_of(limit, rule) then
    clear(key)
  end
end

-- 低限：v <= limit 触发；v > limit + hys 恢复
local function eval_lo(devid, tag, v, rule, limit, kind, level)
  local key = key_of(devid, tag, kind)
  if not fired[key] and v <= limit then
    fire(key, level, string.format("%s %s 低限越限 %.3f %s（%s≤%.3f）",
      devid, tag, v, rule.unit or "", kind, limit))
  elseif fired[key] and v > limit + hys_of(limit, rule) then
    clear(key)
  end
end

local function eval_bool(devid, tag, v, rule)
  local key = key_of(devid, tag, "alarm")
  if v then
    if not fired[key] then
      fire(key, rule.level or "error", string.format("%s %s：%s", devid, rule.text or tag, "触发"))
    end
  elseif fired[key] then
    clear(key)
  end
end

function on_rtdata(devid, tag, value)
  local dev = RULES[devid]
  if not dev then return end
  local rule = dev[tag]
  if not rule then return end

  if rule.alarm then
    eval_bool(devid, tag, value == true, rule)
    return
  end

  local v = tonumber(value)
  if not v or v ~= v then return end -- 非数值 / NaN 不判定
  if rule.hh then eval_hi(devid, tag, v, rule, rule.hh, "hh", "error") end
  if rule.h then eval_hi(devid, tag, v, rule, rule.h, "hi", "warn") end
  if rule.l then eval_lo(devid, tag, v, rule, rule.l, "lo", "warn") end
  if rule.ll then eval_lo(devid, tag, v, rule, rule.ll, "ll", "error") end
end
