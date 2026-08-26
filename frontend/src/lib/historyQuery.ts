// frontend/src/lib/historyQuery.ts
// 历史查询面板（Task 5，P7/P8）纯逻辑——GET /api/v1/history 前端侧口径：
// - buildHistoryUrl：gateway 全路径 /api/rsiot/api/v1/history（P8，devreg
//   fetch 口径同源），参数序 dev_id,tag,start,end,agg?,interval?,limit，
//   秒半开区间（rest-api.md §4.14）；
// - decimate：min/max/last 抽稀（≤800 桶，P7）——ts 线性分桶，
//   span=(t1-t0)/buckets，末桶 clamp 含 t1，空桶剔除；
// - numericPoints：非数值点过滤（H2 前端防御——后端 NaN 序列化为 null、
//   旧数据/纵深场景逐点剔除并计数，曲线只绘数值点不再整条消失）；
// - buildSparklinePath：手写 SVG 单一折线（每桶 (x,ymax) 与 (x,ymin)
//   交替两点，max 线在上 min 线在下合并一条 path——零图表库依赖）；
// - historyErrorMessage：503 reason 三态中文归一（feature_not_compiled /
//   history_disabled / init_failed）+ 400 双形态（解析层纯文本 / 引擎层
//   JSON error 字段）+ 其余状态透传状态码；
// - windowToRange：窗口预设（15m/1h/6h/24h）→ 秒区间 {startSec,endSec}。

/** 查询参数（组件表单 → URL；agg 与 intervalSec 成对出现）。 */
export interface HistoryQueryParams {
  devId: string
  tag: string
  startSec: number
  endSec: number
  agg?: string
  intervalSec?: number
  limit?: number
}

/** 抽稀桶（ts=桶内首点 ts；last=桶内末点 value）。 */
export interface DecimatedPoint {
  ts: number
  min: number
  max: number
  last: number
}

/** 窗口预设（Δ 秒：900/3600/21600/86400）。 */
export type WindowPreset = '15m' | '1h' | '6h' | '24h'

/** 六聚合（rest-api.md §4.14：agg∈min|max|avg|sum|count|last，与 interval 成对）。 */
export type HistoryAgg = 'min' | 'max' | 'avg' | 'sum' | 'count' | 'last'

/** GET /api/v1/history 响应点（value 类型保真：数值/Bool/Str）。 */
export interface TsdbHistoryPoint {
  ts: number
  value: number | boolean | string
}

/** GET /api/v1/history 响应（count 与 points 同长，后端契约）。 */
export interface TsdbHistoryResponse {
  dev_id: string
  tag: string
  points: TsdbHistoryPoint[]
  count: number
}

const HISTORY_BASE = '/api/rsiot/api/v1/history'

/** 构建可直接 fetch 的全路径 URL（Bearer 头由调用方附加）。
 *  limit 缺省 10000；devId/tag 经 encodeURIComponent；秒参数命名 start/end。 */
export function buildHistoryUrl(p: HistoryQueryParams): string {
  const parts = [
    `dev_id=${encodeURIComponent(p.devId)}`,
    `tag=${encodeURIComponent(p.tag)}`,
    `start=${p.startSec}`,
    `end=${p.endSec}`,
  ]
  // agg 与 intervalSec 成对出现（无 agg 则无 interval）
  if (p.agg !== undefined) {
    parts.push(`agg=${encodeURIComponent(p.agg)}`)
    parts.push(`interval=${p.intervalSec}`)
  }
  parts.push(`limit=${p.limit ?? 10000}`)
  return `${HISTORY_BASE}?${parts.join('&')}`
}

/** 无效点过滤结果：numeric=仅数值点（保序）；skipped=剔除的非数值点数。 */
export interface NumericPointsResult {
  numeric: Array<{ ts: number; value: number }>
  skipped: number
}

/** 逐点过滤非 typeof number 值（H2 前端防御）：后端 NaN→null 序列化点、
 *  Bool/Str 点均剔除并计数——曲线只绘数值点，一个 null 不再整条消失。
 *  points 取宽 unknown 值域（TsdbHistoryPoint[] 可直接传入）。 */
export function numericPoints(
  points: Array<{ ts: number; value: unknown }>,
): NumericPointsResult {
  const numeric: Array<{ ts: number; value: number }> = []
  let skipped = 0
  for (const p of points) {
    if (typeof p.value === 'number') numeric.push({ ts: p.ts, value: p.value })
    else skipped++
  }
  return { numeric, skipped }
}

