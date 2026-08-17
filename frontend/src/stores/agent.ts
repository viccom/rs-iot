// frontend/src/stores/agent.ts
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import type {
  AskAnswer,
  AskQuestion,
  ChatMessage,
  LLMProvider,
  ServeEvent,
  ServeHistoryMessage,
  ServeModel,
  ServeSession,
  ServeStatus,
  ToolApprovalMode,
  WorkspaceEntry,
} from '../types'
import { useAuthStore } from './auth'
import { useMonitorStore } from './monitor'
import { api } from '../api/client'
import { useWorkspaceStore } from './workspace'
import { buildAgentContextBlock } from '../lib/agentContext'

/**
 * effort 档位映射（来源：Reasonix internal/config/effort.go:48 EffortCapabilityForEntry
 * 及其分支函数——词汇表随版本演进，改动需同步官方源码）。
 *
 * serve 的 HTTP 契约不暴露 effort 词汇表（/status /models 均无该字段）：官方 desktop
 * 是进程内直接调 config 层函数渲染下拉（.effortsw__menu），serve 自带前端也只有
 * `/effort <level>` 命令面板。但词汇表输入只有 provider + model（/models 正好返回），
 * 故在此维护同款映射供前端下拉；未知 provider/model 返回空数组 → UI 退化为自由文本
 * （serve 非法值 500 提示合法档位）。
 */
export function effortLevelsFor(provider: string, model: string): string[] {
  const p = provider.toLowerCase()
  const m = model.toLowerCase()
  // deepseek-v4-flash 独有 low 档（effortCapabilityFromModel）；其余 deepseek 无 low/medium。
  if (m.includes('deepseek-v4-flash')) return ['auto', 'disabled', 'low', 'high', 'max']
  if (m.includes('deepseek')) return ['auto', 'disabled', 'high', 'max']
  // MiniMax-M3 只有二进制思考旋钮 adaptive|disabled。
  if (p.includes('minimax') || m.includes('minimax') || m.includes('-m3')) return ['auto', 'adaptive', 'disabled']
  // Zhipu GLM / LongCat：二进制思考旋钮。
  if (p.includes('zhipu') || m.includes('glm') || m.includes('longcat')) return ['auto', 'enabled', 'disabled']
  // Ollama Cloud：low|medium|high|max + none 省略字段。
  if (p.includes('ollama') || m.includes('ollama')) return ['auto', 'none', 'low', 'medium', 'high', 'max']
  // Anthropic：深度档位 + xhigh。
  if (p.includes('anthropic') || m.includes('claude')) return ['auto', 'low', 'medium', 'high', 'xhigh', 'max']
  // 通用 OpenAI 兼容端点（openAIEffortCapability）。
  return ['auto', 'low', 'medium', 'high']
}

/**
 * @ 引用路径转义（对标 Reasonix internal/control/refs.go EscapeRefPath）：
 * 空格/制表符前加反斜杠——@token 以空白分界，路径含空格不转义会被截断。
 * 其余字符（含 Windows 反斜杠，本面板只产生 `/` 路径）原样保留。
 */
export function escapeRefPath(path: string): string {
  return path.replace(/[ \t]/g, (ch) => `\\${ch}`)
}

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
/**
 * 剥离 Reasonix 注入用户消息的 transient 块（会话标题/预览/rewind 选择器同款，
 * 源码 internal/agent/preview.go TransientUserBlockTags——单一权威清单）。
 * 宿主把运行时上下文（reasoning-language 偏好、capability-route 工具路由提示
 * 等）前缀注入 user turn 并持久化；serve `/history` 未剥离直接返回（官方 UI
 * 客户端侧剥），前端同款处理——否则 markup 漏进历史会话的用户气泡
 * （<capability-route> 实测泄漏）。execution-policy 特殊：追加在 user 文本
 * 之后（尾缀块，带属性），按上游 reTrailingExecutionPolicy 剥尾。
 */
