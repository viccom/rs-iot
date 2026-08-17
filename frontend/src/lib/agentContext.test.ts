import { describe, expect, it } from 'vitest'
import { buildAgentContextBlock } from './agentContext'

describe('buildAgentContextBlock', () => {
  it('formats page route, focus entity and visible facts for agent context', () => {
    const text = buildAgentContextBlock({
      routeTitle: '实时数据',
      routePath: '/telemetry',
      pageSummary: '正在查看 devA 的关键点位变化',
      selectedInstanceId: 'mb-power',
      selectedDeviceId: 'devA',
      facts: [
        '筛选关键字: temp',
        '可见点位 12 个',
        '最近更新时间 12:30:00',
      ],
    })

    expect(text).toContain('当前页面: 实时数据')
    expect(text).toContain('页面路径: /telemetry')
    expect(text).toContain('当前实例: mb-power')
    expect(text).toContain('当前设备: devA')
    expect(text).toContain('页面摘要: 正在查看 devA 的关键点位变化')
    expect(text).toContain('- 筛选关键字: temp')
    expect(text).toContain('- 可见点位 12 个')
  })
})