/** min/max/last 抽稀（points 须按 ts 升序传入）。
 *  桶归属按 ts 线性划分：t0=首点 ts、t1=末点 ts、span=(t1-t0)/buckets，
 *  桶索引 = min(floor((ts-t0)/span), buckets-1)（末桶闭区间含 t1；
 *  span===0 即全部点同 ts 时整体归桶 0——除零防御）；空桶剔除后输出。 */
export function decimate(
  points: Array<{ ts: number; value: number }>,
  buckets: number,
): DecimatedPoint[] {
  if (!points.length || buckets <= 0) return []
  const t0 = points[0].ts
  const t1 = points[points.length - 1].ts
  const span = (t1 - t0) / buckets
  const bucketOf = (ts: number): number =>
    span === 0 ? 0 : Math.min(Math.floor((ts - t0) / span), buckets - 1)
  // ts 升序 → 桶索引单调不减 → 顺序分组即桶分组
  const out: DecimatedPoint[] = []
  let cur: DecimatedPoint | null = null
  let curIdx = -1
  for (const p of points) {
    const idx = bucketOf(p.ts)
    if (!cur || idx !== curIdx) {
      cur = { ts: p.ts, min: p.value, max: p.value, last: p.value }
      curIdx = idx
      out.push(cur)
    } else {
      if (p.value < cur.min) cur.min = p.value
      if (p.value > cur.max) cur.max = p.value
      cur.last = p.value
    }
  }
  return out
}

/** 手写 SVG 折线串 `M x y L x y ...`（每桶先 (x,ymax) 后 (x,ymin) 交替——
 *  max/min 双线合并为一条 path）；d 为空返回 ''（不渲染 path）。
 *  x_k = round(k*w/(n-1))（首桶 0 末桶 w；n===1 除零防御取 x=0）；
 *  y = round(h - (v-vmin)/(vmax-vmin)*h)（vmin/vmax 取全部桶 min/max 全局，
 *  SVG y 向下：值大 y 小）；vmin===vmax（全等值）时 y 恒为 round(h/2)。 */
export function buildSparklinePath(d: DecimatedPoint[], w: number, h: number): string {
  const n = d.length
  if (!n) return ''
  let vmin = Infinity
  let vmax = -Infinity
  for (const b of d) {
    if (b.min < vmin) vmin = b.min
    if (b.max > vmax) vmax = b.max
  }
  const yOf = (v: number): number =>
    vmin === vmax ? Math.round(h / 2) : Math.round(h - ((v - vmin) / (vmax - vmin)) * h)
  const segs: string[] = []
  d.forEach((b, k) => {
    const x = n === 1 ? 0 : Math.round((k * w) / (n - 1))
    segs.push(`${x} ${yOf(b.max)}`)
    segs.push(`${x} ${yOf(b.min)}`)
  })
  return `M ${segs.join(' L ')}`
}

/** 错误文案归一：503 JSON reason 三态中文（编译未含 feature / 未启用 /
 *  初始化失败），其他 reason 原样透传；400 JSON 取 error 字段、纯文本取
 *  原文；其余状态码（含 500）透传十进制串。 */
export function historyErrorMessage(status: number, body: string): string {
  let obj: Record<string, unknown> | null = null
  try {
    const v = JSON.parse(body) as unknown
    if (v && typeof v === 'object' && !Array.isArray(v)) obj = v as Record<string, unknown>
  } catch {
    // 400 解析层错误体为纯文本——走原文分支
  }
  if (status === 503) {
    const reason = typeof obj?.reason === 'string' ? obj.reason : ''
    const cause = reason === 'feature_not_compiled'
      ? '编译未含 history feature（后端需以 --features history 重新编译）'
      : reason === 'history_disabled'
        ? '历史存储未启用（[history] enabled=false）'
        : reason === 'init_failed'
          ? '历史引擎初始化失败（详见服务端日志）'
          : reason
    return cause ? `历史查询不可用（503）：${cause}` : '历史查询不可用（503）'
  }
  if (status === 400) {
    const err = typeof obj?.error === 'string' ? obj.error : ''
    return `查询参数错误（400）：${err || body}`
  }
  return `查询失败（HTTP ${status}）`
}

const WINDOW_DELTAS_SEC: Record<WindowPreset, number> = {
  '15m': 900,
  '1h': 3600,
  '6h': 21600,
  '24h': 86400,
}

/** 窗口预设 → 秒区间（endSec==nowSec，半开区间口径）。 */
export function windowToRange(
  preset: WindowPreset,
  nowSec: number,
): { startSec: number; endSec: number } {
  return { startSec: nowSec - WINDOW_DELTAS_SEC[preset], endSec: nowSec }
}
