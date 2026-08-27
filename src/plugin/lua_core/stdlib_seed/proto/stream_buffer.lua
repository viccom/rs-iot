---
-- 流缓冲区模块
--
-- 本模块提供用于模式匹配的流数据缓冲区。
-- 用于解析带有开始/结束标记的协议。
---

-- vendored from freeioe@1398d47 2026-08-20 (MIT) lualib/app/utils/stream_buffer.lua
-- 起止标记流分帧（串口/TCP 粘包拆包）：
--   local SB = require('proto.stream_buffer')
--   local buf = SB:new(max_len)
--   buf:append(data)                          -- 喂流（任意分片）
--   local frame, len = buf:find(sk, ek)       -- 取下一帧；nil+err 表示暂无完整帧
--   buf:pop(len)                              -- 消费该帧
--   buf:len() / buf:droped() / buf:clean()    -- 观测 / 复位
-- 实际语义（现场实测锚定，勿按直觉改）：
--   * find 返回的 frame **含起止标记**；载荷 = frame:sub(#sk+1, frame:find(ek, #sk+1, true)-1)。
--   * len = ek 起始位 + #ek，比「标记内帧长」多 1：frame 可能带 1 个标记外尾字节，
--     pop(len) 也会多吃 1 字节——上游如此（freeioe hwtest/stream/ping_pong.lua 靠
--     逐轮 clean() 规避连续流漂移）；连续多帧场景建议同样逐帧处理或帧后 clean()。
--   * droped 为**最近一次 find 丢弃的前缀噪声字节数**（赋值非累加）。
-- 改动（rsiot stdlib 约定，设计 §3.3）：require 'middleclass' → 'oo.middleclass'
-- （_stdlib 闭包内路由，保留段 oo/proto 均在封闭集）；追加 buffer:selftest()；
-- 其余逐字保留。

local class = require 'oo.middleclass'

---
-- 流缓冲区类
--
-- 缓冲流数据并提取由开始和结束标记字符串分隔的数据包。
---
local buffer = class("APP_UTILS_STREAM_BUFFER")

---
-- 初始化流缓冲区
-- @param max_len: 强制清理前的最大缓冲区长度
---
function buffer:initialize(max_len)
	self._buf = {}
	self._droped = 0
	self._max_len = max_len
end

---
-- 将所有缓冲区块连接为单个字符串
-- @return: 连接后的缓冲区字符串
---
function buffer:concat()
	if #self._buf > 1 then
		local buf = table.concat(self._buf)
		--- 保留缓冲区
		self._buf = { buf }
	end
	return self._buf[1]
end

---
-- 查找由开始和结束键分隔的数据包
-- @param sk: 开始键字符串
-- @param ek: 结束键字符串（可选）
-- @return: 数据包数据和长度，或nil和错误信息
---
function buffer:find(sk, ek)
	local buf = self:concat()

	if not buf or #buf == 0 then
		return nil, "Buffer empty"
	end

	if #buf <= #sk + (ek and #ek or 0) then
		return nil, "Buffer not enough"
	end

	--- 尝试查找开始键
	local pos = string.find(buf, sk, 1, true)

	--- 如果没有开始键
	if not pos then
		--- 丢弃噪声数据
		if #buf > #sk then
			if #sk > 1 then
				--- 丢弃的大小
				self._droped = #buf - #sk
				buf = string.sub(buf, 0 - #sk)
				self._buf = { buf }
			else
				self._droped = #buf
				buf = nil
				self._buf = {}
			end
		end

		return nil, "Start key not found"
	else
		self._droped = pos - 1
		buf = string.sub(buf, pos)
		self._buf = { buf }
	end

	if not ek then
		return buf, #buf
	end

	local pos = string.find(buf, ek, string.len(sk) + 1, true)

	if pos then
		local len = pos + #ek
		local data = string.sub(buf, 1, len)
		self._buf = { data }
		if #data < #buf then
			self._buf[#self._buf + 1] = string.sub(buf, len + 1)
		end
		return data, len
	end

	if #buf > self._max_len then
		buf = string.sub(buf, 2)
		self._droped = self._droped + 1
		self._buf = { buf }
		return self:find(sk, ek)
	end

	return nil, "End key not found"
end

---
-- 从缓冲区弹出指定长度
-- @param len: 要移除的字节数
---
function buffer:pop(len)
	local data = self._buf[1]
	assert(data, "NO DATA CAN POP")

	if #data == len then
		table.remove(self._buf, 1)
		return
	end

	if #data > len then
		self._buf[1] = string.sub(data, len + 1)
	else
		local buf = table.concat(self._buf)

		if #buf <= len then
			self._buf = {}
		else
			self._buf = { string.sub(buf, len + 1) }
		end
	end
end

---
-- 获取当前缓冲区长度
-- @return: 缓冲区中的总字节数
---
function buffer:len()
	local len = 0
	for _, v in ipairs(self._buf) do
		len = len + #v
	end
	return len
end

---
-- 获取丢弃的字节数
-- @return: 丢弃的字节计数
---
function buffer:droped()
	return self._droped
end

---
-- 将数据块追加到缓冲区
-- @param data: 要追加的数据字符串
---
function buffer:append(data)
	self._buf[#self._buf + 1] = data
end

---
-- 清理所有缓冲区内容
-- 将所有当前数据标记为已丢弃
---
function buffer:clean()
	if #self._buf == 0 then
		return
	end

	local buf = self:concat()
	if not buf or #buf == 0 then
		self._buf = {}
		return
	end

	self._droped = self._droped + #buf
	self._buf = {}
end

--- selftest（rsiot stdlib 约定，设计 §3.3；向量语义见文件头"实际语义"）：
-- 已知字节流 "JUNK AA 01 02 BB NOISE AA 03 BB"（sk=AA ek=BB）应分出载荷
-- {01 02}、{03} 两帧，第三次 find 报空；帧含起止标记（首帧实得 AA0102BB4E，
-- 带标记外尾字节 4E='N'——上游 off-by-one 语义，载荷按帧内 ek 定位提取）。
function buffer:selftest()
	local sk, ek = '\xAA', '\xBB'
	local b = buffer:new(256)
	b:append("JUNK\xAA\x01\x02\xBBNOISE\xAA\x03\xBB")

	local f1, l1 = b:find(sk, ek)
	if not f1 then return false end
	b:pop(l1)
	local f2, l2 = b:find(sk, ek)
	if not f2 then return false end
	b:pop(l2)
	local f3, err3 = b:find(sk, ek)

	--- 载荷 = 帧内 sk 之后、ek 之前（帧可能带标记外尾字节，不能按固定偏移切）
	local function payload(f)
		return f:sub(#sk + 1, f:find(ek, #sk + 1, true) - 1)
	end

	return payload(f1) == '\x01\x02'
		and payload(f2) == '\x03'
		and f3 == nil and err3 == 'Buffer empty'
		and b:len() == 0
		and b:droped() == 4        -- 最近一次 find 丢弃的前缀噪声（OISE），非累加
end

return buffer
