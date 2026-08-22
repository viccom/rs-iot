// frontend/src/lib/instRelations.test.ts
// 实例↔设备/脚本关联派生纯函数单测（方向 2：树形追溯数据源）。
import { describe, it, expect } from 'vitest'
import {
  relatedLuaInstances,
  buildDeviceOwnership,
  unclaimedDevices,
  instDeviceView,
} from './instRelations'
import type { InstConfig, InstanceStat, RtdataCell } from '../types'

// ---- 测试夹具 ----

function luaCfg(instId: string, script: string, devices: string[]): InstConfig {
  return {
    inst_id: instId,
    inst_name: instId,
    app_code: 'luaClient',
    app_type: 'south',
    auto_start: true,
    app_config: { script },
    devices: devices.map(d => ({
      label: d,
      value: d,
      children: [{ label: 'v', value: `${d}.v`, type: 'float' }],
      slave_id: null,
    })),
    host_kind: 'builtin',
  }
}

function modbusCfg(instId: string, devices: string[]): InstConfig {
  return {
    ...luaCfg(instId, '', devices),
    app_code: 'modbusClient',
    app_config: {},
  }
}

function stat(instId: string, state: string, runtimeDevices?: string[]): InstanceStat {
  return {
    inst_id: instId,
    app_code: 'luaClient',
    host_kind: 'builtin',
    state,
    health: runtimeDevices
      ? {
          status: 'running',
          tick_count: 1,
          fail_count: 0,
          last_activity_ts: 1,
          last_activity_duration_ms: 1,
          last_err: '',
          runtime_devices: runtimeDevices,
        }
      : null,
  }
}

const CELLS: Record<string, RtdataCell> = { 'v': [1, 't', 1] }

// ---- relatedLuaInstances：脚本 → 实例名单 ----

describe('relatedLuaInstances', () => {
  it('反查 app_code=luaClient 且 app_config.script 匹配的实例，roster 补运行态', () => {
    const cfgs = {
      lua1: luaCfg('lua1', 'devA', ['d1']),
      lua2: luaCfg('lua2', 'devA', ['d2', 'd3']),
      luaOther: luaCfg('luaOther', 'devB', ['d1']),
      mb1: modbusCfg('mb1', ['d9']),
    }
    const roster = [
      stat('lua1', 'running', ['d1', 'rt1']),
      stat('lua2', 'stopped'),
    ]
    const got = relatedLuaInstances(cfgs, roster, 'devA')
    expect(got.map(i => i.inst_id)).toEqual(['lua1', 'lua2'])
    expect(got[0]).toMatchObject({
      inst_id: 'lua1',
      state: 'running',
      deviceCount: 1,
      runtimeDevices: ['d1', 'rt1'],
    })
    expect(got[1]).toMatchObject({
      inst_id: 'lua2',
      state: 'stopped',
      deviceCount: 2,
      runtimeDevices: [],
    })
  })

  it('无实例引用返回空数组（未关联脚本）', () => {
    expect(relatedLuaInstances({ x: luaCfg('x', 'other', []) }, [], 'devA')).toEqual([])
  })

  it('roster 缺席的实例（未启动且无 stat）按 stopped 补全', () => {
    const cfgs = { lua1: luaCfg('lua1', 'devA', []) }
    const got = relatedLuaInstances(cfgs, [], 'devA')
    expect(got).toHaveLength(1)
    expect(got[0].state).toBe('stopped')
  })
})

// ---- buildDeviceOwnership：devId → 归属（配置 ∪ 运行时） ----

