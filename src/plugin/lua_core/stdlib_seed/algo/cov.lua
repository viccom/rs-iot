-- vendored from freeioe@1398d47 2026-08-27 (MIT) lualib/cov.lua
-- COV（Change-On-Value）变化上报引擎：死区（float_threshold）/TTL 补发/质量戳
-- 变化三路判定，未变化值吞掉——脚本侧数据去重的标准件（与宿主侧
-- TagProps.deadband 互补：宿主管归档死区，本库管上报/触发死区）。
--   local cov = require('algo.cov')
--   local c = cov.new(function(key, value, ts, quality) ... end, {ttl = 60})
--   c:handle("dev1.temp", 23.5, os.time(), 0)  -- 首见触发；未超死区吞掉返回 true
--   c:timer(now)                               -- TTL 到期补发（on_timer 驱动）
--   c:snapshot() / c:fire_snapshot() / c:clean() / c:clean_with_match(fn)
--   c:handle_batch(datas[, fire_cb[, key_cb]]) -- 批量（默认 datas={{k1,k2,v,ts,q},...}）
-- 改动（rsiot stdlib 约定，设计 §3.3）：
--   * middleclass → 'oo.middleclass'、utils.table.equals → 'util.tablex.equals'
--     （_stdlib 闭包内路由，保留段 oo/util 均在封闭集）；
--   * 删除 skynet/ioe 宿主依赖：require 'skynet'/'skynet.coroutine'/'ioe' 与
--     utils.logger 移除；start/stop（skynet.fork 驱动循环）整体删除——rsiot
--     脚本无抢占调度，TTL 轮询由脚本在 on_timer 里手动调 cov:timer(now) 驱动
--     （now 用 os.time 或宿主时钟，秒级浮点）；log.error → 以 error 上抛替代
--     （值类型非法属编程错误，静默吞掉反而难查）；
--   * 类构造改工厂函数 cov.new(...)（保留原 initialize 语义）；
--   * 追加 _M.selftest；其余逐字保留。

local class = require 'oo.middleclass'
local tbl_equals = require('util.tablex').equals

local cov = class("_ChangeOnValue_LIB")

--- 工厂（rsiot 约定：模块表直暴露 new；class 实例 initialize 语义不变）
local _M = { _CLASS = cov }

function _M.new(cb, opt)
	assert(cb)
	return cov(cb, opt)
end

function cov:initialize(cb, opt)
	assert(cb)
	local opt = opt or {}

	opt.float_threshold = opt.float_threshold or 0.000001
	opt.try_convert_string = false

	opt.ttl = tonumber(opt.ttl)
	if opt.ttl and opt.ttl <= 0 then
		opt.ttl = nil
	end
	opt.min_ttl_gap = opt.min_ttl_gap or 10  -- 0.1 seconds

	self._cb = cb
	self._opt = opt
	self._retained_map = {}

	self._stop = nil
end

function cov:clean()
	self._retained_map = {}
end

function cov:clean_with_match(mfunc)
	for key, v in pairs(self._retained_map) do
		if mfunc(key) then
			self._retained_map[key] = nil
		end
	end
end

function cov:_handle_number(key, value, timestamp, quality, cb, nomore)
	assert(nomore==nil)
	assert(cb)
	local opt = self._opt
	local org_value = self._retained_map[key]
	local new_value = {value, timestamp, quality}

	if not org_value then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if opt.ttl and ((timestamp - org_value[2]) >= opt.ttl) then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if org_value[3] ~= quality then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if math.abs(value - org_value[1]) > opt.float_threshold then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end

	return true
end

function cov:_handle_string(key, value, timestamp, quality, cb, nomore)
	assert(nomore==nil)
	assert(cb)
	local opt = self._opt
	local org_value = self._retained_map[key]
	local new_value = {value, timestamp, quality}

	if not org_value then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if opt.ttl and ((timestamp - org_value[2]) >= opt.ttl) then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if org_value[3] ~= quality then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if value ~= org_value[1] then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end

	return true
end

function cov:_handle_table(key, value, timestamp, quality, cb, nomore)
	assert(nomore==nil)
	assert(cb)
	local opt = self._opt
	local org_value = self._retained_map[key]
	local new_value = {value, timestamp, quality}

	if not org_value then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if opt.ttl and ((timestamp - org_value[2]) >= opt.ttl) then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end
	if org_value[3] ~= quality then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end

	if not tbl_equals(org_value, new_value, true) then
		self._retained_map[key] = new_value
		return cb(key, value, timestamp, quality)
	end

	return true
end

function cov:_handle(key, value, timestamp, quality, cb, nomore)
	assert(nomore==nil)
	assert(key and value and timestamp)
	assert(cb)
	local opt = self._opt
	if opt.disable then
		return cb(key, value, timestamp, quality)
	end

	if type(value) == 'number' then
		return self:_handle_number(key, value, timestamp, quality, cb)
	elseif type(value) == 'string' then
		if opt.try_convert_string then
			local nval = tonumber(value)
			if nval then
				return self:_handle_number(key, value, timestamp, quality, cb)
			end
		end
		return self:_handle_string(key, value, timestamp, quality, cb)
	elseif type(value) == 'table' then
		return self:_handle_table(key, value, timestamp, quality, cb)
	else
		-- 上游 log.error 静默吞；rsiot 改为上抛（编程错误宜早爆，见头注）
		error('cov: value type error (' .. type(value) .. ')')
	end
