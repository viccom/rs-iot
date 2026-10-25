-- agg-filter：滑动窗口均值 + 野点剔除（transform 角色完整示例，阶段 C）。
-- 配合 docs/guides/lua-script-guide.md「transform 角色（luaTransform）」章。
--
-- 数据流：订阅原始点（宿主 subscribe 过滤，如 ["raw1.temp*"]）→ on_rtdata 逐点：
--   1) 限幅清洗——超出 [CLAMP_LO, CLAMP_HI] 的野点剔除（rejected 计数，不进窗不产出）；
--   2) 滑动窗口——有效点进窗，超窗丢最旧；窗口状态存 kv（C4 持久：跨重启/热加载不丢）；
--   3) 聚合输出——窗口均值经 ctx:set_value 写派生点（与 collector 同出口：
--      hub → pipeline → lux rtdata:<devid>，前端/北向零差别消费）。
--
-- 实例配置（app_code="luaTransform"）：
--   { "script": "agg-filter", "period": 0, "subscribe": ["raw1.temp*"] }
-- 纯定时模式（不订 EventBus）：period > 0 且 subscribe 留空，on_timer 周期加工。
--
-- @role transform
-- @api_ver 1
-- @stdlib >=1.0
-- @param window number 可选 滑动窗口点数 默认5 范围:1-100（kv 键 cfg_window 优先，reload 生效）
-- @param clamp_lo number 可选 野点下限（低于剔除）默认-40（改脚本常量，热加载生效）
-- @param clamp_hi number 可选 野点上限（高于剔除）默认120

-- 限幅阈值（纯 Lua 判定，野点 = 超出闭区间的点，剔除不钳位）。
local CLAMP_LO, CLAMP_HI = -40, 120
-- 滑动窗口大小（on_init 里可被 kv cfg_window 覆盖——窗口大小可配的演示）。
local WINDOW = 5
-- 派生点输出目标：agg1 的 temp_avg（与订阅面 raw1.* 不相交，语义上无自环；
-- 即使订阅自身输出 tag，宿主 self-write 窗口也会滤掉自身回流，见指南自环防护节）。
-- 注意 devid 不要含 "."：subscribe 模式以 "." 分段（"dev" / "dev.tagpat" 两段），
-- 带点的 devid 无法用模式表达（如 "dev.raw" 会被拆三段拒载）。
local OUT_DEV, OUT_TAG = "agg1", "temp_avg"

function on_init()
    -- 窗口大小可配：kv cfg_window 优先（tonumber 容错），缺省用脚本默认。
    -- 改 kv 后 POST /api/v1/lua/reload?inst_id=<id> 或等文件 watcher 热加载生效。
    local w = tonumber(ctx:kv_get("cfg_window"))
    if w and w >= 1 then
        WINDOW = math.floor(w)
    end
    ctx:log_info(("agg-filter started: window=%d clamp=[%d,%d] out=%s.%s")
        :format(WINDOW, CLAMP_LO, CLAMP_HI, OUT_DEV, OUT_TAG))
end

function on_rtdata(devid, tag, value)
    -- 只加工数值点（字符串/布尔点本例不处理）。
    if type(value) ~= "number" then
        return
    end
    -- 1) 限幅清洗：野点剔除（rejected 计数可观测，坏数据不污染窗口）。
    if value < CLAMP_LO or value > CLAMP_HI then
        ctx:kv_set("rejected", (ctx:kv_get("rejected") or 0) + 1)
        ctx:log_warn(("agg-filter 野点剔除: %s.%s=%s 超出 [%d,%d]")
            :format(devid, tag, value, CLAMP_LO, CLAMP_HI))
        return
    end
    -- 2) 滑动窗口：kv 持久（JSON table 往返），尾部进新点、超窗丢最旧。
    local win = ctx:kv_get("win") or {}
    win[#win + 1] = value
    while #win > WINDOW do
        table.remove(win, 1)
    end
    ctx:kv_set("win", win)
    -- 3) 聚合输出：窗口均值写派生点（set_value → hub → pipeline 全链路入库）。
    local sum = 0
    for _, v in ipairs(win) do
        sum = sum + v
    end
    ctx:set_value(OUT_DEV, OUT_TAG, sum / #win)
end
