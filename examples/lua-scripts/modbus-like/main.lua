-- Modbus TCP 协议（MBAP + FC03 读 / FC06 写）脚本示例。
-- ctx:request 一问一答；collect 主动读，on_write 处理写命令。
-- 配合 docs/guides/lua-script-guide.md。
--
-- @role collector
-- @api_ver 1
-- @stdlib >=1.0

-- 官方 stdlib（_stdlib/ 同步文件优先，离线回落 embed 种子）。
local crc16 = require("algo.crc16")

function on_init()
    ctx:log_info("modbus-like started")
    ctx:kv_set("boot_at", ctx:time())
end

function collect()
    -- MBAP(tid=1,pid=0,len=6,uid=1) + FC03(fc=3,addr=0,qty=2)。
    local req = string.pack(">I2I2I2BBI2I2", 1, 0, 6, 1, 3, 0, 2)
    -- TCP 帧无 CRC（MBAP 自带事务签名）；RTU 传输时：req = req .. crc16.crc16(req)
    -- （crc16.crc16 返回小端 2 字节串，可直接拼帧尾）。
    local resp = ctx:request(req, 200)
    if not resp or #resp < 11 then return nil end
    -- 跳 MBAP(7)+fc(1)+bc(1)=9，解两寄存器（big-endian int16）。
    local r0, r1 = string.unpack(">I2I2", resp:sub(10))
    return { temp = r0 / 10, speed = r1 }
end

function on_write(c)
    -- FC06 单寄存器写：MBAP(tid=2,...) + fc=6 + addr=c.addr + value=c.value。
    local req = string.pack(">I2I2I2BBI2I2", 2, 0, 6, 1, 6, c.addr, c.value)
    return ctx:request(req, 200) ~= nil
end
