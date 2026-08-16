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

/** rtdata 变更通知事件（SSE `rtdata` kind，pipeline flush_batch 批次级聚合）。
 *  dev_ids 去重 + 截断 50；count 是本批真实点数（未截断）。不含 value——前端收到后
 *  用现有 2s 轮询 /rtdatas 拉最新值，推送只作刷新触发（防 50万点/秒爆 SSE）。 */
export interface RtdataEvent {
  dev_ids: string[]
  count: number
  ts: string
}

/** 插件实例方向（对标 AppType serde：lowercase "south"/"north"，实测后端 reject "South"）。 */
export type AppType = 'south' | 'north'

/** 宿主形态（对标 HostKind serde：lowercase "builtin"/"external"）。 */
export type HostKind = 'builtin' | 'external'

/** 变量规格（对标 ChildSpec：label/value/type）。type 为 "float"/"int"/"bool" 等。 */
export interface ChildSpec {
  label: string
  value: string
  type: string
}

/** 设备规格（对标 DeviceSpec：label/value(=devId)/children/slave_id）。 */
export interface DeviceSpec {
  label: string
  value: string
  children: ChildSpec[]
  slave_id?: number | null
}

/**
 * 插件实例配置（对标 InstConfig）。app_config 为任意 JSON（各插件自行解析：
 * modbus/lua/opcua/s7 各自字段）。经 `GET /insts` 拉取、`POST /inst/set` 整包写。
 */
export interface InstConfig {
  inst_id: string
  inst_name: string
  app_code: string
  app_type: AppType
  auto_start: boolean
  app_config: Record<string, unknown>
  devices: DeviceSpec[]
  host_kind: HostKind
}

/** 实例配置表（GET /insts 返回：inst_id → InstConfig）。 */
export type InstanceConfigs = Record<string, InstConfig>

/** lua 脚本内容回读（GET /lua/script?name=）。 */
export interface LuaScript {
  name: string
  content: string
  size: number
  mtime?: number | null
}

/** lua 脚本历史版本（GET /lua/script/versions，按 ts 降序）。 */
export interface LuaVersion {
  ts: number
  size: number
}

/** lua 脚本清单项（含前端反查的关联实例数）。 */
export interface LuaScriptListItem {
  name: string
  /** 关联该脚本的 lua 实例数（前端从 instances 按 app_config.script==name 反查）。 */
  instanceCount: number
}

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
  /** ask_request 的结构化提问载荷（serve `eventwire.Ask`：AI 主动向用户提问，需回答才能继续）。 */
  ask?: {
    id: string
    questions: AskQuestion[]
  }
  [k: string]: unknown
}

/** serve `eventwire.AskOption`：一个选项（label/description）。 */
export interface AskOption {
  label: string
  description?: string
}

/** serve `eventwire.AskQuestion`：一个问题（多选/单选）。 */
export interface AskQuestion {
  id: string
  header?: string
  prompt: string
  options: AskOption[]
  multi?: boolean
}

/** serve `eventwire.AskAnswer`：对单个问题的回答（selected = 选中的 label 列表）。 */
export interface AskAnswer {
  question_id: string
  selected: string[]
}

/** LLM 供应商视图（gateway /api/agent/providers；密钥只报有无，不回传内容）。 */
export interface LLMProvider {
  /** 供应商名（[[providers]] name，如 "deepseek"）。 */
  name: string
  /** 协议族："openai" | "anthropic"（openai 兼容缺省）。 */
  kind: string
  /** API 基址（如 "https://api.deepseek.com/anthropic"）。 */
  base_url: string
  /** 可用模型 ID 列表。 */
  models: string[]
  /** 条目自带缺省模型（default 字段；缺省 Reasonix 取 models[0]）。 */
  default?: string
  /** 密钥环境变量名（.env 键）。 */
  api_key_env: string
  /** 密钥是否已配置（.env 有非空值）。 */
  has_key: boolean
}

