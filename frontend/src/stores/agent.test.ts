// frontend/src/stores/agent.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// agent.ts 静态 import { api } → api/client.ts import { router } →
// router.ts createWebHistory() 在模块加载期读 window（node 测试环境无 window）。
// mock 掉 api 短路整条 import 链（vi.mock 提升至所有 import 之前，真实 client.ts 不加载）。
// 默认返回 200 + 空 body；个别用例用 mockResolvedValueOnce 覆盖（如 approve 失败路径）。
vi.mock('../api/client', () => ({ api: vi.fn(async () => new Response('{}')) }))

import { api } from '../api/client'
import { useAgentStore, effortLevelsFor, escapeRefPath, PLAN_MODE_MARKERS } from './agent'
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

  it('notice 事件归集到当前 assistant 消息（路由提示随思考折叠展示，对标 aifa）', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'notice', text: 'routed to cloud model' })
    s.onEvent({ kind: 'notice', text: 'tool fallback' })
    expect(s.messages.at(-1)?.notices).toEqual(['routed to cloud model', 'tool fallback'])
  })

  it('historyToChatMessages 剥离 Reasonix 注入的 transient 偏好块（用户气泡不漏指令）', async () => {
    // serve /history 原样返回持久化内容：注入块 + 真实 prompt（现场实测形态）。
    // 官方 UI 客户端侧剥，前端同款——否则切换历史会话时指令文本漏进气泡。
    const s = useAgentStore()
    vi.mocked(api).mockClear()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(
        JSON.stringify([
          { role: 'user', content: '<reasoning-language>\n必须使用简体中文书写全部可见思考/推理文本。\n</reasoning-language>\n\n列出所有设备' },
          { role: 'assistant', content: '3 台设备' },
          { role: 'user', content: '你好' },
        ]),
      ),
    )
    await s.loadHistory()
    // 注入块剥离且去除前导空行；assistant 与普通用户消息不受影响。
    expect(s.messages[0].text).toBe('列出所有设备')
    expect(s.messages[1].text).toBe('3 台设备')
    expect(s.messages[2].text).toBe('你好')
  })

  it('historyToChatMessages 剥离叠加瞬态块（capability-route 带属性）+ 尾缀 execution-policy', async () => {
    // 现场实测形态（2026-08-16）：user turn 前缀叠加 capability-route + reasoning-language，
    // 上游 preview.go TransientUserBlockTags 全清单客户端剥离；execution-policy 是
    // 唯一追加在 user 文本之后的尾缀块（带属性）。
    const s = useAgentStore()
    vi.mocked(api).mockClear()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(
        JSON.stringify([
          {
            role: 'user',
            content:
              '<capability-route version="1">\nRelevant capabilities for this turn:\n- mcp-tool:rsiot/rsiot_list_devices prefer: ...\n</capability-route>\n\n<reasoning-language>\n必须使用简体中文书写。\n</reasoning-language>\n\n@docs/faq.md 总结这个文件\n\n<execution-policy mode="ask">\nsome trailing policy\n</execution-policy>',
          },
        ]),
      ),
    )
    await s.loadHistory()
    expect(s.messages[0].text).toBe('@docs/faq.md 总结这个文件')
  })

  it('historyToChatMessages 剥 Referenced context 注入块 + 页面上下文尾缀（气泡=实时所见）', async () => {
    // 现场实测形态（2026-08-16）：@ 引用解析后 serve 持久化
    // "Referenced context:\n\n<file ...>…</file>\n\n+ 用户原文"；rs-iot 前端
    // 另追加 [页面上下文] 尾缀。桌面端显示管线 StripReferencedContextPrefix
    // (StripComposePrefixes(content)) 剥掉宿主注入，/history 不剥——前端同款。
    const s = useAgentStore()
    vi.mocked(api).mockClear()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(
        JSON.stringify([
          {
            role: 'user',
            content:
              'Referenced context:\n\n<file path=".reasonix/attachments/upload-20260816-215755.466221-000001.txt">\nfield pump daily check 2026-08-16: vibration 2.1mm/s OK, temp 41C OK\n</file>\n\n@.reasonix/attachments/upload-20260816-215755.466221-000001.txt 请引用附件原文片段回答：附件第一句写了什么？\n\n[页面上下文]\n当前页面: 全局总览\n页面路径: /overview\n页面摘要: 总览当前实例健康\n当前可见数据:\n- 实例总数 3',
          },
        ]),
      ),
    )
    await s.loadHistory()
    expect(s.messages[0].text).toBe(
      '@.reasonix/attachments/upload-20260816-215755.466221-000001.txt 请引用附件原文片段回答：附件第一句写了什么？',
    )
  })

  it('historyToChatMessages 剥 plan 模式标记（Compose 前缀注入）', async () => {
    const s = useAgentStore()
    const marker = PLAN_MODE_MARKERS[0]
    vi.mocked(api).mockClear()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(JSON.stringify([{ role: 'user', content: `${marker}\n\n帮我规划采集实例扩容方案` }])),
    )
    await s.loadHistory()
    expect(s.messages[0].text).toBe('帮我规划采集实例扩容方案')
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

  it('approve body 契约匹配 serve {id, allow}（非 approval_id/decision，现场 bug）', async () => {
    // 锁的不变量：approve 字段名必须对齐 serve POST /approve（id/allow）。
    // 历史发 approval_id+decision 全错，serve 返 400 missing id，用户无法批准
    // 任何写操作（包括建实例/写脚本）。单测此前 mock 掩盖了契约错误。
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'approval_request', approval: { id: 'ap1', tool: 'write_file' } })
    await s.approve(true)
    const [path, options] = vi.mocked(api).mock.calls.at(-1) ?? []
    expect(path).toBe('/api/agent/approve')
    const body = options?.body ? JSON.parse(String(options.body)) : {}
    expect(body).toEqual({ id: 'ap1', allow: true })
    // 确保旧错误字段不再出现。
    expect(body.approval_id).toBeUndefined()
    expect(body.decision).toBeUndefined()
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

  it('stop 双语义：默认只断 SSE 不取消（关面板），stop(true) 取消后台 turn', async () => {
    // 锁的不变量：serve 是独立进程，turn 生命周期不随抽屉开合改变——
    // 关抽屉（stop()）不得 cancel（否则"看一眼数据再回来"任务被静默杀掉）；
    // 登出（stop(true)）需 cancel + 本地复位 running（SSE 断后 turn_done 不可达）。
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    expect(s.running).toBe(true)
    vi.mocked(api).mockClear()
    s.stop()  // 关面板：running 保持 true（后台继续）
    expect(s.running).toBe(true)
    await new Promise(r => setTimeout(r, 0))
    expect(vi.mocked(api)).not.toHaveBeenCalled()  // 未发 cancel
    s.stop(true)  // 登出：复位 + fire-and-forget cancel
    expect(s.running).toBe(false)
    await new Promise(r => setTimeout(r, 0))
    expect(vi.mocked(api)).toHaveBeenCalledWith('/api/agent/cancel', expect.objectContaining({ method: 'POST' }))
  })

  it('start 重连恢复：/status.running=true 恢复运行态 + 补空 assistant 占位接续流式', async () => {
    // 关面板期间后台 turn 继续执行；重开抽屉时 /status.running 是权威运行态。
    // 历史里的 assistant 是已完成步骤，不能作为流式追加目标——须补空占位。
    class FakeEventSource {
      onmessage: unknown = null
      onerror: unknown = null
      constructor(_url: string) {}
      close() {}
    }
    // localStorage 桩（auth store 初始化读 token，同 monitor.test.ts 模式）
    const ls: Record<string, string> = { rsiot_token: 't' }
    ;(globalThis as { localStorage?: Storage }).localStorage = {
      getItem: (k: string) => ls[k] ?? null,
      setItem: (k: string, v: string) => { ls[k] = v },
      removeItem: (k: string) => { delete ls[k] },
      clear: () => { for (const k of Object.keys(ls)) delete ls[k] },
      key: (i: number) => Object.keys(ls)[i] ?? null,
      get length() { return Object.keys(ls).length },
    } as Storage
    ;(globalThis as { EventSource?: unknown }).EventSource = FakeEventSource
    const s = useAgentStore()
    vi.mocked(api).mockClear()
    vi.mocked(api).mockImplementation(async (path: string) => {
      if (String(path).includes('/status')) return new Response(JSON.stringify({ running: true }))
      if (String(path).includes('/history')) return new Response(JSON.stringify([]))
      return new Response('{}')
    })
    await s.start()
    expect(s.running).toBe(true)
    const last = s.messages.at(-1)
    expect(last?.role).toBe('assistant')
    expect(last?.text).toBe('')
    // 后续流式增量挂到占位消息
    s.onEvent({ kind: 'text', text: '后台继续输出' })
    expect(s.messages.at(-1)?.text).toBe('后台继续输出')
    // turn_done 复位（且触发 /history 自愈重拉）
    s.onEvent({ kind: 'turn_done' })
    expect(s.running).toBe(false)
  })

  it('ask_request 设置 ask 载荷，answer 调 /api/agent/answer 后清除（解卡死修复）', async () => {
    // 锁的不变量：AI 主动提问（ask_request）必须落到 store 供 UI 决策；
    // answer() 带 {id, answers} 提交，2xx 后清 ask（serve 继续 turn）。
    const s = useAgentStore()
    s.onEvent({
      kind: 'ask_request',
      ask: {
        id: 'ask-1',
        questions: [
          { id: 'q1', header: 'CriticalWrite 处理', prompt: '如何继续?', options: [{ label: 'A', description: '甲' }, { label: 'B' }] },
        ],
      },
    })
    expect(s.ask?.id).toBe('ask-1')
    expect(s.ask?.questions[0].options.length).toBe(2)
    await s.answer([{ question_id: 'q1', selected: ['A'] }])
    const [path, options] = vi.mocked(api).mock.calls.at(-1) ?? []
    expect(path).toBe('/api/agent/answer')
    const body = options?.body ? JSON.parse(String(options.body)) : {}
    expect(body).toEqual({ id: 'ask-1', answers: [{ question_id: 'q1', selected: ['A'] }] })
    expect(s.ask).toBeNull()
  })

  it('turn_done 清残留 ask（防御：未答即结束的 turn）', () => {
    const s = useAgentStore()
    s.onEvent({ kind: 'ask_request', ask: { id: 'ask-x', questions: [] } })
    expect(s.ask).not.toBeNull()
    s.onEvent({ kind: 'turn_done' })
    expect(s.ask).toBeNull()
  })

  it('stop 在非 running 时不发 cancel（正常关闭无副作用）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockClear()  // 清跨测试累积的调用历史
    s.stop()
    await new Promise(r => setTimeout(r, 0))
    expect(vi.mocked(api)).not.toHaveBeenCalled()
  })

  it('historyToChatMessages: tool 结果配对到 assistant 的 toolCalls（字段名 arguments→args）', () => {
    // 锁的不变量：history 的 toolCalls 字段名是 arguments（非 args），且工具结果
    // 由独立 role=tool 消息携带 toolCallId 配对回填（对标 serve.go:838-884）。
    const s = useAgentStore()
    // 模拟 loadHistory 内部映射：直接测 store 暴露的 messages 构造路径。
    // 经 onEvent 构造一条带 toolCalls 的 assistant 消息（实时路径），验证结构一致。
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'tool_dispatch', tool: { id: 'h1', name: 'rsiot_read_tags', args: '{"dev":"devA"}' } })
    s.onEvent({ kind: 'tool_result', tool: { id: 'h1', name: 'rsiot_read_tags', output: '{"v":1}' } })
    const tc = s.messages.at(-1)?.toolCalls?.[0]
    expect(tc?.args).toBe('{"dev":"devA"}')
    expect(tc?.result).toBe('{"v":1}')
  })

  it('historyToChatMessages: system role（系统提示词）被过滤，不渲染为可见气泡', async () => {
    // 锁的不变量：serve 新建会话时 JSONL 首条写入 role=system 的系统提示词
    // （"You are Reasonix, a coding agent..."），是给模型的指令而非对话内容。
    // historyToChatMessages 必须跳过 system role，否则被三元降级成 assistant 气泡
    // 原样展示给用户（实测 bug：新建会话即冒出系统提示词）。
    const s = useAgentStore()
    vi.mocked(api).mockImplementation(async (path: string) => {
      if (path === '/api/agent/history') {
        return new Response(JSON.stringify([
          { role: 'system', content: 'You are Reasonix, a coding agent. Use the available tools when they help.' },
          { role: 'user', content: '你好' },
          { role: 'assistant', content: '你好！有什么可以帮你？' },
        ]))
      }
      if (path === '/api/agent/sessions') return new Response('[]')
      return new Response('{}')
    })
    try {
      await s.newSession()
      // system 提示词不应出现在任何消息文本中
      expect(s.messages.find(m => m.text.includes('Reasonix, a coding agent'))).toBeUndefined()
      expect(s.messages.length).toBe(2)  // user + assistant，system 被过滤
      expect(s.messages[0].role).toBe('user')
      expect(s.messages[0].text).toBe('你好')
      expect(s.messages[1].role).toBe('assistant')
    } finally {
      // 恢复默认 mock（文件顶部工厂的 async () => new Response('{}')），
      // 避免 mockImplementation 污染后续依赖空 body 的测试。
      vi.mocked(api).mockImplementation(async () => new Response('{}'))
    }
  })

  it('setModel 发 /model 命令（serve submit 拦截，gateway 跳 HINT）', async () => {
    // 锁的不变量：模型切换走 submit 发 `/model <ref>`（serve.go:719 前缀拦截 → 204）。
    // gateway 已加斜杠命令跳过 HINT 注入（proxy.rs input_should_get_hint）。
    // 成功后还会 loadModels/loadStatus（额外 api 调用），故按路径筛 submit 调用。
    const s = useAgentStore()
    await s.setModel('anthropic/claude')
    const submitCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/submit')
    expect(submitCall).toBeTruthy()
    const body = submitCall?.[1]?.body ? JSON.parse(String(submitCall[1].body)) : {}
    expect(body.input).toBe('/model anthropic/claude')
  })

  it('setEffort 发 /effort 命令（datalist 下拉候选 + 自由输入兜底）', async () => {
    // 锁的不变量：effort level 是 provider 相关枚举，下拉候选项由 effortLevelsFor
    // 映射（来源官方 config/effort.go），仍可手输；submit 发 `/effort <level>`。trim 处理。
    const s = useAgentStore()
    vi.mocked(api).mockClear()  // 清前序 setModel 测试遗留的 submit 调用
    await s.setEffort('  high  ')
    const submitCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/submit')
    expect(submitCall).toBeTruthy()
    const body = submitCall?.[1]?.body ? JSON.parse(String(submitCall[1].body)) : {}
    expect(body.input).toBe('/effort high')
  })

  it('setEffort 空值不发请求（防御）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockClear()
    await s.setEffort('   ')
    expect(vi.mocked(api)).not.toHaveBeenCalled()
  })

  it('setPlanMode 发 POST /plan {on}（独立端点，turn 中可切）', async () => {
    const s = useAgentStore()
    await s.setPlanMode(true)
    const planCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/plan')
    expect(planCall).toBeTruthy()
    const body = planCall?.[1]?.body ? JSON.parse(String(planCall[1].body)) : {}
    expect(body).toEqual({ on: true })
  })

  it('setGoal 发 POST /goal {goal}（目标·持续推进；非空去空格）', async () => {
    const s = useAgentStore()
    await s.setGoal('  持续采集所有设备数据  ')
    const goalCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/goal')
    expect(goalCall).toBeTruthy()
    const body = goalCall?.[1]?.body ? JSON.parse(String(goalCall[1].body)) : {}
    expect(body).toEqual({ goal: '持续采集所有设备数据' })
  })

  it('clearGoal 发 POST /goal {goal:""}（空串=清除，serve.go:1105）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockClear()  // 清前序 setGoal 测试的同路径调用
    await s.clearGoal()
    const goalCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/goal')
    expect(goalCall).toBeTruthy()
    const body = goalCall?.[1]?.body ? JSON.parse(String(goalCall[1].body)) : {}
    expect(body).toEqual({ goal: '' })
  })

  it('setGoal 空值不发请求（防御）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockClear()
    await s.setGoal('   ')
    expect(vi.mocked(api)).not.toHaveBeenCalled()
  })

  it('setApprovalMode 发 POST /tool-approval-mode {mode}（ask/auto/yolo 三档）', async () => {
    const s = useAgentStore()
    await s.setApprovalMode('yolo')
    const modeCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/tool-approval-mode')
    expect(modeCall).toBeTruthy()
    const body = modeCall?.[1]?.body ? JSON.parse(String(modeCall[1].body)) : {}
    expect(body).toEqual({ mode: 'yolo' })
  })

  it('newSession 发 POST /new 并清空 messages/approval/ask', async () => {
    // newSession 在 running 时防重入（serve turn 中切会话语义混乱）——故先 stop 复位。
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })
    s.onEvent({ kind: 'approval_request', approval: { id: 'x', tool: 't' } })
    expect(s.messages.length).toBeGreaterThan(0)
    expect(s.approval).not.toBeNull()
    s.stop(true)  // 复位 running（关 SSE + 取消后台 turn——会话切换前清理）
    expect(s.running).toBe(false)
    await s.newSession()
    expect(s.messages).toEqual([])
    expect(s.approval).toBeNull()
    expect(vi.mocked(api)).toHaveBeenCalledWith('/api/agent/new', expect.objectContaining({ method: 'POST' }))
  })

  it('switchSession 发 POST /resume {path}，409 抛租约冲突文案', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(new Response('', { status: 409 }))
    await expect(s.switchSession('/some/path.jsonl')).rejects.toThrow('租约冲突')
    const resumeCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/resume')
    const body = resumeCall?.[1]?.body ? JSON.parse(String(resumeCall[1].body)) : {}
    expect(body).toEqual({ path: '/some/path.jsonl' })
  })

  it('deleteSession 发 POST /delete-session {name}，409 抛不可删当前会话文案', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(new Response('', { status: 409 }))
    await expect(s.deleteSession('20260813-143012-session')).rejects.toThrow('不能删除当前活动会话')
    const delCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/agent/delete-session')
    const body = delCall?.[1]?.body ? JSON.parse(String(delCall[1].body)) : {}
    expect(body).toEqual({ name: '20260813-143012-session' })
  })

  it('控件操作 running 时 setModel/setEffort 不发请求（serve turn 中拒绝）', async () => {
    // 锁的不变量：serve switchModel/switchEffort 在 turn 运行中返 500（serve.go:194/402），
    // 故 running 时 action 守卫不发（UI disabled 双层防御）。
    const s = useAgentStore()
    s.onEvent({ kind: 'turn_started' })  // running=true
    expect(s.running).toBe(true)
    vi.mocked(api).mockClear()
    await s.setModel('x/y')
    await s.setEffort('high')
    expect(vi.mocked(api)).not.toHaveBeenCalled()
  })
})

