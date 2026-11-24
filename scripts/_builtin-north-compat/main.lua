-- @role mqtt-north
-- 内置 mqttPub 兼容脚本（阶段 B T8 无损迁移，默认实例 mqtt-pub-default 使用）。
-- 行为逐字段对标旧 MqttPub（src/plugin/builtin/mqtt_pub，已删）。
-- 本文件由宿主首次启动写入，已存在不覆盖——修改后即为自定义脚本（热加载生效）。
local json = require("json")

local HWID = cfg.hwid

-- MqttPub 等价：values 键 = lux hash 原始 field（<devid>.<tag>）。
-- luaNorth 批量入口已剥 <devid>. 前缀，此处拼回（db::rtdata 落盘均为前缀 field）。
local function build_item(e)
  local values = {}
  for tag, v in pairs(e.values) do
    values[e.devid .. "." .. tag] = v
  end
  local payload = json.encode({
    hwid = HWID,
    devid = e.devid,
    values = values,
    timestamp = e.ts,
  })
  return { topic = HWID .. "/" .. e.devid .. "/datas", payload = payload, qos = 0 }
end

-- 无平台认证：返回 nil → cfg 的 username/password/client_id 直用（MqttPub 同款）。
function auth(c)
  return nil
end

-- 每设备一条发布：topic <hwid>/<devid>/datas、qos 0。
function up(batch)
  local out = {}
  for _, e in ipairs(batch) do
    out[#out + 1] = build_item(e)
  end
  return out
end

-- <hwid>/command（订阅项由宿主配置）上的 CtlMessage JSON → 转发目标实例。
-- 字段缺失/类型不符静默丢弃（同旧 MqttPub serde 反序列化失败即忽略）。
function down(topic, payload)
  local ok, msg = pcall(json.decode, payload)
  if not ok or type(msg) ~= "table" then return end
  if type(msg.instance) ~= "string" or type(msg.devid) ~= "string"
    or type(msg.cmd) ~= "string" or type(msg.data) ~= "string" then
    return
  end
  ctx:send_command(msg.instance, msg.devid, msg.cmd, msg.data)
end
