<script setup lang="ts">
// IndustryDashboard.vue —— 工业设备实时监控 + 历史分析驾驶舱。
// 无需新依赖：实时值走 monitor store（SSE rtdata），历史曲线复用
// lib/historyQuery 的 buildHistoryUrl/decimate/buildSparklinePath（手写 SVG）。
// 设备按水表/电表/燃气表/其他分组（devid 前缀 dev_mb_*/dev_ua_* + 标签语义）。
// 本视图只读 monitor store：SSE 订阅的生命周期归 AppShell 管（挂载时 start、
// 卸载时 stop），叶子视图不得再调 monitor.start()——它不是幂等的（先 stop 再
// 新建 EventSource），子视图先于 AppShell 挂载时会把刚建好的连接断开重连一次。
import { computed, ref } from 'vue'
import { useMonitorStore } from '../stores/monitor'
import { apiFetch } from '../api/fetch'
import {
  buildHistoryUrl, decimate, buildSparklinePath, numericPoints,
  type TsdbHistoryPoint, type TsdbHistoryResponse,
} from '../lib/historyQuery'

const monitor = useMonitorStore()

type DeviceGroup = 'water' | 'power' | 'gas' | 'env' | 'other'
const GROUP_LABEL: Record<DeviceGroup, string> = {
  water: '水表', power: '电表', gas: '燃气表', env: '环境/压力', other: '其他设备',
}
const GROUP_ORDER: DeviceGroup[] = ['water', 'power', 'gas', 'env', 'other']

interface DeviceInfo {
  devid: string
  group: DeviceGroup
  short: string
  tags: Array<{ key: string; name: string; value: string; ts: number; numeric: number | null }>
}

function classify(devid: string): { group: DeviceGroup; short: string } {
  // 采集设备现为 dev_mb_* / dev_ua_*（含前缀协议）。按名称关键字分组。
  const s = devid.toLowerCase()
  if (s.includes('wm') || s.includes('water')) return { group: 'water', short: devid.replace(/^dev_(mb|ua)_/, '') }
  if (s.includes('em') || s.includes('power') || s.includes('meter')) return { group: 'power', short: devid.replace(/^dev_(mb|ua)_/, '') }
  if (s.includes('gm') || s.includes('gas')) return { group: 'gas', short: devid.replace(/^dev_(mb|ua)_/, '') }
  if (s.includes('env') || s.includes('pt')) return { group: 'env', short: devid.replace(/^dev_(mb|ua)_/, '') }
  return { group: 'other', short: devid.replace(/^dev_(mb|ua)_/, '') }
}

const devices = computed<DeviceInfo[]>(() => {
  const out: DeviceInfo[] = []
  for (const [devid, cells] of Object.entries(monitor.rtdata)) {
    if (devid === 'rsiot' || devid.startsWith('_')) continue
    const { group, short } = classify(devid)
    const tags = Object.entries(cells).map(([key, cell]) => {
      const arr = Array.isArray(cell) ? cell : [0, '', '']
      const raw = String(arr[2] ?? '')
      const num = Number(raw)
      return {
        key,
        name: key.includes('.') ? key.split('.').slice(1).join('.') : key,
        value: raw,
        ts: Number(arr[0]) || 0,
        numeric: raw.trim() !== '' && Number.isFinite(num) ? num : null,
      }
    })
    out.push({ devid, group, short, tags })
  }
  return out.sort((a, b) => a.devid.localeCompare(b.devid))
})

const grouped = computed(() => GROUP_ORDER.map(g => ({
  group: g, label: GROUP_LABEL[g],
  devices: devices.value.filter(d => d.group === g),
})).filter(x => x.devices.length > 0))

const totalDevices = computed(() => devices.value.length)
const totalTags = computed(() => devices.value.reduce((n, d) => n + d.tags.length, 0))

// ---- 历史曲线（按需点开某 tag）----
const historyDev = ref('')
const historyTag = ref('')
const historyPath = ref('')
const historyMeta = ref('')
const historyLoading = ref(false)
const historyError = ref('')

async function loadHistory(devid: string, key: string) {
  const tag = key.includes('.') ? key.split('.').slice(1).join('.') : key
  historyDev.value = devid
  historyTag.value = tag
  historyPath.value = ''
  historyMeta.value = ''
  historyError.value = ''
  historyLoading.value = true
  const endSec = Math.floor(Date.now() / 1000)
  const startSec = endSec - 3600
  try {
    const url = buildHistoryUrl({ devId: devid, tag, startSec, endSec, limit: 10000 })
    const r = await apiFetch(url)
    if (!r.ok) {
      historyError.value = `HTTP ${r.status}（历史存储可能尚未写入此点）`
    } else {
      const body = await r.json() as TsdbHistoryResponse
      const numeric = numericPoints(body.points as TsdbHistoryPoint[]).numeric
      if (numeric.length === 0) {
        historyMeta.value = `窗口内 ${body.count} 点（非数值或空）`
      } else {
        const buckets = decimate(numeric, 200)
        historyPath.value = buildSparklinePath(buckets, HISTORY_W, HISTORY_H)
        const vals = numeric.map(p => p.value)
        historyMeta.value = `${body.count} 点 · 范围 ${Math.min(...vals).toFixed(2)} ~ ${Math.max(...vals).toFixed(2)}`
      }
    }
  } catch {
    historyError.value = '网络错误（网关不可达）'
  } finally {
    historyLoading.value = false
  }
}

