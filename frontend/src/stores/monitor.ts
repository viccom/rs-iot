// frontend/src/stores/monitor.ts
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import type {
  AlarmEvent,
  AlarmItem,
  InstanceStat,
  LogEntry,
  PacketEntry,
  StateEvent,
  RtdataCell,
  RtdataEvent,
  RtdataSample,
  InstConfig,
  InstanceConfigs,
  DeviceSpec,
  SysStatus,
} from '../types'
import { useAuthStore } from './auth'
import { apiFetch } from '../api/fetch'

/** 轮询节拍（审计 B6 具名化；start 与 visibility 恢复双分支共用同一组——
 *  原两处内联同值，调一处漏一处会出现隐藏/恢复后节拍不一致）。 */
const POLL_RTDATA_MS = 2000
const POLL_SYSSTATUS_MS = 5000
const POLL_ROSTER_MS = 5000
const POLL_ALARM_MS = 30_000

import { createPreviewSnapshot, isPreviewAuthToken } from '../lib/previewSession'

/** 日志环形缓冲容量（最新在前）。 */
const MAX_LOGS = 500
/** 报文环形缓冲容量（最新在前）。 */
const MAX_PACKETS = 200
/** 日志/报文 SSE 攒批窗口（ms）：每帧即时 unshift 会触发全列表响应式更新，
 *  高频日志（debug 级/多实例）下每秒几十次；攒批 flush 把渲染压到 ~2-3 次/秒。 */
const SSE_FLUSH_MS = 400
/** rtdata 通知触发轮询的去抖窗口（ms）——连续通知风暴只拉一次最新值。 */
const RTDATA_NOTIFY_DEBOUNCE_MS = 200

