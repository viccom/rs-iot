// frontend/src/stores/agent.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { ChatMessage, ServeEvent } from '../types'
import { useAuthStore } from './auth'
import { useMonitorStore } from './monitor'
import { api } from '../api/client'

/**
 * agent store：serve SSE kind 分发 + submit / approve / cancel。
 *
 * 与 monitor store 的关键差异——serve SSE 是**无 `event:` 头的裸 NDJSON**，
 * kind 在 JSON body 内（`{kind:"text",text:"..."}`）。故前端不走
 * `addEventListener(kind)`（那样一帧都收不到，因为没有 `event:` 头），而是
 * `onmessage` 收全部帧 → `JSON.parse` → 按 body 内 `kind` 分发。
 *
 * 事件路径走 gateway `/api/agent/*`（**不带** `/api/v1/`，serve 端点本身无该
 * 前缀；这与 monitor 的 `/api/rsiot/api/v1/*` 不同——rs-iot 端点带 /api/v1/）。
 */
export const useAgentStore = defineStore('agent', () => {
  /** 聊天消息列表（user 输入 + assistant 聚合 turn）。 */
  const messages = ref<ChatMessage[]>([])
  /** 当前 turn 是否进行中（turn_started → turn_done）。 */
  const running = ref(false)
  /** 待审批请求（写操作触发，由 UI 决策 approve/deny）。 */
  const approval = ref<{ id: string; tool: string; args?: unknown } | null>(null)

  let nextId = 1
  let sse: EventSource | null = null

  /**
   * 处理 serve SSE 事件（无 `event:` 头，kind 在 JSON body）。
   *
   * 一条 assistant 消息聚合一个 turn：turn_started 建空消息，text/reasoning
   * 增量拼接，tool_dispatch 建立工具槽位由 tool_result 填充结果。
   */
  function onEvent(e: ServeEvent) {
    const cur = () => messages.value.at(-1)
    switch (e.kind) {
      case 'turn_started':
        messages.value.push({ id: nextId++, role: 'assistant', text: '', toolCalls: [] })
        running.value = true
        break
      case 'text':
        if (cur()?.role === 'assistant') cur()!.text += e.text || ''
        break
      case 'reasoning':
        if (cur()?.role === 'assistant') cur()!.reasoning = (cur()!.reasoning || '') + (e.text || '')
        break
      case 'tool_dispatch':
        if (e.tool) cur()?.toolCalls?.push({ id: e.tool.id, name: e.tool.name })
        break
      case 'tool_result': {
        // serve 把工具元信息放 body 的 tool 字段（含 id），与 dispatch 对齐。
        const tid = (e.tool as { id?: string } | undefined)?.id
        const tc = cur()?.toolCalls?.find(t => t.id === tid)
        if (tc) { tc.result = e; tc.error = false }
        break
      }
      case 'approval_request':
        approval.value = {
          id: (e.approval_request as { id: string }).id,
          tool: (e.approval_request as { tool: string }).tool,
          args: (e.approval_request as { args?: unknown }).args,
        }
        break
      case 'turn_done':
        running.value = false
        break
    }
  }

  /** 发任务。附当前监控选中实例上下文（页面感知：让 agent 知道用户在看哪个采集器）。 */
  async function submit(input: string) {
    messages.value.push({ id: nextId++, role: 'user', text: input })
    const monitor = useMonitorStore()
    const ctx = monitor.selectedInstance
      ? `\n[上下文: 当前查看实例 ${monitor.selectedInstance}]`
      : ''
    await api('/api/agent/submit', { method: 'POST', body: JSON.stringify({ input: input + ctx }) })
  }

  /** 提交审批决策（写操作）：true 放行 / false 拒绝。 */
  async function approve(decision: boolean) {
    if (!approval.value) return
    await api('/api/agent/approve', {
      method: 'POST',
      body: JSON.stringify({ approval_id: approval.value.id, decision }),
    })
    approval.value = null
  }

  /** 取消当前 turn（用户中止）。 */
  async function cancel() {
    await api('/api/agent/cancel', { method: 'POST', body: '{}' })
  }

  /** 启动 SSE 订阅。登录后调用一次。token 走 query（EventSource 不能设 header）。 */
  function start() {
    if (sse) stop() // 重入守卫（如重连场景）
    const tok = useAuthStore().token
    sse = new EventSource(`/api/agent/events?token=${encodeURIComponent(tok)}`)
    // onmessage 接全部帧（无 event: 头）→ safe 包 parse（畸形帧不让 onmessage 抛错，
    // 抛错后该 listener 被 EventSource 摘除）。parse 必须在 safe thunk 内执行
    // （若作 safe 实参先求值，parse 抛错会绕过 try）——对齐 monitor store 既有约定。
    sse.onmessage = ev => safe(() => onEvent(JSON.parse((ev as MessageEvent).data) as ServeEvent))
    // onerror：浏览器自动重连，这里静默。
    sse.onerror = () => {}
  }

  /** SSE 帧 parse/handle 容错：畸形帧 warn 并跳过，不冒泡到 onmessage。 */
  function safe(fn: () => void) {
    try { fn() } catch (e) { console.warn('agent: drop malformed SSE frame', e) }
  }

  /** 停止 SSE（登出 / 路由切走时调用）。 */
  function stop() {
    sse?.close()
    sse = null
  }

  return { messages, running, approval, onEvent, submit, approve, cancel, start, stop }
})
