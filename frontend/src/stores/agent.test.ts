// frontend/src/stores/agent.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// agent.ts 静态 import { api } → api/client.ts import { router } →
// router.ts createWebHistory() 在模块加载期读 window（node 测试环境无 window）。
// 本测只验 onEvent 分发（不调 submit/approve/cancel），api 不在测路径上，mock
// 掉即可短路整条 import 链（vi.mock 提升至所有 import 之前，真实 client.ts 不加载）。
vi.mock('../api/client', () => ({ api: vi.fn(async () => new Response('{}')) }))

import { useAgentStore } from './agent'

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
})