const HISTORY_W = 640
const HISTORY_H = 160
function sparkViewBox(path: string): string {
  return path ? `0 0 ${HISTORY_W} ${HISTORY_H}` : `0 0 ${HISTORY_W} ${HISTORY_H}`
}
</script>

<template>
  <section class="dashboard">
    <header class="dash-header">
      <div>
        <h2>工业设备驾驶舱</h2>
        <p>水 / 电 / 气 / 环境设备实时监控与历史分析 · 数据源 simulator（Modbus / OPC UA / IEC104）</p>
      </div>
      <div class="dash-stats">
        <div><span>设备</span><strong>{{ totalDevices }}</strong></div>
        <div><span>实时点</span><strong>{{ totalTags }}</strong></div>
        <div><span>连接</span><strong :class="monitor.offline ? 'bad' : 'good'">{{ monitor.offline ? '重连中' : '在线' }}</strong></div>
      </div>
    </header>

    <div v-for="g in grouped" :key="g.group" class="dash-group">
      <h3 class="group-title">{{ g.label }} <small>{{ g.devices.length }} 台</small></h3>
      <div class="device-grid">
        <article v-for="d in g.devices" :key="d.devid" class="device-card">
          <div class="card-head">
            <strong>{{ d.short }}</strong>
            <code>{{ d.devid }}</code>
          </div>
          <div class="tag-list">
            <button
              v-for="t in d.tags" :key="t.key" class="tag-row"
              :class="{ selected: historyDev === d.devid && historyTag === t.name }"
              :title="'点击查看历史趋势 · ' + t.key"
              @click="loadHistory(d.devid, t.key)"
            >
              <span class="tag-name">{{ t.name }}</span>
              <span class="tag-value">{{ t.value }}</span>
            </button>
          </div>
        </article>
      </div>
    </div>

    <section v-if="historyDev || historyLoading" class="history-panel">
      <div class="history-head">
        <strong>历史趋势：{{ historyDev }} / {{ historyTag }}</strong>
        <span class="history-meta">{{ historyMeta }}</span>
      </div>
      <div v-if="historyLoading" class="history-msg">加载中…</div>
      <div v-else-if="historyError" class="history-msg error">{{ historyError }}</div>
      <svg v-else-if="historyPath" class="history-chart" :viewBox="sparkViewBox(historyPath)" preserveAspectRatio="none">
        <path :d="historyPath" fill="none" stroke="currentColor" stroke-width="1.5" />
      </svg>
      <div v-else class="history-msg">该点近 1 小时暂无数值历史</div>
    </section>
  </section>
</template>

<style scoped>
.dashboard { display: grid; gap: 22px; color: var(--color-ink); }
.dash-header { display: flex; justify-content: space-between; align-items: flex-end; gap: 20px; flex-wrap: wrap; }
.dash-header h2 { margin: 0 0 6px; font-size: 22px; letter-spacing: .03em; }
.dash-header p { margin: 0; color: var(--color-ink-dim); font-size: 13px; }
.dash-stats { display: flex; gap: 10px; }
.dash-stats div { padding: 8px 14px; border-radius: 12px; background: var(--shell-card-bg); border: 1px solid var(--shell-card-border); text-align: center; }
.dash-stats span { display: block; font-size: 10px; letter-spacing: .14em; color: var(--color-ink-dim); text-transform: uppercase; }
.dash-stats strong { font-family: Consolas, monospace; font-size: 18px; }
.dash-stats .good { color: var(--color-success); }
.dash-stats .bad { color: var(--color-danger); }
.dash-group { display: grid; gap: 12px; }
.group-title { margin: 0; font-size: 15px; letter-spacing: .06em; padding-bottom: 8px; border-bottom: 1px solid var(--shell-card-border); }
.group-title small { color: var(--color-ink-dim); font-weight: 400; margin-left: 8px; }
.device-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(270px, 1fr)); gap: 12px; }
.device-card { border: 1px solid var(--shell-card-border); border-radius: 16px; background: var(--shell-card-bg); padding: 12px 14px; }
.card-head { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 10px; }
.card-head strong { font-size: 15px; }
.card-head code { font-size: 10px; color: var(--color-ink-dim); }
.tag-list { display: grid; gap: 3px; max-height: 250px; overflow-y: auto; }
.tag-row { display: flex; justify-content: space-between; gap: 8px; width: 100%; padding: 4px 8px; border: none; border-radius: 8px; background: transparent; color: var(--color-ink); cursor: pointer; font-size: 12px; text-align: left; }
.tag-row:hover { background: var(--shell-nav-active-bg); }
.tag-row.selected { background: var(--shell-theme-active-bg); color: var(--shell-theme-active-ink); }
.tag-name { color: var(--color-ink-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tag-value { font-family: Consolas, monospace; max-width: 45%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.history-panel { border: 1px solid var(--shell-card-border); border-radius: 16px; background: var(--shell-card-bg); padding: 14px 16px; }
.history-head { display: flex; justify-content: space-between; gap: 12px; margin-bottom: 10px; flex-wrap: wrap; }
.history-meta { color: var(--color-ink-dim); font-size: 12px; }
.history-chart { width: 100%; height: 160px; color: var(--color-primary-hover); }
.history-msg { padding: 30px 0; text-align: center; color: var(--color-ink-dim); }
.history-msg.error { color: var(--color-danger); }
</style>
