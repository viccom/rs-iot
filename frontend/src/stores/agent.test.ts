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
    s.onEvent({ kind: 'tool_dispatch', tool: { id: 'c1', name: 'rsiot_read_tags' } })
    s.onEvent({ kind: 'tool_result', name: 'rsiot_read_tags', tool: { id: 'c1', name: 'rsiot_read_tags' } })
    expect(s.messages.at(-1)?.toolCalls?.[0].name).toBe('rsiot_read_tags')
  })

  it('reasoning event appends to reasoning buffer', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'reasoning', text: 'Planning' })
    expect(s.messages.at(-1)?.reasoning).toBe('Planning')
  })
})