/** 工作区目录项（gateway /api/agent/workspace，reasonix @ 引用选择器数据源）。 */
export interface WorkspaceEntry {
  name: string
  is_dir: boolean
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
  /** 系统提示（notice 事件归集，如 capability proxy 路由信息；历史回放不回填）。 */
  notices?: string[]
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

/**
 * serve 历史会话条目（GET /sessions，对标 serve sessionEntry，serve.go:1481-1487）。
 *
 * serve 是单会话进程：一个进程=一个活动会话，sessions 列表是磁盘上所有会话文件
 * 的扫描结果（含 current 标记当前活动会话）。切换会话=用 path 调 POST /resume，
 * 把整个进程的 active session 换成目标。
 */
export interface ServeSession {
  /** 会话名（文件名去 .jsonl 后缀）。 */
  name: string
  /** 会话文件绝对路径（POST /resume 用它定位）。 */
  path: string
  /** LLM 生成的短标题（可选）。 */
  title?: string
  /** 消息轮数（可选）。 */
  turns?: number
  /** 是否当前活动会话（前端据此高亮 + 启动恢复定位）。 */
  current?: boolean
}

/** serve 可选模型条目（GET /models，对标 serve modelEntry，serve.go:1264-1271）。 */
export interface ServeModel {
  /** 模型引用（`provider/model` 格式，submit `/model <ref>` 用它）。 */
  ref: string
  /** provider 名。 */
  provider: string
  /** 模型名。 */
  model: string
  /** provider kind（如 anthropic/openai）。 */
  kind?: string
  /** 是否当前活动模型。 */
  active?: boolean
  /** 是否默认模型。 */
  default?: boolean
}

/** serve 工具审批模式三档（POST /tool-approval-mode body {mode}，serve.go:1079）。 */
export type ToolApprovalMode = 'ask' | 'auto' | 'yolo'

/**
 * serve 当前会话状态快照（GET /status，serve.go:1370-1384）。
 *
 * 4 个控件的初始态来源：plan（Plan Mode）/toolApprovalMode（权限模式）。
 * 模型 active 态从 ServeModel.active 取；effort **不在 status 内**（serve 不暴露，
 * 见 ServeHistoryMessage 注释）——故 effort 控件用自由文本输入，无法回读。
 */
export interface ServeStatus {
  label: string
  running: boolean
  /** Plan Mode 开关（"执行方式"控件绑定）。 */
  plan: boolean
  autoApproveTools: boolean
  bypass: boolean
  /** 工具审批模式（"权限模式"控件绑定，三档）。 */
  toolApprovalMode: ToolApprovalMode
  goal?: string
  goalStatus?: string
  /** 会话目录（注意：serve 此字段实为 SessionDir 非工作目录）。 */
  cwd?: string
  used?: number
  window?: number
  cacheHit?: number
  cacheMiss?: number
  [k: string]: unknown
}

/** serve history 的工具调用项（GET /history，serve.go:838-845）。
 *  注意字段名是 **arguments**（非 args），且**无 output**——工具结果由独立的
 *  role=tool 消息携带 toolCallId 配对回填（historyToChatMessages 处理）。 */
export interface ServeHistoryToolCall {
  id: string
  name: string
  /** 调用参数（映射到 ToolCallEntry.args）。 */
  arguments?: string
}

/**
 * serve 历史消息条目（GET /history，serve.go:838-884）。
 *
 * role 可能是：`user`/`assistant`/`tool`/`notice`。tool 角色是工具结果（靠
 * toolCallId 配对到 assistant 的 toolCalls）；notice 是 steer 文本（跳过）。
 * historyToChatMessages 负责转 ChatMessage。
 */
export interface ServeHistoryMessage {
  role: string
  content: string
  reasoning?: string
  toolCalls?: ServeHistoryToolCall[]
  /** tool 角色消息携带，配对到对应 toolCall.id。 */
  toolCallId?: string
  toolName?: string
}
