-- 自定义协议（STX/cmd/len/payload）+ on_data 被动广播上报示例。
-- collect 主动查询；on_data 收被动广播帧 → set_value 上报 + kv 持久化。
-- 配合 docs/lua-script-guide.md。

function collect()
    -- 主动查询：STX=0x02 cmd=0x01 len=0x03。
    local r = ctx:request("\x02\x01\x00\x03", 200)
    if r and #r >= 6 then
        return { level = r:byte(4) }
    end
    return nil
end

function on_data(frame)
    -- 被动广播：STX=0x02 cmd=0x10 → 心跳，set_value 上报 + kv 持久化（跨重启）。
    if frame:byte(1) == 0x02 and frame:byte(2) == 0x10 then
        local dev = ctx:list_devices()[1].value   -- 本实例首设备 devid（M2：set_value 需显式 devid）
        ctx:set_value(dev, "heartbeat", ctx:time())
        ctx:kv_set("last_heartbeat", ctx:time())
    end
end