describe('LLM 供应商管理（gateway /api/agent/providers*）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.mocked(api).mockClear() // 模块级 mock 的 calls 跨用例累积，逐用例清零再按下标断言
  })

  it('loadProviders 填充 providers/defaultModel（密钥只报有无）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          default_model: 'deepseek/deepseek-v4-flash',
          providers: [{ name: 'deepseek', kind: 'anthropic', base_url: 'https://x', models: ['deepseek-v4-flash'], api_key_env: 'DEEPSEEK_API_KEY', has_key: true }],
        }),
      ),
    )
    await s.loadProviders()
    expect(s.providers).toHaveLength(1)
    expect(s.providers[0].has_key).toBe(true)
    expect(s.defaultModel).toBe('deepseek/deepseek-v4-flash')
    expect(JSON.stringify(s.providers[0])).not.toContain('sk-') // 密钥内容不可出现在前端状态
  })

  it('saveProvider POST 供应商载荷（api_key 缺省不传字段）', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(new Response(JSON.stringify({ saved: true, created: true, restarted: true })))
    vi.mocked(api).mockResolvedValueOnce(new Response(JSON.stringify({ default_model: '', providers: [] })))
    await s.saveProvider({ name: 'mm', base_url: 'https://x', models: ['m1'] })
    const [url, init] = vi.mocked(api).mock.calls[0]
    expect(url).toBe('/api/agent/providers')
    expect(init?.method).toBe('POST')
    expect(JSON.parse(init!.body as string)).toEqual({ name: 'mm', base_url: 'https://x', models: ['m1'] })
  })

  it('setDefaultModel / deleteProvider 打正确端点；非 2xx 抛错带响应体', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(new Response('{}'))
    await s.setDefaultModel('mm/m1')
    expect(vi.mocked(api).mock.calls[0][0]).toBe('/api/agent/providers/default')

    vi.mocked(api).mockResolvedValueOnce(new Response('still in use', { status: 409 }))
    await expect(s.deleteProvider('deep seek')).rejects.toThrow('409 still in use')
    expect(vi.mocked(api).mock.calls[1][1]?.method).toBe('DELETE')
    // 名字含空格需 URL 编码（否则路由错位）。
    expect(vi.mocked(api).mock.calls[1][0]).toBe('/api/agent/providers/deep%20seek')
  })
})

