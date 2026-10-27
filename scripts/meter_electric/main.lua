-- 跨回调状态（严格沙箱：全局须显式初始化）：[dev]=累计电量 kWh；.__last=上拍 epoch ms
st_e = {}

-- 沙箱无 os 库：ctx:now()=epoch ms（能力卡 L6）。本地时刻（东八区）：epoch 分钟 +480 取模 1440。
local function hour_frac()
  return ((math.floor(ctx:now() / 1000 / 60) + 480) % 1440) / 60 -- 0~24 小数小时
end

local function base_load(h)
  if h < 1 then return 0.15 elseif h < 7 then return 0.12
  elseif h < 9 then return 1.6 elseif h < 11 then return 1.0
  elseif h < 18 then return 0.6 elseif h < 22 then return 2.2
  else return 0.8 end
end

local PHASE = { ["home-meter-e1"] = 0.0, ["home-meter-e2"] = 0.35 }
local BASE_E = { ["home-meter-e1"] = 12580.0, ["home-meter-e2"] = 13520.0 }

function on_timer()
  -- 积分步长 = 上拍至今实测小时数：与 instance 的 period 配置解耦（改周期不改
  -- 累计速率）；热加载后首拍 st_e 无 .__last → dt=0 不积分，防跳变。
  local now = ctx:now()
  local dt = (now - (st_e.__last or now)) / 1000 / 3600
  st_e.__last = now
  local h = hour_frac()
  for dev, ph in pairs(PHASE) do
    local hf = (h + ph) % 24
    local power = math.max(0.05, base_load(hf) * (0.9 + 0.2 * math.random()))
    local prev = st_e[dev] or BASE_E[dev]
    local e = prev + power * dt
    st_e[dev] = e
    ctx:set_values(dev, {
      voltage = 220 + (math.random() - 0.5) * 6,
      current = power * 1000 / 220 * (0.9 + 0.2 * math.random()),
      power = math.floor(power * 1000) / 1000,
      pf = math.floor((0.82 + 0.12 * math.random()) * 100) / 100,
      frequency = math.floor((49.96 + math.random() * 0.08) * 100) / 100,
      energy = math.floor(e * 10000) / 10000,
      energy_today = math.floor((e % 30) * 100) / 100,
      pulse = (math.random() < power / 2.5),
      status = 0,
    })
  end
end
