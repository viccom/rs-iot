// frontend/src/stores/monitor.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { InstanceStat, LogEntry, PacketEntry, StateEvent, RtdataCell } from '../types'
import { useAuthStore } from './auth'
import { createPreviewSnapshot, isPreviewAuthToken } from '../lib/previewSession'

/** 日志环形缓冲容量（最新在前）。 */
const MAX_LOGS = 500
/** 报文环形缓冲容量（最新在前）。 */
const MAX_PACKETS = 200

/**
 * 监控 store：rs-iot SSE 事件分发（log/packet/state）+ rtdata/sysstatus 周期轮询。
 *
 * 事件来源 `GET /api/v1/events?types=log,packet,state`（经 gateway
 * `/api/rsiot/api/v1/events`），每帧带 `event:` 头，前端 `addEventListener(kind)` 分发。
 * rtdata/sysstatus 走普通 fetch（轮询），EventSource 无法设 header 故 token 走 query。
 */
export const useMonitorStore = defineStore('monitor', () => {
  /** 实例列表（进程级 state + health）。 */
  const instances = ref<InstanceStat[]>([])
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
  /** SSE 连接是否断开（onerror 置 true / onopen + 补拉成功复位 false）。 */
  const offline = ref(false)

  let sse: EventSource | null = null
  let rtdataTimer: ReturnType<typeof setInterval> | null = null
  let sysstatusTimer: ReturnType<typeof setInterval> | null = null

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
      if (r.ok) instances.value = (await r.json()) as InstanceStat[]
    } catch { /* 轮询失败静默，下周期重试 */ }
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
      if (r.ok) sysstatus.value = await r.json()
    } catch { /* 轮询失败静默，下周期重试 */ }
  }

  /**
   * 补拉最近日志（SSE 断线兜底，对标 scada.html:945）。
   *
   * SSE 断线期间丢的帧经 `GET /api/v1/logs/recent` 补回——返回数组最新在前，
   * 与 `logs` 形态一致；按 `ts|message` 去重，新条目 prepend 后截断到 MAX_LOGS。
   * 补拉成功（r.ok）复位 offline（说明网络恢复）；失败保持 offline，下轮重连再补。
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
      if (r.ok) {
        const entries: LogEntry[] = await r.json()
        const seen = new Set(logs.value.map(l => `${l.ts}|${l.message}`))
        // entries 最新在前；逐条 prepend 前反转，保持时间顺序（旧→新 unshift）。
        for (const e of [...entries].reverse()) {
          const k = `${e.ts}|${e.message}`
          if (!seen.has(k)) { logs.value.unshift(e); seen.add(k) }
        }
        if (logs.value.length > MAX_LOGS) logs.value.length = MAX_LOGS
        offline.value = false  // 补拉成功说明在线
      }
    } catch { /* 补拉失败保持 offline，下轮重连再补 */ }
  }

  /** 标签页隐藏时暂停轮询，恢复时继续（省资源 + 防后台节流）。 */
  function onVisibility() {
    if (document.hidden) {
      if (rtdataTimer) { clearInterval(rtdataTimer); rtdataTimer = null }
      if (sysstatusTimer) { clearInterval(sysstatusTimer); sysstatusTimer = null }
    } else if (!rtdataTimer || !sysstatusTimer) {
      rtdataTimer = setInterval(loadRtdata, 2000)
      sysstatusTimer = setInterval(loadSysstatus, 5000)
    }
  }

  /** 启动 SSE 订阅 + 周期轮询。登录后调用一次。 */
  function start() {
    // 重入守卫：避免 start() 二次调用泄漏上一组 SSE/timer（如登录→登出→再登录）。
    if (sse) stop()
    const tok = useAuthStore().token
    if (isPreviewAuthToken(tok)) {
      applyPreviewSnapshot()
      offline.value = false
      return
    }
    // token 走 query（EventSource 不能设 header，gateway auth 支持 ?token=<jwt>）。
    sse = new EventSource(
      `/api/rsiot/api/v1/events?types=log,packet,state&token=${encodeURIComponent(tok)}`,
    )
    // JSON.parse 容错：畸形帧不应让 EventSource listener 抛错（抛错后该 kind 不再分发）。
    // parse 须在 safe 内执行（thunk）——若作为 safe 实参先求值，parse 抛错会绕过 try。
    sse.addEventListener('log', e => safe(() => onLog(JSON.parse((e as MessageEvent).data) as LogEntry)))
    sse.addEventListener('packet', e => safe(() => onPacket(JSON.parse((e as MessageEvent).data) as PacketEntry)))
    sse.addEventListener('state', e => safe(() => onState(JSON.parse((e as MessageEvent).data) as StateEvent)))
    // SSE 连接成功 → 复位离线状态。
    sse.onopen = () => { offline.value = false }
    // onerror：置离线，浏览器自动重连；补拉 logs/recent 兜底断线期间丢帧
    // （对标 scada.html:945）。
    sse.onerror = () => {
      offline.value = true
      void loadLogsRecent()
    }
    // 立即拉一次 + 周期轮询。loadLogsRecent 必须初始拉：SSE 只推实时新帧，
    // 稳定运行（无新日志）时 LogStream 若只靠 SSE 会永远空白。
    void loadInstances()
    void loadRtdata()
    void loadSysstatus()
    void loadLogsRecent()
    rtdataTimer = setInterval(loadRtdata, 2000)
    sysstatusTimer = setInterval(loadSysstatus, 5000)
    document.addEventListener('visibilitychange', onVisibility)
  }

  /**
   * 激活当前实例的报文 trace（POST /api/v1/inst/trace，对标 routes_trace::TraceReq）。
   *
   * 激活后该实例收发字节经 io_trace dump → EventBus Packet 帧 → SSE 推给本 store
   * （onPacket 填充 packets）。duration_secs 缺省 60（rs-iot 缺省同值）。
   * 未选实例时 no-op（组件层按钮已禁用，此处防御直调）。
   */
  async function startPacketTrace(durationSecs = 60) {
    if (!selectedInstance.value) return
    try {
      const r = await fetch('/api/rsiot/api/v1/inst/trace', {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${useAuthStore().token}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ inst_id: selectedInstance.value, duration_secs: durationSecs }),
      })
      if (!r.ok) console.warn('monitor: activate packet trace failed', r.status)
    } catch { /* 激活失败静默，报文区显示空提示 */ }
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

  /** 停止 SSE + 清理定时器（登出 / 路由切走时调用）。 */
  function stop() {
    sse?.close()
    sse = null
    if (rtdataTimer) { clearInterval(rtdataTimer); rtdataTimer = null }
    if (sysstatusTimer) { clearInterval(sysstatusTimer); sysstatusTimer = null }
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
    logs,
    packets,
    rtdata,
    sysstatus,
    selectedInstance,
    selectedDevice,
    offline,
    onLog,
    onPacket,
    onState,
    loadInstances,
    loadRtdata,
    loadSysstatus,
    loadLogsRecent,
    start,
    stop,
    startPacketTrace,
    selectInstance,
    selectDevice,
  }
})
