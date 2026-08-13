// frontend/src/stores/agent.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// agent.ts 静态 import { api } → api/client.ts import { router } →
// router.ts createWebHistory() 在模块加载期读 window（node 测试环境无 window）。
// mock 掉 api 短路整条 import 链（vi.mock 提升至所有 import 之前，真实 client.ts 不加载）。
// 默认返回 200 + 空 body；个别用例用 mockResolvedValueOnce 覆盖（如 approve 失败路径）。
vi.mock('../api/client', () => ({ api: vi.fn(async () => new Response('{}')) }))

import { api } from '../api/client'
import { useAgentStore } from './agent'
import { useMonitorStore } from './monitor'
import { useWorkspaceStore } from './workspace'

describe('agent store', () => {
  beforeEach(() => setActivePinia(createPinia()))

  it('text event appends to current assistant message', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'text', text: 'hello ' })
    s.onEvent({ kind: 'text', text: 'world' })
    expect(s.messages.at(-1)?.role).toBe('assistant')
    expect(s.messages.at(-1)?.text).toBe('hello world')
  })

  it('tool_dispatch then tool_result populates toolCalls', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'tool_dispatch', tool: { id: 'c1', name: 'rsiot_read_tags', args: '{"dev":"devA"}' } })
    s.onEvent({ kind: 'tool_result', tool: { id: 'c1', name: 'rsiot_read_tags', output: '{"v":1}', durationMs: 12 } })
    const tc = s.messages.at(-1)?.toolCalls?.[0]
    expect(tc?.name).toBe('rsiot_read_tags')
    expect(tc?.args).toBe('{"dev":"devA"}')
    expect(tc?.result).toBe('{"v":1}')
    expect(tc?.error).toBe(false)
    expect(tc?.durationMs).toBe(12)
  })

  it('tool_result with err marks the call failed (T9 I1)', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'tool_dispatch', tool: { id: 'c2', name: 'rsiot_write_tag' } })
    s.onEvent({ kind: 'tool_result', tool: { id: 'c2', name: 'rsiot_write_tag', err: 'device offline' } })
    const tc = s.messages.at(-1)?.toolCalls?.[0]
    expect(tc?.error).toBe(true)
    expect(tc?.errorMsg).toBe('device offline')
  })

  it('approval_request sets the approval state (e.approval wire path)', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'approval_request', approval: { id: 'a1', tool: 'rsiot_write_tag', subject: '写入 devA.tag1=1', reason: '写操作' } })
    expect(s.approval?.id).toBe('a1')
    expect(s.approval?.tool).toBe('rsiot_write_tag')
    expect(s.approval?.subject).toBe('写入 devA.tag1=1')
  })

  it('reasoning event appends to reasoning buffer', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'reasoning', text: 'Planning' })
    expect(s.messages.at(-1)?.reasoning).toBe('Planning')
  })

  it('approve keeps approval on non-2xx (review I1: no deadlock)', async () => {
    // I1 意图：gateway/serve 返错时绝不能清 approval——否则 agent 仍在等决策、
    // UI 已无弹窗，用户无法重发 → 死锁。approval 必须保留到下一次成功提交。
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'approval_request', approval: { id: 'a1', tool: 'rsiot_write_tag' } })
    expect(s.approval?.id).toBe('a1')
    vi.mocked(api).mockResolvedValueOnce(new Response('bad', { status: 500 }))
    await expect(s.approve(true)).rejects.toThrow('approve failed: 500')
    // 关键断言：失败后 approval 仍在（用户可重发决策）
    expect(s.approval?.id).toBe('a1')
  })

  it('approve clears approval on 2xx success', async () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'approval_request', approval: { id: 'a2', tool: 'rsiot_write_tag' } })
    expect(s.approval?.id).toBe('a2')
    // 默认 mock 返回 200
    await s.approve(false)
    expect(s.approval).toBeNull()
  })

  it('submit injects current page context and visible data summary', async () => {
    const monitor = useMonitorStore()
    monitor.selectInstance('mb-power')
    monitor.selectDevice('devA')

    const workspace = useWorkspaceStore()
    workspace.setRouteContext({
      routeKey: 'telemetry',
      routeTitle: '实时数据',
      routePath: '/telemetry',
    })
    workspace.setPageContext({
      pageSummary: '正在查看 devA 的关键点位变化',
      facts: ['筛选关键字: temp', '可见点位 12 个'],
    })

    const s = useAgentStore()
    await s.submit('帮我看看当前是不是有异常')

    const [, options] = vi.mocked(api).mock.calls.at(-1) ?? []
    const body = options?.body ? JSON.parse(String(options.body)) : {}
    expect(body.input).toContain('帮我看看当前是不是有异常')
    expect(body.input).toContain('当前页面: 实时数据')
    expect(body.input).toContain('页面路径: /telemetry')
    expect(body.input).toContain('当前实例: mb-power')
    expect(body.input).toContain('当前设备: devA')
    expect(body.input).toContain('可见点位 12 个')
  })

  it('stop 在 running 时本地复位 + fire-and-forget cancel（审查 Important 修复）', async () => {
    // 锁的不变量：关抽屉断 SSE 后 turn_done 可能永久丢失（纯字节管道无 replay），
    // stop() 必须本地兜底复位 running（不再等 turn_done）+ 尽力 cancel 服务端 turn。
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    expect(s.running).toBe(true)
    s.stop()
    // running 立即复位（不等异步 cancel 完成）。
    expect(s.running).toBe(false)
    // cancel 被调（fire-and-forget，flush 微任务后断言）。
    await new Promise(r => setTimeout(r, 0))
    expect(vi.mocked(api)).toHaveBeenCalledWith('/api/agent/cancel', expect.objectContaining({ method: 'POST' }))
  })

  it('stop 在非 running 时不发 cancel（正常关闭无副作用）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockClear()  // 清跨测试累积的调用历史
    s.stop()
    await new Promise(r => setTimeout(r, 0))
    expect(vi.mocked(api)).not.toHaveBeenCalled()
  })
})
