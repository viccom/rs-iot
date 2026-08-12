// frontend/src/stores/monitor.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useMonitorStore } from './monitor'
import { useAuthStore } from './auth'
import type { LogEntry, PacketEntry, StateEvent } from '../types'

// jsdom 不实现 EventSource，store.start() 内会 new EventSource，测试需提供桩。
class FakeEventSource {
  static last: FakeEventSource | null = null
  static instances: FakeEventSource[] = []
  url: string
  onerror: (() => void) | null = null
  onopen: (() => void) | null = null
  private listeners: Record<string, ((e: { data: string }) => void)[]> = {}
  constructor(url: string) {
    this.url = url
    FakeEventSource.instances.push(this)
    FakeEventSource.last = this
  }
  addEventListener(kind: string, cb: (e: { data: string }) => void) {
    (this.listeners[kind] ||= []).push(cb)
  }
  emit(kind: string, data: unknown) {
    (this.listeners[kind] || []).forEach(cb => cb({ data: JSON.stringify(data) }))
  }
  /** 直发原始 data（不经 JSON.stringify）——测畸形帧专用。 */
  _emit(kind: string, raw: string) {
    (this.listeners[kind] || []).forEach(cb => cb({ data: raw }))
  }
  close() { /* no-op */ }
}

describe('monitor store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    FakeEventSource.instances = []
    FakeEventSource.last = null
    // jsdom 未装，auth.ts 初始化读 localStorage、setAuth 写 localStorage，提供最小桩。
    const store: Record<string, string> = {}
    ;(globalThis as { localStorage?: Storage }).localStorage = {
      getItem: (k: string) => store[k] ?? null,
      setItem: (k: string, v: string) => { store[k] = v },
      removeItem: (k: string) => { delete store[k] },
      clear: () => { for (const k of Object.keys(store)) delete store[k] },
      key: (i: number) => Object.keys(store)[i] ?? null,
      get length() { return Object.keys(store).length },
    } as Storage
    ;(globalThis as { EventSource?: unknown }).EventSource = FakeEventSource
  })

  it('onLog prepends log and caps at 500', () => {
    const s = useMonitorStore()
    for (let i = 0; i < 510; i++) {
      const e: LogEntry = { ts: String(i), level: 'info', target: 't', message: `m${i}` }
      s.onLog(e)
    }
    expect(s.logs.length).toBe(500)
    expect(s.logs[0].message).toBe('m509') // 最新在前
  })

  it('onPacket prepends packet and caps at 200', () => {
    const s = useMonitorStore()
    for (let i = 0; i < 210; i++) {
      const e: PacketEntry = { ts: String(i), dir: 'tx', hex: 'aa', inst_id: 'i' }
      s.onPacket(e)
    }
    expect(s.packets.length).toBe(200)
  })

  it('onState updates existing instance state', () => {
    const s = useMonitorStore()
    s.instances = [
      { inst_id: 'i1', app_code: 'modbusClient', host_kind: 'native', state: 'starting', health: null },
    ]
    const e: StateEvent = { inst_id: 'i1', app_code: 'modbusClient', host_kind: 'native', state: 'running', ts: '' }
    s.onState(e)
    expect(s.instances[0].state).toBe('running')
  })

  it('onState adds new instance when unknown', () => {
    const s = useMonitorStore()
    s.onState({ inst_id: 'i2', app_code: 'opcuaClient', host_kind: 'builtin', state: 'running', ts: '' })
    expect(s.instances.length).toBe(1)
    expect(s.instances[0]).toMatchObject({ inst_id: 'i2', state: 'running', health: null })
  })

  it('start wires SSE listeners for log/packet/state and surfaces URL with query token', () => {
    const auth = useAuthStore()
    auth.setAuth('tok-abc', 'admin')
    // 桩 fetch：start() 会立即调 loadInstances/loadRtdata/loadSysstatus（相对 URL，
    // Node undici 不接受相对 URL，且本测只验 SSE 连线，不验轮询）。每次调返回新
    // Response（Response body 只能读一次，三处 load 共享同一实例会报 Body unusable）。
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{}', { status: 200 }),
    )
    // 桩 document：start/stop 用 visibilitychange 监听 + document.hidden 读，jsdom 未装。
    const docStub = {
      hidden: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    }
    ;(globalThis as unknown as { document: unknown }).document = docStub
    const s = useMonitorStore()
    s.start()
    expect(FakeEventSource.last).not.toBeNull()
    expect(FakeEventSource.last!.url).toContain('/api/rsiot/api/v1/events')
    expect(FakeEventSource.last!.url).toContain('token=tok-abc')
    expect(FakeEventSource.last!.url).toContain('types=log,packet,state')
    // 事件分发到 ring buffer
    FakeEventSource.last!.emit('log', { ts: 't', level: 'info', target: 't', message: 'hello' })
    FakeEventSource.last!.emit('packet', { ts: 't', dir: 'rx', hex: '01', inst_id: 'i1' })
    FakeEventSource.last!.emit('state', { inst_id: 'i1', app_code: 'a', host_kind: 'b', state: 'running', ts: 't' })
    expect(s.logs.length).toBe(1)
    expect(s.logs[0].message).toBe('hello')
    expect(s.packets.length).toBe(1)
    expect(s.instances[0]).toMatchObject({ inst_id: 'i1', state: 'running' })
    s.stop()
    fetchSpy.mockRestore()
  })

  it('SSE onerror sets offline + loadLogsRecent补拉合并去重 + 复位 offline', async () => {
    // 锁的不变量：SSE 断线 → offline=true + 补拉 logs/recent 把断线期间丢的帧
    // 补回 logs（去重，不重复已有条目）→ 补拉成功后 offline=false。
    // 对标 scada.html:945 的断线兜底模式。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    // 桩 fetch：start() 的 load* + loadLogsRecent 都走 fetch；后者 URL 含 logs/recent。
    // 对非 logs/recent 请求返空对象（loadInstances/loadRtdata/loadSysstatus）；
    // 对 logs/recent 返 3 条（1 条与现有重复，2 条新）——验去重 + 合并。
    const existing: LogEntry = { ts: 't1', level: 'info', target: 't', message: 'old' }
    const fresh: LogEntry[] = [
      { ts: 't3', level: 'warn', target: 't', message: 'new2' },  // 最新
      { ts: 't2', level: 'info', target: 't', message: 'new1' },
      { ts: 't1', level: 'info', target: 't', message: 'old' },   // 与 existing 重复
    ]
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
      const url = String(input)
      if (url.includes('/logs/recent')) return new Response(JSON.stringify(fresh), { status: 200 })
      return new Response('{}', { status: 200 })
    })
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }
    const s = useMonitorStore()
    s.onLog(existing)  // 预置一条已有日志（验去重）
    expect(s.offline).toBe(false)
    s.start()
    // 模拟 SSE 断线：触发 onerror。
    expect(FakeEventSource.last).not.toBeNull()
    FakeEventSource.last!.onerror!()
    expect(s.offline).toBe(true)
    // onerror 内 void loadLogsRecent() 异步；loadLogsRecent 多层 await（fetch + r.json）
    // 需排空微任务队列——用 setTimeout(0) 兜底（比 Promise.resolve 嵌套稳）。
    await new Promise(r => setTimeout(r, 0))
    // 补拉成功复位 offline；新增 2 条（new2/new1）合并，old 不重复。
    expect(s.offline).toBe(false)
    expect(s.logs.length).toBe(3)
    // 最新在前：new2（补拉数组首项，最新）应在顶部。
    expect(s.logs[0].message).toBe('new2')
    expect(s.logs.map(l => l.message)).toEqual(['new2', 'new1', 'old'])
    s.stop()
    fetchSpy.mockRestore()
  })

  it('SSE onerror 补拉失败保持 offline（网络未恢复）', async () => {
    // 补拉失败（fetch reject / 网络）时 offline 不复位——下轮重连再补。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
      const url = String(input)
      if (url.includes('/logs/recent')) throw new Error('network down')
      return new Response('{}', { status: 200 })
    })
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }
    const s = useMonitorStore()
    s.start()
    expect(s.offline).toBe(false)
    FakeEventSource.last!.onerror!()
    expect(s.offline).toBe(true)
    await new Promise(r => setTimeout(r, 0))
    // 补拉失败 → offline 保持 true（等浏览器自动重连下轮再补）。
    expect(s.offline).toBe(true)
    s.stop()
    fetchSpy.mockRestore()
  })

  it('SSE onopen 复位 offline', () => {
    // 重连成功后 onopen 应复位离线状态（与 onerror 配对）。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{}', { status: 200 }),
    )
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }
    const s = useMonitorStore()
    s.start()
    s.offline = true  // 模拟先断线
    FakeEventSource.last!.onopen!()
    expect(s.offline).toBe(false)
    s.stop()
    fetchSpy.mockRestore()
  })

  it('selectInstance sets selectedInstance', () => {
    const s = useMonitorStore()
    s.selectInstance('foo')
    expect(s.selectedInstance).toBe('foo')
  })

  it('safe drops malformed SSE frame without throwing', () => {
    // safe() 兑现的不变量：畸形帧（JSON.parse 抛错）被 safe 吞掉，不让异常逸出
    // listener（逸出后该 kind 整类事件不再分发）。锁的是"parse 必须在 safe 内执行"。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{}', { status: 200 }),
    )
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }
    const s = useMonitorStore()
    s.start()
    // 模拟畸形帧：直发非 JSON data。
    expect(() => FakeEventSource.last!._emit('log', 'not json')).not.toThrow()
    // 畸形帧被丢，logs 不增长。
    expect(s.logs.length).toBe(0)
    s.stop()
    fetchSpy.mockRestore()
  })
})