const TRANSIENT_BLOCK_TAGS = [
  'response-language', 'reasoning-language', 'memory-update', 'background-jobs',
  'active-goal', 'autoresearch-runtime', 'hook-context', 'capability-route',
  'interrupted-turn-recovery',
] as const
const TRANSIENT_LEADING_RE = new RegExp(
  `^\\s*(?:<(?:${TRANSIENT_BLOCK_TAGS.join('|')})(?:\\s+[^>]*)?>[\\s\\S]*?</(?:${TRANSIENT_BLOCK_TAGS.join('|')})>[\\r\\n]*)+`,
)
const TRAILING_EXECUTION_POLICY_RE = /\n*<execution-policy(?:\s+[^>]*)?>[\s\S]*?<\/execution-policy>\s*$/

function stripTransientBlocks(content: string): string {
  return content
    .replace(TRANSIENT_LEADING_RE, '')
    .replace(TRAILING_EXECUTION_POLICY_RE, '')
}

/**
 * plan 模式标记（上游 internal/planmode/policy.go Marker + control/input.go
 * legacy 变体，Compose 在 plan 模式下前缀注入 user turn）。桌面端
 * StripComposePrefixes 剥它后显示，前端同款——两串字面量由脚本从 Go 源码
 * 直接生成（字节级校验），勿手改。
 */
export const PLAN_MODE_MARKERS = [
  '[Plan mode — planning workflow. Gather context, ask clarifying questions with ask, maintain planning state with todo_write, and delegate focused research when useful. Do not begin implementation in this mode: avoid file writes, unsafe shell commands, capability installation, memory mutation, writer-capable delegation, long-lived process control, or execution-step completion. This is a workflow instruction, not a permission boundary; every tool call remains governed by the active Permissions and Sandbox policy. Before planning, if a decision that is genuinely the user\'s — tech stack, an ambiguous requirement, scope, an irreversible choice — would materially shape the plan and you can\'t settle it from the codebase or a sensible default, use the ask tool to clarify it first; otherwise pick the obvious default and state the assumption in the plan instead of asking. Then present a LAYERED plan as your reply and stop. Structure the plan as a two-level markdown list so it becomes a layered task list: each PHASE is a top-level numbered list item (a coherent milestone, e.g. "1. Add the config loader"), and each phase\'s concrete, verifiable sub-steps are bullets indented beneath it (e.g. "   - parse the TOML into Config"). Use plain numbered list items for phases — do NOT write phases as markdown headings (##, ###) — so both levels parse. Keep phases few (about 2-6). The user will be asked to approve the plan before the workflow switches to implementation.]',
  '[Plan mode — read-only. Explore the codebase first (read_file, ls, grep, glob, web_fetch, task, ask are available; writers are refused by the harness). Before planning, if a decision that is genuinely the user\'s — tech stack, an ambiguous requirement, scope, an irreversible choice — would materially shape the plan and you can\'t settle it from the codebase or a sensible default, use the ask tool to clarify it first; otherwise pick the obvious default and state the assumption in the plan instead of asking. Then present a LAYERED plan as your reply and stop — do not write files, edit, or run side-effecting bash. Structure the plan as a two-level markdown list so it becomes a layered task list: each PHASE is a top-level numbered list item (a coherent milestone, e.g. "1. Add the config loader"), and each phase\'s concrete, verifiable sub-steps are bullets indented beneath it (e.g. "   - parse the TOML into Config"). Use plain numbered list items for phases — do NOT write phases as markdown headings (##, ###) — so both levels parse. Keep phases few (about 2-6). The user will be asked to approve before any changes are made.]',
]

function stripPlanModeMarker(content: string): string {
  let s = content
  for (const marker of PLAN_MODE_MARKERS) {
    if (s.startsWith(marker + '\n\n')) s = s.slice(marker.length + 2)
    else if (s.startsWith(marker)) s = s.slice(marker.length)
  }
  return s
}

