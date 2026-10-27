-- 标准 Modbus TCP 客户端（Lua 重写，验证 luaClient 表达力上限）。
-- FC03 读保持寄存器（addr=0, qty=2）；FC06 写单寄存器。
--
-- luaClient API 约定（Plan 1）：
--   - `ctx` 是**全局** table（由 host 注册：request/log_info/now/...），脚本直接引用，
--     不作为函数参数传入（call_collect 传 nil、call_on_write 传 cmd 作为首参；
--     若参数命名为 ctx 会 shadow 全局 → ctx:request 报错）。
--   - `collect()` 无参，返回 `{tag=value,...}` 表（value 为 number/bool/string）。
--   - `on_write(cmd)` 首参为命令表（host 将 data JSON 解析后传入，如 {addr=0,value=1}）。

function collect()
  -- MBAP(tid=1,pid=0,len=6,uid=1) + FC03(fc=3,addr=0,qty=2)，全大端。
  local req = string.pack(">I2I2I2BBI2I2", 1, 0, 6, 1, 3, 0, 2)
  local resp = ctx:request(req, 200)
  if (not resp) or #resp < 11 then
    return nil
  end
  -- 响应布局：MBAP(7) + fc(1) + bytecount(1) + data(qty*2)。
  -- 跳过前 9 字节，从第 10 字节读两个 u16（保持寄存器值）。
  local r0, r1 = string.unpack(">I2I2", resp:sub(10))
  return { temp = r0 / 10, speed = r1 }
end

function on_write(c)
  -- MBAP(tid=2,pid=0,len=6,uid=1) + FC06(fc=6,addr,value)。
  local req = string.pack(">I2I2I2BBI2I2", 2, 0, 6, 1, 6, c.addr, c.value)
  return ctx:request(req, 200) ~= nil
end
