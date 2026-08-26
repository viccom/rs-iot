// frontend/src/lib/historyQuery.test.ts
// 历史查询面板（Task 5，P7/P8）纯函数红测试——本文件先行，historyQuery.ts
// 尚不存在，导入失败即红。绿阶段实现者照本文件头部的【实现契约】实现，
// 禁改本文件断言。
//
// ── 实现契约（绿阶段照此实现）────────────────────────────────────────
// 类型导出（名称/字段精确）：
//   HistoryQueryParams { devId: string; tag: string; startSec: number;
//                        endSec: number; agg?: string; intervalSec?: number;
//                        limit?: number }
//   DecimatedPoint { ts: number; min: number; max: number; last: number }
//   WindowPreset = '15m' | '1h' | '6h' | '24h'
//
// buildHistoryUrl(p)：
//   产出可直接传 devreg store fetchJson 的全路径（P8：gateway 前缀）：
//   `/api/rsiot/api/v1/history?dev_id=<enc>&tag=<enc>&start=<秒>&end=<秒>
//    [&agg=<a>&interval=<秒>]&limit=<n>`
//   - 参数序固定：dev_id, tag, start, end, agg?, interval?, limit；
//   - agg 与 intervalSec 成对出现（无 agg 则无 interval）；
//   - limit 缺省输出 10000；devId/tag 经 encodeURIComponent；
//   - startSec/endSec 映射为 start/end（秒，半开区间口径）。
//
// decimate(points, buckets)：
//   - points: {ts:number; value:number}[]（须按 ts 升序传入）；
//   - 桶归属按 ts 线性划分：t0=首点 ts，t1=末点 ts，span=(t1-t0)/buckets，
//     桶索引 = min(floor((ts-t0)/span), buckets-1)（即末桶闭区间含 t1；
//     span===0 时全部点归桶 0）；空桶剔除后输出；
//   - 每桶输出 {ts: 桶内首点 ts, min, max, last: 桶内末点 value}。
//
// buildSparklinePath(d, w, h)：
//   - d 为空 → 返回 ''（空串，SVG 直接不渲染 path）；
//   - 非空 → 单一折线串，格式 `M x0 y0 L x1 y1 L x2 y2 ...`（动词与数字
//     间、数字与数字间单空格），点序 = 每桶先 (x, ymax) 后 (x, ymin)
//     交替（max 线在上、min 线在下的单一 path）；
//   - x 按 ts 线性映射（M8）：ts0/ts1 取首末桶 ts，
//     x = Math.round((ts-ts0)/(ts1-ts0) * w)（首桶 0 末桶 w）；全同 ts
//     （ts1==ts0）回退桶序均分 x_k = Math.round(k * w / (n-1))，n===1 取 0；
//   - y = Math.round(h - (v - vmin) / (vmax - vmin) * h)，vmin/vmax 取全部
//     桶 min/max 全局（值大 y 小，SVG y 向下）；vmin===vmax（全等值）时
//     y 恒为 Math.round(h/2)；坐标一律取整、十进制无小数输出。
//
// historyErrorMessage(status, body)：
//   - 503 且 body 为 JSON 含 reason 时映射中文：
//       feature_not_compiled → 文案含「编译未含 history feature」
//       history_disabled     → 文案含「历史存储未启用」
//       init_failed          → 文案含「历史引擎初始化失败」
//     其他 reason → 文案含原始 reason 串；
//   - 400：body 为 JSON 时文案含其 error 字段值；纯文本时文案含原文；
//   - 其他状态码（含 500）→ 文案含状态码十进制串。
//
// windowToRange(preset, nowSec)：
//   返回 { startSec: nowSec-Δ, endSec: nowSec }，Δ：15m=900、1h=3600、
//   6h=21600、24h=86400。
// ── 契约结束 ─────────────────────────────────────────────────────────

import { describe, expect, it } from 'vitest'
import {
  buildHistoryUrl,
  buildSparklinePath,
  decimate,
  historyErrorMessage,
  numericPoints,
  parseHistoryBody,
  windowToRange,
  type DecimatedPoint,
  type HistoryQueryParams,
  type WindowPreset,
} from './historyQuery'

/** 从 `M x y L x y ...` 路径串解析出坐标对（性质断言用）。 */
function parsePath(path: string): Array<[number, number]> {
  const nums = (path.match(/-?\d+(?:\.\d+)?/g) ?? []).map(Number)
  const pts: Array<[number, number]> = []
  for (let i = 0; i + 1 < nums.length; i += 2) pts.push([nums[i], nums[i + 1]])
  return pts
}

