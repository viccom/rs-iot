export type WorkspaceRouteKey =
  | 'overview'
  | 'dashboard'
  | 'telemetry'
  | 'collectors'
  | 'models'
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
    key: 'dashboard',
    path: '/dashboard',
    title: '工业驾驶舱',
    shortTitle: '驾驶舱',
    summary: '水/电/气/环境设备实时监控与历史趋势分析',
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
    key: 'models',
    path: '/models',
    title: '设备模型',
    shortTitle: '模型',
    summary: '物模型模板与全局设备注册表——设备/标签元数据唯一出生地',
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
