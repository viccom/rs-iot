// frontend/src/stores/monitor.ts
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import type {
  InstanceStat,
  LogEntry,
  PacketEntry,
  StateEvent,
  RtdataCell,
  RtdataEvent,
  InstConfig,
  InstanceConfigs,
} from '../types'
import { useAuthStore } from './auth'
import { createPreviewSnapshot, isPreviewAuthToken } from '../lib/previewSession'

/** 日志环形缓冲容量（最新在前）。 */
const MAX_LOGS = 500
/** 报文环形缓冲容量（最新在前）。 */
const MAX_PACKETS = 200
/** rtdata 通知触发轮询的去抖窗口（ms）——连续通知风暴只拉一次最新值。 */
const RTDATA_NOTIFY_DEBOUNCE_MS = 200

/**
 * 监控 store：rs-iot SSE 事件分发 + rtdata/sysstatus 轮询 + 实例 CRUD。
 *
 * **订阅模型（M3 改动）**：
 * - rtdata：登录即起 2s 轮询 + rtdata SSE 通知（通知仅触发立即补拉，不含 value）。
 * - log/packet：**默认不订阅**，由诊断 Tab 手动 `subscribeLog()`/`subscribePacket()`
 *   激活（用户要求"默认不激活推送，手动选实例激活后才推送"）。
 *
 * 事件来源 `GET /api/v1/events?types=...`（经 gateway `/api/rsiot/api/v1/events`），
 * 每帧带 `event:` 头，前端 `addEventListener(kind)` 分发。
 * token 走 query（EventSource 不能设 header，gateway auth 支持 ?token=<jwt>）。
 */
