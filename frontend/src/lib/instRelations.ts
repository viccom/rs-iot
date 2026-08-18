// frontend/src/lib/instRelations.ts
// 实例↔设备/脚本关联派生纯函数（方向 2：树形追溯数据源）。
//
// 数据源全部为前端既有 store 状态（instanceConfigs / roster / rtdata），零后端新增请求。
// 归属口径：**配置声明 ∪ 运行时上报**（后端 luaClient stat extra 的 runtime_devices）。

import type { InstConfig, InstanceConfigs, InstanceStat, RtdataCell } from '../types'

/** 脚本 → 关联 lua 实例行（Lua.vue 三级树第二级）。 */
export interface LuaInstanceRef {
  inst_id: string
  /** 运行状态（roster 补全，缺席按 stopped）。 */
  state: string
  /** 配置设备数（InstConfig.devices.length）。 */
  deviceCount: number
  /** 运行时上报设备（health.runtime_devices，旧后端/未 flush 时空集）。 */
  runtimeDevices: string[]
}

/** 设备归属（多实例引用同一 devId 时 owners 为数组——修正“首个胜出”歧义）。 */
export interface DeviceOwnership {
  owners: string[]
  appCodes: string[]
}

/** 实例 → 设备行（Lua.vue 三级树第三级；source 两态：configured 已覆盖 both 语义，
 *  配置内设备无论运行时是否上报过都算 configured，是否采到数据由 hasData 表达）。 */
export interface InstDeviceRow {
  devId: string
  /** 配置设备取 children 数；runtime-only 取 rtdata 实际点数。 */
  tagCount: number
  source: 'configured' | 'runtime-only'
  hasData: boolean
}

/** 反查引用指定脚本的 lua 实例（app_code=luaClient 且 app_config.script 匹配）。 */
export function relatedLuaInstances(
  instanceConfigs: InstanceConfigs,
  roster: InstanceStat[],
  scriptName: string,
): LuaInstanceRef[] {
  const stateById = new Map(roster.map(i => [i.inst_id, i]))
  const out: LuaInstanceRef[] = []
  for (const cfg of Object.values(instanceConfigs)) {
    if (cfg.app_code !== 'luaClient') continue
    if (cfg.app_config?.script !== scriptName) continue
    const st = stateById.get(cfg.inst_id)
    out.push({
      inst_id: cfg.inst_id,
      state: st?.state ?? 'stopped',
      deviceCount: cfg.devices.length,
      runtimeDevices: st?.health?.runtime_devices ?? [],
    })
  }
  return out
}

/** devId → 归属映射（配置声明 ∪ 运行时上报，同实例去重，跳过空 devId）。 */
export function buildDeviceOwnership(
  instanceConfigs: InstanceConfigs,
  instances: InstanceStat[],
): Record<string, DeviceOwnership> {
  const m: Record<string, DeviceOwnership> = {}
  /** 去重追加一个归属（实例 + 插件名同序）。 */
  function add(devId: string, instId: string, appCode: string) {
    if (!devId) return
    const entry = (m[devId] ??= { owners: [], appCodes: [] })
    if (!entry.owners.includes(instId)) {
      entry.owners.push(instId)
      entry.appCodes.push(appCode)
    }
  }
  for (const cfg of Object.values(instanceConfigs)) {
    for (const dev of cfg.devices) {
      add(dev.value, cfg.inst_id, cfg.app_code)
    }
  }
  // 运行时上报归属（lua stat 的 runtime_devices；旧后端无此字段 → 空，优雅降级）。
  // 插件名取 roster 的 app_code（与配置侧同源）。
  for (const inst of instances) {
    for (const devId of inst.health?.runtime_devices ?? []) {
      add(devId, inst.inst_id, inst.app_code)
    }
  }
  return m
}

/** 孤儿设备：rtdata 有数据但无任何归属（配置/运行时均无——多为已删实例残留）。 */
export function unclaimedDevices(
  rtdata: Record<string, Record<string, RtdataCell>>,
  ownership: Record<string, DeviceOwnership>,
): string[] {
  return Object.keys(rtdata).filter(d => d && !ownership[d])
}

/** 实例设备视图：配置 ∪ 运行时并集 + 来源标注（配置序在前，运行时新增字典序在后）。 */
export function instDeviceView(
  cfg: InstConfig,
  rtdata: Record<string, Record<string, RtdataCell>>,
  runtimeDevs: string[],
): InstDeviceRow[] {
  const configured = new Set(cfg.devices.map(d => d.value).filter(Boolean))
  const rows: InstDeviceRow[] = cfg.devices
    .filter(d => d.value)
    .map(d => ({
      devId: d.value,
      tagCount: d.children.length,
      source: 'configured' as const,
      hasData: Boolean(rtdata[d.value] && Object.keys(rtdata[d.value]).length > 0),
    }))
  const extra = [...new Set(runtimeDevs)]
    .filter(d => d && !configured.has(d))
    .sort()
  for (const devId of extra) {
    rows.push({
      devId,
      tagCount: Object.keys(rtdata[devId] ?? {}).length,
      source: 'runtime-only',
      hasData: Boolean(rtdata[devId] && Object.keys(rtdata[devId]).length > 0),
    })
  }
  return rows
}
