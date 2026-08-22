import { describe, expect, it } from 'vitest'
import { classifyLogTarget, matchesLogSource } from './logSource'

describe('classifyLogTarget（诊断页来源分类：builtin 实例 vs 其余系统）', () => {
  it('builtin 插件 target → instance', () => {
    expect(classifyLogTarget('rs_iot::plugin::builtin::self_monitor')).toBe('instance')
    expect(classifyLogTarget('rs_iot::plugin::builtin::modbus_client')).toBe('instance')
    expect(classifyLogTarget('rs_iot::plugin::builtin::opcua_client')).toBe('instance')
    expect(classifyLogTarget('rs_iot::plugin::builtin::lua_client')).toBe('instance')
    expect(classifyLogTarget('rs_iot::plugin::builtin::lua_north')).toBe('instance')
    expect(classifyLogTarget('rs_iot::plugin::builtin::lua_transform')).toBe('instance')
  })

  it('编排层/脚本引擎/框架模块 → system', () => {
    expect(classifyLogTarget('rs_iot::plugin::instance')).toBe('system')
    expect(classifyLogTarget('rs_iot::plugin::lua_core::hotload')).toBe('system')
    expect(classifyLogTarget('rs_iot::plugin::stat_tracker')).toBe('system')
    expect(classifyLogTarget('rs_iot::api::routes_inst')).toBe('system')
    expect(classifyLogTarget('rs_iot::pipeline')).toBe('system')
  })

  it('近似前缀不误判（builtinx / builtin 模块本身 / 空串）', () => {
    expect(classifyLogTarget('rs_iot::plugin::builtinx::foo')).toBe('system')
    expect(classifyLogTarget('rs_iot::plugin::builtin')).toBe('system')
    expect(classifyLogTarget('')).toBe('system')
  })
})

describe('matchesLogSource', () => {
  it('all 恒真；instance/system 精确分流', () => {
    expect(matchesLogSource('rs_iot::api::routes', 'all')).toBe(true)
    expect(matchesLogSource('rs_iot::plugin::builtin::modbus_client', 'all')).toBe(true)
    expect(matchesLogSource('rs_iot::plugin::builtin::modbus_client', 'instance')).toBe(true)
    expect(matchesLogSource('rs_iot::plugin::builtin::modbus_client', 'system')).toBe(false)
    expect(matchesLogSource('rs_iot::api::routes', 'instance')).toBe(false)
    expect(matchesLogSource('rs_iot::api::routes', 'system')).toBe(true)
  })
})
