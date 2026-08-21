// frontend/src/stores/monitor.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useMonitorStore } from './monitor'
import { useAuthStore } from './auth'
import type { LogEntry, PacketEntry, StateEvent, RtdataEvent } from '../types'
import { PREVIEW_AUTH_TOKEN } from '../lib/previewSession'

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

  it('start wires SSE listeners for rtdata/state (M3: log/packet 改按需订阅)', () => {
    const auth = useAuthStore()
    auth.setAuth('tok-abc', 'admin')
    // 桩 fetch：start() 会立即调 loadInstances/loadInstanceConfigs/loadRtdata/loadSysstatus。
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{}', { status: 200 }),
    )
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
    // M3 改动：start() 只订 rtdata,state（log/packet 移到诊断 Tab 手动订阅）。
    expect(FakeEventSource.last!.url).toContain('types=rtdata,state')
    expect(FakeEventSource.last!.url).not.toContain('log')
    // rtdata 通知分发（Q6：带 samples 直合入 + 去抖补拉兜底）；state 仍联动实例状态。
    const evt: RtdataEvent = { dev_ids: ['devA'], count: 3, ts: 't' }
    expect(() => FakeEventSource.last!.emit('rtdata', evt)).not.toThrow()
    FakeEventSource.last!.emit('state', { inst_id: 'i1', app_code: 'a', host_kind: 'b', state: 'running', ts: 't' })
    expect(s.instances[0]).toMatchObject({ inst_id: 'i1', state: 'running' })
    // start() 不订 log/packet，emit 这两类到 rtdata SSE 不应填充 ring buffer。
    s.stop()
    fetchSpy.mockRestore()
  })

  it('onRtdataNotify 带 samples 直合入 rtdata store（Q6 带值直用），缺 samples 行为不变', () => {
    // 锁的不变量：payload 带 samples 时按 loadRtdata 产物形状合入——cell `[ts, localTime, value]`，
    // field key 对齐后端落库规则（tag 已含 devid 时原样，否则 `${devid}.${tag}`），
    // value 字符串化（对齐 lux cell 的 valStr），localTime 为 "YYYY-MM-DD HH:mm:ss" 本地时间。
    // 缺 samples（旧后端/lagged）：不写 rtdata、不抛错（去抖补拉路径照旧）。
    const s = useMonitorStore()
    const evt: RtdataEvent = {
      dev_ids: ['devA'], count: 2, ts: 't',
      samples: [
        { devid: 'devA', values: { temp: 1.5, ok: true, label: 'x', 'devA.dup': 7 }, ts: 1700000000 },
      ],
    }
    s.onRtdataNotify(evt)
    const dev = s.rtdata['devA']
    expect(dev).toBeTruthy()
    expect(dev['devA.temp']).toEqual([1700000000, expect.any(String), '1.5'])
    expect(dev['devA.ok']).toEqual([1700000000, expect.any(String), 'true'])
    expect(dev['devA.label']).toEqual([1700000000, expect.any(String), 'x'])
    // tag 已含 devid → field 原样（对齐 db::rtdata write_batch 的 field 规则）。
    expect(dev['devA.dup']).toEqual([1700000000, expect.any(String), '7'])
    // localTime 形状 "YYYY-MM-DD HH:mm:ss"（对齐 chrono "%Y-%m-%d %H:%M:%S"）。
    expect(dev['devA.temp'][1]).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/)
    // 缺 samples：不写 rtdata、不抛错。
    const before = JSON.stringify(s.rtdata)
    expect(() => s.onRtdataNotify({ dev_ids: ['devB'], count: 1, ts: 't' })).not.toThrow()
    expect(JSON.stringify(s.rtdata)).toBe(before)
    // samples 空数组：同样不写（空批无意义，与缺失同路径）。
    expect(() => s.onRtdataNotify({ dev_ids: ['devB'], count: 1, ts: 't', samples: [] })).not.toThrow()
    expect(JSON.stringify(s.rtdata)).toBe(before)
    // 旧值保护（复审 P3-1）：SSE 迟到旧事件（ts 旧于现值）不倒退覆盖。
    s.onRtdataNotify({
      dev_ids: ['devA'], count: 1, ts: 't',
      samples: [{ devid: 'devA', values: { temp: 0.1 }, ts: 1699999000 }],
    })
    expect(s.rtdata['devA']['devA.temp'][2]).toBe('1.5')
    // 新值正常覆盖 + localTime 由样本 ts 推导（复审 P3-2：期望值用同 Date 逻辑
    // 推导——不依赖测试机时区，且证明与接收时刻无关：ts 是确定的历史/未来时刻）。
    const ts2 = 1800000000 // 2027 年某确定时刻
    s.onRtdataNotify({
      dev_ids: ['devA'], count: 1, ts: 't',
      samples: [{ devid: 'devA', values: { temp: 2.5 }, ts: ts2 }],
    })
    const d2 = new Date(ts2 * 1000)
    const p2 = (n: number) => String(n).padStart(2, '0')
    const expectLt = `${d2.getFullYear()}-${p2(d2.getMonth() + 1)}-${p2(d2.getDate())} ${p2(d2.getHours())}:${p2(d2.getMinutes())}:${p2(d2.getSeconds())}`
    expect(s.rtdata['devA']['devA.temp']).toEqual([ts2, expectLt, '2.5'])
    s.stop()
  })

  it('subscribeLog 激活日志 SSE + 初始拉取 logs/recent（M3 按需订阅）', async () => {
    // 锁的不变量：诊断 Tab 调 subscribeLog 后才建日志 SSE + 拉 logs/recent 历史。
    // start() 不再自动订日志——日志 Tab 不激活时 LogStream 永远空（用户要求"默认不激活"）。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fresh: LogEntry[] = [
      { ts: 't2', level: 'info', target: 't', message: 'm2' },
      { ts: 't1', level: 'warn', target: 't', message: 'm1' },
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
    s.start()
    // start 不订日志：此时 FakeEventSource.last 是 rtdata SSE，logs 应为空。
    const rtdataSse = FakeEventSource.last
    expect(s.logs.length).toBe(0)
    s.subscribeLog()
    // subscribeLog 建独立日志 SSE（FakeEventSource.last 现在指向它）。
    const logSse = FakeEventSource.last
    expect(logSse).not.toBe(rtdataSse)
    expect(logSse!.url).toContain('types=log,state')
    await new Promise(r => setTimeout(r, 0))
    // logs/recent 被调（subscribeLog 内初始拉历史）。
    expect(s.logs.length).toBe(2)
    expect(s.logs[0].message).toBe('m2')
    // 实时 log 帧分发到 ring buffer。
    logSse!.emit('log', { ts: 't', level: 'info', target: 't', message: 'live' })
    expect(s.logs[0].message).toBe('live')
    s.unsubscribeLog()
    expect(s.logSubscribed).toBe(false)
    s.stop()
    fetchSpy.mockRestore()
  })

  it('startPacketTrace POST inst/trace 激活当前实例报文采集', async () => {
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{"status":"ok","deadline_ms":123}', { status: 200 }),
    )
    const s = useMonitorStore()
    s.selectInstance('mb-tcp-2002')
    await expect(s.startPacketTrace(60)).resolves.toBe(true)
    // 验证意图：激活请求带正确 method/path/body（对标 routes_trace TraceReq）。
    const call = fetchSpy.mock.calls[0]
    expect(String(call[0])).toBe('/api/rsiot/api/v1/inst/trace')
    const init = call[1] as RequestInit
    expect(init.method).toBe('POST')
    expect(init.headers).toMatchObject({ Authorization: 'Bearer tok', 'Content-Type': 'application/json' })
    const body = JSON.parse(String(init.body))
    expect(body).toEqual({ inst_id: 'mb-tcp-2002', duration_secs: 60 })
    fetchSpy.mockRestore()
  })

  it('startPacketTrace 后端 404（实例未运行）返回 false——调用方据此不启动假倒计时', async () => {
    // 锁的不变量：激活失败必须可被调用方感知（返回 false），否则前端倒计时照跑、
    // 显示"已激活"误导现场排查（假激活 bug）。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('instance not running', { status: 404 }),
    )
    const s = useMonitorStore()
    s.selectInstance('stopped-inst')
    await expect(s.startPacketTrace(60)).resolves.toBe(false)
    fetchSpy.mockRestore()
  })

  it('startPacketTrace 未选实例时不发请求', async () => {
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{}', { status: 200 }),
    )
    const s = useMonitorStore()
    await s.startPacketTrace(60)
    expect(fetchSpy).not.toHaveBeenCalled()
    fetchSpy.mockRestore()
  })

  it('rtdata SSE onerror sets offline（M3: rtdata 断线置离线，轮询兜底数据正确性）', async () => {
    // 锁的不变量：rtdata 通知 SSE 断线 → offline=true（提示用户实时性降级）。
    // 数据正确性靠 2s 轮询兜底，无需断线补拉日志（M3 改动：log 补拉移到 subscribeLog）。
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
    expect(s.offline).toBe(false)
    FakeEventSource.last!.onerror!()
    expect(s.offline).toBe(true)
    s.stop()
    fetchSpy.mockRestore()
  })

  it('subscribeLog 的 SSE onerror 触发 loadLogsRecent 补拉合并去重', async () => {
    // 锁的不变量：日志 SSE（subscribeLog 建的独立连接）断线 → 补拉 logs/recent 把
    // 断线期间丢的帧补回 logs（去重）。对标 scada.html:945 的断线兜底（现在只在日志订阅激活时）。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const existing: LogEntry = { ts: 't1', level: 'info', target: 't', message: 'old' }
    const fresh: LogEntry[] = [
      { ts: 't3', level: 'warn', target: 't', message: 'new2' },
      { ts: 't2', level: 'info', target: 't', message: 'new1' },
      { ts: 't1', level: 'info', target: 't', message: 'old' },  // 重复
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
    s.start()
    s.onLog(existing)  // 预置已有日志（验去重）
    s.subscribeLog()
    await new Promise(r => setTimeout(r, 0))
    // subscribeLog 首次拉已合并 fresh（existing 重复不增）。length=3 (old/new1/new2)
    expect(s.logs.length).toBe(3)
    // 模拟日志 SSE 断线：触发 onerror 补拉。
    const logSse = FakeEventSource.last
    logSse!.onerror!()
    await new Promise(r => setTimeout(r, 0))
    // 补拉去重：无新增（fresh 已全合并），仍 3 条。
    expect(s.logs.length).toBe(3)
    expect(s.logs[0].message).toBe('new2')
    s.unsubscribeLog()
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

  it('start seeds preview snapshot and skips live connections for preview auth', () => {
    const auth = useAuthStore()
    auth.setAuth(PREVIEW_AUTH_TOKEN, 'preview')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{}', { status: 200 }),
    )
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }

    const s = useMonitorStore()
    s.start()

    expect(FakeEventSource.last).toBeNull()
    expect(fetchSpy).not.toHaveBeenCalled()
    expect(s.instances.length).toBeGreaterThan(0)
    expect(s.logs.length).toBeGreaterThan(0)
    expect(s.packets.length).toBeGreaterThan(0)
    expect(Object.keys(s.rtdata).length).toBeGreaterThan(0)
    expect(s.offline).toBe(false)
  })

  it('saveInstance POST /inst/set 整包写（后端无单点端点）', async () => {
    // 锁的不变量：保存实例走 POST /inst/set {inst_id, object: <完整 InstConfig>}，
    // 写后重拉配置 + 状态表。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{"status":"ok"}', { status: 200 }),
    )
    const s = useMonitorStore()
    const cfg = {
      inst_id: 'mb-1', inst_name: '', app_code: 'modbusClient', app_type: 'south' as const,
      auto_start: true, app_config: { kind: 'tcp' }, devices: [], host_kind: 'builtin' as const,
    }
    const ok = await s.saveInstance(cfg)
    expect(ok).toBe(true)
    const call = fetchSpy.mock.calls.find(c => String(c[0]).includes('/inst/set'))
    expect(call).toBeTruthy()
    const init = call![1] as RequestInit
    expect(init.method).toBe('POST')
    const body = JSON.parse(String(init.body))
    expect(body.inst_id).toBe('mb-1')
    expect(body.object.app_code).toBe('modbusClient')
    fetchSpy.mockRestore()
  })

  it('deleteInstance POST /inst/delete + 重拉', async () => {
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{"status":"ok"}', { status: 200 }),
    )
    const s = useMonitorStore()
    const ok = await s.deleteInstance('mb-1')
    expect(ok).toBe(true)
    const call = fetchSpy.mock.calls.find(c => String(c[0]).includes('/inst/delete'))
    expect(call).toBeTruthy()
    expect(JSON.parse(String((call![1] as RequestInit).body))).toEqual({ inst_id: 'mb-1' })
    fetchSpy.mockRestore()
  })

  it('purge POST /api/v1/purge 携带 scope 参数 + 重拉', async () => {
    // 锁的不变量：清空入口走 POST /purge {scope: "rtdata"|"all"}（后端级联/兜底清理，
    // 保留登录账号），成功后备重拉配置 + 状态表。
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{"status":"purged"}', { status: 200 }),
    )
    const s = useMonitorStore()
    expect(await s.purge('rtdata')).toBe(true)
    expect(await s.purge('all')).toBe(true)
    const rtdataCall = fetchSpy.mock.calls.find(c => String(c[0]).includes('/purge'))
    expect(rtdataCall).toBeTruthy()
    const bodies = fetchSpy.mock.calls
      .filter(c => String(c[0]).includes('/purge'))
      .map(c => JSON.parse(String((c[1] as RequestInit).body)))
    expect(bodies).toContainEqual({ scope: 'rtdata' })
    expect(bodies).toContainEqual({ scope: 'all' })
    fetchSpy.mockRestore()
  })

  it('startInstance/stopInstance 调对应端点', async () => {
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{"status":"ok"}', { status: 200 }),
    )
    const s = useMonitorStore()
    await s.startInstance('mb-1')
    await s.stopInstance('mb-1')
    const startCall = fetchSpy.mock.calls.find(c => String(c[0]).includes('/startinst'))
    const stopCall = fetchSpy.mock.calls.find(c => String(c[0]).includes('/stopinst'))
    expect(startCall).toBeTruthy()
    expect(stopCall).toBeTruthy()
    expect((startCall![1] as RequestInit).method).toBe('POST')
    fetchSpy.mockRestore()
  })

  it('loadRtdata 收到 401 清 token 跳登录（审查 Important 修复：打破无限 401 空转）', async () => {
    // 锁的不变量：token 失效后轮询不再静默 401 空转——handleUnauthorized 清 token
    // 并触发跳转（测试环境无 window，跳转被 typeof 守卫跳过，但 token 清除必须发生）。
    const auth = useAuthStore()
    auth.setAuth('tok-expired', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('unauthorized', { status: 401 }),
    )
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }
    const s = useMonitorStore()
    await s.loadRtdata()
    // 401 → token 被清（auth.clear），不再持有失效凭据空转。
    expect(auth.token).toBe('')
    // rtdata 不更新（401 响应不写入）。
    expect(Object.keys(s.rtdata).length).toBe(0)
    s.stop()
    fetchSpy.mockRestore()
  })

  it('loadRtdata 非 401 错误静默（网络抖动不误清 token）', async () => {
    // 边界：只有 401 触发会话失效处理，网络错误/500 保持静默重试语义。
    const auth = useAuthStore()
    auth.setAuth('tok-ok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('oops', { status: 500 }),
    )
    const s = useMonitorStore()
    await s.loadRtdata()
    expect(auth.token).toBe('tok-ok')  // 非 401 不清 token
    fetchSpy.mockRestore()
  })

  it('subscribePacket 激活报文 trace + 建 packet SSE（M3 按需）', async () => {
    const auth = useAuthStore()
    auth.setAuth('tok', 'admin')
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async () =>
      new Response('{"status":"ok","deadline_ms":123}', { status: 200 }),
    )
    ;(globalThis as unknown as { document: unknown }).document = {
      hidden: false, addEventListener: () => {}, removeEventListener: () => {},
    }
    const s = useMonitorStore()
    s.selectInstance('mb-tcp-2002')
    await s.subscribePacket()
    // 先 POST /inst/trace 激活，再建 packet SSE。
    expect(s.packetSubscribed).toBe(true)
    expect(FakeEventSource.last!.url).toContain('types=packet')
    const traceCall = fetchSpy.mock.calls.find(c => String(c[0]).includes('/inst/trace'))
    expect(traceCall).toBeTruthy()
    s.unsubscribePacket()
    expect(s.packetSubscribed).toBe(false)
    fetchSpy.mockRestore()
  })
})
