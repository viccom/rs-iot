// frontend/src/lib/pluginSchemas.ts
/**
 * 各南向插件 app_config 字段 schema（供 InstanceForm 表单化渲染）。
 *
 * **单位对齐（重要）**：rs-iot 全插件 `period` 单位统一是**秒**（见各插件
 * mod.rs 的 `Duration::from_secs(cfg.period)`）——非毫秒。历史 schema 误标 ms
 * 且默认 1000，会误导所有建实例操作（1000 秒 = 16 分钟一次）。现统一标"秒"
 * 且默认 1（1 秒采集一次）。
 *
 * 字段定义对标各插件 config.rs 的 parse_cfg（默认值与后端对齐）+ CLAUDE.md。
 * app_config 是自由 JSON（InstConfig.app_config），各插件自行解析；前端按
 * app_code 选 schema 渲染表单，未匹配的 app_code 回退 JSON 文本框。
 *
 * 字段类型决定表单控件：string→文本框 / number→数字框 / select→下拉 / bool→勾选。
 */

/** 表单字段类型。 */
export type FieldType = 'string' | 'number' | 'bool' | 'select'

/** 单个 app_config 字段定义。 */
export interface PluginField {
  /** JSON key（写入 app_config 的字段名）。 */
  key: string
  /** 显示标签（中文，含单位）。 */
  label: string
  /** 控件类型。 */
  type: FieldType
  /** select 类型的可选值。 */
  options?: string[]
  /** 默认值（新建实例时填入；与后端 parse_cfg 默认值对齐）。 */
  default?: unknown
  /** 是否必填（仅提示，后端最终校验）。 */
  required?: boolean
  /** 帮助文本（占位/说明）。 */
  hint?: string
}

