-- vendored from freeioe@1398d47 2026-08-27 (MIT) lualib/buffer/threshold.lua
-- 时间窗限流缓冲：窗内前 count 次放行（透传回调），超额走 on_drop 回调并丢弃。
--   local th = require('util.threshold').new(60, 10, cb, on_drop[, now_fn])
--   th:push(...)   -- 窗内限流；返回回调结果，满则 nil, "Droped as threshold reached!!"
--   th:count()     -- 窗内当前条数（自动先清过期）
-- 典型用法：告警/事件频控（如报警风暴时每分钟最多发 N 条，其余丢弃计数）。
-- 改动（rsiot stdlib 约定，设计 §3.3）：middleclass → 'oo.middleclass'；
-- ioe.time() → **now_fn 注入**（构造第 5 参，返回秒级数值；rsiot 沙箱白名单
-- 无 os 全局——脚本侧传 function() return ctx:time() end；非沙箱缺省 os.time）；
-- 类构造改工厂 threshold.new(...)；追加 _M.selftest；其余逐字保留。

local class = require 'oo.middleclass'

local buffer = class("_ThresHold_Buffer_LIB")

local _M = { _CLASS = buffer }

function _M.new(time_gap, count, callback, on_drop_callback, now_fn)
	return buffer(time_gap, count, callback, on_drop_callback, now_fn)
end

function buffer:initialize(time_gap, count, callback, on_drop_callback, now_fn)
	assert(time_gap ~= nil and count ~= nil and callback ~= nil)
	self._time_gap = time_gap
	self._count = count
	self._cb = callback
	self._on_drop = on_drop_callback
	-- 惰性回退（存 nil，调用时才索引 os）：rsiot 沙箱无 os 全局，构造期
	-- `now_fn or os.time` 在缺省分支即炸（nil index）——回退延迟到取时。
	self._now = now_fn
	self._buf = {}
end

function buffer:_now_time()
	if self._now then return self._now() end
	return os.time()
end

function buffer:clean()
	local now = self:_now_time()
	local buf = self._buf
	for i, v in ipairs(self._buf) do
		if now - v.time < self._time_gap then
			--- There no one expired
			if i == 1 then
				return
			end

			--- Only keep those who are not expired
			self._buf = table.move(buf, i, #buf, 1, {})
			return
		end
	end

	--- All expired
	self._buf = {}
end

function buffer:push(...)
	self:clean()
	if #self._buf < self._count then
		table.insert(self._buf, {time=self:_now_time(), data={...}})
		return self._cb(...)
	else
		if self._on_drop then
			self._on_drop(...)
		end
		return nil, "Droped as threshold reached!!"
	end
end

function buffer:count()
	self:clean()
	return #self._buf
end

--- selftest（rsiot stdlib 约定，设计 §3.3）：放行/限流/过期恢复三路。
--- 时钟显式注入（now_fn）：沙箱无 os 全局，注入是 rsiot 场景的正道；
--- 用可控假钟同时覆盖"过期恢复"路径（真实钟没法快进）。
function _M.selftest()
	local now = 1000000 -- 假钟（秒）
	local sent, drops = 0, 0
	local th = _M.new(3600, 3, function(...)
		sent = sent + 1
		return true
	end, function()
		drops = drops + 1
	end, function() return now end)
	-- 前 3 次放行
	for _ = 1, 3 do
		if not th:push("x") then return false, "pass" end
	end
	-- 第 4 次限流丢弃
	local r, e = th:push("y")
	if r ~= nil or not e:match("Droped") then return false, "drop" end
	if sent ~= 3 or drops ~= 1 then return false, "counts" end
	if th:count() ~= 3 then return false, "count()" end
	-- 假钟快进 2h：全部过期，恢复放行
	now = now + 7200
	if not th:push("z") then return false, "expire-recover" end
	if th:count() ~= 1 then return false, "after-expire count" end
	return true
end

return _M
