-- vendored from freeioe@1398d47 2026-08-19 (MIT) lualib/hwtest/crc16.lua
-- Modbus CRC16（多项式 0xA001 反射、初值 0xFFFF）：对 ADU 逐字节更新，返回
-- string.pack('I2', crc) 小端 2 字节串（可直接拼接在请求帧尾部）。
-- 改动（rsiot stdlib 约定，设计 §3.3）：匿名函数 → 模块表 M（M.crc16 + M.selftest）；
-- 仅保留 Lua 5.3+ 原生位运算分支（宿主 mlua lua54），删除 Lua 5.1/5.2 bit 库分支。

local M = {}

function M.crc16(adu)
	local crc;

	local function initCrc()
		crc = 0xffff;
	end
	local function updCrc(byte)
		crc = crc ~ byte
		for i = 1, 8 do
			local j = crc & 1
			crc = crc >> 1
			if j ~= 0 then
				crc = crc ~ 0xA001
			end
		end
	end

	initCrc();
	for i = 1, #adu  do
		updCrc(adu:byte(i));
	end
	return string.pack('I2', crc);
end

function M.selftest()
	-- 已知向量（独立 rustc u16 实现核验，该实现先通过 "123456789"→0x4B37 标准检查值
	-- 与 01 04 02 FF FF→0x80B8 参照帧双重自校验）：
	--   01 03 00 00 00 01 → CRC=0x0A84（线上字节 84 0A，低字节在前）
	--   01 04 02 FF FF    → CRC=0x80B8（线上字节 B8 80）
	-- 两侧都用 string.pack('I2', 期望值) 比较：断言数值本身，书写不依赖宿主字节序。
	local ok1 = M.crc16("\x01\x03\x00\x00\x00\x01") == string.pack("I2", 0x0A84)
	local ok2 = M.crc16("\x01\x04\x02\xFF\xFF") == string.pack("I2", 0x80B8)
	-- 编码自洽兜底：确定性 + 输出定长 2 字节
	local a = M.crc16("rsiot")
	return ok1 and ok2 and a == M.crc16("rsiot") and #a == 2
end

return M
