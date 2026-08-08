-- 单实例多设备示例：一条 RS485 总线挂多台从站（同协议、同参数、不同 unit id）。
-- collect 嵌套返回 {[devid]={tag=v,...}} → 宿主为每 devid 各产一个 Sample（M2）。
-- 配合 docs/lua-script-guide.md「多设备」节。

-- 构造 Modbus RTU 读保持寄存器请求（FC03）：unit + addr + qty + CRC16。
local function read_req(unit, addr, qty)
    local pdu = string.pack(">BBHH", unit, 3, addr, qty)
    -- CRC16（Modbus）附尾——实际 RTU 传输需；本示例演示组帧。
    local s = 0xFFFF
    for i = 1, #pdu do
        s = (s ~ pdu:byte(i)) & 0xFFFF
        for _ = 1, 8 do
            s = (s & 1 ~= 0) and ((s >> 1) ~ 0xA001) or (s >> 1)
        end
    end
    return pdu .. string.pack("<H", s)
end

function collect()
    local out = {}
    -- 遍历本实例设备树（devices[].value = devid），逐从站轮询。
    for _, dev in ipairs(ctx:list_devices()) do
        local unit = tonumber(ctx:kv_get("unit_" .. dev.value)) or 1  -- devid→unit 映射存 kv
        local resp = ctx:request(read_req(unit, 0, 2), 200)
        if resp and #resp >= 5 then
            -- 跳 unit(1)+fc(1)+bc(1)=3，解两寄存器（big-endian int16）。
            local r0 = string.unpack(">i2", resp:sub(4, 5))
            out[dev.value] = { voltage = r0 / 10 }   -- 嵌套：out[devid] = { tag = v }
        end
    end
    return out   -- {[meter1]={voltage=..}, [meter2]={voltage=..}} → 2 个 Sample
end