/**
 * 剥 `Referenced context:` 前导块（上游 control/input.go StripReferencedContextPrefix
 * 的 TS 移植）：@ 引用解析后 serve 把 `Referenced context:\n\n<file ...>...</file>\n\n
 * + 原输入` 整体持久化，桌面端显示层剥掉——/history 原样返回，前端同款剥，
 * 否则注入的文件内容漏进历史气泡（实测泄漏）。块标签多含 path-instructions
 * （resolveReferencedInstructions 注入，上游清单遗漏——此处补上）。
 */
function stripReferencedContextPrefix(content: string): string {
  const preamble = 'Referenced context:'
  let s = content.trim()
  if (!s.startsWith(preamble)) return content
  s = s.slice(preamble.length).trim()
  for (;;) {
    s = s.trim()
    if (!s) return ''
    const open = /^<(file|dir|resource|image|path-instructions) /.exec(s)
    if (!open) break
    const closeTag = `</${open[1]}>`
    const closeIdx = s.indexOf(closeTag)
    if (closeIdx < 0) break
    s = s.slice(closeIdx + closeTag.length).trim()
  }
  return s
}

/** rs-iot 自有页面上下文尾缀（buildAgentContextBlock 追加在 user 输入之后）。 */
const PAGE_CONTEXT_SUFFIX_RE = /\n\n\[页面上下文\]\n[\s\S]*$/

/**
 * 历史回放的用户气泡文本 = 用户实时所见（对标桌面端显示管线
 * StripReferencedContextPrefix(StripComposePrefixes(content))）：
 * 瞬态块 → plan 标记 → Referenced context 前缀逐层剥，再剥页面上下文尾缀。
 */
function userDisplayText(content: string): string {
  return stripReferencedContextPrefix(stripPlanModeMarker(stripTransientBlocks(content)))
    .replace(PAGE_CONTEXT_SUFFIX_RE, '')
    .trim()
}

