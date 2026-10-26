-- 跨回调状态（严格沙箱：全局须显式初始化）
st_g = {}
st_b = {}

-- 沙箱无 os 库：ctx:now()=epoch ms（能力卡 L6）。本地小时（东八区）= epoch/3600s 偏移 8h 取模。
local function hour_frac()
  return ((math.floor(ctx:now() / 1000 / 60) + 480) % 1440) / 60 -- 0~24 小数小时
end

local PHASE = { ["home-meter-g1"] = 0.0, ["home-meter-g2"] = 0.1 }
local BASE_G = { ["home-meter-g1"] = 1654.0, ["home-meter-g2"] = 1876.0 }
local BASE_B = { ["home-meter-g1"] = 112.7, ["home-meter-g2"] = 86.4 }

local function base_flow(h)
  if h < 6 then return 0.01 elseif h < 8 then return 0.32
  elseif h < 11 then return 0.02 elseif h < 13 then return 0.26
  elseif h < 17 then return 0.02 elseif h < 20 then return 0.42
  else return 0.03 end
end

function on_timer()
  local h = hour_frac()
  for dev, ph in pairs(PHASE) do
    local inst = base_flow((h + ph) % 24) * (0.85 + 0.3 * math.random())
    local prev = st_g[dev] or BASE_G[dev]
    local total = prev + inst * 2 / 3600
    st_g[dev] = total
    local bal = (st_b[dev] or BASE_B[dev]) - inst * 2 / 3600 * 3.05
    st_b[dev] = bal
    local leak = (math.random() < 0.002)
    ctx:set_values(dev, {
      gas_total = math.floor(total * 100000) / 100000,
      gas_instant = math.floor(inst * 1000) / 1000,
      balance = math.floor(bal * 100) / 100,
      battery = math.floor((3.62 - ((total % 1) * 0.02)) * 100) / 100,
      valve = (bal > 10),
      leak_alarm = leak,
      status = leak and 8 or 0,
    })
  end
end