describe('buildHistoryUrl（P8：gateway 全路径 /api/rsiot/api/v1/history，参数序 dev_id,tag,start,end,agg?,interval?,limit）', () => {
  it('基础参数：秒命名 start/end，limit 缺省 10000，精确整串', () => {
    const p: HistoryQueryParams = { devId: 'devA', tag: 'temp', startSec: 100, endSec: 200 }
    expect(buildHistoryUrl(p)).toBe(
      '/api/rsiot/api/v1/history?dev_id=devA&tag=temp&start=100&end=200&limit=10000',
    )
  })
  it('agg+intervalSec 成对出现，序在 end 后 limit 前', () => {
    const p: HistoryQueryParams = {
      devId: 'devA', tag: 'temp', startSec: 0, endSec: 600, agg: 'avg', intervalSec: 60,
    }
    expect(buildHistoryUrl(p)).toBe(
      '/api/rsiot/api/v1/history?dev_id=devA&tag=temp&start=0&end=600&agg=avg&interval=60&limit=10000',
    )
  })
  it('limit 显式 500 → &limit=500', () => {
    const p: HistoryQueryParams = { devId: 'devA', tag: 'temp', startSec: 100, endSec: 200, limit: 500 }
    expect(buildHistoryUrl(p)).toBe(
      '/api/rsiot/api/v1/history?dev_id=devA&tag=temp&start=100&end=200&limit=500',
    )
  })
  it('agg 有 intervalSec 无（半配置）→ throw「agg 与 intervalSec 必须成对提供」', () => {
    const p: HistoryQueryParams = { devId: 'devA', tag: 'temp', startSec: 0, endSec: 600, agg: 'avg' }
    expect(() => buildHistoryUrl(p)).toThrowError('agg 与 intervalSec 必须成对提供')
  })
  it('devId/tag 特殊字符经 encodeURIComponent（空格→%20、&→%26）', () => {
    const p: HistoryQueryParams = { devId: 'de v.1', tag: 'tag&x', startSec: 1, endSec: 2, limit: 10 }
    expect(buildHistoryUrl(p)).toBe(
      '/api/rsiot/api/v1/history?dev_id=de%20v.1&tag=tag%26x&start=1&end=2&limit=10',
    )
  })
})

describe('decimate（min/max/last 抽稀：ts 线性分桶 span=(t1-t0)/buckets，末桶含 t1）', () => {
  it('10 个等距点分 3 桶 → 3 组精确 min/max/last（桶 ts=桶内首点 ts；末桶含 end 点）', () => {
    const points = Array.from({ length: 10 }, (_, i) => ({ ts: i, value: i }))
    const expected: DecimatedPoint[] = [
      { ts: 0, min: 0, max: 2, last: 2 }, // ts 0,1,2
      { ts: 3, min: 3, max: 5, last: 5 }, // ts 3,4,5
      { ts: 6, min: 6, max: 9, last: 9 }, // ts 6,7,8,9（end=9 clamp 入末桶）
    ]
    expect(decimate(points, 3)).toEqual(expected)
  })
  it('空数组 → []', () => {
    expect(decimate([], 3)).toEqual([])
  })
  it('单点单桶（span 退化 0 时不除零）', () => {
    expect(decimate([{ ts: 42, value: 7 }], 1)).toEqual([{ ts: 42, min: 7, max: 7, last: 7 }])
  })
  it('points 数 < buckets：空桶剔除，每点一桶，长度==points.length', () => {
    const points = [
      { ts: 0, value: 3 },
      { ts: 10, value: 1 },
      { ts: 20, value: 2 },
    ]
    const out = decimate(points, 5) // span=4 → ts 0→桶0、10→桶2、20→桶4
    expect(out).toHaveLength(points.length)
    expect(out.map(d => d.ts)).toEqual([0, 10, 20])
    expect(out).toEqual<DecimatedPoint[]>([
      { ts: 0, min: 3, max: 3, last: 3 },
      { ts: 10, min: 1, max: 1, last: 1 },
      { ts: 20, min: 2, max: 2, last: 2 },
    ])
  })
  it('非均匀点：桶归属按 ts 线性划分而非按点序均分', () => {
    const points = [0, 1, 5, 6, 10].map(ts => ({ ts, value: ts }))
    const out = decimate(points, 2) // span=5：桶0=[0,5) 含 ts 0,1；桶1=[5,10] 含 ts 5,6,10
    expect(out).toEqual<DecimatedPoint[]>([
      { ts: 0, min: 0, max: 1, last: 1 },
      { ts: 5, min: 5, max: 10, last: 10 },
    ])
  })
})

