// frontend/src/types.ts

/** rs-iot 实例状态。来源：`GET /api/v1/inst/stats`（经 gateway `/api/rsiot/api/v1/inst/stats`）。 */
export interface InstanceStat {
  /** 实例 ID。 */
  inst_id: string
  /** 插件 app_code（如 "modbusClient"）。 */
  app_code: string
  /** 宿主类型："builtin" / "external"。 */
  host_kind: string
  /** 进程级状态：running / stopped / failed / starting / stopping（serde lowercase）。 */
  state: string
  /** 插件级 health（lux stat:<inst> JSON），无则 null。 */
  health: InstanceHealth | null
}

/** 实例 health 快照（来源：rs-iot `db::stat::InstanceStat`）。 */
export interface InstanceHealth {
  /** health 维度："running" / "degraded" / "stopped"。 */
  status: string
  /** 累计活动成功次数（counter）。 */
  tick_count: number
  /** 累计活动失败次数（counter）。 */
  fail_count: number
  /** 最近一次活动墙上时间（unix 秒）。 */
  last_activity_ts: number
  /** 最近一次活动耗时（毫秒）。 */
  last_activity_duration_ms: number
  /** 最近一次失败原因（成功时清空）。 */
  last_err: string
}

/** SSE `log` 事件 payload（与 `/api/v1/logs/recent` 同构）。ts 为 ISO8601 字符串。 */
export interface LogEntry {
  ts: string
  level: string
  target: string
  message: string
  fields?: Record<string, string>
}

/** SSE `packet` 事件 payload。ts 为 ISO8601 字符串。 */
export interface PacketEntry {
  inst_id: string
  dir: 'tx' | 'rx'
  hex: string
  ts: string
}

/** SSE `state` 事件 payload。ts 为 ISO8601 字符串。 */
export interface StateEvent {
  inst_id: string
  state: string
  app_code: string
  host_kind: string
  ts: string
}

/** rtdata cell：`[ts, localTime, value]`（经 lux JSON 序列化，数值可能是字符串）。 */
export type RtdataCell = [number, string, string | number]
