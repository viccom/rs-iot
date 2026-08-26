<script setup lang="ts">
// frontend/src/components/devmeta/HistoryQuery.vue
// 历史数据查询面板（Task 5，P7/P8）：GET /api/rsiot/api/v1/history（Bearer，
// devreg store 同口径）——raw / 六聚合（agg+interval 秒成对）、窗口预设
// （15m/1h/6h/24h + 自定义 datetime-local）、limit clamp 1..100000。
// 结果区：全数值 series 手写 SVG min/max 抽稀折线（≤800 桶 + 值域/首末
// 时间标注）；非数值 series（Bool/Str）表格-only 不画曲线（P7）；表格仅
// 前 200 行防万行 DOM（R6）。503 reason 三态 / 400 双形态错误归一横幅。
// 显隐由父层 v-if 控制；本组件不自带折叠头部（保持简单）。
import { computed, ref, watch } from 'vue'
import { useAuthStore } from '../../stores/auth'
import {
  buildHistoryUrl, buildSparklinePath, decimate, historyErrorMessage, windowToRange,
  type HistoryAgg, type HistoryQueryParams, type TsdbHistoryResponse, type WindowPreset,
} from '../../lib/historyQuery'

const props = defineProps<{
  /** 预填 dev_id（面板③行「查」入口点击时更新，watch 跟进）。 */
  devId?: string
  /** 预填 tag。 */
  tag?: string
}>()

const auth = useAuthStore()

// ---- 表单态 ----
const devIdInput = ref(props.devId ?? '')
const tagInput = ref(props.tag ?? '')
const windowChoice = ref<WindowPreset | 'custom'>('1h')
/** 自定义窗口（datetime-local 字符串，本地时区、秒精度 step=1）。 */
const customStart = ref('')
const customEnd = ref('')
const mode = ref<'raw' | 'agg'>('raw')
const aggFunc = ref<HistoryAgg>('avg')
const intervalSec = ref(60)
const limitInput = ref(10000)

const AGG_OPTIONS: HistoryAgg[] = ['min', 'max', 'avg', 'sum', 'count', 'last']

// 预填跟进（父层行入口可多次改写；空串占位不覆盖——面板开在无选中行时）
watch([() => props.devId, () => props.tag], ([d, t]) => {
  if (d) devIdInput.value = d
  if (t) tagInput.value = t
})

// 切到自定义且尚未填过时给一对缺省值（now-1h .. now）
watch(windowChoice, v => {
  if (v !== 'custom' || customStart.value || customEnd.value) return
  const now = Math.floor(Date.now() / 1000)
  customEnd.value = secToDatetimeLocal(now)
  customStart.value = secToDatetimeLocal(now - 3600)
})

