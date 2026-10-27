-- 跨回调状态（严格沙箱：全局须显式初始化）：[dev]=累计气量 m³（st_g）/ 预付费余额元（st_b）；
-- st_g.__last=上拍 epoch ms。
st_g = {}
st_b = {}

-- 沙箱无 os 库：ctx:now()=epoch ms（能力卡 L6）。本地时刻（东八区）：epoch 分钟 +480 取模 1440。
local function hour_frac()
  return ((math.floor(ctx:now() / 1000 / 60) + 480) % 1440) / 60 -- 0~24 小数小时
end

local PHASE = { ["home-meter-g1"] = 0.0, ["home-meter-g2"] = 0.1 }
local BASE_G = { ["home-meter-g1"] = 1654.0, ["home-meter-g2"] = 1876.0 }
local BASE_B = { ["home-meter-g1"] = 112.7, ["home-meter-g2"] = 86.4 }

local PRICE = 3.05   -- 元/m³
local VALVE_AT = 10  -- 余额阈值：≤ 即关表前阀
local TOPUP_AT = 9.9 -- 关阀后内漏缓降至此 → 模拟购气 +100 复开（关阀窗口约 16h，演示可观察复开）
local LEAK_FLOW = 0.002 -- 关阀后残余内漏 m³/h（表前阀非零密封）

local function base_flow(h)
  if h < 6 then return 0.01 elseif h < 8 then return 0.32
  elseif h < 11 then return 0.02 elseif h < 13 then return 0.26
  elseif h < 17 then return 0.02 elseif h < 20 then return 0.42
  else return 0.03 end
end

function on_timer()
  -- 积分步长 = 上拍至今实测小时数：与 instance 的 period 配置解耦（改周期不改
  -- 累计速率）；热加载后首拍 st_g 无 .__last → dt=0 不积分，防跳变。
  local now = ctx:now()
  local dt = (now - (st_g.__last or now)) / 1000 / 3600
  st_g.__last = now
  local h = hour_frac()
  for dev, ph in pairs(PHASE) do
    local prev_bal = st_b[dev] or BASE_B[dev]
    -- 表前阀状态参与流量计算：关阀后仅余微量内漏（否则「关阀+正常流量」自相矛盾，
    -- 余额也会无限下穿为负——审查 2026-10-27）。
    local inst
    if prev_bal > VALVE_AT then
      inst = base_flow((h + ph) % 24) * (0.85 + 0.3 * math.random())
    else
      inst = LEAK_FLOW
    end
    local total = (st_g[dev] or BASE_G[dev]) + inst * dt
    st_g[dev] = total
    local bal = prev_bal - inst * dt * PRICE
    if bal < TOPUP_AT then bal = bal + 100 end -- 模拟用户购气充值（自动复开阀）
    st_b[dev] = bal
    local leak = (math.random() < 0.002)
    ctx:set_values(dev, {
      gas_total = math.floor(total * 100000) / 100000,
      gas_instant = math.floor(inst * 1000) / 1000,
      balance = math.floor(bal * 100) / 100,
      battery = math.floor((3.62 - ((total % 1) * 0.02)) * 100) / 100,
      valve = (bal > VALVE_AT),
      leak_alarm = leak,
      status = leak and 8 or 0,
    })
  end
end
