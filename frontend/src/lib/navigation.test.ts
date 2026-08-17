import { describe, expect, it } from 'vitest'
import { PRIMARY_ROUTES } from './navigation'

describe('PRIMARY_ROUTES', () => {
  it('defines dashboard pages in navigation order', () => {
    expect(PRIMARY_ROUTES.map(route => route.key)).toEqual([
      'overview',
      'telemetry',
      'collectors',
      'diagnostics',
      'control',
      'lua',
    ])
    expect(PRIMARY_ROUTES.map(route => route.path)).toEqual([
      '/overview',
      '/telemetry',
      '/collectors',
      '/diagnostics',
      '/control',
      '/lua',
    ])
  })

  it('provides page metadata for workspace context and shell labels', () => {
    const telemetryRoute = PRIMARY_ROUTES.find(route => route.key === 'telemetry')

    expect(telemetryRoute).toMatchObject({
      key: 'telemetry',
      title: '实时数据',
      summary: '观察设备点位、异常波动与最新刷新状态',
    })
  })
})