/**
 * 监控 store：rs-iot SSE 事件分发 + rtdata/sysstatus 轮询 + 实例 CRUD。
 *
 * **订阅模型（M3 改动）**：
 * - rtdata：登录即起 2s 轮询 + rtdata SSE 通知（Q6 带值：samples 直合入即时刷新，
 *   去抖补拉 + 轮询兜底 lagged/截断丢帧）。
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
  /** 后端归属表（GET /devices 条目的 owner_inst——dev2inst 全插件口径，
   *  devreg 设备与运行时设备均可能有归属）。 */
  const deviceOwners = ref<Record<string, string>>({})
  /** 设备聚合视图全量（GET /devices；M-2 起点表来自设备模型——类型映射/预览用）。 */
  const deviceCatalog = ref<Record<string, DeviceSpec>>({})
  /** sysstatus 快照（dbsize / metrics / process / history 等，类型见 types.ts SysStatus）。 */
  const sysstatus = ref<SysStatus>({})
  /** 活跃告警清单（最新在前；来源 GET /alarms 拉取 + SSE alarm 帧收敛，Q12 T5）。 */
  const alarms = ref<AlarmItem[]>([])
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
  /** 告警清单低频轮询（30s——复审 P2-2：SSE Lagged 丢 Fire 帧后面板会无限期缺条目，
   * rtdata 有 2s 轮询兜底而告警此前只有登录/onopen/手动三个触发点）。 */
  let alarmTimer: ReturnType<typeof setInterval> | null = null
  /** rtdata 通知去抖句柄（风暴期间只触发一次 loadRtdata）。 */
  let rtdataDebounce: ReturnType<typeof setTimeout> | null = null

  /**
   * 401 会话失效处理（审查 Important 修复）。
   *
   * 审计 B5：fetch 走 apiFetch（无 router 链，可测），401 语义留在本 store
   * 统一兜底：清 token + 硬跳转 /login（整页刷新顺带清掉所有 SSE 僵尸连接，
   * 与 api() 的软路由跳转不同——轮询/SSE 场景需要硬跳）。
   * 仅在有 token 时跳转（防登录页自身 401 触发循环）。
   */
  function handleUnauthorized() {
    const auth = useAuthStore()
    if (!auth.token) return
    auth.clear()
    stop()
    if (typeof window !== 'undefined') window.location.href = '/login'
  }

  /** 前端递增序号：SSE/补拉/预览注入时统一分配，作 v-for 稳定 key——
   *  同一 tick 同内容条目也不撞（此前用索引 key，prepend 下全列表错位重 patch）。 */
  let entrySeq = 0
  /** 攒批队列（到达序 = 旧→新）。 */
  const pendingLogs: LogEntry[] = []
  let logFlushTimer: ReturnType<typeof setTimeout> | null = null
  const pendingPackets: PacketEntry[] = []
  let packetFlushTimer: ReturnType<typeof setTimeout> | null = null

  /** 收到一条日志：入攒批队列，SSE_FLUSH_MS 后合并 prepend（截断到 MAX_LOGS）。 */
  function onLog(e: LogEntry) {
    e.seq = ++entrySeq
    pendingLogs.push(e)
    if (!logFlushTimer) logFlushTimer = setTimeout(flushLogs, SSE_FLUSH_MS)
  }

  /** 冲刷日志攒批队列（定时器到期/退订时调用）。 */
  function flushLogs() {
    if (logFlushTimer) { clearTimeout(logFlushTimer); logFlushTimer = null }
    if (!pendingLogs.length) return
    // 队列内旧→新，unshift 需新→旧序（保持 logs 最新在前语义）。
    const batch = pendingLogs.splice(0).reverse()
    logs.value.unshift(...batch)
    if (logs.value.length > MAX_LOGS) logs.value.length = MAX_LOGS
  }

  /** 收到一帧报文：入攒批队列（同 onLog 模式）。 */
  function onPacket(e: PacketEntry) {
    e.seq = ++entrySeq
    pendingPackets.push(e)
    if (!packetFlushTimer) packetFlushTimer = setTimeout(flushPackets, SSE_FLUSH_MS)
  }

  /** 冲刷报文攒批队列。 */
  function flushPackets() {
    if (packetFlushTimer) { clearTimeout(packetFlushTimer); packetFlushTimer = null }
    if (!pendingPackets.length) return
    const batch = pendingPackets.splice(0).reverse()
    packets.value.unshift(...batch)
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

  /**
   * SSE samples 直合入 rtdata store（Q6 带值直用，C 阶段 T1）。
   *
   * 值形状严格对齐 loadRtdata 产物：cell `[ts, localTime, value]`——
   * - ts 用样本自身时间戳（Unix 秒）；
   * - localTime 按后端 write_batch 同格式（chrono "%Y-%m-%d %H:%M:%S" 本地时间）生成；
   * - value 字符串化（对齐 lux cell 的 valStr——数字/布尔经 String()，字符串原样）；
   * - field key 对齐 db::rtdata 落库规则：tag 已含 devid 时原样，否则 `${devid}.${tag}`。
   * 仅合入推送值，不删既有 dev/tag（增量覆盖；全量刷新靠轮询整包替换）。
   */
  function applyRtdataSamples(samples: RtdataSample[]) {
    const p2 = (n: number) => String(n).padStart(2, '0')
    for (const s of samples) {
      const dev = rtdata.value[s.devid] ?? (rtdata.value[s.devid] = {})
      // localTime 由样本 ts 推导（复审 P3-2：此前用接收时刻——事件迟到/积压时
      // 显示时间与数据时刻偏差；与后端轮询产物同以数据时刻为准）。
      const t = new Date(s.ts * 1000)
      const localTime = `${t.getFullYear()}-${p2(t.getMonth() + 1)}-${p2(t.getDate())} ${p2(t.getHours())}:${p2(t.getMinutes())}:${p2(t.getSeconds())}`
      for (const [tag, v] of Object.entries(s.values)) {
        const field = tag.includes(s.devid) ? tag : `${s.devid}.${tag}`
        // 旧值保护（复审 P3-1）：SSE 迟到旧事件 vs 轮询已取新值的两条路径时序
        // 交叉，无条件覆盖会短暂倒退显示——ts 不新于现值时跳过（轮询兜底不变）。
        const cur = dev[field]
        if (cur && s.ts < cur[0]) continue
        dev[field] = [s.ts, localTime, typeof v === 'string' ? v : String(v)]
      }
    }
  }

  /**
   * 收到 rtdata 变更通知：payload 带 samples 时直合入 rtdata store（即时刷新），
   * 并保留去抖补拉 loadRtdata（lagged 丢帧 / dev 截断 / 旧后端无 samples 时数据
   * 不全或没有，轮询兜底数据正确性——Q6「尽量处理」语义）。
   */
  function onRtdataNotify(e: RtdataEvent) {
    if (e.samples?.length) applyRtdataSamples(e.samples)
    if (rtdataDebounce) clearTimeout(rtdataDebounce)
    rtdataDebounce = setTimeout(() => {
      rtdataDebounce = null
      void loadRtdata()
    }, RTDATA_NOTIFY_DEBOUNCE_MS)
  }

  /**
   * SSE alarm 帧收敛（Q12 告警最小闭环，spec §5）。
   *
   * - fire → upsert：同 key 替换（level/message/ts 取本次值）并置顶（最新在前，
   *   对齐后端清单 ts 倒序）；事件不带 count——保留已见计数，新键置 1（宿主
   *   count++ 与前端首见=1 同调近似，全量准确值靠 loadAlarms 拉取）。
   * - clear → 移除该 key（脚本 clear_event 与 REST 手动清除同走本路径，多客户端
   *   视图收敛一致；未知 key 无害——幂等）。
   */
  function onAlarm(e: AlarmEvent) {
    if (e.action === 'clear') {
      alarms.value = alarms.value.filter(a => a.key !== e.key)
      return
    }
    const cur = alarms.value.find(a => a.key === e.key)
    alarms.value = [
      { key: e.key, level: e.level, source: e.source, message: e.message, ts: e.ts, count: cur?.count ?? 1 },
      ...alarms.value.filter(a => a.key !== e.key),
    ]
  }

  /** 拉取实例列表（进程级 state + health）。 */
  async function loadInstances() {
    if (isPreviewAuthToken(useAuthStore().token)) {
      applyPreviewSnapshot()
      return
    }
    try {
      const r = await apiFetch('/api/rsiot/api/v1/inst/stats')
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) instances.value = (await r.json()) as InstanceStat[]
    } catch { /* 轮询失败静默，下周期重试 */ }
  }

  /** 拉取后端设备归属表（GET /devices；提取条目 owner_inst——dev2inst 全插件
   *  口径，devreg 设备也可有归属。归属变化频率低，页面 setup 拉一次即可）。 */
  async function loadDeviceOwners() {
    if (isPreviewAuthToken(useAuthStore().token)) return
    try {
      const r = await apiFetch('/api/rsiot/api/v1/devices')
      if (r.status === 401) { handleUnauthorized(); return }
      if (!r.ok) return
      const data = (await r.json()) as Record<string, DeviceSpec>
      deviceCatalog.value = data
      const out: Record<string, string> = {}
      for (const [devId, entry] of Object.entries(data)) {
        if (entry?.owner_inst) out[devId] = entry.owner_inst
      }
      deviceOwners.value = out
    } catch { /* 静默：归属表缺席时前端退化为配置+runtime_devices 口径 */ }
  }

  /** 拉取实例完整配置表（GET /insts，含 app_config/devices；实例管理 + 类型映射用）。 */
  async function loadInstanceConfigs() {
    try {
      const r = await apiFetch('/api/rsiot/api/v1/insts')
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
      const r = await apiFetch('/api/rsiot/api/v1/rtdatas')
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
      const r = await apiFetch('/api/rsiot/api/v1/sysstatus')
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) sysstatus.value = await r.json()
    } catch { /* 轮询失败静默，下周期重试 */ }
  }

  /**
   * 拉取活跃告警清单（GET /alarms，ts 倒序由后端保证）——**全量替换**：
   * 后端清单是活跃告警的事实源（外部 REST 清除/其他客户端触发的收敛以拉取为准，
   * 不做合并）。登录即拉（start 内）+ SSE 重连 onopen 兜底补拉（断线期间丢的
   * fire/clear 帧收敛——告警无轮询，onopen 是唯一补拉点）。
   */
  async function loadAlarms() {
    if (isPreviewAuthToken(useAuthStore().token)) return
    try {
      const r = await apiFetch('/api/rsiot/api/v1/alarms')
      if (r.status === 401) { handleUnauthorized(); return }
      // Array.isArray 防御：非数组响应（旧后端/网关错误页）不写——alarm 收敛路径
      // 依赖数组方法（filter），写入非数组会炸面板渲染。
      if (r.ok) {
        const d = await r.json()
        if (Array.isArray(d)) alarms.value = d as AlarmItem[]
      }
    } catch { /* 拉取失败静默（SSE 帧继续收敛） */ }
  }

  /**
   * 手动清除一条告警（DELETE /alarms?key=，运维兜底）。**不本地乐观移除**——
   * 后端删除后恒发 Clear 事件，经与脚本 clear_event 相同的 SSE 路径收敛（onAlarm
   * 移除），避免"本地移除 + SSE Clear"双路径状态不一致。
   */
  async function clearAlarm(key: string): Promise<boolean> {
    try {
      const r = await apiFetch(`/api/rsiot/api/v1/alarms?key=${encodeURIComponent(key)}`, { method: 'DELETE' })
      if (r.status === 401) { handleUnauthorized(); return false }
      return r.ok
    } catch { return false }
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
      const r = await apiFetch('/api/rsiot/api/v1/logs/recent?limit=200')
      if (r.status === 401) { handleUnauthorized(); return }
      if (r.ok) {
        const entries: LogEntry[] = await r.json()
        const seen = new Set(logs.value.map(l => `${l.ts}|${l.message}`))
        for (const e of [...entries].reverse()) {
          const k = `${e.ts}|${e.message}`
          if (!seen.has(k)) { e.seq = ++entrySeq; logs.value.unshift(e); seen.add(k) }
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
      const r = await apiFetch(path, { method: 'POST',
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

  // ---- 写命令下发（command/send 前端面）----

  /**
   * 写命令下发（POST /api/v1/command/send，routes_command.rs §4.7）。data 为插件
   * 自定义 JSON 文本——modbus 地址串 / opcua node_id 形态见 rest-api.md §4.7，
   * lua 实例走脚本 on_write。前置校验 data 是合法 JSON（常见手误早拦，不发坏请求）。
   * 后端仅确认投递到写总线（{"status":"ok"}），无执行回执；实例未运行 404。
   */
  async function sendCommand(
    instance: string,
    devid: string,
    data: string,
  ): Promise<{ ok: boolean; error?: string; status?: number }> {
    try { JSON.parse(data) } catch { return { ok: false, error: 'data 不是合法 JSON' } }
    try {
      const r = await apiFetch('/api/rsiot/api/v1/command/send', { method: 'POST',
        body: JSON.stringify({ instance, devid, cmd: 'write', data }),
      })
      if (r.status === 401) { handleUnauthorized(); return { ok: false, error: '登录已过期，请重新登录', status: 401 } }
      if (!r.ok) return { ok: false, error: `HTTP ${r.status}`, status: r.status }
      return { ok: true, status: r.status }
    } catch (e) {
      return { ok: false, error: e instanceof Error ? e.message : '下发失败' }
    }
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

  /** 关闭日志 SSE 订阅（离开诊断 Tab）。冲刷攒批尾巴防丢帧。 */
  function unsubscribeLog() {
    logSse?.close()
    logSse = null
    flushLogs()
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

  /** 关闭报文 SSE 订阅。冲刷攒批尾巴；同时停当前实例的 trace 推送（后端 per-instance 注册表）。 */
  function unsubscribePacket() {
    packetSse?.close()
    packetSse = null
    flushPackets()
    packetSubscribed.value = false
    if (selectedInstance.value) void stopPacketTrace(selectedInstance.value)
  }

  /** 标签页隐藏时暂停轮询，恢复时继续（省资源 + 防后台节流）。 */
  function onVisibility() {
    if (document.hidden) {
      if (rtdataTimer) { clearInterval(rtdataTimer); rtdataTimer = null }
      if (sysstatusTimer) { clearInterval(sysstatusTimer); sysstatusTimer = null }
      if (rosterTimer) { clearInterval(rosterTimer); rosterTimer = null }
      if (alarmTimer) { clearInterval(alarmTimer); alarmTimer = null }
    } else if (!rtdataTimer || !sysstatusTimer || !rosterTimer || !alarmTimer) {
      rtdataTimer = setInterval(loadRtdata, POLL_RTDATA_MS)
      sysstatusTimer = setInterval(loadSysstatus, POLL_SYSSTATUS_MS)
      rosterTimer = setInterval(() => { void loadInstances(); void loadInstanceConfigs() }, POLL_ROSTER_MS)
      alarmTimer = setInterval(() => void loadAlarms(), POLL_ALARM_MS)
    }
  }

  /**
   * 启动：rtdata 轮询 + rtdata SSE 通知 + sysstatus 轮询。
   *
   * **M3 改动**：不再自动订阅 log/packet（移到诊断 Tab 手动 subscribe*）。
   * rtdata SSE 通知（Q6 带值）samples 直合入即时刷新 + loadRtdata 去抖补拉兜底。
   */
  function start() {
    if (sse) stop()
    const tok = useAuthStore().token
    if (isPreviewAuthToken(tok)) {
      applyPreviewSnapshot()
      offline.value = false
      return
    }
    // rtdata 变更通知 SSE（订阅 state 用于实例状态联动；alarm 告警默认订阅——运维
    // 刚需，照 rtdata 面板模式不做按需激活；log/packet 不在此订阅）。
    sse = new EventSource(
      `/api/rsiot/api/v1/events?types=rtdata,state,alarm&token=${encodeURIComponent(tok)}`,
    )
    sse.addEventListener('rtdata', e => safe(() => onRtdataNotify(JSON.parse((e as MessageEvent).data) as RtdataEvent)))
    sse.addEventListener('state', e => safe(() => onState(JSON.parse((e as MessageEvent).data) as StateEvent)))
    sse.addEventListener('alarm', e => safe(() => onAlarm(JSON.parse((e as MessageEvent).data) as AlarmEvent)))
    sse.onopen = () => {
      offline.value = false
      // 重连兜底：断线期间丢的 fire/clear 帧经全量拉取收敛（告警无轮询）。
      void loadAlarms()
    }
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
    void loadAlarms()
    rtdataTimer = setInterval(loadRtdata, POLL_RTDATA_MS)
    sysstatusTimer = setInterval(loadSysstatus, POLL_SYSSTATUS_MS)
    rosterTimer = setInterval(() => { void loadInstances(); void loadInstanceConfigs() }, POLL_ROSTER_MS)
    // 复审 P2-2：告警低频轮询兜底（SSE 丢帧后 30s 内收敛；可见性联动同其它 timer）。
    alarmTimer = setInterval(() => void loadAlarms(), POLL_ALARM_MS)
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
      const r = await apiFetch('/api/rsiot/api/v1/inst/trace', { method: 'POST',
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
      const r = await apiFetch(`/api/rsiot/api/v1/inst/trace?inst_id=${encodeURIComponent(instId)}`, { method: 'DELETE' })
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
    logs.value = previewSnapshot.logs.map(l => ({ ...l, seq: ++entrySeq }))
    packets.value = previewSnapshot.packets.map(p => ({ ...p, seq: ++entrySeq }))
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
    if (alarmTimer) { clearInterval(alarmTimer); alarmTimer = null }
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
    deviceOwners,
    deviceCatalog,
    sysstatus,
    alarms,
    selectedInstance,
    selectedDevice,
    offline,
    logSubscribed,
    packetSubscribed,
    onLog,
    onPacket,
    onState,
    onRtdataNotify,
    onAlarm,
    loadInstances,
    loadDeviceOwners,
    loadInstanceConfigs,
    loadRtdata,
    loadSysstatus,
    loadLogsRecent,
    loadAlarms,
    clearAlarm,
    saveInstance,
    deleteInstance,
    purge,
    startInstance,
    stopInstance,
    sendCommand,
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