describe('buildDeviceOwnership', () => {
  it('归属 = 配置声明 ∪ 运行时上报（同实例去重）', () => {
    const cfgs = { lua1: luaCfg('lua1', 's', ['d1']) }
    const instances = [stat('lua1', 'running', ['d1', 'd5'])]
    const own = buildDeviceOwnership(cfgs, instances)
    expect(own['d1'].owners).toEqual(['lua1'])
    expect(own['d5'].owners).toEqual(['lua1'])
  })

  it('多实例配同一 devId → owners 为数组（修正“首个胜出”歧义）', () => {
    const cfgs = {
      a: luaCfg('a', 's', ['shared']),
      b: modbusCfg('b', ['shared']),
    }
    const own = buildDeviceOwnership(cfgs, [])
    expect(own['shared'].owners.sort()).toEqual(['a', 'b'])
    expect(own['shared'].appCodes.sort()).toEqual(['luaClient', 'modbusClient'])
  })

  it('跳过空 devId（实例配置设备 ID 未填的现场错误）', () => {
    const bad = luaCfg('a', 's', [''])
    expect(buildDeviceOwnership({ a: bad }, [])).toEqual({})
  })

  it('旧后端无 runtime_devices 字段时优雅降级为空集', () => {
    const cfgs = { lua1: luaCfg('lua1', 's', ['d1']) }
    const legacy = stat('lua1', 'running') // health 无 runtime_devices
    const own = buildDeviceOwnership(cfgs, [legacy])
    expect(own['d1'].owners).toEqual(['lua1'])
  })

  it('归属表 owner_int（单宿主）——selfMonitor 等无 runtime_devices 插件的运行时设备', () => {
    const cfgs = {} // selfMonitor devices 为空（tag 由代码生成）
    const instances = [stat('self', 'running')] // roster 有 self（app_code 供展示）
    const deviceOwners = { rsiot: 'self' }
    const own = buildDeviceOwnership(cfgs, instances, deviceOwners)
    expect(own['rsiot'].owners).toEqual(['self'])
  })

  it('归属表 owner_inst 多宿主逗号串拆分，与配置源并集去重', () => {
    const cfgs = { a: luaCfg('a', 's', ['shared']) }
    const instances = [stat('a', 'running'), stat('b', 'running')]
    const deviceOwners = { shared: 'a, b', rtdev3: 'b' }
    const own = buildDeviceOwnership(cfgs, instances, deviceOwners)
    expect(own['shared'].owners.sort()).toEqual(['a', 'b']) // a 配置+归属表双源不重复
    expect(own['rtdev3'].owners).toEqual(['b'])
  })

  it('归属表缺席/空串条目跳过（向后兼容两参调用）', () => {
    const cfgs = { lua1: luaCfg('lua1', 's', ['d1']) }
    expect(buildDeviceOwnership(cfgs, [], {})).toEqual({ d1: { owners: ['lua1'], appCodes: ['luaClient'] } })
    expect(buildDeviceOwnership(cfgs, [], { d2: '' })).toEqual({ d1: { owners: ['lua1'], appCodes: ['luaClient'] } })
  })
})

// ---- unclaimedDevices：孤儿设备（有数据、无任何归属） ----

describe('unclaimedDevices', () => {
  it('rtdata 有但配置/运行时均无归属 → 列出（多为已删实例残留）', () => {
    const own = buildDeviceOwnership({ a: luaCfg('a', 's', ['d1']) }, [])
    const rtdata = { d1: CELLS, orphan: CELLS }
    expect(unclaimedDevices(rtdata, own)).toEqual(['orphan'])
  })

  it('运行时归属的设备不算孤儿', () => {
    const own = buildDeviceOwnership({}, [stat('lua1', 'running', ['rt1'])])
    const rtdata = { rt1: CELLS }
    expect(unclaimedDevices(rtdata, own)).toEqual([])
  })
})

// ---- instDeviceView：实例 → 设备行（配置 ∪ 运行时 + 来源标注） ----

describe('instDeviceView', () => {
  const rtdata = {
    d1: CELLS,          // 配置 + 有数据
    rt1: CELLS,         // 仅运行时
    // d2：配置但无数据
  }

  it('配置 ∪ 运行时并集，来源标注 + hasData', () => {
    const cfg = luaCfg('i', 's', ['d1', 'd2'])
    const got = instDeviceView(cfg, rtdata, ['d1', 'rt1'])
    expect(got).toEqual([
      { devId: 'd1', tagCount: 1, source: 'configured', hasData: true },
      { devId: 'd2', tagCount: 1, source: 'configured', hasData: false },
      { devId: 'rt1', tagCount: 1, source: 'runtime-only', hasData: true },
    ])
  })

  it('runtime-only 设备 tagCount 取 rtdata 实际点数（无配置可数）', () => {
    const cfg = luaCfg('i', 's', [])
    const rt = { rt1: { a: CELLS.a, b: CELLS.a } }
    const got = instDeviceView(cfg, rt, ['rt1'])
    expect(got[0].tagCount).toBe(2)
  })

  it('设备排序稳定（配置序在前，运行时新增按字典序在后）', () => {
    const cfg = luaCfg('i', 's', ['zDev', 'aDev'])
    const got = instDeviceView(cfg, {}, ['bRt', 'aRt'])
    expect(got.map(r => r.devId)).toEqual(['zDev', 'aDev', 'aRt', 'bRt'])
  })
})