export const useAgentStore = defineStore('agent', () => {
  /** 聊天消息列表（user 输入 + assistant 聚合 turn）。 */
  const messages = ref<ChatMessage[]>([])
  /** 当前 turn 是否进行中（turn_started → turn_done）。 */
  const running = ref(false)
  /** 待审批请求（写操作触发，由 UI 决策 approve/deny）。 */
  const approval = ref<{ id: string; tool: string; subject?: string; reason?: string } | null>(null)
  /** 待回答的结构化提问（ask_request 事件：AI 主动向用户提问，回答前 turn 挂起）。 */
  const ask = ref<{ id: string; questions: AskQuestion[] } | null>(null)

  /** 历史会话列表（GET /sessions）。serve 单会话进程：current 标记当前活动会话。 */
  const sessions = ref<ServeSession[]>([])
  /** 当前活动会话（sessions 里 current==true 的条目；启动恢复据此定位）。 */
  const currentSession = ref<ServeSession | null>(null)
  /** 可选模型列表（GET /models，控件下拉数据源；active 标记当前模型）。 */
  const models = ref<ServeModel[]>([])
  /** 当前会话状态（GET /status，控件初始态来源：plan/toolApprovalMode）。 */
  const status = ref<ServeStatus | null>(null)
  /** 会话切换/新建/删除进行中（UI 全 disable）。 */
  const sessionBusy = ref(false)
  /** 控件操作进行中（4 控件 disable）。 */
  const controlBusy = ref(false)
  /** LLM 供应商列表（gateway /api/agent/providers，密钥只报有无）。 */
  const providers = ref<LLMProvider[]>([])
  /** 新会话默认模型（"<provider>/<model>"，reasonix config default_model）。 */
  const defaultModel = ref('')
  /** 供应商管理操作进行中（弹窗 disable + 重启等待态）。 */
  const providerBusy = ref(false)

  let nextId = 1
  /** SSE 连接（重连场景 start 前先关）。 */
  let sse: EventSource | null = null
  /** 本次连接是否在 turn 进行中重连（start 时 /status.running=true 置位）。
   *  置位期间关闭面板错过的事件（text 前缀/tool 槽位）无法经 SSE replay——
   *  turn_done 后重拉 /history 自愈完整对话。 */
  let resumedMidTurn = false

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
      case 'notice': {
        // 系统提示（如 capability proxy 路由信息）归集到当前 assistant 消息，
        // 随思考过程折叠展示（对标 aifa）。历史回放的 notice 角色仍是 steer 文本，
        // historyToChatMessages 继续跳过——两者来源不同，不混同。
        const last = cur()
        if (e.text && last?.role === 'assistant') {
          last.notices = [...(last.notices || []), e.text]
        }
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
        void maybeHealHistory()
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


/**
 * serve 历史消息 → ChatMessage 映射（loadHistory 用）。
   *
   * 规则（对标 serve.go:838-884 historyMessage）：
   * - user → 新建 user 消息；assistant → 新建 assistant 消息（toolCalls 字段名
   *   arguments→args 映射，无 result）。
   * - tool（工具结果）→ 不新建消息，回溯配对到上一条 assistant 的 toolCalls
   *   （id===toolCallId）填 result。
   * - notice（steer 文本）→ 跳过（保持消息流干净，对标 serve 前端行为）。
   * - system（系统提示词）→ 跳过（serve 新建会话时 JSONL 首条写入 role=system 的
   *   系统提示词，是给模型的指令，不应展示给用户；不过滤会被降级渲染成 assistant 气泡）。
   */
  function historyToChatMessages(hist: ServeHistoryMessage[]): ChatMessage[] {
    const out: ChatMessage[] = []
    for (const m of hist) {
      if (m.role === 'notice' || m.role === 'system') continue
      if (m.role === 'tool') {
        // 工具结果：回溯配对到上一条 assistant 的 toolCalls（id===toolCallId）。
        const last = out.at(-1)
        const tc = last?.role === 'assistant' ? last.toolCalls?.find(t => t.id === m.toolCallId) : undefined
        if (tc) {
          tc.result = m.content
          tc.error = false
        }
        continue
      }
      out.push({
        id: nextId++,
        role: m.role === 'user' ? 'user' : 'assistant',
        text: m.role === 'user' ? userDisplayText(m.content || '') : m.content || '',
        reasoning: m.reasoning,
        toolCalls: m.toolCalls?.map(t => ({ id: t.id, name: t.name, args: t.arguments })),
      })
    }
    return out
  }

  /** 读历史会话列表（GET /sessions）。serve 单会话：current 标记活动会话，
   *  启动恢复据此定位"serve 在后台跑的会话"。失败不抛（容错：serve 暂不可达
   *  不阻塞 SSE），仅 warn。 */
  async function loadSessions() {
    try {
      const r = await api('/api/agent/sessions')
      if (!r.ok) return
      sessions.value = await r.json()
      currentSession.value = sessions.value.find(s => s.current) ?? null
    } catch (e) {
      console.warn('agent: loadSessions failed', e)
    }
  }

  /** 读当前会话历史消息（GET /history）回填 messages。ETag 304 可选优化（本期不做）。
   *  nextId 取 max+1 防与后续实时 SSE 帧 id 冲突。失败不抛。 */
  async function loadHistory() {
    try {
      const r = await api('/api/agent/history')
      if (!r.ok) return
      const hist = (await r.json()) as ServeHistoryMessage[]
      messages.value = historyToChatMessages(hist)
      nextId = messages.value.reduce((mx, m) => Math.max(mx, m.id), 0) + 1
    } catch (e) {
      console.warn('agent: loadHistory failed', e)
    }
  }

  /** 读控件态：模型列表（GET /models）+ 状态（GET /status）。失败不抛（控件降级）。 */
  async function loadModels() {
    try {
      const r = await api('/api/agent/models')
      if (!r.ok) return
      // /models 包一层 object：{current, default, label, models:[...]}（serve.go:1258）。
      // 取 .models 数组填充控件下拉数据源。
      const data = await r.json()
      models.value = Array.isArray(data) ? data : (data?.models ?? [])
    } catch (e) {
      console.warn('agent: loadModels failed', e)
    }
  }
  async function loadStatus() {
    try {
      const r = await api('/api/agent/status')
      if (r.ok) status.value = await r.json()
    } catch (e) {
      console.warn('agent: loadStatus failed', e)
    }
  }

  /** 会话切换后并行重拉四件套（sessions/status/models 并行 + history 串行依赖 nextId）。 */
  async function reloadSessionState() {
    await Promise.all([loadSessions(), loadStatus(), loadModels()])
    await loadHistory()
  }

  /** 新建会话（POST /new）。serve 单会话：旧会话 Snapshot 存盘，active 换成新空会话。
   *  防重入（sessionBusy/running）。成功后清 messages/approval/ask + 重拉四件套。 */
  async function newSession() {
    if (sessionBusy.value || running.value) return
    sessionBusy.value = true
    try {
      const r = await api('/api/agent/new', { method: 'POST', body: '{}' })
      if (!r.ok) throw new Error(`new session failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      messages.value = []
      approval.value = null
      ask.value = null
      nextId = 1
      await reloadSessionState()
    } finally {
      sessionBusy.value = false
    }
  }

  /** 切换到历史会话（POST /resume {path}）。409=租约冲突（会话被其他进程持有）。
   *  防重入。成功后清 approval/ask + 重拉四件套（messages 由 loadHistory 替换）。 */
  async function switchSession(path: string) {
    if (sessionBusy.value) return
    sessionBusy.value = true
    try {
      const r = await api('/api/agent/resume', { method: 'POST', body: JSON.stringify({ path }) })
      if (r.status === 409) throw new Error('该会话正被其他窗口使用（租约冲突），请稍后再试')
      if (!r.ok) throw new Error(`switch session failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      approval.value = null
      ask.value = null
      await reloadSessionState()
    } finally {
      sessionBusy.value = false
    }
  }

  /** 删除历史会话（POST /delete-session {name}）。409=不能删当前活动会话。
   *  成功后仅 loadSessions（当前活动会话未变，无需 loadHistory）。 */
  async function deleteSession(name: string) {
    if (sessionBusy.value) return
    const r = await api('/api/agent/delete-session', { method: 'POST', body: JSON.stringify({ name }) })
    if (r.status === 409) throw new Error('不能删除当前活动会话，请先切换到其他会话')
    if (!r.ok) throw new Error(`delete session failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    await loadSessions()
  }

  /**
   * 设置模型（submit 发 `/model <ref>`，gateway 跳 HINT 透传给 serve 拦截 → 204）。
   *
   * **turn 运行中拒绝**：serve switchModel 返 500（serve.go:194），故 running 时
   * 不发（action 守卫 + UI disabled 双层防御）。成功后重拉 models/status 确认 active 切换。
   */
  async function setModel(ref: string) {
    if (controlBusy.value || running.value) return
    controlBusy.value = true
    try {
      const r = await api('/api/agent/submit', { method: 'POST', body: JSON.stringify({ input: `/model ${ref}` }) })
      if (!r.ok) throw new Error(`set model failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      await Promise.all([loadModels(), loadStatus()])
    } finally {
      controlBusy.value = false
    }
  }

  /**
   * 设置思考深度（submit 发 `/effort <level>`，serve 拦截 → 204）。
   *
   * level 是 **provider 相关枚举**，合法档位由 effortLevelsFor(provider, model)
   * 映射（来源官方 config/effort.go）；下拉优先、自由输入兜底（未知 provider）。
   * **turn 运行中拒绝**（serve switchEffort serve.go:402）。非法值 serve 返 500 +
   * 合法值文案，前端抛错给调用方 toast 展示。
   * 成功后**不 loadStatus**（/status 无 effort 字段，无法回读）。
   */
  async function setEffort(level: string) {
    const lvl = level.trim()
    if (!lvl || controlBusy.value || running.value) return
    controlBusy.value = true
    try {
      const r = await api('/api/agent/submit', { method: 'POST', body: JSON.stringify({ input: `/effort ${lvl}` }) })
      if (!r.ok) throw new Error(`set effort failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      // effort 无状态回读（serve 不暴露）；成功仅重拉 status（更新其他控件态）。
      await loadStatus()
    } finally {
      controlBusy.value = false
    }
  }

  /** 切换 Plan Mode（POST /plan {on}）。独立端点，serve 不拒 turn 运行中切换。
   *  成功后 loadStatus 更新 plan 字段。 */
  async function setPlanMode(on: boolean) {
    if (controlBusy.value) return
    controlBusy.value = true
    try {
      const r = await api('/api/agent/plan', { method: 'POST', body: JSON.stringify({ on }) })
      if (!r.ok) throw new Error(`set plan mode failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      await loadStatus()
    } finally {
      controlBusy.value = false
    }
  }

  /**
   * 设置目标（POST /goal {goal}）——「目标·持续推进」执行方式。
   *
   * serve 契约（serve.go:1096）：非空 goal 设置时**自动关闭 plan**（镜像 desktop），
   * 之后 AI 持续工作直到 complete/blocked/stopped；goalStatus 只在 /status 暴露
   * （SSE 无 goal 事件），由 UI 轮询 loadStatus 刷新。
   */
  async function setGoal(goal: string) {
    const text = goal.trim()
    if (!text || controlBusy.value) return
    controlBusy.value = true
    try {
      const r = await api('/api/agent/goal', { method: 'POST', body: JSON.stringify({ goal: text }) })
      if (!r.ok) throw new Error(`set goal failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      await loadStatus()
    } finally {
      controlBusy.value = false
    }
  }

  /** 清除目标（POST /goal {goal:""}）——AI 停止持续工作。 */
  async function clearGoal() {
    if (controlBusy.value) return
    controlBusy.value = true
    try {
      const r = await api('/api/agent/goal', { method: 'POST', body: JSON.stringify({ goal: '' }) })
      if (!r.ok) throw new Error(`clear goal failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      await loadStatus()
    } finally {
      controlBusy.value = false
    }
  }

  /** 设置权限模式（POST /tool-approval-mode {mode}，ask/auto/yolo）。独立端点，
   *  serve 不拒 turn 运行中切换。成功后 loadStatus 更新 toolApprovalMode 字段。 */
  async function setApprovalMode(mode: ToolApprovalMode) {
    if (controlBusy.value) return
    controlBusy.value = true
    try {
      const r = await api('/api/agent/tool-approval-mode', { method: 'POST', body: JSON.stringify({ mode }) })
      if (!r.ok) throw new Error(`set approval mode failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      await loadStatus()
    } finally {
      controlBusy.value = false
    }
  }

  /** 中途重连（关面板重开）后的 turn 完成自愈：重拉 /history 补全关面板期间
   *  错过的流式内容（SSE 纯字节管道无 replay）。仅当尾消息是 assistant 时替换——
   *  尾消息是 user 说明有新提交在途，替换会吞掉本地乐观气泡（竞态防护）。 */
  async function maybeHealHistory() {
    if (!resumedMidTurn) return
    resumedMidTurn = false
    if (messages.value.at(-1)?.role === 'assistant') await loadHistory()
  }

  /** 启动 SSE 订阅 + 拉会话/历史/控件态（启动恢复）。
   *  登录后/打开抽屉时调用。token 走 query（EventSource 不能设 header）。
   *  SSE 建立后并行拉 sessions/status/models + 串行 history（回填 serve 在跑的会话，
   *  解决"重启后前端看不到"痛点）。拉取失败不阻塞 SSE（容错：serve 暂不可达时
   *  SSE 重连自愈）。
   *
   *  重连恢复（关面板重开）：/status.running 是权威运行态——后台 turn 可能仍在
   *  执行，据此恢复 running 并补一条空 assistant 占位消息，后续 text/reasoning/
   *  tool 增量挂到它（历史里的 assistant 是已完成步骤，不能当作流式目标）。 */
  async function start() {
    if (sse) stop() // 重入守卫（如重连场景）
    const tok = useAuthStore().token
    resumedMidTurn = false
    sse = new EventSource(`/api/agent/events?token=${encodeURIComponent(tok)}`)
    // onmessage 接全部帧（无 event: 头）→ safe 包 parse（畸形帧不让 onmessage 抛错，
    // 抛错后该 listener 被 EventSource 摘除）。parse 必须在 safe thunk 内执行
    // （若作 safe 实参先求值，parse 抛错会绕过 try）——对齐 monitor store 既有约定。
    sse.onmessage = ev => safe(() => onEvent(JSON.parse((ev as MessageEvent).data) as ServeEvent))
    // onerror：浏览器自动重连，这里静默。
    sse.onerror = () => {}
    // 启动恢复：拉 serve 在跑的会话 + 历史 + 控件态（各自容错，互不阻塞）。
    try {
      await Promise.all([loadSessions(), loadStatus(), loadModels()])
      await loadHistory()
      // 重连恢复运行态：serve 是独立进程，关面板只断 SSE、turn 继续在后台执行。
      running.value = !!status.value?.running
      if (running.value) {
        resumedMidTurn = true
        const last = messages.value.at(-1)
        if (!last || last.role !== 'assistant' || last.text || last.toolCalls?.length) {
          messages.value.push({ id: nextId++, role: 'assistant', text: '', toolCalls: [] })
        }
      }
    } catch (e) {
      console.warn('agent: startup recovery failed', e)
    }
  }

  /** SSE 帧 parse/handle 容错：畸形帧 warn 并跳过，不冒泡到 onmessage。 */
  function safe(fn: () => void) {
    try { fn() } catch (e) { console.warn('agent: drop malformed SSE frame', e) }
  }

  /** 停止 SSE（登出 / 路由切走 / 关抽屉时调用）。
   *
   * 双语义（serve 是独立进程，turn 生命周期不应随抽屉开合改变）：
   * - `stop()`（默认，关抽屉）：**只断 SSE，不取消后台 turn**——Reasonix 继续执行，
   *   重开抽屉时 start() 经 /status.running 恢复运行态并补占位消息接续流式。
   * - `stop(true)`（登出）：同时取消进行中的 turn（会话留给下个用户前清理干净）。
   *
   * 历史：旧版 stop 无条件 cancel——"关面板看数据再回来"会静默杀掉正在执行的
   * 任务（用户无感知且回不来）。旧注释担心的"running 永卡 true"已由 start() 的
   * /status.running 权威恢复解决，不再需要靠 cancel 兜底复位。 */
  function stop(cancelTurn = false) {
    sse?.close()
    sse = null
    resumedMidTurn = false
    if (cancelTurn && running.value) {
      running.value = false
      void cancel().catch(() => { /* cancel 失败静默：本地已复位，服务端 turn 自行结束 */ })
    }
  }

  /** 读 LLM 供应商列表 + 默认模型（gateway 本地实现，不落 serve）。非 2xx 抛错带响应体。 */
  async function loadProviders() {
    const r = await api('/api/agent/providers')
    if (!r.ok) throw new Error(`load providers failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    const data = await r.json()
    providers.value = data.providers ?? []
    defaultModel.value = data.default_model ?? ''
  }

  /** 新增/更新供应商（POST /api/agent/providers）。api_key 留空 = 不动现有密钥。
   *  返回 {created, restarted}——restarted=false 时需提示手动重启 reasonix。 */
  async function saveProvider(payload: {
    name: string
    kind?: string
    base_url: string
    models: string[]
    api_key_env?: string
    api_key?: string
  }) {
    providerBusy.value = true
    try {
      const r = await api('/api/agent/providers', {
        method: 'POST',
        body: JSON.stringify(payload),
      })
      if (!r.ok) throw new Error(`save provider failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      const data = await r.json()
      await loadProviders()
      return data as { created: boolean; restarted: boolean; restart_error?: string }
    } finally {
      providerBusy.value = false
    }
  }

  /** 删除供应商（条目 + .env 密钥；URL 名字需 encode 防 `/` 等字符）。 */
  async function deleteProvider(name: string) {
    providerBusy.value = true
    try {
      const r = await api(`/api/agent/providers/${encodeURIComponent(name)}`, { method: 'DELETE' })
      if (!r.ok) throw new Error(`delete provider failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      await loadProviders()
    } finally {
      providerBusy.value = false
    }
  }

  /** 设新会话默认模型（"<provider>/<model>"）。变更后 reasonix 需重启生效（gateway 已触发）。 */
  async function setDefaultModel(model: string) {
    providerBusy.value = true
    try {
      const r = await api('/api/agent/providers/default', {
        method: 'POST',
        body: JSON.stringify({ model }),
      })
      if (!r.ok) throw new Error(`set default model failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
      defaultModel.value = model
    } finally {
      providerBusy.value = false
    }
  }

  /** 从上游拉模型 id 列表（勾选代替手填，对标 cc-switch-go 动态发现）。
   *  编辑场景 name 传供应商名即可自动用已存 .env 密钥（免重输）。 */
  async function fetchUpstreamModels(payload: {
    base_url: string
    kind?: string
    api_key?: string
    name?: string
  }) {
    const r = await api('/api/agent/providers/fetch-models', {
      method: 'POST',
      body: JSON.stringify(payload),
    })
    if (!r.ok) throw new Error(`fetch models failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    const data = await r.json()
    return (data.models ?? []) as string[]
  }

  /** 上传附件到 reasonix 工作区（gateway 落 `.reasonix/attachments/`），返回 @ 引用路径。
   *  发送时把路径以 `@` 前缀写进消息文本，serve 提交时解析内容/图片输入。 */
  async function uploadAttachment(file: File) {
    const fd = new FormData()
    fd.append('file', file, file.name)
    const r = await api('/api/agent/attachments', { method: 'POST', body: fd })
    if (!r.ok) throw new Error(`upload failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    const data = await r.json()
    const f = data.files?.[0]
    if (!f?.path) throw new Error('upload response missing path')
    return f.path as string
  }

  /** 列工作区一层目录（gateway 沙箱 + 噪音过滤），@ 引用选择器数据源。 */
  async function listWorkspace(path: string) {
    const r = await api(`/api/agent/workspace?path=${encodeURIComponent(path)}`)
    if (!r.ok) throw new Error(`list workspace failed: ${r.status} ${await r.text().catch(() => '')}`.trim())
    const data = await r.json()
    return (data.entries ?? []) as WorkspaceEntry[]
  }

  /** 当前活动模型 ref（models 里 active==true 的条目，模型下拉绑它）。 */
  const activeModelRef = computed(() => models.value.find(m => m.active)?.ref ?? '')

  return {
    messages, running, approval, ask,
    sessions, currentSession, models, status, sessionBusy, controlBusy, activeModelRef,
    providers, defaultModel, providerBusy,
    loadProviders, saveProvider, deleteProvider, setDefaultModel, fetchUpstreamModels,
    uploadAttachment, listWorkspace,
    onEvent, submit, approve, cancel, answer,
    loadSessions, loadHistory, loadModels, loadStatus,
    newSession, switchSession, deleteSession,
    setModel, setEffort, setPlanMode, setGoal, clearGoal, setApprovalMode,
    start, stop,
  }
})
