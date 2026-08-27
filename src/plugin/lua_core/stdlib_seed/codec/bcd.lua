-- vendored from freeioe@1398d47 2026-08-20 (MIT) lualib/bcd.lua
-- BCD 编解码（仪表/表计协议常用）：每字节打包两个十进制位，高位在左。
--   bcd.encode(v, fmt) / bcd.decode(str, fmt)
--   fmt："X"=十进制位，"."分整数/小数段，"X4"="XXXX" 重复简写；
--   如 encode(1234,"XXXX")→"\x12\x34"、encode(123.45,"XXX.XX")→"\x01\x23\x45"、
--   decode("\x12\x34","XXXX")→1234（小数格式经浮点缩放，比较须带容差）。
-- 注意：encode 不给 fmt 时走 string.len(number) 分支，Lua 5.3+ 直接报错
-- （上游如此）——调用方必须显式给 fmt。
-- 改动（rsiot stdlib 约定，设计 §3.3）：删除原文件尾 require 时定义**全局**
-- test() 的死演示函数（唯一调用点本就被注释 `--test()`，且全局名 test 极易与
-- 脚本现场冲突）；追加 _M.selftest；其余逐字保留。

--- Format
-- XXXX.XX six
-- nil - formating integer as its length
local _M = {}

local function unfold_format(format)
	return string.gsub(format, "([XxNn#])(%d+)", function(c, num)
		local ret = {}
		for i = 1, num do
			ret[#ret + 1] = c
		end
		return table.concat(ret)
	end)
end

function _M.encode(v, format)
	local format = unfold_format(format or ""):gsub('x', 'X'):gsub('n', 'N')
	--print(format)
	local int_s, float_s = string.match(format, '([XN#]*).?([XN#]*)')
	local len = string.len(int_s) + string.len(float_s)
	local float_len = string.len(float_s)

	if len == 0 then
		len = string.len(math.floor(v))
	else
		v = v % ( 10 ^ string.len(int_s))
	end

	if float_len > 0 then
		v = math.floor(v * (10 ^ float_len))
	end

	local ret = {}
	local len = math.ceil(len / 2)
	for i = 1, len do
		local val = math.floor(v % 100)
		ret[len + 1 - i] = string.char( (val // 10) * 16 + val % 10)
		v = v // 100
	end
	return table.concat(ret)
end

function _M.decode(str, format)
	local format = unfold_format(format or ""):gsub('x', 'X'):gsub('n', 'N')
	local int_s, float_s = string.match(format, '([XN#]*).?([XN#]*)')
	local int_len, float_len = string.len(int_s), string.len(float_s)

	local v = 0

	for i = 1, #str do
		local val = string.byte(str, i)
		v = v * 100 + (val // 16) * 10 + val % 16
	end

	if (int_len + float_len) > 0 then
		v = v % (10 ^ (int_len + float_len))
	end

	if float_len > 0 then
		v = v * (0.1 ^ float_len)
	else
		--- convert to integer if format has no .
		v = math.floor(v)
	end

	return v
end

--- selftest（rsiot stdlib 约定，设计 §3.3）：
-- 已知向量：encode 1234/"XXXX" ↔ 0x12 0x34（表计读数形态，高位在左）；
-- 小数格式 "XXX.XX"：123.45 ↔ 01 23 45；重复简写 "X4"≡"XXXX"，数字字符串
-- 输入经数值强制同结果（上游 test() 演示的等价断言化）。
function _M.selftest()
	local enc = _M.encode(1234, "XXXX")
	local ok = enc == "\x12\x34"
		and _M.encode("1234", "X4") == "\x12\x34"
		and _M.decode("\x12\x34", "XXXX") == 1234
	local fenc = _M.encode(123.45, "XXX.XX")
	local fdec = _M.decode(fenc, "XXX.XX")
	return ok
		and fenc == "\x01\x23\x45"
		and math.abs(fdec - 123.45) < 1e-9   -- 小数经 0.1^2 缩放，浮点残差（实测 …002）
end

return _M