describe('buildSparklinePath（每桶 (x,ymax) 与 (x,ymin) 交替单一折线；等值时 y=h/2）', () => {
  it('全等值序列（各桶 min==max）：y 恒为 h/2，路径无 NaN，精确整串', () => {
    const d: DecimatedPoint[] = [0, 1, 2, 3].map(k => ({ ts: k, min: 5, max: 5, last: 5 }))
    const path = buildSparklinePath(d, 100, 40)
    expect(path).not.toContain('NaN')
    // x=round(k*100/3)=0,33,67,100；等值 y=round(40/2)=20；每桶 max/min 两点同坐标
    expect(path).toBe('M 0 20 L 0 20 L 33 20 L 33 20 L 67 20 L 67 20 L 100 20 L 100 20')
  })
  it('正常序列：精确整串——vmin/vmax 全局映射，max 点在前 min 点在后', () => {
    const d: DecimatedPoint[] = [
      { ts: 0, min: 0, max: 10, last: 0 },
      { ts: 1, min: 2, max: 8, last: 5 },
      { ts: 2, min: 4, max: 6, last: 9 },
    ]
    // 全局 vmin=0 vmax=10；y=round(40-(v-0)/10*40)=40-4v；x=0,50,100
    // 桶0: (0,y(10)=0),(0,y(0)=40)；桶1: (50,8),(50,32)；桶2: (100,16),(100,24)
    expect(buildSparklinePath(d, 100, 40)).toBe(
      'M 0 0 L 0 40 L 50 8 L 50 32 L 100 16 L 100 24',
    )
  })
  it('贴边极值序列：M 开头 L 连接，x 单调不减且 ∈[0,w]，y ∈[0,h]（解析断言）', () => {
    const d: DecimatedPoint[] = [
      { ts: 0, min: 0, max: 10, last: 0 },
      { ts: 1, min: 10, max: 10, last: 10 },
      { ts: 2, min: 0, max: 0, last: 0 },
      { ts: 3, min: 3, max: 7, last: 5 },
      { ts: 4, min: 5, max: 5, last: 5 },
    ]
    const w = 200
    const h = 50
    const path = buildSparklinePath(d, w, h)
    // 结构：动词 token 首个 M、其余 L（每桶 2 点 → 10 点）
    const tokens = path.split(' ')
    expect(tokens[0]).toBe('M')
    const verbs = tokens.filter((_, i) => i % 3 === 0)
    expect(verbs[0]).toBe('M')
    expect(verbs.slice(1)).toEqual(Array<string>(verbs.length - 1).fill('L'))
    // 坐标性质：x 单调不减且落 [0,w]；y 落 [0,h]（vmax/vmin 贴边不越界）
    const pts = parsePath(path)
    expect(pts).toHaveLength(d.length * 2)
    for (let i = 0; i < pts.length; i++) {
      const [x, y] = pts[i]
      expect(x).toBeGreaterThanOrEqual(0)
      expect(x).toBeLessThanOrEqual(w)
      expect(y).toBeGreaterThanOrEqual(0)
      expect(y).toBeLessThanOrEqual(h)
      if (i > 0) expect(x).toBeGreaterThanOrEqual(pts[i - 1][0])
    }
    expect(pts[0][0]).toBe(0)
    expect(pts[pts.length - 1][0]).toBe(w)
  })
  it('非均匀 ts 间隔：x 按 ts 线性比例 ts=0/90/100 → x=0/90/100，非桶序均分（M8）', () => {
    const d: DecimatedPoint[] = [
      { ts: 0, min: 0, max: 0, last: 0 },
      { ts: 90, min: 5, max: 5, last: 5 },
      { ts: 100, min: 10, max: 10, last: 10 },
    ]
    // vmin=0 vmax=10 h=40 → y=40-4v（0→40、5→20、10→0）；每桶 min==max 两点同坐标；
    // 桶序均分将得 x=0,50,100——中桶 50≠90 即红（稀疏序列时间轴失真）
    expect(buildSparklinePath(d, 100, 40)).toBe(
      'M 0 40 L 0 40 L 90 20 L 90 20 L 100 0 L 100 0',
    )
  })
  it('全同 ts（ts1==ts0，n>1）：回退桶序均分 x=round(k*w/(n-1))（M8 除零回退）', () => {
    const d: DecimatedPoint[] = [7, 7, 7, 7].map(ts => ({ ts, min: 3, max: 3, last: 3 }))
    // 等值 y=round(40/2)=20；x=round(k*100/3)=0,33,67,100
    expect(buildSparklinePath(d, 100, 40)).toBe(
      'M 0 20 L 0 20 L 33 20 L 33 20 L 67 20 L 67 20 L 100 20 L 100 20',
    )
  })
  it('空数组 → 空串（契约：不渲染 path）', () => {
    expect(buildSparklinePath([], 100, 40)).toBe('')
  })
})

