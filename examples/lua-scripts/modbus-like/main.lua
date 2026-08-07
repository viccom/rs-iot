-- Modbus TCP 协议（MBAP + FC03 读 / FC06 写）脚本示例。
-- ctx:request 一问一答；collect 主动读，on_write 处理写命令。
-- 配合 docs/lua-script-guide.md。

local function crc16(buf)
    -- Modbus RTU CRC16（TCP 无 CRC，此处示范 helper；RTU 传输时附在帧尾）。
    local s = 0
    for i = 1, #buf do
        s = (s ~ buf:byte(i)) & 0xFFFF
        for _ = 1, 8 do
            s = (s & 1 ~= 0) and ((s >> 1) ~ 0xA001) or (s >> 1)
        end
    end
    return s
end

function on_init()
    ctx:log_info("modbus-like started")
    ctx:kv_set("boot_at", ctx:time())
end

function collect()
    -- MBAP(tid=1,pid=0,len=6,uid=1) + FC03(fc=3,addr=0,qty=2)。
    local req = string.pack(">I2I2I2BBI2I2", 1, 0, 6, 1, 3, 0, 2)
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