end


function cov:handle(key, value, timestamp, quality, nomore)
	assert(nomore == nil)
	return self:_handle(key, value, timestamp, quality, self._cb)
end

---
-- return the changed data list
-- if fire_cb is true then call the cov handler callback
-- key_cb need return data's key (normally is sn, input, prop) and the offset
--	of data value item in list
--
function cov:handle_batch(datas, fire_cb, key_cb, nomore)
	assert(nomore == nil)
	local ret = {}
	local cb = fire_cb or function(...) return true end
	if key_cb then
		for _, v in ipairs(datas) do
			local key, v_s = key_cb(v)
			if key and v_s then
				-- assert(type(v_s) == 'number')
				if self:_handle(key, v[v_s], v[v_s + 1], v[v_s + 2], cb) then
					ret[#ret + 1] = v
				end
			else
				--- do nothing
			end
		end
	else
		for _, v in ipairs(datas) do
			local key = v[1]..'/'..v[2]
			if self:_handle(key, v[3], v[4], v[5], cb) then
				ret[#ret + 1] = v
			end
		end
	end
	return ret
end

function cov:fire_snapshot(cb)
	local cb = cb or self._cb
	for key, v in pairs(self._retained_map) do
		cb(key, table.unpack(v))
	end
end

function cov:snapshot()
	local ret = {}
	for key, v in pairs(self._retained_map) do
		ret[key] = {table.unpack(v)}
	end
	return ret
end

--- Call this timer function manually if you won't using start/stop method
-- @tparam now number Skynet time in seconds ( float )
-- @tparam cb function  Callback function for fire data out
-- @treturn number Skynet time in seconds ( float )
function cov:timer(now, cb)
	local cb = cb or self._cb
	local opt = self._opt
	local opt_ttl = opt.ttl
	-- rsiot 修补（上游 timer 裸调在无 ttl 时对 nil 做算术崩——上游靠 start() 的
	-- ttl 护栏规避，本版删除 start 后护栏归位到 timer 自身）：无 ttl 直接返回 nil。
	if not opt_ttl then return nil end
	local next_loop = opt_ttl
	-- Loop all inputs
	for key, v in pairs(self._retained_map) do
		-- Get current input next ttl fire time gap
		local tv = v[2]
		local quality = v[3] or 0
		local gap = opt_ttl - math.abs(now - tv)
		-- Fire data if reached the ttl
		if quality == 0 and gap <= 0 then
			v[2] = now
			local r = cb(key, table.unpack(v))
			if not r then
				--- Currently we skip this update???
				--- v[2] = tv
			end
			self._retained_map[key] = v
		end
		-- return min next time gap
		if quality == 0 and gap > 0 and next_loop > gap then
			next_loop = gap
		end
	end
	--return math.floor(next_loop)
	return next_loop
end

--- selftest（rsiot stdlib 约定，设计 §3.3）：死区/TTL/质量戳/批量/快照五路。
--- 注意 handle 返回值 = cb 返回值穿透（约定 cb 触发时 return false 以便区分：
--- false=触发，true=吞掉路径的上游返回——与 freeioe app 用法一致）。
function _M.selftest()
	local fired = {}
	local c = _M.new(function(key, value, ts, quality)
		fired[#fired + 1] = { key, value }
		return false
	end, { float_threshold = 0.5, ttl = 10 })

	-- 首见必触发（cb 返回 false 穿透）
	if c:handle("t", 1.0, 100, 0) ~= false then return false, "first fire" end
	-- 死区内吞掉（|1.0-1.2|<0.5），回调不增
	if c:handle("t", 1.2, 101, 0) ~= true then return false, "swallow" end
	if #fired ~= 1 then return false, "swallow fired=" .. #fired end
	-- 超死区触发
	if c:handle("t", 2.0, 102, 0) ~= false then return false, "delta fire" end
	-- 质量戳变化触发（同值）
	if c:handle("t", 2.0, 103, 1) ~= false then return false, "quality fire" end
	-- TTL 到期触发（同值同质，ts 差 ≥ ttl）
	if c:handle("t", 2.0, 113, 1) ~= false then return false, "ttl fire" end
	if #fired ~= 4 then return false, "fired=" .. #fired end
	-- 字符串路径：同值吞、变值发
	if c:handle("s", "a", 100, 0) ~= false then return false, "str first" end
	if c:handle("s", "a", 101, 0) ~= true then return false, "str swallow" end
	if c:handle("s", "b", 102, 0) ~= false then return false, "str fire" end
	-- 快照
	local snap = c:snapshot()
	if not snap["t"] or not snap["s"] then return false, "snapshot" end
	-- 批量（默认 key=v[1].."/"..v[2] 形态）：ret 语义 = "已处理的数据列表"
	-- （上游如此：_handle 返回真值即计入，无论是否触发 cb），非"触发列表"。
	local c2 = _M.new(function() return false end, { float_threshold = 1 })
	local changed = c2:handle_batch({ { "d", "x", 5, 100, 0 }, { "d", "x", 5, 101, 0 } })
	if #changed ~= 2 then return false, "batch changed=" .. #changed end
	-- timer 无 ttl 时不触发；有 ttl 未到期不触发
	if c2:timer(100) ~= nil then return false, "timer no-ttl" end
	local gap = c:timer(115)
	if type(gap) ~= "number" or gap <= 0 then return false, "timer gap" end
	return true
end

return _M
