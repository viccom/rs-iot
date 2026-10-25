// frontend/src/lib/schema-values.test.ts
// 镜像契约锁（审计 C1）：pluginSchemas 的 default 与后端 parse 常量逐值锚定。
// 后端侧锚点：modbus_client/config.rs DEFAULT_*、lua_core/config.rs
// DEFAULT_{PERIOD,TIMEOUT,RECV_IDLE,RECONNECT}、opcua/s7 mod.rs、
// lua_transform/config.rs（各带 defaults 锁定测试）。任一侧改值此处即红——
// 提醒双侧同步（长期方案：后端 schema 下发后本测试退役）。
import { describe, expect, it } from 'vitest'
import { PLUGIN_SCHEMAS as pluginSchemas } from './pluginSchemas'

function def(app: string, key: string): unknown {
  const f = pluginSchemas[app].find((x: { key: string }) => x.key === key)
  if (!f) throw new Error(`${app}.${key} 不在 schema 中`)
  return (f as { default?: unknown }).default
}

describe('pluginSchemas 默认值镜像锁（后端 DEFAULT_* 常量逐值锚定）', () => {
  it('modbusClient：period 3 / reconnect 2 / gap 16 / timeout 2 / max_regs 100（config.rs DEFAULT_*）', () => {
    expect(def('modbusClient', 'period')).toBe(3)
    expect(def('modbusClient', 'reconnect_secs')).toBe(2)
    expect(def('modbusClient', 'gap_threshold')).toBe(16)
    expect(def('modbusClient', 'request_timeout_secs')).toBe(2)
    expect(def('modbusClient', 'max_regs')).toBe(100)
    expect(def('modbusClient', 'baud_rate')).toBe(9600)
    expect(def('modbusClient', 'slave_id')).toBe(1)
  })
  it('opcuaClient：period 3（mod.rs DEFAULT_PERIOD_SECS）', () => {
    expect(def('opcuaClient', 'period')).toBe(3)
  })
  it('s7Client：period 2 / timeout 5000（mod.rs DEFAULT_*）', () => {
    expect(def('s7Client', 'period')).toBe(2)
    expect(def('s7Client', 'timeout_ms')).toBe(5000)
    expect(def('s7Client', 'plc_type')).toBe('s1200')
  })
  it('luaClient：period 3 / timeout 200 / recv_idle 50 / reconnect 5（lua_core/config.rs）', () => {
    expect(def('luaClient', 'period')).toBe(3)
    expect(def('luaClient', 'timeout_ms')).toBe(200)
    expect(def('luaClient', 'recv_idle_ms')).toBe(50)
    expect(def('luaClient', 'reconnect_secs')).toBe(5)
  })
  it('luaTransform：period 3 / callback 10000（parse_defaults 已锁）', () => {
    expect(def('luaTransform', 'period')).toBe(3)
    expect(def('luaTransform', 'callback_timeout_ms')).toBe(10000)
  })
})