describe('附件上传 + 工作区引用（gateway /api/agent/attachments、/workspace）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.mocked(api).mockClear()
  })

  it('uploadAttachment 以 FormData POST file 字段，返回落盘 @ 路径', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(
        JSON.stringify({ files: [{ name: '泵房.png', path: '.reasonix/attachments/upload-x.png' }] }),
        { status: 201 },
      ),
    )
    const path = await s.uploadAttachment(new File([new Uint8Array([1])], '泵房.png'))
    expect(path).toBe('.reasonix/attachments/upload-x.png')
    const [url, init] = vi.mocked(api).mock.calls[0]
    expect(url).toBe('/api/agent/attachments')
    expect(init?.method).toBe('POST')
    expect(init?.body).toBeInstanceOf(FormData)
    expect((init!.body as FormData).get('file')).toBeInstanceOf(File)
  })

  it('uploadAttachment 非 2xx 抛错带响应体', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(new Response('超过 25 MB 上限', { status: 413 }))
    await expect(s.uploadAttachment(new File([new Uint8Array([1])], 'a.zip'))).rejects.toThrow('413')
  })

  it('listWorkspace GET 带编码 path，返回条目数组', async () => {
    const s = useAgentStore()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(JSON.stringify({ path: 'docs', entries: [{ name: 'a.md', is_dir: false }] })),
    )
    const es = await s.listWorkspace('docs 旧')
    expect(es).toEqual([{ name: 'a.md', is_dir: false }])
    expect(vi.mocked(api).mock.calls[0][0]).toBe(`/api/agent/workspace?path=${encodeURIComponent('docs 旧')}`)
    expect(vi.mocked(api).mock.calls[0][1]).toBeUndefined() // GET 无 body
  })
})

