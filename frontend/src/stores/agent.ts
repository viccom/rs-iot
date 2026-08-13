// frontend/src/stores/agent.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { AskAnswer, AskQuestion, ChatMessage, ServeEvent } from '../types'
import { useAuthStore } from './auth'
import { useMonitorStore } from './monitor'
import { api } from '../api/client'
import { useWorkspaceStore } from './workspace'
import { buildAgentContextBlock } from '../lib/agentContext'

/**
 * agent store：serve SSE kind 分发 + submit / approve / cancel。
 *
 * 与 monitor store 的关键差异——serve SSE 是**无 `event:` 头的裸 NDJSON**，
 * kind 在 JSON body 内（`{kind:"text",text:"..."}`）。故前端不走
 * `addEventListener(kind)`（那样一帧都收不到，因为没有 `event:` 头），而是
 * `onmessage` 收全部帧 → `JSON.parse` → 按 body 内 `kind` 分发。
 *
 * 字段路径对标 serve eventwire 契约（`DeepSeek-Reasonix/internal/eventwire/wire.go`）：
 * `tool_dispatch` / `tool_result` 的工具信息在 `e.tool`（含 id/name/args/output/err）；
 * `approval_request` 的审批载荷在 `e.approval`（含 id/tool/subject/reason）。
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
  const approval = ref<{ id: string; tool: string; subject?: string; reason?: string } | null>(null)
  /** 待回答的结构化提问（ask_request 事件：AI 主动向用户提问，回答前 turn 挂起）。 */
  const ask = ref<{ id: string; questions: AskQuestion[] } | null>(null)

  let nextId = 1
  let sse: EventSource | null = null

  /**
   * 处理 serve SSE 事件（无 `event:` 头，kind 在 JSON body）。
   *
   * 一条 assistant 消息聚合一个 turn：turn_started 建空消息，text/reasoning
   * 增量拼接，tool_dispatch 建立工具槽位（携带 args）由 tool_result 填充
   * output/err。
   */
  function onEvent(e: ServeEvent) {
    const cur = () => messages.value.at(-1)
    switch (e.kind) {
      case 'turn_started':
        messages.value.push({ id: nextId++, role: 'assistant', text: '', toolCalls: [] })
        running.value = true
        break
      case 'text': {
        // 缓存 last 一次：消除 cur()! 非空断言链的 NPE 风险（review I2）——
        // 同步路径下两次 cur() 间 messages 不变，但缓存零成本且防御未来 reset/异步插入。
        const last = cur()
        if (last?.role === 'assistant') last.text += e.text || ''
        break
      }
      case 'reasoning': {
        const last = cur()
        if (last?.role === 'assistant') last.reasoning = (last.reasoning || '') + (e.text || '')
        break
      }
      case 'tool_dispatch': {
        // 携带 args（serve tool.args 字符串）；readOnly 标记只读工具（不审批）。
        const last = cur()
        if (e.tool && last?.role === 'assistant' && last.toolCalls) {
          last.toolCalls.push({ id: e.tool.id, name: e.tool.name, args: e.tool.args })
        }
        break
      }
      case 'tool_result': {
        // serve 把工具结果放 body 的 tool 字段（含 id）：output=正常输出串、
        // err=失败原因串（非空表示失败，对标 T9 follow-up I1）。
        const tid = e.tool?.id
        const last = cur()
        const tc = last?.toolCalls?.find(t => t.id === tid)
        if (tc && e.tool) {
          tc.result = e.tool.output
          tc.error = !!e.tool.err
          tc.errorMsg = e.tool.err || undefined
          tc.durationMs = e.tool.durationMs
        }
        break
      }
      case 'approval_request':
        // 审批载荷在 e.approval（对标 serve eventwire.Approval）。
        if (e.approval) {
          approval.value = {
            id: e.approval.id,
            tool: e.approval.tool,
            subject: e.approval.subject,
            reason: e.approval.reason,
          }
        }
        break
      case 'ask_request':
        // AI 主动向用户提问（eventwire.Ask：id + questions[header/prompt/options/multi]）。
        // 回答前 serve 挂起 turn（前端 AskModal 决策后调 answer() 解除）。
        if (e.ask) {
          ask.value = { id: e.ask.id, questions: e.ask.questions }
        }
        break
      case 'turn_done':
        running.value = false
        ask.value = null  // turn 结束清残留提问（防御：未答即结束的 turn）
        break
    }
  }

  /** 发任务。附当前监控选中实例上下文（页面感知：让 agent 知道用户在看哪个采集器）。
   *  非 2xx 响应抛错（M2：submit 失败给 UI 反馈，由调用方 catch 提示用户），
   *  错误带响应体（M5：让 AgentDrawer 的 errorMsg 更可读）。 */
  async function submit(input: string) {
    messages.value.push({ id: nextId++, role: 'user', text: input })
    const monitor = useMonitorStore()
    const workspace = useWorkspaceStore()
    const contextBlock = buildAgentContextBlock({
      routeTitle: workspace.routeContext.routeTitle,
      routePath: workspace.routeContext.routePath,
      pageSummary: workspace.pageContext.pageSummary,
      selectedInstanceId: monitor.selectedInstance,
      selectedDeviceId: monitor.selectedDevice,
      facts: workspace.pageContext.facts,
    })
    const requestInput = contextBlock
      ? `${input}\n\n[页面上下文]\n${contextBlock}`
      : input
    const r = await api('/api/agent/submit', {
      method: 'POST',
      body: JSON.stringify({ input: requestInput }),
    })
    if (!r.ok) throw new Error(`submit failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
  }

  /** 提交审批决策（写操作）：true 放行 / false 拒绝。
   *
   *  body 契约对标 serve `POST /approve`（serve.go）：`{id, allow, session?,,
   *  persist?}`——注意是 `id`/`allow`（非 approval_id/decision）。历史代码字段名
   *  全错（approval_id+decision），单测 mock 掩盖致现场 approval 从未真实往返
   *  （approve 返 400 missing id，用户无法批准任何写操作）。
   *
   *  非 2xx 响应抛错（review I1：approve 失败死锁——若 gateway/serve 返错且不
   *  检查就清 approval，agent 仍在等决策但 UI 已无弹窗 → 用户无法重发 → 死锁），
   *  错误带响应体（M5）。仅 2xx 才清 approval。 */
  async function approve(decision: boolean) {
    if (!approval.value) return
    const r = await api('/api/agent/approve', {
      method: 'POST',
      body: JSON.stringify({ id: approval.value.id, allow: decision }),
    })
    if (!r.ok) throw new Error(`approve failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    approval.value = null
  }

  /** 取消当前 turn（用户中止）。失败不抛：turn_done SSE 会复位 running（容错
   *  容忍高——用户已按停止，最坏情况是 running 多停留到下一个 turn_done）。 */
  async function cancel() {
    await api('/api/agent/cancel', { method: 'POST', body: '{}' })
  }

  /** 回答 ask_request（POST /api/agent/answer，body {id, answers:[{question_id, selected}]}）。
   *  2xx 后清 ask（serve 继续 turn）；失败不抛（ask 保持，用户可重选）。 */
  async function answer(answers: AskAnswer[]) {
    if (!ask.value) return
    const r = await api('/api/agent/answer', {
      method: 'POST',
      body: JSON.stringify({ id: ask.value.id, answers }),
    })
    if (!r.ok) throw new Error(`answer failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    ask.value = null
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

  /** 停止 SSE（登出 / 路由切走 / 关抽屉时调用）。
   *
   * 审查 Important 修复：running 时关抽屉（AgentDrawer onUnmounted → 本函数）断 SSE，
   * 断线期间 serve 发出的 turn_done 永久丢失（纯字节管道无 replay）——重开抽屉后
   * running 会永远卡 true、输入框禁用。此处语义收敛为「stop = 放弃当前 turn」：
   * fire-and-forget cancel（fetch 不依赖 SSE）+ 本地兜底复位，不再等 turn_done。 */
  function stop() {
    sse?.close()
    sse = null
    if (running.value) {
      running.value = false
      void cancel().catch(() => { /* cancel 失败静默：本地已复位，服务端 turn 自行结束 */ })
    }
  }

  return { messages, running, approval, ask, onEvent, submit, approve, cancel, answer, start, stop }
})