/** 各插件 app_config 字段表（key = app_code）。 */
export const PLUGIN_SCHEMAS: Record<string, PluginField[]> = {
  // Modbus TCP/RTU/over-tcp/over-udp（modbus_client/config.rs）
  modbusClient: [
    { key: 'kind', label: '传输类型', type: 'select', options: ['tcp', 'rtu_serial', 'rtu_tcp', 'rtu_udp'], default: 'tcp', required: true },
    { key: 'endpoint', label: 'TCP 端点', type: 'string', hint: 'host:port（tcp/rtu_tcp/rtu_udp 用）' },
    { key: 'serial_port', label: '串口', type: 'string', hint: '如 COM3 / /dev/ttyUSB0（rtu_serial 用）' },
    { key: 'baud_rate', label: '波特率', type: 'number', default: 9600 },
    { key: 'data_bits', label: '数据位', type: 'number', default: 8 },
    { key: 'parity', label: '校验', type: 'select', options: ['none', 'even', 'odd', 'mark', 'space'], default: 'none' },
    { key: 'stop_bits', label: '停止位', type: 'number', default: 1 },
    { key: 'flow_control', label: '流控', type: 'select', options: ['none', 'software', 'hardware'], default: 'none' },
    { key: 'slave_id', label: '从站 ID', type: 'number', default: 1 },
    { key: 'period', label: '采集周期(秒)', type: 'number', default: 1, hint: '全插件统一秒（非毫秒）；修改后需重启实例生效' },
    { key: 'gap_threshold', label: '最大单空洞', type: 'number', hint: '相邻点最大寄存器空隙（合并批量读用）' },
    { key: 'max_regs', label: '单区间最大寄存器数', type: 'number', default: 100 },
    { key: 'reconnect_secs', label: '重连间隔(秒)', type: 'number', default: 3 },
    { key: 'poll_strategy', label: '轮询策略', type: 'select', options: ['round_robin', 'sequential'], default: 'round_robin' },
    { key: 'request_timeout_secs', label: '请求超时(秒)', type: 'number', default: 2 },
  ],
  // OPC UA（opcua_client）
  opcuaClient: [
    { key: 'endpoint', label: 'OPC UA 端点', type: 'string', hint: 'opc.tcp://host:port', required: true },
    { key: 'security_policy', label: '安全策略', type: 'select', options: ['none', 'Basic256Sha256'], default: 'none' },
    { key: 'period', label: '采集周期(秒)', type: 'number', default: 1, hint: '全插件统一秒（非毫秒）；修改后需重启实例生效' },
    { key: 'verify_server', label: '校验服务器证书', type: 'bool', default: false, hint: '生产环境建议开' },
  ],
  // 西门子 S7（s7_client）
  s7Client: [
    { key: 'endpoint', label: 'PLC 端点', type: 'string', hint: 'host:port（端口固定 102）', required: true },
    { key: 'plc_type', label: 'PLC 类型', type: 'select', options: ['s200', 's200smart', 's300', 's400', 's1200', 's1500', 'sinumerik828d'], default: 's1200' },
    { key: 'rack', label: '机架号', type: 'number' },
    { key: 'slot', label: '插槽号', type: 'number' },
    { key: 'period', label: '采集周期(秒)', type: 'number', default: 1, hint: '全插件统一秒（非毫秒）；修改后需重启实例生效' },
    { key: 'timeout_ms', label: '超时(ms)', type: 'number' },
    { key: 'auto_reconnect', label: '自动重连', type: 'bool', default: true },
  ],
  // Lua 脚本（lua_client/config.rs）
  // 注意：luaClient 沙箱无 os/io 库（脚本勿用 os.time()，随机数用 math.random 即可）；
  // collect 返回 {tag=val} 扁平模式（单设备）或 {devid={tag=val}} 嵌套模式（多设备，
  // 任一 value 是 table 即判嵌套）——详见 work/docs/system-manual.md。
  luaClient: [
    { key: 'script', label: '脚本目录名', type: 'string', hint: 'scripts/<name>，对应 main.lua', required: true },
    { key: 'kind', label: '链路类型', type: 'select', options: ['tcp', 'serial', 'udp'], default: 'tcp' },
    { key: 'endpoint', label: 'TCP/UDP 端点', type: 'string', hint: 'host:port（纯仿真无链路时填 rs-iot loopback 即可，collect 不依赖它）' },
    { key: 'serial_port', label: '串口', type: 'string', hint: 'serial 类型用' },
    { key: 'baud_rate', label: '波特率', type: 'number', default: 9600 },
    { key: 'data_bits', label: '数据位', type: 'number', default: 8 },
    { key: 'parity', label: '校验', type: 'select', options: ['none', 'even', 'odd'], default: 'none' },
    { key: 'stop_bits', label: '停止位', type: 'number', default: 1 },
    { key: 'period', label: '采集周期(秒)', type: 'number', default: 1, hint: '全插件统一秒（非毫秒）；修改后需重启实例生效' },
    { key: 'timeout_ms', label: '请求超时(ms)', type: 'number', default: 200 },
    { key: 'recv_idle_ms', label: '接收空闲(ms)', type: 'number', default: 50 },
    { key: 'reconnect_secs', label: '重连间隔(秒)', type: 'number', default: 5 },
    { key: 'mem_limit_mb', label: '内存上限(MB)', type: 'number', default: 32 },
    { key: 'hook_instr_limit', label: '指令数上限', type: 'number', default: 1000000 },
    { key: 'fail_threshold', label: '连续失败降级阈值', type: 'number', default: 10 },
  ],
}

/** 模拟采集（dataSim，demo 用）。dataSim 无 devices 会直接退出（必须配点表）。 */
export const DATASIM_SCHEMA: PluginField[] = [
  { key: 'period', label: '采集周期(秒)', type: 'number', default: 3, hint: '全插件统一秒（非毫秒）；修改后需重启实例生效' },
]

/** 内置 app_code 选项（新建实例下拉）。北向 mqttPub 不在此（实例管理聚焦南向采集）。 */
export const SOUTH_APP_CODES = ['modbusClient', 'opcuaClient', 's7Client', 'luaClient', 'dataSim']

/** 取某 app_code 的 schema；未匹配返空数组（InstanceForm 回退 JSON 文本框）。 */
export function schemaFor(appCode: string): PluginField[] {
  if (appCode === 'dataSim') return DATASIM_SCHEMA
  return PLUGIN_SCHEMAS[appCode] ?? []
}

/** 用 schema 默认值生成空 app_config（新建实例）。 */
export function defaultAppConfig(appCode: string): Record<string, unknown> {
  const cfg: Record<string, unknown> = {}
  for (const f of schemaFor(appCode)) {
    if (f.default !== undefined) cfg[f.key] = f.default
  }
  return cfg
}
