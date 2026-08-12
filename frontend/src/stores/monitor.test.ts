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