/** 秒 → datetime-local 字符串（YYYY-MM-DDTHH:mm:ss，本地时区）。 */
function secToDatetimeLocal(sec: number): string {
  const d = new Date(sec * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}

/** datetime-local 字符串 → 秒（ms 精度字符串 → Date.parse/1000 取整；
 *  空串/非法 → NaN，由提交守卫拦截）。 */
function datetimeLocalToSec(s: string): number {
  return Math.floor(Date.parse(s) / 1000)
}

/** limit clamp 1..100000（后端同区间；缺省 10000）。 */
const limitClamped = computed(() =>
  Math.min(100000, Math.max(1, Math.floor(Number(limitInput.value)) || 10000)))

// ---- 查询执行 ----
const querying = ref(false)
const errorBanner = ref('')
const result = ref<TsdbHistoryResponse | null>(null)

async function onQuery() {
  if (querying.value) return
  const devId = devIdInput.value.trim()
  const tag = tagInput.value.trim()
  if (!devId || !tag) {
    errorBanner.value = '请填写 dev_id 与 tag'
    return
  }
  let startSec: number
  let endSec: number
  if (windowChoice.value === 'custom') {
    startSec = datetimeLocalToSec(customStart.value)
    endSec = datetimeLocalToSec(customEnd.value)
    if (!Number.isFinite(startSec) || !Number.isFinite(endSec) || startSec >= endSec) {
      errorBanner.value = '自定义窗口无效：开始时间须早于结束时间（秒精度，本地时区）'
      return
    }
  } else {
    ;({ startSec, endSec } = windowToRange(windowChoice.value, Math.floor(Date.now() / 1000)))
  }
  const params: HistoryQueryParams = {
    devId, tag, startSec, endSec, limit: limitClamped.value,
    ...(mode.value === 'agg'
      ? { agg: aggFunc.value, intervalSec: Math.max(1, Math.floor(Number(intervalSec.value)) || 60) }
      : {}),
  }
  querying.value = true
  errorBanner.value = ''
  result.value = null
  try {
    const r = await fetch(buildHistoryUrl(params), {
      headers: { Authorization: `Bearer ${auth.token}` },
    })
    if (r.status === 401) {
      auth.clear()
      errorBanner.value = '登录已过期，请重新登录'
    } else if (!r.ok) {
      errorBanner.value = historyErrorMessage(r.status, await r.text())
    } else {
      result.value = (await r.json()) as TsdbHistoryResponse
    }
  } catch {
    errorBanner.value = '网络错误（网关不可达）'
  }
  querying.value = false
}

// ---- 结果派生 ----
/** 全数值 series 才有曲线（P7：Bool/Str 无意义不画）；否则 null。 */
const numPoints = computed<Array<{ ts: number; value: number }> | null>(() => {
  const r = result.value
  if (!r) return null
  return r.points.every(p => typeof p.value === 'number')
    ? (r.points as Array<{ ts: number; value: number }>)
    : null
})

const SPARK_W = 800
const SPARK_H = 240
const sparkPath = computed(() => {
  const pts = numPoints.value
  if (!pts?.length) return ''
  return buildSparklinePath(decimate(pts, Math.min(800, pts.length)), SPARK_W, SPARK_H)
})

/** 值域/首末时间标注（全局 min/max + 首末点 ts）。 */
const sparkExtent = computed(() => {
  const pts = numPoints.value
  if (!pts?.length) return null
  let vmin = Infinity
  let vmax = -Infinity
  for (const p of pts) {
    if (p.value < vmin) vmin = p.value
    if (p.value > vmax) vmax = p.value
  }
  return { vmin, vmax, firstTs: pts[0].ts, lastTs: pts[pts.length - 1].ts }
})

/** 表格最多前 200 行（防万行 DOM，R6）；总数走 count 摘要行。 */
const TABLE_MAX = 200
const tableRows = computed(() => result.value?.points.slice(0, TABLE_MAX) ?? [])

const fmtTs = (ts: number): string => new Date(ts * 1000).toLocaleString()
/** value 原始类型展示：字符串加引号界定，数值/布尔回 String 原样。 */
const fmtValue = (v: number | boolean | string): string =>
  typeof v === 'string' ? `"${v}"` : String(v)
</script>

<template>
  <div class="hist-q">
    <p class="hq-label">历史查询（GET /api/v1/history · raw / 六聚合 · 503 归一）</p>

    <div class="hq-form">
      <label class="f"><span>dev_id</span>
        <input v-model="devIdInput" class="mono" placeholder="line1.base1" />
      </label>
      <label class="f"><span>tag</span>
        <input v-model="tagInput" class="mono" placeholder="temp" />
      </label>
      <label class="f"><span>窗口</span>
        <select v-model="windowChoice">
          <option value="15m">近 15 分钟</option>
          <option value="1h">近 1 小时</option>
          <option value="6h">近 6 小时</option>
          <option value="24h">近 24 小时</option>
          <option value="custom">自定义…</option>
        </select>
      </label>
      <template v-if="windowChoice === 'custom'">
        <label class="f"><span>开始（本地时区）</span>
          <input v-model="customStart" type="datetime-local" step="1" />
        </label>
        <label class="f"><span>结束（本地时区）</span>
          <input v-model="customEnd" type="datetime-local" step="1" />
        </label>
      </template>
      <div class="f"><span>模式</span>
        <div class="mode-seg">
          <label class="seg"><input type="radio" value="raw" v-model="mode" />raw</label>
          <label class="seg"><input type="radio" value="agg" v-model="mode" />agg</label>
        </div>
      </div>
      <template v-if="mode === 'agg'">
        <label class="f"><span>聚合</span>
          <select v-model="aggFunc">
            <option v-for="a in AGG_OPTIONS" :key="a" :value="a">{{ a }}</option>
          </select>
        </label>
        <label class="f"><span>interval（秒）</span>
          <input v-model.number="intervalSec" type="number" min="1" step="1" />
        </label>
      </template>
      <label class="f"><span>limit（1..100000）</span>
        <input v-model.number="limitInput" type="number" min="1" max="100000" step="1" />
      </label>
      <button class="btn-run" :disabled="querying" @click="onQuery">
        {{ querying ? '查询中…' : '查询' }}
      </button>
    </div>

    <p v-if="errorBanner" class="hq-error">{{ errorBanner }}</p>

    <template v-if="result">
      <p class="hq-summary">
        <span class="mono">{{ result.dev_id }}.{{ result.tag }}</span>
        · 共 {{ result.count }} 点（返回 {{ result.points.length }}）<template
          v-if="result.points.length > TABLE_MAX"
        >，表格仅列前 {{ TABLE_MAX }} 行</template>
      </p>

      <p v-if="!result.points.length" class="hq-empty">窗口内无归档数据点。</p>

      <template v-else-if="numPoints">
        <svg
          class="hq-chart" :viewBox="`0 0 ${SPARK_W} ${SPARK_H}`"
          preserveAspectRatio="none" role="img"
          aria-label="历史曲线（min/max 抽稀折线）"
        >
          <path
            v-if="sparkPath" :d="sparkPath" fill="none"
            stroke="var(--color-primary)" stroke-width="1.5"
            vector-effect="non-scaling-stroke"
          />
        </svg>
        <div v-if="sparkExtent" class="hq-axis">
          <span>min {{ sparkExtent.vmin }} · max {{ sparkExtent.vmax }}</span>
          <span class="mono">{{ fmtTs(sparkExtent.firstTs) }} → {{ fmtTs(sparkExtent.lastTs) }}</span>
        </div>
      </template>
      <p v-else class="hq-empty">非数值 series（Bool/Str）仅表格展示，不绘制曲线。</p>

      <table v-if="result.points.length" class="mini-table">
        <thead>
          <tr><th>ts（本地时间）</th><th>value</th></tr>
        </thead>
        <tbody>
          <tr v-for="(p, i) in tableRows" :key="i">
            <td class="mono">{{ fmtTs(p.ts) }}</td>
            <td class="mono">{{ fmtValue(p.value) }}</td>
          </tr>
        </tbody>
      </table>
    </template>
  </div>
</template>

<style scoped>
.hist-q {
  border: 1px dashed var(--color-primary);
  border-radius: 10px;
  background: var(--card-bg);
  padding: 12px;
}
.hq-label {
  margin: 0 0 10px; font-size: 11px; letter-spacing: 0.14em;
  text-transform: uppercase; color: var(--color-ink-dim);
}
.hq-form { display: flex; gap: 12px; align-items: end; flex-wrap: wrap; }
.f { display: flex; flex-direction: column; gap: 4px; font-size: 11px; }
.f span { color: var(--color-ink-dim); white-space: nowrap; }
.f input, .f select {
  padding: 6px 10px; border-radius: 8px; font-size: 12px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: inherit;
}
.f input { width: 170px; }
.mode-seg { display: flex; gap: 10px; align-items: center; height: 30px; }
.seg {
  display: inline-flex; align-items: center; gap: 4px;
  font-size: 12px; cursor: pointer; user-select: none;
  color: var(--color-ink-secondary);
}
.seg input { accent-color: var(--color-primary); cursor: pointer; }
.btn-run {
  padding: 7px 18px; border-radius: 8px; font-size: 12px;
  border: 1px solid var(--color-primary); background: var(--color-primary);
  color: white; cursor: pointer;
}
.btn-run:disabled { opacity: 0.4; cursor: not-allowed; }

.hq-error {
  color: var(--color-danger); font-size: 12px;
  margin: 10px 0 0; word-break: break-all;
}
.hq-summary { margin: 12px 0 8px; font-size: 12px; color: var(--color-ink-secondary); }
.hq-empty { margin: 10px 0 0; font-size: 12px; color: var(--color-ink-dim); }

.hq-chart { width: 100%; height: 240px; display: block; }
.hq-axis {
  display: flex; justify-content: space-between; gap: 12px; flex-wrap: wrap;
  font-size: 11px; color: var(--color-ink-dim); margin: 4px 0 10px;
}

.mini-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.mini-table th {
  text-align: left; font-weight: 500; color: var(--color-ink-dim);
  padding: 6px 8px; border-bottom: 1px solid var(--color-divider);
  font-size: 11px; white-space: nowrap;
}
.mini-table td {
  padding: 6px 8px;
  border-bottom: 1px solid var(--color-divider-soft, rgba(0,0,0,0.05));
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 180px;
}
.mono { font-family: Consolas, monospace; }
</style>
