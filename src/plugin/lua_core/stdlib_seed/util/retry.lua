-- vendored from freeioe@1398d47 2026-08-19 (MIT) lualib/utils/retry.lua
-- 可重试调用包装：retry(count, func, ...)——func 首返回值为 falsy 时重试至多 count 次。
-- 改动（rsiot stdlib 约定，设计 §3.3）：返回裸函数 → 模块表 M（M.retry + M.selftest）。

local unpack_table = table.unpack or unpack

local function retry(count, func, ...)
	local count = tonumber(count) or 0
	if count <= 0 then
		return func(...)
	end

	local args = {...}
	local rf = nil
	rf = function (r, ...)
		if not r and count > 0 then
			count = count - 1
			return rf(func(unpack_table(args)))
		end
		return r, ...
	end
	return rf(func(unpack_table(args)))
end

local M = {}
M.retry = retry

function M.selftest()
	-- 首调失败、次调成功：验真重试发生（calls==2 锁重试次数）且成功返回值透传。
	local calls = 0
	local r, v = M.retry(3, function()
		calls = calls + 1
		return calls >= 2, "rsiot"
	end)
	return r == true and v == "rsiot" and calls == 2
end

return M
