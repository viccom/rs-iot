export type WorkspaceRouteKey =
  | 'overview'
  | 'telemetry'
  | 'collectors'
  | 'diagnostics'
  | 'control'
  | 'lua'

export interface PrimaryRouteDefinition {
  key: WorkspaceRouteKey
  path: string
  title: string
  shortTitle: string
  summary: string
}

export const PRIMARY_ROUTES: PrimaryRouteDefinition[] = [
  {
    key: 'overview',
    path: '/overview',
    title: '全局总览',
    shortTitle: '总览',
    summary: '把握实例健康、实时流量与当前告警态势',
  },
  {
    key: 'telemetry',
    path: '/telemetry',
    title: '实时数据',
    shortTitle: '数据',
    summary: '观察设备点位、异常波动与最新刷新状态',
  },
  {
    key: 'collectors',
    path: '/collectors',
    title: '采集器',
    shortTitle: '采集器',
    summary: '查看采集实例状态、宿主类型与运行分布',
  },
  {
    key: 'diagnostics',
    path: '/diagnostics',
    title: '诊断中心',
    shortTitle: '诊断',
    summary: '排查日志、报文链路与系统运行指标',
  },
  {
    key: 'control',
    path: '/control',
    title: '控制台',
    shortTitle: '控制',
    summary: '结合 AI 助手与人工操作执行巡检和处置',
  },
  {
    key: 'lua',
    path: '/lua',
    title: 'Lua 脚本',
    shortTitle: '脚本',
    summary: '编辑/版本管理 lua 脚本，重载关联采集实例',
  },
]