describe('numericPoints（无效点过滤：非 typeof number 值剔除并计数，H2 前端防御）', () => {
  it('混合 [数值, null, 字符串] → numeric 仅数值点、skipped==2（精确整串）', () => {
    const pts: Array<{ ts: number; value: unknown }> = [
      { ts: 1, value: 5 },
      { ts: 2, value: null },
      { ts: 3, value: 'x' },
    ]
    expect(numericPoints(pts)).toEqual({
      numeric: [{ ts: 1, value: 5 }],
      skipped: 2,
    })
  })
  it('全数值点 → numeric 全量保序、skipped==0', () => {
    expect(numericPoints([{ ts: 1, value: 1.5 }, { ts: 2, value: -2 }])).toEqual({
      numeric: [{ ts: 1, value: 1.5 }, { ts: 2, value: -2 }],
      skipped: 0,
    })
  })
  it('Bool/Str 混排 → 布尔同样剔除（typeof number 之外全非）', () => {
    const pts: Array<{ ts: number; value: unknown }> = [
      { ts: 1, value: true },
      { ts: 2, value: 3 },
      { ts: 3, value: false },
    ]
    expect(numericPoints(pts)).toEqual({
      numeric: [{ ts: 2, value: 3 }],
      skipped: 2,
    })
  })
})

describe('historyErrorMessage（503 reason 三态中文归一 + 400 双形态 + 其他状态透传）', () => {
  it('503 reason=history_disabled → 含「历史存储未启用」', () => {
    const msg = historyErrorMessage(503, '{"error":"history unavailable","reason":"history_disabled"}')
    expect(msg).toContain('历史存储未启用')
  })
  it('503 reason=init_failed → 含「历史引擎初始化失败」；feature_not_compiled → 含「编译未含 history feature」', () => {
    expect(historyErrorMessage(503, '{"error":"history unavailable","reason":"init_failed"}'))
      .toContain('历史引擎初始化失败')
    expect(historyErrorMessage(503, '{"error":"history unavailable","reason":"feature_not_compiled"}'))
      .toContain('编译未含 history feature')
  })
  it('503 其他 reason → 含原始 reason 串', () => {
    const msg = historyErrorMessage(503, '{"error":"history unavailable","reason":"disk_dead"}')
    expect(msg).toContain('disk_dead')
  })
  it('400 纯文本 body → 含原文', () => {
    expect(historyErrorMessage(400, 'invalid parameter: start')).toContain('invalid parameter: start')
  })
  it('400 JSON body → 含 error 字段值', () => {
    expect(historyErrorMessage(400, '{"error":"unsupported aggregation"}'))
      .toContain('unsupported aggregation')
  })
  it('500 纯文本 body → 含状态码串「500」（未知态透传兜底）', () => {
    expect(historyErrorMessage(500, 'oops')).toContain('500')
  })
  it('500 JSON body → 含 error 字段值（引擎错误串透传，审查 L9）', () => {
    expect(historyErrorMessage(500, '{"error":"history engine: lock occupied"}'))
      .toContain('history engine: lock occupied')
  })
})

describe('windowToRange（窗口预设 → {startSec,endSec}，endSec==nowSec）', () => {
  const now = 1_000_000
  it('四预设差值精确（900/3600/21600/86400）', () => {
    const presets: WindowPreset[] = ['15m', '1h', '6h', '24h']
    const deltas: Record<WindowPreset, number> = { '15m': 900, '1h': 3600, '6h': 21600, '24h': 86400 }
    for (const preset of presets) {
      const r = windowToRange(preset, now)
      expect(r).toEqual({ startSec: now - deltas[preset], endSec: now })
    }
  })
})

describe('parseHistoryBody（200 响应体解析，M9：解析失败≠网关不可达）', () => {
  it('合法 JSON → ok:true 且 data 保形', () => {
    const r = parseHistoryBody('{"dev_id":"devA","tag":"temp","points":[{"ts":1,"value":2}],"count":1}')
    expect(r.ok).toBe(true)
    if (r.ok) {
      expect(r.data.dev_id).toBe('devA')
      expect(r.data.tag).toBe('temp')
      expect(r.data.points).toEqual([{ ts: 1, value: 2 }])
      expect(r.data.count).toBe(1)
    }
  })
  it('非法 JSON（网关误路由/HTML 错误页）→ ok:false 且文案区分于网络错误', () => {
    const r = parseHistoryBody('<html>502 Bad Gateway</html>')
    expect(r.ok).toBe(false)
    if (!r.ok) expect(r.message).toBe('响应解析失败（网关返回非 JSON）')
  })
})