describe('escapeRefPath（对标 Reasonix refs.go EscapeRefPath——@token 以空白分界）', () => {
  it('空格/制表符转义，其余原样（/ 分隔符不动）', () => {
    expect(escapeRefPath('docs/read me.md')).toBe('docs/read\\ me.md')
    expect(escapeRefPath('a\tb')).toBe('a\\\tb')
    expect(escapeRefPath('plain/path.txt')).toBe('plain/path.txt')
  })
})

describe('effortLevelsFor（映射来源 Reasonix config/effort.go EffortCapabilityForEntry）', () => {
  it('deepseek-v4-flash 独有 low 档', () => {
    expect(effortLevelsFor('deepseek-flash', 'deepseek-v4-flash')).toEqual([
      'auto', 'disabled', 'low', 'high', 'max',
    ])
  })

  it('deepseek 其他模型无 low/medium（deepSeekEffortCapability）', () => {
    expect(effortLevelsFor('deepseek-pro', 'deepseek-v4-pro')).toEqual([
      'auto', 'disabled', 'high', 'max',
    ])
  })

  it('MiniMax-M3 二进制旋钮 adaptive|disabled（isMiniMaxEntry 分支）', () => {
    expect(effortLevelsFor('Minimax', 'MiniMax-M3')).toEqual([
      'auto', 'adaptive', 'disabled',
    ])
  })

  it('Anthropic 深度档位含 xhigh', () => {
    expect(effortLevelsFor('anthropic', 'claude-sonnet')).toEqual([
      'auto', 'low', 'medium', 'high', 'xhigh', 'max',
    ])
  })

  it('Ollama Cloud 含 none', () => {
    expect(effortLevelsFor('ollama', 'llama3-cloud')).toEqual([
      'auto', 'none', 'low', 'medium', 'high', 'max',
    ])
  })

  it('未知 provider 回落通用 OpenAI 档位（openAIEffortCapability）', () => {
    expect(effortLevelsFor('some-vendor', 'mystery-model')).toEqual([
      'auto', 'low', 'medium', 'high',
    ])
  })

  it('大小写不敏感', () => {
    expect(effortLevelsFor('DEEPSEEK-FLASH', 'DeepSeek-V4-Flash')).toContain('low')
    expect(effortLevelsFor('deepseek-flash', 'DEEPSEEK-V4-FLASH')).toContain('low')
  })
})