export const useMonitorStore = defineStore('monitor', () => {
  /** 实例列表（进程级 state + health，来自 /inst/stats）。 */
  const instances = ref<InstanceStat[]>([])
  /** 实例完整配置表（inst_id → InstConfig，来自 GET /insts；用于实例管理 + 点表类型映射）。 */
  const instanceConfigs = ref<InstanceConfigs>({})

  /**
   * 实例名册：**配置全集 ∪ 运行态**。`/inst/stats` 只含启动/注册过的实例
   * （auto_start=false 的实例不在其中），若名册单用 stats，未启动实例在管理页
   * 不可见不可管理。故以配置全集为底、stats 为运行态覆盖合并。
   */
  const roster = computed<InstanceStat[]>(() => {
    const statsById = new Map(instances.value.map(i => [i.inst_id, i]))
    const out: InstanceStat[] = []
    for (const [id, cfg] of Object.entries(instanceConfigs.value)) {
      const st = statsById.get(id)
      out.push(
        st ?? {
          inst_id: id,
          app_code: cfg.app_code,
          host_kind: cfg.host_kind,
          state: 'stopped',
          health: null,
        },
      )
    }
    // 运行态中存在但配置缺失的（理论不出现，防御性补全）。
    for (const i of instances.value) {
      if (!(i.inst_id in instanceConfigs.value)) out.push(i)
    }
    return out
  })
  /** 日志环形缓冲（最新在前，上限 MAX_LOGS）。 */
  const logs = ref<LogEntry[]>([])
  /** 报文环形缓冲（最新在前，上限 MAX_PACKETS）。 */
  const packets = ref<PacketEntry[]>([])
  /** rtdata 快照：`{devId: {tag: [ts, localTime, value]}}`。 */
  const rtdata = ref<Record<string, Record<string, RtdataCell>>>({})
  /** sysstatus 快照（dbsize / metrics / process 等）。 */
  const sysstatus = ref<Record<string, unknown>>({})
  /** 当前选中实例 ID（组件联动）。 */
  const selectedInstance = ref<string>('')
  /** 当前选中设备 ID（页面感知 AI / rtdata 联动）。 */
  const selectedDevice = ref<string>('')
  /** SSE(rtdata 通知) 连接是否断开。 */
  const offline = ref(false)
  /** 日志 Tab 是否已激活订阅。 */
  const logSubscribed = ref(false)
  /** 报文 Tab 是否已激活订阅。 */
  const packetSubscribed = ref(false)

  let sse: EventSource | null = null  // rtdata 通知 SSE（登录即起）
  let logSse: EventSource | null = null  // 日志 SSE（诊断 Tab 手动激活）
  let packetSse: EventSource | null = null  // 报文 SSE（诊断 Tab 手动激活）
  let rtdataTimer: ReturnType<typeof setInterval> | null = null
  let sysstatusTimer: ReturnType<typeof setInterval> | null = null
  /** 实例名册轮询：感知外部增删（如 AI 经 REST 直接改后端），SSE state 事件覆盖不到
   *  未运行实例的删除（stop no-op 不 emit）。5s 一次，补拉 instances + configs。 */
  let rosterTimer: ReturnType<typeof setInterval> | null = null
  /** rtdata 通知去抖句柄（风暴期间只触发一次 loadRtdata）。 */
  let rtdataDebounce: ReturnType<typeof setTimeout> | null = null

  /**
   * 401 会话失效处理（审查 Important 修复）。
   *
   * 本 store 刻意绕过 client.ts（其 import router 链破坏 vitest），故 client.ts 的
   * 401 清 token+跳登录分支对本 store 失效——token 失效后轮询/SSE 会无限 401 空转。
   * 此处统一兜底：清 token + 硬跳转 /login（整页刷新顺带清掉所有 SSE 僵尸连接）。
   * 不 import router（测试环境无 window/createWebHistory），用 location.href。
   * 仅在有 token 时跳转（防登录页自身 401 触发循环）。
   */
  function handleUnauthorized() {
    const auth = useAuthStore()
    if (!auth.token) return
    auth.clear()
    stop()
    if (typeof window !== 'undefined') window.location.href = '/login'
  }

  /** 收到一条日志：prepend 并截断到 MAX_LOGS。 */
  function onLog(e: LogEntry) {
    logs.value.unshift(e)
    if (logs.value.length > MAX_LOGS) logs.value.length = MAX_LOGS
  }

  /** 收到一帧报文：prepend 并截断到 MAX_PACKETS。 */
  function onPacket(e: PacketEntry) {
    packets.value.unshift(e)
    if (packets.value.length > MAX_PACKETS) packets.value.length = MAX_PACKETS
  }

  /** 收到实例状态变更：更新现有实例或追加新实例。 */
  function onState(e: StateEvent) {
    const i = instances.value.findIndex(x => x.inst_id === e.inst_id)
    if (i >= 0) {
      instances.value[i] = { ...instances.value[i], state: e.state }
    } else {
      instances.value.push({
        inst_id: e.inst_id,
        app_code: e.app_code,
        host_kind: e.host_kind,
        state: e.state,
        health: null,
      })
    }
  }

  /** 收到 rtdata 变更通知：去抖触发 loadRtdata 补拉最新值（不含 value，只作刷新触发）。 */
  function onRtdataNotify(_e: RtdataEvent) {
    if (rtdataDebounce) clearTimeout(rtdataDebounce)
    rtdataDebounce = setTimeout(() => {
      rtdataDebounce = null
      void loadRtdata()
    }, RTDATA_NOTIFY_DEBOUNCE_MS)
  }

  /** 拉取实例列表（进程级 state + health）。 */
  async function loadInstances() {
    if (isPreviewAuthToken(useAuthStore().token)) {
      applyPreviewSnapshot()
      return
    }
    try {
      const r = await fetch('/api/rsiot/api/v1/inst/stats', {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) instances.value = (await r.json()) as InstanceStat[]
    } catch { /* 轮询失败静默，下周期重试 */ }
  }

  /** 拉取实例完整配置表（GET /insts，含 app_config/devices；实例管理 + 类型映射用）。 */
  async function loadInstanceConfigs() {
    try {
      const r = await fetch('/api/rsiot/api/v1/insts', {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) instanceConfigs.value = (await r.json()) as InstanceConfigs
    } catch { /* 静默，CRUD 后会重拉 */ }
  }

  /** 拉取 rtdata 全量快照。 */
  async function loadRtdata() {
    if (isPreviewAuthToken(useAuthStore().token)) {
      applyPreviewSnapshot()
      return
    }
    try {
      const r = await fetch('/api/rsiot/api/v1/rtdatas', {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) rtdata.value = await r.json()
    } catch { /* 轮询失败静默，下周期重试 */ }
  }

  /** 拉取进程级 sysstatus。 */
  async function loadSysstatus() {
    if (isPreviewAuthToken(useAuthStore().token)) {
      applyPreviewSnapshot()
      return
    }
    try {
      const r = await fetch('/api/rsiot/api/v1/sysstatus', {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) sysstatus.value = await r.json()
    } catch { /* 轮询失败静默，下周期重试 */ }
  }

  /**
   * 补拉最近日志（SSE 断线兜底，对标 scada.html:945）。
   *
   * 返回数组最新在前，与 `logs` 形态一致；按 `ts|message` 去重，新条目 prepend 后截断到 MAX_LOGS。
   */
  async function loadLogsRecent() {
    if (isPreviewAuthToken(useAuthStore().token)) {
      applyPreviewSnapshot()
      return
    }
    try {
      const r = await fetch('/api/rsiot/api/v1/logs/recent?limit=200', {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) {
        const entries: LogEntry[] = await r.json()
        const seen = new Set(logs.value.map(l => `${l.ts}|${l.message}`))
        for (const e of [...entries].reverse()) {
          const k = `${e.ts}|${e.message}`
          if (!seen.has(k)) { logs.value.unshift(e); seen.add(k) }
        }
        if (logs.value.length > MAX_LOGS) logs.value.length = MAX_LOGS
      }
    } catch { /* 补拉失败静默 */ }
  }

  // ---- 实例 CRUD（模块 M1）----

  /**
   * POST JSON（带 JWT）。store 内部 helper——不复用 api/client.ts 的 `api()`
   * （后者 import router，测试环境 createWebHistory 需 window，会破坏 vitest）。
   * 返回 Response 或 null（fetch 抛错）。
   */
  async function postJson(path: string, body: unknown): Promise<Response | null> {
    try {
      const r = await fetch(path, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${useAuthStore().token}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify(body),
      })
      if (r.status === 401) { handleUnauthorized(); return null }
      return r
    } catch {
      return null
    }
  }

  /** 新建/修改实例（POST /inst/set 整包写，后端无单点端点）。写后重拉配置 + 状态。 */
  async function saveInstance(cfg: InstConfig): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/inst/set', { inst_id: cfg.inst_id, object: cfg })
    if (!r || !r.ok) return false
    await loadInstanceConfigs()
    await loadInstances()
    return true
  }

  /** 删除实例（POST /inst/delete，后端级联清 rtdata/stat/trace/运行 task）。删后重拉。 */
  async function deleteInstance(id: string): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/inst/delete', { inst_id: id })
    if (!r || !r.ok) return false
    await loadInstanceConfigs()
    await loadInstances()
    return true
  }

  /** 清空业务数据（POST /api/v1/purge）。
   *  scope='rtdata'：只清实时数据缓存（保留实例配置）；scope='all'：重置——
   *  删全部实例（级联清 rtdata/stat/trace）+ 兜底清孤儿，**保留登录账号**。 */
  async function purge(scope: 'rtdata' | 'all'): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/purge', { scope })
    if (!r || !r.ok) return false
    await loadInstanceConfigs()
    await loadInstances()
    return true
  }

  /** 启动实例（POST /startinst）。 */
  async function startInstance(id: string): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/startinst', { inst_id: id })
    if (r?.ok) await loadInstances()
    return !!r && r.ok
  }

  /** 停止实例（POST /stopinst）。 */
  async function stopInstance(id: string): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/stopinst', { inst_id: id })
    if (r?.ok) await loadInstances()
    return !!r && r.ok
  }

  // ---- 诊断 SSE 按需订阅（模块 M3）----

  /** 激活日志 SSE 订阅（诊断 Tab 手动）。订阅 log+state kind，并补拉最近日志。 */
  function subscribeLog() {
    if (logSse) return
    const tok = useAuthStore().token
    if (isPreviewAuthToken(tok)) { logSubscribed.value = true; return }
    logSse = new EventSource(
      `/api/rsiot/api/v1/events?types=log,state&token=${encodeURIComponent(tok)}`,
    )
    logSse.addEventListener('log', e => safe(() => onLog(JSON.parse((e as MessageEvent).data) as LogEntry)))
    logSse.addEventListener('state', e => safe(() => onState(JSON.parse((e as MessageEvent).data) as StateEvent)))
    logSse.onopen = () => { logSubscribed.value = true }
    logSse.onerror = () => { void loadLogsRecent() }
    void loadLogsRecent()  // 首次补拉历史
  }

  /** 关闭日志 SSE 订阅（离开诊断 Tab）。 */
  function unsubscribeLog() {
    logSse?.close()
    logSse = null
    logSubscribed.value = false
  }

  /** 激活报文 SSE 订阅（诊断 Tab 手动）。先 POST /inst/trace 激活当前实例抓样，再订 packet kind。 */
  async function subscribePacket() {
    if (packetSse) { packetSubscribed.value = true; return }
    // 先激活当前实例报文 trace（60s deadline），否则无 packet 事件。
    if (selectedInstance.value) await startPacketTrace(60)
    const tok = useAuthStore().token
    if (isPreviewAuthToken(tok)) { packetSubscribed.value = true; return }
    packetSse = new EventSource(
      `/api/rsiot/api/v1/events?types=packet&token=${encodeURIComponent(tok)}`,
    )
    packetSse.addEventListener('packet', e => safe(() => onPacket(JSON.parse((e as MessageEvent).data) as PacketEntry)))
    packetSse.onopen = () => { packetSubscribed.value = true }
    packetSse.onerror = () => { /* 浏览器自动重连 */ }
    packetSubscribed.value = true
  }

  /** 关闭报文 SSE 订阅。同时停当前实例的 trace 推送（后端 per-instance 注册表）。 */
  function unsubscribePacket() {
    packetSse?.close()
    packetSse = null
    packetSubscribed.value = false
    if (selectedInstance.value) void stopPacketTrace(selectedInstance.value)
  }

  /** 标签页隐藏时暂停轮询，恢复时继续（省资源 + 防后台节流）。 */
  function onVisibility() {
    if (document.hidden) {
      if (rtdataTimer) { clearInterval(rtdataTimer); rtdataTimer = null }
      if (sysstatusTimer) { clearInterval(sysstatusTimer); sysstatusTimer = null }
      if (rosterTimer) { clearInterval(rosterTimer); rosterTimer = null }
    } else if (!rtdataTimer || !sysstatusTimer || !rosterTimer) {
      rtdataTimer = setInterval(loadRtdata, 2000)
      sysstatusTimer = setInterval(loadSysstatus, 5000)
      rosterTimer = setInterval(() => { void loadInstances(); void loadInstanceConfigs() }, 5000)
    }
  }

  /**
   * 启动：rtdata 轮询 + rtdata SSE 通知 + sysstatus 轮询。
   *
   * **M3 改动**：不再自动订阅 log/packet（移到诊断 Tab 手动 subscribe*）。
   * rtdata SSE 通知（dev_ids 不含 value）仅触发 loadRtdata 去抖补拉，加速刷新感知。
   */
  function start() {
    if (sse) stop()
    const tok = useAuthStore().token
    if (isPreviewAuthToken(tok)) {
      applyPreviewSnapshot()
      offline.value = false
      return
    }
    // rtdata 变更通知 SSE（订阅 state 用于实例状态联动；log/packet 不在此订阅）。
    sse = new EventSource(
      `/api/rsiot/api/v1/events?types=rtdata,state&token=${encodeURIComponent(tok)}`,
    )
    sse.addEventListener('rtdata', e => safe(() => onRtdataNotify(JSON.parse((e as MessageEvent).data) as RtdataEvent)))
    sse.addEventListener('state', e => safe(() => onState(JSON.parse((e as MessageEvent).data) as StateEvent)))
    sse.onopen = () => { offline.value = false }
    sse.onerror = () => {
      offline.value = true
      // 浏览器自动重连；rtdata 轮询兜底数据正确性。
      // 立即探活一次：loadRtdata 带 401 检测——token 失效时尽快跳登录
      // （SSE 读不到状态码，EventSource 会携失效 token 无限重连，靠轮询打破死循环）；
      // 纯网络断则 fetch 抛错静默，offline 保持等浏览器重连。
      void loadRtdata()
    }
    void loadInstances()
    void loadInstanceConfigs()
    void loadRtdata()
    void loadSysstatus()
    rtdataTimer = setInterval(loadRtdata, 2000)
    sysstatusTimer = setInterval(loadSysstatus, 5000)
    rosterTimer = setInterval(() => { void loadInstances(); void loadInstanceConfigs() }, 5000)
    document.addEventListener('visibilitychange', onVisibility)
  }

  /**
   * 激活当前实例的报文 trace（POST /api/v1/inst/trace，对标 routes_trace::TraceReq）。
   *
   * 激活后该实例收发字节经 io_trace dump → EventBus Packet 帧 → SSE 推给本 store
   * （onPacket 填充 packets）。duration_secs 缺省 60（rs-iot 缺省同值）。
   * 返回是否激活成功（实例须 Running，后端否则 404）——调用方据此决定是否启动
   * 本地倒计时/提示，避免"假激活"误导现场排查。
   */
  async function startPacketTrace(durationSecs = 60): Promise<boolean> {
    if (!selectedInstance.value) return false
    try {
      const r = await fetch('/api/rsiot/api/v1/inst/trace', {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${useAuthStore().token}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ inst_id: selectedInstance.value, duration_secs: durationSecs }),
      })
      if (r.status === 401) { handleUnauthorized(); return false }
      if (!r.ok) {
        console.warn('monitor: activate packet trace failed', r.status)
        return false
      }
      return true
    } catch { return false }
  }

  /**
   * 关闭指定实例的报文 trace（DELETE /api/v1/inst/trace?inst_id=）。
   * 后端 trace 是 per-instance 注册表（多实例可并存，deadline 自动过期）——
   * 切换抓样目标时显式停旧实例推送（不等 60s 自然过期）；退订时同理。
   */
  async function stopPacketTrace(instId: string) {
    if (!instId) return
    try {
      const r = await fetch(`/api/rsiot/api/v1/inst/trace?inst_id=${encodeURIComponent(instId)}`, {
        method: 'DELETE',
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) { handleUnauthorized(); return }
      if (!r.ok) console.warn('monitor: deactivate packet trace failed', r.status)
    } catch { /* 关闭失败静默（deadline 兜底过期） */ }
  }

  /** SSE 帧 parse/handle 容错：畸形帧 warn 并跳过，不冒泡到 EventSource。 */
  function safe(fn: () => void) {
    try { fn() } catch (e) { console.warn('monitor: drop malformed SSE frame', e) }
  }

  /** 注入本地演示数据，供 UI/UX 评审使用。 */
  function applyPreviewSnapshot() {
    const previewSnapshot = createPreviewSnapshot()
    instances.value = previewSnapshot.instances
    logs.value = previewSnapshot.logs
    packets.value = previewSnapshot.packets
    rtdata.value = previewSnapshot.rtdata
    sysstatus.value = previewSnapshot.sysstatus
    if (!selectedInstance.value) {
      selectedInstance.value = previewSnapshot.selectedInstance
    }
    if (!selectedDevice.value) {
      selectedDevice.value = previewSnapshot.selectedDevice
    }
  }

  /** 停止所有 SSE + 清理定时器（登出 / 路由切走时调用）。 */
  function stop() {
    sse?.close()
    sse = null
    unsubscribeLog()
    unsubscribePacket()
    if (rtdataTimer) { clearInterval(rtdataTimer); rtdataTimer = null }
    if (sysstatusTimer) { clearInterval(sysstatusTimer); sysstatusTimer = null }
    if (rosterTimer) { clearInterval(rosterTimer); rosterTimer = null }
    if (rtdataDebounce) { clearTimeout(rtdataDebounce); rtdataDebounce = null }
    document.removeEventListener('visibilitychange', onVisibility)
  }

  /** 选中实例（组件联动）。 */
  function selectInstance(id: string) {
    selectedInstance.value = id
  }

  /** 选中设备（rtdata / AI 上下文联动）。 */
  function selectDevice(id: string) {
    selectedDevice.value = id
  }

  return {
    instances,
    roster,
    instanceConfigs,
    logs,
    packets,
    rtdata,
    sysstatus,
    selectedInstance,
    selectedDevice,
    offline,
    logSubscribed,
    packetSubscribed,
    onLog,
    onPacket,
    onState,
    onRtdataNotify,
    loadInstances,
    loadInstanceConfigs,
    loadRtdata,
    loadSysstatus,
    loadLogsRecent,
    saveInstance,
    deleteInstance,
    purge,
    startInstance,
    stopInstance,
    subscribeLog,
    unsubscribeLog,
    subscribePacket,
    unsubscribePacket,
    start,
    stop,
    startPacketTrace,
    stopPacketTrace,
    selectInstance,
    selectDevice,
  }
})
