-- 跨回调状态（严格沙箱：全局须显式初始化）
st_t = {}
st_r = {}

-- 沙箱无 os 库：ctx:now()=epoch ms（能力卡 L6）。本地小时（东八区）= epoch/3600s 偏移 8h 取模。
local function hour_frac()
  return ((math.floor(ctx:now() / 1000 / 60) + 480) % 1440) / 60 -- 0~24 小数小时
end

local PHASE = { ["home-meter-w1"] = 0.0, ["home-meter-w2"] = 0.2 }
local BASE_T = { ["home-meter-w1"] = 2846.0, ["home-meter-w2"] = 3120.0 }

local function base_flow(h)
  if h < 6 then return 0.0 elseif h < 9 then return 0.45
  elseif h < 13 then return 0.28 elseif h < 17 then return 0.05
  elseif h < 21 then return 0.5 else return 0.08 end
end

function on_timer()
  local h = hour_frac()
  for dev, ph in pairs(PHASE) do
    local inst = base_flow((h + ph) % 24) * (0.85 + 0.3 * math.random())
    local prev = st_t[dev] or BASE_T[dev]
    local total = prev + inst * 2 / 3600
    st_t[dev] = total
    local rev_event = (math.random() < 0.003)
    local rev = (st_r[dev] or 0.0) + (rev_event and 0.0004 or 0)
    st_r[dev] = rev
    ctx:set_values(dev, {
      flow_total = math.floor(total * 100000) / 100000,
      flow_instant = math.floor(inst * 1000) / 1000,
      flow_reverse = math.floor(rev * 10000) / 10000,
      reverse_flag = rev_event,
      battery = math.floor((3.6 - ((total % 1) * 0.02)) * 100) / 100,
      valve = true,
      status = rev_event and 2 or 0,
    })
  end
end
