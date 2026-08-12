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

/**
 * serve 事件流帧（无 `event:` 头，kind 在 JSON body 内）。
 *
 * 与 rs-iot 的 SSE 不同（rs-iot 每帧带 `event:` 头、前端走
 * `addEventListener(kind)`）；serve 的 SSE 是裸 NDJSON，前端用 `onmessage` 接
 * 全部帧后按 body 内的 `kind` 分发。索引签名保留未知字段（usage /
 * completion_summary 等），避免 serve 新增 kind 时前端 parse 失败。
 */
export interface ServeEvent {
  /** 事件种类：turn_started / turn_phase / reasoning / text / tool_dispatch / tool_result / approval_request / turn_done 等。 */
  kind: string
  /** text / reasoning kind 的文本片段（增量）。 */
  text?: string
  /** turn_phase kind 的阶段标识。 */
  phase?: string
  /** tool_dispatch / tool_result 的工具调度元信息（serve eventwire.Tool）。 */
  tool?: {
    id: string
    name: string
    /** dispatch 携带的调用参数（字符串，常为 JSON 编码）。 */
    args?: string
    /** result 携带的工具输出（字符串，常为 JSON 或文本）。 */
    output?: string
    /** result 携带的错误信息（非空串表示工具失败——对标 serve `tool.err`）。 */
    err?: string
    /** dispatch 是否只读（不触发审批）。 */
    readOnly?: boolean
    /** partial 流式分片标记（serve tool.partial）。 */
    partial?: boolean
    /** 工具运行耗时（毫秒，result 携带）。 */
    durationMs?: number
    [k: string]: unknown
  }
  /** approval_request 的审批载荷（serve `eventwire.Approval`：写操作触发）。 */
  approval?: { id: string; tool: string; subject?: string; reason?: string; kind?: string }
  [k: string]: unknown
}

/** 单条聊天消息（对标 zacp ChatMessage，简化）。一条 assistant 消息聚合一个 turn。 */
export interface ChatMessage {
  /** 自增 ID（前端分配，无持久化）。 */
  id: number
  /** 角色：用户输入 / 助手回复。 */
  role: 'user' | 'assistant'
  /** 文本内容（assistant 消息由多个 text 事件增量拼接）。 */
  text: string
  /** 推理过程（reasoning 事件累积，可选）。 */
  reasoning?: string
  /** 本轮调用的工具列表（tool_dispatch 建立、tool_result 填充结果）。 */
  toolCalls?: ToolCallEntry[]
}

/** ChatMessage.toolCalls 元素类型（对标 serve eventwire.Tool 的前端投影）。 */
export interface ToolCallEntry {
  /** 工具调度 ID（serve tool.id，跨 dispatch/result 配对）。 */
  id: string
  /** 工具名（serve tool.name，如 rsiot_read_tags）。 */
  name: string
  /** 调用参数（dispatch 携带，字符串原样——常为 JSON 编码）。 */
  args?: string
  /** 工具输出（result 携带，字符串原样——常为 JSON 或文本）。 */
  result?: string
  /** 工具失败标记（result.err 非空时为 true）。 */
  error?: boolean
  /** 工具失败原因（result.err 原文，error=true 时展示）。 */
  errorMsg?: string
  /** 工具调用耗时（毫秒，result 携带）。 */
  durationMs?: number
}
