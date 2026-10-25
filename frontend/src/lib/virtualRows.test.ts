import { describe, expect, it } from 'vitest'
import { visibleRange } from './virtualRows'

const H = 36

describe('visibleRange（固定行高虚拟窗口）', () => {
  it('常规：scrollTop=0 取首屏 + overscan', () => {
    const r = visibleRange(0, 720, 1000, H, 8)
    expect(r.start).toBe(0)
    expect(r.end).toBe(Math.ceil(720 / H) + 8) // 20 行视口 + 8 overscan
  })

  it('滚动中部：start/end 随 scrollTop 平移', () => {
    const r = visibleRange(3600, 720, 1000, H, 8) // 第 100 行处
    expect(r.start).toBe(100 - 8)
    expect(r.end).toBe(100 + Math.ceil(720 / H) + 8)
  })

  it('尾部夹取：end ≤ total 且 start 不越界', () => {
    const r = visibleRange(1000 * H - 100, 720, 1000, H, 8)
    expect(r.end).toBe(1000)
    expect(r.start).toBe(Math.floor((1000 * H - 100) / H) - 8)
  })

  it('头部夹取：start 不为负（scrollTop 小于 overscan 高度）', () => {
    expect(visibleRange(H, 720, 1000, H, 8).start).toBe(0)
  })

  it('total=0 / rowHeight=0 防御', () => {
    expect(visibleRange(0, 720, 0, H)).toEqual({ start: 0, end: 0 })
    expect(visibleRange(0, 720, 10, 0)).toEqual({ start: 0, end: 0 })
  })

  it('视口大于总高：全渲染', () => {
    expect(visibleRange(0, 100000, 10, H, 8)).toEqual({ start: 0, end: 10 })
  })

  it('负 scrollTop 防御：start 夹到 0', () => {
    expect(visibleRange(-50, 720, 100, H, 8).start).toBe(0)
  })
})
