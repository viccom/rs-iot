import type { InstanceStat, LogEntry, PacketEntry, RtdataCell } from '../types'

export const PREVIEW_AUTH_TOKEN = 'preview-demo-token'

export interface PreviewSnapshot {
  instances: InstanceStat[]
  logs: LogEntry[]
  packets: PacketEntry[]
  rtdata: Record<string, Record<string, RtdataCell>>
  sysstatus: Record<string, unknown>
  selectedInstance: string
  selectedDevice: string
}

export function isPreviewAuthToken(token: string): boolean {
  return token === PREVIEW_AUTH_TOKEN
}

export function createPreviewSnapshot(): PreviewSnapshot {
  return {
    instances: [
      {
        inst_id: 'mb-power',
        app_code: 'modbusClient',
        host_kind: 'builtin',
        state: 'running',
        health: {
          status: 'running',
          tick_count: 48120,
          fail_count: 3,
          last_activity_ts: 1723521600,
          last_activity_duration_ms: 18,
          last_err: '',
        },
      },
      {
        inst_id: 'opc-boiler',
        app_code: 'opcuaClient',
        host_kind: 'external',
        state: 'running',
        health: {
          status: 'degraded',
          tick_count: 18740,
          fail_count: 14,
          last_activity_ts: 1723521600,
          last_activity_duration_ms: 42,
          last_err: 'heartbeat jitter > threshold',
        },
      },
      {
        inst_id: 'bacnet-floor2',
        app_code: 'bacnetClient',
        host_kind: 'builtin',
        state: 'stopped',
        health: null,
      },
    ],
    logs: [
      {
        ts: '2026-08-13T11:20:48.000Z',
        level: 'warn',
        target: 'opc-boiler',
        message: 'Boiler-2 温度反馈波动偏大，建议检查采样稳定性',
      },
      {
        ts: '2026-08-13T11:20:16.000Z',
        level: 'info',
        target: 'mb-power',
        message: 'Power meter devA 已刷新 12 个关键点位',
      },
      {
        ts: '2026-08-13T11:19:53.000Z',
        level: 'error',
        target: 'bacnet-floor2',
        message: 'Floor2 采集器离线，等待人工恢复',
      },
    ],
    packets: [
      {
        ts: '2026-08-13T11:20:46.000Z',
        dir: 'rx',
        hex: '01 03 00 00 00 06 c5 c8',
        inst_id: 'mb-power',
      },
      {
        ts: '2026-08-13T11:20:40.000Z',
        dir: 'tx',
        hex: '18 ff 50 0e 61 72 6d 2d 31',
        inst_id: 'opc-boiler',
      },
      {
        ts: '2026-08-13T11:20:12.000Z',
        dir: 'rx',
        hex: '01 03 02 00 d6 39 84',
        inst_id: 'mb-power',
      },
    ],
    rtdata: {
      devA: {
        'devA.temp': [1723521648, '11:20:48', 76.8],
        'devA.pressure': [1723521648, '11:20:48', 1.24],
        'devA.power_kw': [1723521648, '11:20:48', 213.5],
      },
      devB: {
        'devB.temp': [1723521648, '11:20:48', 88.4],
        'devB.flow': [1723521648, '11:20:48', 14.1],
        'devB.status': [1723521648, '11:20:48', 'degraded'],
      },
      devC: {
        'devC.temp': [1723521648, '11:20:48', 23.2],
        'devC.humidity': [1723521648, '11:20:48', 44.8],
      },
    },
    sysstatus: {
      dbsize: 1287340032,
      process: {
        uptime_secs: 98234,
        cpu_pct: 18.4,
        mem_bytes: 532676608,
      },
      metrics: {
        pipeline_processed_count: 1429381,
        hub: {
          depth_last: 4,
        },
      },
    },
    selectedInstance: 'mb-power',
    selectedDevice: 'devA',
  }
}
