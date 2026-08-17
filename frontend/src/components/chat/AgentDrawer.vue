<!-- frontend/src/components/chat/AgentDrawer.vue
  AI 面板容器（右侧抽屉 570px，2026-08-13 自 380px 加宽 150%——AI 回复为 markdown
  排版，窄面板下表格/代码块换行严重）。对标 zacp 浅色风：
  - 头部：标题 + 状态（思考中/就绪） + 关闭按钮
  - 消息流：MessageItem 列表（user 右对齐 / assistant 含思考折叠 + 流式文本 + 工具卡）
  - 输入框：附件/引用工具行（📎 本机文件上传 + 📁 工作区 @ 引用）+ textarea + 发送（Enter 发送）
  - 权限弹窗：approval_request 触发 PermissionModal
  滚动跟随用 useChatScroll（自动贴底 + 上翻暂停 + 回到底部按钮）。
  running 态经 provide 注入 MessageItem（思考面板活跃色 + 流式加载占位）。
  T9 M2：submit 失败给 UI 反馈（catch 后在消息流标一条 error 提示，非静默 rejection）。
-->
<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, provide, ref, watch } from 'vue'
import { useAgentStore, effortLevelsFor, escapeRefPath } from '../../stores/agent'
import { useChatScroll } from './composables/useChatScroll'
import MessageItem, { runningKey } from './MessageItem.vue'
import PermissionModal from './PermissionModal.vue'
import AskModal from './AskModal.vue'
import ProviderModal from './ProviderModal.vue'
import WorkspacePicker from './WorkspacePicker.vue'
import type { ToolApprovalMode } from '../../types'

const agent = useAgentStore()
const input = ref('')
/** effort 输入值（serve 不暴露当前 effort，控件无法回读；datalist 下拉 + 自由输入兜底）。 */
const effortInput = ref('')
/** 当前模型的 effort 档位（datalist 候选项，来源官方 effort.go 映射；未知模型为空 → 自由输入）。 */
const effortOptions = computed(() => {
  const active = agent.models.find((m) => m.active) ?? agent.models.find((m) => m.ref === agent.activeModelRef)
  return active ? effortLevelsFor(active.provider, active.model) : []
})
/** 目标输入态（执行方式=目标 且尚未提交目标时，输入框内容将作为目标提交——
 *  对齐 desktop goalDraftMode：切到 goal 清空旧目标进入 draft，提交后回归补充指令）。 */
const goalDraftMode = ref(false)
/** 目标状态轮询定时器（SSE 无 goal 事件，goalStatus 只在 /status 暴露，goal 运行期间轮询刷新）。 */
let goalPollTimer: ReturnType<typeof setInterval> | null = null
/** AskModal "稍后再说"后的关闭态：不清 agent.ask（serve 侧 turn 仍挂起等回答），
 *  只折叠弹窗并保留重开入口——否则提问被丢弃后用户只能"停止"整个 turn。 */
const askDismissed = ref(false)
/** 新提问到达时自动重置关闭态（弹出新一轮提问）。 */
watch(() => agent.ask?.id, id => { if (id) askDismissed.value = false })

/** 本地错误提示（submit/会话/控件 失败等，非 agent 消息流的一部分——单独显示在输入区上方）。 */
const errorMsg = ref('')
const listRef = ref<HTMLElement | null>(null)
/** 会话下拉展开态（点击外部关闭）。 */
const sessionMenuOpen = ref(false)
/** 会话下拉根元素（点击外部检测用）。 */
const sessionMenuRef = ref<HTMLElement | null>(null)
/** LLM 供应商管理弹窗（初装配 key/模型 + 默认模型设置）。 */
const providerModalOpen = ref(false)
/** 工作区引用选择器弹窗（@ 引用插入消息）。 */
const workspacePickerOpen = ref(false)
/** 已上传待发送附件（chips 展示原名；token 发送时以 @ 引用前置进消息）。 */
const attachments = ref<{ name: string; token: string }[]>([])
/** 附件上传中（逐个上传期间禁发送按钮）。 */
const uploading = ref(false)
/** 隐藏 file input（📎 按钮触发）。 */
const fileInput = ref<HTMLInputElement | null>(null)
/** composer textarea（@ 引用光标插入用）。 */
const textareaEl = ref<HTMLTextAreaElement | null>(null)

const emit = defineEmits<{ close: [] }>()

/** 抽屉宽度（px，C7）：默认 570，支持边缘拖拽调宽 400–800，localStorage 持久化。 */
const DRAWER_WIDTH_KEY = 'rsiot_drawer_width'
function readDrawerWidth(): number {
  const saved = Number(window.localStorage.getItem(DRAWER_WIDTH_KEY))
  return saved >= 400 && saved <= 800 ? saved : 570
}
const drawerWidth = ref(readDrawerWidth())
/** 拖拽进行中（临时禁用元素过渡，避免拖拽时抖动）。 */
const dragging = ref(false)

/** 拖拽手柄指针按下：记录起点宽度 + 起点 clientX（handle 上绑定 @pointermove/@pointerup）。 */
function onResizeStart(e: PointerEvent) {
  dragging.value = true
  const startX = e.clientX
  const startW = drawerWidth.value
  const onMove = (ev: PointerEvent) => {
    drawerWidth.value = Math.min(800, Math.max(400, startW + (startX - ev.clientX)))
  }
  const onUp = () => {
    dragging.value = false
    window.localStorage.setItem(DRAWER_WIDTH_KEY, String(drawerWidth.value))
    window.removeEventListener('pointermove', onMove)
    window.removeEventListener('pointerup', onUp)
  }
  window.addEventListener('pointermove', onMove)
  window.addEventListener('pointerup', onUp)
}

const { showBackToBottom, onScroll, snapToBottom, followIfAtBottom } = useChatScroll(listRef)

/** provide running 态给 MessageItem（思考面板活跃色 + 加载占位判断）。用 computed
 *  保持响应式——turn_started/turn_done 改变 agent.running 时子组件自动更新。 */
provide(runningKey, computed(() => agent.running))

/** 当前显示的会话标题（下拉触发器文案）。 */
const currentSessionLabel = computed(
  () => agent.currentSession?.title || agent.currentSession?.name || '新会话',
)

/** 执行方式三态（对齐 desktop collaborationMode）：常规/计划(plan)/目标(goal)。 */
const execMode = computed<'normal' | 'plan' | 'goal'>(() => {
  if (agent.status?.plan) return 'plan'
  if (agent.status?.goal || goalDraftMode.value) return 'goal'
  return 'normal'
})

/** goalStatus 中文文案（serve goal.go statusForDisplay：running/complete/blocked/stopped）。 */
const goalStatusLabel = computed(() => {
  switch (agent.status?.goalStatus) {
    case 'running': return '运行中'
    case 'complete': return '已完成'
    case 'blocked': return '阻塞中'
    case 'stopped': return '已停止'
    default: return '未启动'
  }
})

/** 输入框 placeholder 随执行方式分流（目标 draft 态提示输入目标）。 */
const inputPlaceholder = computed(() => {
  if (goalDraftMode.value && !agent.status?.goal) return '输入目标，AI 将持续工作直到完成或阻塞…'
  if (agent.status?.goal) return '目标执行中，可补充指令…'
  return '问 AI…（如：读 devA 实时数据）'
})

/** 点击外部关闭会话下拉。 */
function onDocClick(e: MouseEvent) {
  if (sessionMenuRef.value && !sessionMenuRef.value.contains(e.target as Node)) {
    sessionMenuOpen.value = false
  }
}

onMounted(async () => {
  document.addEventListener('click', onDocClick)
  await agent.start().catch(() => { /* 启动恢复失败已在 store 内 warn，不阻塞 UI */ })
  snapToBottom()
})
onUnmounted(() => {
  document.removeEventListener('click', onDocClick)
  if (goalPollTimer) clearInterval(goalPollTimer)
  agent.stop()
})

/** goal 非空期间轮询 /status 刷新 goalStatus（SSE 无 goal 事件；desktop 进程内 1s 轮询，HTTP 3s 够）。 */
watch(
  () => agent.status?.goal,
  (goal) => {
    if (goalPollTimer) { clearInterval(goalPollTimer); goalPollTimer = null }
    if (goal) {
      goalPollTimer = setInterval(() => { agent.loadStatus().catch(() => {}) }, 3000)
    }
  },
)

/**
 * 消息列表变化时贴底跟随（流式 token 追加、新 user 消息、工具结果到达均触发）。
 * 用户上翻阅读时暂停跟随（atBottom=false），点击「回到底部」恢复。
 */
watch(
  () => [
    agent.messages.length,
    agent.messages.at(-1)?.text.length ?? 0,
    agent.messages.at(-1)?.reasoning?.length ?? 0,
    agent.messages.at(-1)?.toolCalls?.length ?? 0,
  ],
  () => followIfAtBottom(),
)

/**
 * Enter 发送（Shift+Enter 换行由 .exact/.shift 修饰符排除）。
 * IME 组词中的 Enter（选词/确认，isComposing=true）不发送且保留默认行为——
 * 否则中文输入法里选词即误发半句话（与 aifa InputBar 同款防护）。
 */
function onEnterKey(e: KeyboardEvent) {
  if (e.isComposing) return
  e.preventDefault()
  void send()
}

/** 组装提交文本：附件以 @ 引用前置（serve 提交时解析为内容/图片输入，
 *  对标 Reasonix desktop 把 @token 直接写进 composer 文本的语义）。 */
function composeInput(text: string) {
  const refs = attachments.value.map((a) => `@${a.token}`).join(' ')
  return refs ? (text ? `${refs} ${text}` : refs) : text
}

/** 发送：目标 draft 态提交为目标（POST /goal），否则普通 submit。失败给 UI 反馈（T9 M2）。 */
async function send() {
  const text = input.value.trim()
  if ((!text && !attachments.value.length) || agent.running || uploading.value) return
  errorMsg.value = ''
  const composed = composeInput(text)
  try {
    if (goalDraftMode.value && !agent.status?.goal) {
      await agent.setGoal(composed)
      goalDraftMode.value = false  // 目标已提交，输入框回归补充指令
    } else {
      await agent.submit(composed)
    }
    input.value = ''
    attachments.value = []
    snapToBottom()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '发送失败'
  }
}

/** 📎 选本机文件 → gateway 落 reasonix 工作区 `.reasonix/attachments/` → chips 待发送。 */
function pickFiles() {
  fileInput.value?.click()
}

async function onFilesChosen(e: Event) {
  const el = e.target as HTMLInputElement
  const files = Array.from(el.files ?? [])
  el.value = ''  // 清空以允许重复选择同一文件
  if (!files.length) return
  errorMsg.value = ''
  uploading.value = true
  try {
    for (const f of files) {
      attachments.value.push({ name: f.name, token: await agent.uploadAttachment(f) })
    }
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '附件上传失败'
  } finally {
    uploading.value = false
  }
}

function removeAttachment(i: number) {
  attachments.value.splice(i, 1)
}

/** 把 @ 引用插入 textarea 光标处（路径含空格按 Reasonix EscapeRefPath 转义）。 */
function insertRef(path: string) {
  const token = `@${escapeRefPath(path)} `
  const ta = textareaEl.value
  if (!ta) {
    input.value = input.value ? `${input.value} ${token}`.trimEnd() : token
    return
  }
  const start = ta.selectionStart ?? input.value.length
  const end = ta.selectionEnd ?? start
  input.value = input.value.slice(0, start) + token + input.value.slice(end)
  // 光标移到 token 之后并聚焦，便于继续输入
  void nextTick(() => {
    ta.focus()
    ta.selectionStart = ta.selectionEnd = start + token.length
  })
}

/** 工作区选择器选中路径 → 插入 @ 引用并关闭弹窗。 */
function onWorkspaceSelect(path: string) {
  workspacePickerOpen.value = false
  insertRef(path)
}

/** 取消当前 turn。 */
async function cancel() {
  try {
    await agent.cancel()
  } catch {
    /* cancel 失败静默（用户已知 running 态，不强提示） */
  }
}

/** 审批决策（转发给 store，失败不静默——用户在等结果）。 */
async function onApprove(decision: boolean) {
  try {
    await agent.approve(decision)
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '审批提交失败'
  }
}

/** 回答 ask_request（转发给 store，失败不静默——AI 在等回答才能继续）。 */
async function onAskAnswer(answers: { question_id: string; selected: string[] }[]) {
  try {
    await agent.answer(answers)
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '回答提交失败'
  }
}

/** 新建会话（serve 单会话：旧的存盘，active 换新空会话）。turn 运行中明确拒绝
 *  （serve 语义：turn 中切会话混乱；原先静默 no-op，用户点了没反应）。 */
async function onNewSession() {
  if (agent.running) {
    errorMsg.value = '任务进行中，请先停止或等待完成再新建会话'
    return
  }
  sessionMenuOpen.value = false
  errorMsg.value = ''
  try {
    await agent.newSession()
    snapToBottom()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '新建会话失败'
  }
}

/** 切换到历史会话（POST /resume）。 */
async function onSwitchSession(path: string) {
  if (agent.running) {
    errorMsg.value = '任务进行中，请先停止或等待完成再切换会话'
    return
  }
  sessionMenuOpen.value = false
  errorMsg.value = ''
  try {
    await agent.switchSession(path)
    snapToBottom()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '切换会话失败'
  }
}

/** 删除历史会话（删当前活动会话 serve 返 409）。 */
async function onDeleteSession(name: string) {
  if (!confirm(`确定删除会话「${name}」？此操作不可恢复。`)) return
  errorMsg.value = ''
  try {
    await agent.deleteSession(name)
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '删除会话失败'
  }
}

/** 切模型（submit 发 /model，gateway 跳 HINT）。失败不静默（effort 同）。 */
async function onModelChange(e: Event) {
  const ref = (e.target as HTMLSelectElement).value
  if (!ref || ref === agent.activeModelRef) return
  errorMsg.value = ''
  try {
    await agent.setModel(ref)
  } catch (err) {
    errorMsg.value = err instanceof Error ? err.message : '切换模型失败'
  }
}

/** 设思考深度（submit 发 /effort；下拉候选项来自 effortLevelsFor 映射，未知 provider 仍可手输）。 */
async function onEffortSubmit() {
  const lvl = effortInput.value.trim()
  if (!lvl) return
  errorMsg.value = ''
  try {
    await agent.setEffort(lvl)
    effortInput.value = ''
  } catch (err) {
    // serve 非法 level 返 500 + 合法值文案（如 "usage: /effort auto|low|high"），
    // 直接展示让用户知道该 provider 的合法档位。
    errorMsg.value = err instanceof Error ? err.message : '设置思考深度失败'
  }
}

/** 切执行方式（常规/计划/目标）。对齐 desktop：切走 goal 模式先清目标；切到 goal
 *  清旧目标进入目标输入态（draft）；计划开/关走 /plan，目标提交时 serve 自动关 plan。 */
async function onExecMode(mode: 'normal' | 'plan' | 'goal') {
  if (agent.controlBusy || mode === execMode.value) return
  errorMsg.value = ''
  try {
    if (mode === 'goal') {
      if (agent.status?.goal) await agent.clearGoal()
      goalDraftMode.value = true
      return
    }
    goalDraftMode.value = false
    if (agent.status?.goal) await agent.clearGoal()
    await agent.setPlanMode(mode === 'plan')
  } catch (err) {
    errorMsg.value = err instanceof Error ? err.message : '切换执行方式失败'
  }
}

/** 清除目标（POST /goal {goal:""}，AI 停止持续工作；清后回到目标输入态可重设）。 */
async function onClearGoal() {
  errorMsg.value = ''
  try {
    await agent.clearGoal()
    goalDraftMode.value = true
  } catch (err) {
    errorMsg.value = err instanceof Error ? err.message : '清除目标失败'
  }
}

/** 切权限模式（ask/auto/yolo 三档，独立端点 turn 中可切）。 */
async function onApprovalModeChange(e: Event) {
  const mode = (e.target as HTMLSelectElement).value as ToolApprovalMode
  if (!mode || mode === agent.status?.toolApprovalMode) return
  errorMsg.value = ''
  try {
    await agent.setApprovalMode(mode)
  } catch (err) {
    errorMsg.value = err instanceof Error ? err.message : '切换权限模式失败'
  }
}
</script>

<template>
  <aside class="drawer" :class="{ dragging }" aria-label="AI 助手面板" :style="{ width: drawerWidth + 'px' }">
    <!-- 拖拽调宽手柄（C7）：左缘 6px 热区，指针拖拽改宽，400–800 夹取，松手持久化 -->
    <div
      class="resize-handle"
      title="拖拽调整面板宽度"
      aria-hidden="true"
      @pointerdown="onResizeStart"
    />
    <header class="head">
      <div ref="sessionMenuRef" class="session-picker">
        <button
          class="session-trigger"
          :disabled="agent.sessionBusy"
          :title="currentSessionLabel"
          @click="sessionMenuOpen = !sessionMenuOpen"
        >
          <span class="session-label">{{ currentSessionLabel }}</span>
          <span class="caret">▼</span>
        </button>
        <div v-if="sessionMenuOpen" class="session-menu" role="menu">
          <button class="session-new" @click="onNewSession">+ 新建会话</button>
          <div class="session-list">
            <p v-if="!agent.sessions.length" class="session-empty">暂无历史会话</p>
            <div
              v-for="s in agent.sessions"
              :key="s.path"
              class="session-item"
              :class="{ current: s.current }"
              @click="onSwitchSession(s.path)"
            >
              <span class="session-item-title">{{ s.title || s.name }}</span>
              <span class="session-item-meta">
                <span v-if="s.turns != null">{{ s.turns }} 轮</span>
                <span v-if="s.current" class="session-cur-badge">当前</span>
              </span>
              <button
                v-if="!s.current"
                class="session-del"
                title="删除会话"
                aria-label="删除会话"
                @click.stop="onDeleteSession(s.name)"
              >✕</button>
            </div>
          </div>
        </div>
      </div>
      <span class="status" :class="{ on: agent.running }">
        {{ agent.running ? '思考中…' : '就绪' }}
      </span>
      <button
        class="btn-icon"
        title="LLM 供应商管理（新增/编辑/删除供应商、设置默认模型、配置 API Key）"
        aria-label="LLM 供应商管理"
        @click="providerModalOpen = true"
      >⚙</button>
      <button v-if="agent.running" class="btn-text btn-cancel" @click="cancel">停止</button>
      <button class="btn-icon" aria-label="关闭" @click="emit('close')">✕</button>
    </header>

    <div ref="listRef" class="messages" @scroll="onScroll">
      <p v-if="!agent.messages.length" class="placeholder">
        问 AI 助手任何关于采集器/设备/实时数据的问题。<br />
        例如：「读 devA 当前所有实时数据」「devB 的连接状态正常吗？」
      </p>
      <MessageItem v-for="m in agent.messages" :key="m.id" :message="m" />
    </div>

    <button
      v-show="showBackToBottom"
      class="back-to-bottom"
      title="回到底部"
      aria-label="回到底部"
      @click="snapToBottom"
    >↓ 最新</button>

    <div class="input-area">
      <!-- 提问被"稍后再说"折叠后的重开入口（turn 挂起中，不回答 AI 无法继续）。 -->
      <button
        v-if="agent.ask && askDismissed"
        class="ask-reopen"
        @click="askDismissed = false"
      >AI 在等待你的回答（任务挂起中），点击重新打开提问</button>
      <div v-if="errorMsg" class="error-banner" role="alert">
        <span>{{ errorMsg }}</span>
        <button class="btn-text" aria-label="关闭错误提示" @click="errorMsg = ''">✕</button>
      </div>
      <!-- 控件工具栏：模型 / 思考深度 / 执行方式(常规·计划·目标) / 权限模式。
           对接 serve 真实端点（见 agent store 注释）。模型/effort turn 中禁用
           （serve switchModel/switchEffort running 拒绝）；plan/goal 独立端点可切。 -->
      <div class="control-bar">
        <select
          class="ctrl ctrl-model"
          :value="agent.activeModelRef"
          :disabled="agent.controlBusy || agent.running"
          :title="'模型选择（当前: ' + (agent.activeModelRef || '未知') + '）'"
          @change="onModelChange"
        >
          <option value="" disabled>模型</option>
          <option v-for="m in agent.models" :key="m.ref" :value="m.ref">
            {{ m.active ? '● ' : '' }}{{ m.ref }}
          </option>
        </select>
        <input
          v-model="effortInput"
          class="ctrl ctrl-effort"
          type="text"
          list="effort-levels"
          :placeholder="effortOptions.length ? 'effort(' + effortOptions.join('/') + ')' : 'effort(自由输入)'"
          :disabled="agent.controlBusy || agent.running"
          :title="'思考深度（当前模型档位：' + (effortOptions.join(' / ') || '未知，可手输') + '；回车提交，非法值 serve 会提示合法档位）'"
          @keydown.enter.exact.prevent="onEffortSubmit"
        />
        <datalist id="effort-levels">
          <option v-for="lvl in effortOptions" :key="lvl" :value="lvl" />
        </datalist>
        <div class="ctrl ctrl-mode" role="group" aria-label="执行方式">
          <button
            class="mode-btn"
            :class="{ on: execMode === 'normal' }"
            :disabled="agent.controlBusy"
            :title="'常规：边做边推进'"
            @click="onExecMode('normal')"
          >常规</button>
          <button
            class="mode-btn"
            :class="{ on: execMode === 'plan' }"
            :disabled="agent.controlBusy"
            :title="'计划：确认后执行'"
            @click="onExecMode('plan')"
          >计划</button>
          <button
            class="mode-btn"
            :class="{ on: execMode === 'goal' }"
            :disabled="agent.controlBusy"
            :title="'目标：输入目标后持续推进，直到完成或阻塞'"
            @click="onExecMode('goal')"
          >目标</button>
        </div>
        <select
          class="ctrl ctrl-approval"
          :value="agent.status?.toolApprovalMode ?? 'ask'"
          :disabled="agent.controlBusy"
          title="权限模式（ask 逐项审批 / auto 自动 / yolo 全放行）"
          @change="onApprovalModeChange"
        >
          <option value="ask">权限:审批</option>
          <option value="auto">权限:自动</option>
          <option value="yolo">权限:放行</option>
        </select>
      </div>
      <!-- 目标执行中条：目标文本 + goalStatus 状态徽标 + 清除。 -->
      <div v-if="agent.status?.goal" class="goal-bar">
        <span class="goal-text" :title="agent.status.goal">{{ agent.status.goal }}</span>
        <span class="goal-status" :class="agent.status.goalStatus">{{ goalStatusLabel }}</span>
        <button
          class="goal-clear"
          :disabled="agent.controlBusy"
          title="清除目标（AI 停止持续推进）"
          aria-label="清除目标"
          @click="onClearGoal"
        >✕</button>
      </div>
      <!-- 附件/引用工具行：📎 本机文件上传（chips 待发送）+ 📁 工作区 @ 引用选择。 -->
      <div class="attach-row">
        <button
          class="attach-btn"
          :disabled="agent.running || agent.sessionBusy || uploading"
          title="添加文件或图片：上传到工作区后以 @ 引用附加到消息（图片≤10MB，文件≤25MB）"
          @click="pickFiles"
        >📎</button>
        <button
          class="attach-btn"
          :disabled="agent.running || agent.sessionBusy"
          title="引用文件或文件夹：从当前工作区选择上下文"
          @click="workspacePickerOpen = true"
        >📁</button>
        <span v-for="(a, i) in attachments" :key="a.token" class="attach-chip" :title="a.token">
          <span class="chip-name">{{ a.name }}</span>
          <button class="chip-x" aria-label="移除附件" @click="removeAttachment(i)">✕</button>
        </span>
        <span v-if="uploading" class="uploading-hint">上传中…</span>
      </div>
      <div class="input-row">
        <textarea
          ref="textareaEl"
          v-model="input"
          class="textarea"
          rows="2"
          :placeholder="inputPlaceholder"
          :disabled="agent.running || agent.sessionBusy"
          @keydown.enter.exact="onEnterKey"
        />
        <button
          class="btn-send"
          :disabled="(!input.trim() && !attachments.length) || agent.running || agent.sessionBusy || uploading"
          @click="send"
        >{{ goalDraftMode && !agent.status?.goal ? '设目标' : '发送' }}</button>
      </div>
    </div>
    <input ref="fileInput" type="file" multiple hidden @change="onFilesChosen" />

    <PermissionModal v-if="agent.approval" :approval="agent.approval" @approve="onApprove" />
    <AskModal
      v-else-if="agent.ask && !askDismissed"
      :ask="agent.ask"
      @answer="onAskAnswer"
      @close="askDismissed = true"
    />
    <ProviderModal v-if="providerModalOpen" @close="providerModalOpen = false" />
    <WorkspacePicker v-if="workspacePickerOpen" @select="onWorkspaceSelect" @close="workspacePickerOpen = false" />
  </aside>
</template>

<style scoped>
.drawer {
  position: fixed; right: 0; top: 0; bottom: 0;
  width: 570px; max-width: 100vw;
  background: var(--panel-bg);
  border-left: 1px solid var(--panel-border);
  display: grid;
  grid-template-rows: 44px 1fr auto;
  z-index: 100;
  box-shadow: -20px 0 60px color-mix(in srgb, var(--color-surface) 70%, black 30%);
  transition: width 0.12s ease-out;
}
.drawer.dragging { transition: none; }
.resize-handle {
  position: absolute;
  left: 0; top: 0; bottom: 0;
  width: 6px;
  cursor: ew-resize;
  z-index: 5;
}
.resize-handle:hover { background: color-mix(in srgb, var(--color-primary) 22%, transparent); }

/* 头部 */
.head {
  display: flex; align-items: center; gap: 8px;
  padding: 0 12px;
  border-bottom: 1px solid var(--color-divider);
  background: var(--card-bg);
}
.title { margin: 0; flex: 1; font-size: 14px; font-weight: 600; color: var(--color-ink); }
.status {
  font-size: 12px;
  color: var(--color-ink-muted);
  padding: 5px 10px;
  border-radius: 999px;
  background: var(--soft-overlay);
}
.status.on {
  color: var(--chip-ink);
  background: var(--chip-bg);
}
.btn-text {
  background: transparent; border: none;
  font-size: 12px; color: var(--color-ink-muted);
  padding: 2px 4px; cursor: pointer;
}
.btn-text:hover { color: var(--color-ink-secondary); }
.btn-cancel { color: var(--color-danger); }
.btn-icon {
  background: transparent; border: none;
  font-size: 14px; color: var(--color-ink-muted);
  padding: 4px 8px; cursor: pointer; border-radius: 4px;
}
.btn-icon:hover { background: var(--color-surface-hover); color: var(--color-ink); }

/* 消息流 */
.messages {
  overflow-y: auto;
  padding: 12px;
  background:
    radial-gradient(circle at top left, var(--panel-highlight), transparent 22%),
    var(--page-bg);
  scrollbar-width: thin;
  scrollbar-color: rgba(148, 163, 184, 0.6) transparent;
}
.placeholder {
  margin: 40px 16px;
  font-size: 13px; line-height: 1.7;
  color: var(--color-ink-muted);
  text-align: center;
}

/* 回到底部悬浮按钮 */
.back-to-bottom {
  position: absolute;
  bottom: 130px; right: 16px;
  padding: 7px 12px;
  font-size: 12px;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  border-radius: 999px;
  box-shadow: var(--shadow-soft);
  color: var(--color-ink-secondary);
  cursor: pointer;
  z-index: 10;
}
.back-to-bottom:hover { background: var(--color-surface-hover); }

/* 输入区 */
.input-area {
  border-top: 1px solid var(--color-divider);
  background: var(--card-bg);
}
.ask-reopen {
  display: block; width: 100%;
  padding: 8px 12px;
  font-size: 12px; font-family: inherit;
  color: var(--color-warning);
  background: rgba(254, 243, 199, 0.5);
  border: none; border-bottom: 1px solid rgba(252, 211, 77, 0.4);
  cursor: pointer; text-align: center;
}
.ask-reopen:hover { background: rgba(254, 243, 199, 0.8); }
.error-banner {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  padding: 6px 12px;
  font-size: 12px; color: var(--color-danger);
  background: rgba(220, 38, 38, 0.06);
  border-bottom: 1px solid rgba(220, 38, 38, 0.15);
}
.input-row { display: flex; gap: 8px; padding: 10px; padding-top: 0; }
/* 附件/引用工具行（输入区上方）：📎 上传 + 📁 工作区引用 + 待发送附件 chips。 */
.attach-row {
  display: flex; align-items: center; flex-wrap: wrap; gap: 6px;
  padding: 8px 10px 0;
}
.attach-btn {
  font-size: 14px; line-height: 1;
  background: var(--color-surface-hover);
  border: none; border-radius: 8px;
  padding: 6px 9px; cursor: pointer;
}
.attach-btn:hover:not(:disabled) { background: var(--color-surface); outline: 1px solid var(--card-border); }
.attach-btn:disabled { opacity: 0.5; cursor: not-allowed; }
.attach-chip {
  display: inline-flex; align-items: center; gap: 4px;
  max-width: 220px;
  font-size: 12px;
  background: var(--chip-bg); color: var(--chip-ink);
  border-radius: 999px; padding: 3px 4px 3px 10px;
}
.chip-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.chip-x {
  background: transparent; border: none; cursor: pointer;
  font-size: 11px; color: var(--chip-ink); opacity: 0.7;
  padding: 0 4px; border-radius: 999px;
}
.chip-x:hover { opacity: 1; background: rgba(15, 23, 42, 0.08); }
.uploading-hint { font-size: 12px; color: var(--color-ink-muted); }
.textarea {
  flex: 1; resize: none;
  padding: 12px 14px;
  font-size: 13px; line-height: 1.5;
  font-family: inherit;
  border-radius: 14px;
}
.textarea:focus { outline: none; border-color: var(--color-primary); }
.textarea:disabled { background: var(--color-surface-hover); cursor: not-allowed; }
.btn-send {
  align-self: stretch;
  padding: 0 16px;
  font-size: 13px; font-weight: 500;
  background: linear-gradient(135deg, var(--color-primary), var(--color-primary-strong));
  color: var(--shell-theme-active-ink);
  border: none; border-radius: 8px;
  cursor: pointer;
  transition: background-color 0.15s;
  box-shadow: var(--shadow-soft);
}
.btn-send:hover:not(:disabled) { background: var(--color-primary-hover); }
.btn-send:disabled { opacity: 0.5; cursor: not-allowed; }

/* 头部会话下拉 */
.session-picker { position: relative; flex: 1; min-width: 0; }
.session-trigger {
  display: flex; align-items: center; gap: 6px;
  max-width: 100%;
  background: transparent; border: 1px solid transparent;
  padding: 4px 8px; border-radius: 6px;
  font-size: 13px; font-weight: 600; color: var(--color-ink);
  cursor: pointer; white-space: nowrap;
}
.session-trigger:hover:not(:disabled) { background: var(--color-surface-hover); }
.session-trigger:disabled { opacity: 0.5; cursor: not-allowed; }
.session-label { overflow: hidden; text-overflow: ellipsis; }
.caret { font-size: 9px; color: var(--color-ink-muted); }
.session-menu {
  position: absolute; top: 36px; left: 0;
  min-width: 260px; max-width: 360px;
  max-height: 360px; display: flex; flex-direction: column;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  border-radius: 10px;
  box-shadow: var(--shadow-soft), 0 8px 24px rgba(0,0,0,0.12);
  z-index: 50; overflow: hidden;
}
.session-new {
  padding: 9px 12px; text-align: left;
  font-size: 13px; font-weight: 500; color: var(--color-primary);
  background: transparent; border: none; border-bottom: 1px solid var(--color-divider);
  cursor: pointer;
}
.session-new:hover { background: var(--color-surface-hover); }
.session-list { overflow-y: auto; }
.session-empty {
  padding: 16px 12px; margin: 0;
  font-size: 12px; color: var(--color-ink-muted); text-align: center;
}
.session-item {
  display: flex; align-items: center; gap: 8px;
  padding: 8px 12px; cursor: pointer;
  border-bottom: 1px solid var(--color-divider);
}
.session-item:hover { background: var(--color-surface-hover); }
.session-item.current { background: color-mix(in srgb, var(--color-primary) 8%, transparent); }
.session-item-title {
  flex: 1; min-width: 0;
  font-size: 12px; color: var(--color-ink);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.session-item-meta {
  display: flex; align-items: center; gap: 6px;
  font-size: 11px; color: var(--color-ink-muted); flex-shrink: 0;
}
.session-cur-badge {
  color: var(--chip-ink); background: var(--chip-bg);
  padding: 1px 6px; border-radius: 999px;
}
.session-del {
  flex-shrink: 0; width: 20px; height: 20px;
  background: transparent; border: none; border-radius: 4px;
  font-size: 11px; color: var(--color-ink-muted); cursor: pointer;
  opacity: 0;
}
.session-item:hover .session-del { opacity: 1; }
.session-del:hover { background: rgba(220,38,38,0.12); color: var(--color-danger); }

/* 5 控件工具栏：flex 换行而非横向滚动——控件多（模型/effort/三态/权限）窄窗下
   换行保持全部可见，横向滚动条会显得面板"乱"（旧版 overflow-x:auto）。 */
.control-bar {
  display: flex; flex-wrap: wrap; align-items: center; gap: 6px;
  padding: 8px 10px;
  border-bottom: 1px solid var(--color-divider);
  background: var(--panel-bg);
}
.ctrl {
  font-size: 12px; font-family: inherit;
  padding: 4px 8px;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  border-radius: 6px;
  color: var(--color-ink-secondary);
  cursor: pointer;
}
.ctrl:focus { outline: none; border-color: var(--color-primary); }
.ctrl:disabled { opacity: 0.5; cursor: not-allowed; }
.ctrl-model { min-width: 110px; max-width: 170px; flex: 0 1 auto; }
.ctrl-effort { width: 88px; flex: 0 1 auto; }
.ctrl-approval { min-width: 96px; }
/* 执行方式三态分段（常规/计划/目标） */
.ctrl-mode {
  display: flex; padding: 2px; gap: 2px;
  background: var(--card-bg);
  border: 1px solid var(--card-border);
  border-radius: 6px;
}
.mode-btn {
  font-size: 12px; font-family: inherit;
  padding: 2px 8px;
  background: transparent;
  border: none; border-radius: 4px;
  color: var(--color-ink-secondary);
  cursor: pointer; white-space: nowrap;
}
.mode-btn:disabled { opacity: 0.5; cursor: not-allowed; }
.mode-btn.on {
  color: var(--chip-ink); background: var(--chip-bg);
}
/* 目标执行中条 */
.goal-bar {
  display: flex; align-items: center; gap: 8px;
  padding: 6px 10px;
  font-size: 12px;
  border-bottom: 1px solid var(--color-divider);
  background: var(--panel-bg);
}
.goal-text {
  flex: 1; min-width: 0;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  color: var(--color-ink-primary);
}
.goal-status {
  flex: none;
  padding: 1px 8px;
  border-radius: 10px;
  font-size: 11px;
  background: var(--card-bg);
  color: var(--color-ink-secondary);
}
.goal-status.running { background: rgba(34,197,94,0.15); color: #16a34a; }
.goal-status.complete { background: rgba(59,130,246,0.15); color: #2563eb; }
.goal-status.blocked { background: rgba(234,179,8,0.15); color: #ca8a04; }
.goal-status.stopped { background: var(--card-bg); color: var(--color-ink-secondary); }
.goal-clear {
  flex: none;
  border: none; background: transparent;
  color: var(--color-ink-tertiary);
  cursor: pointer; font-size: 12px;
  padding: 0 2px;
}
.goal-clear:hover { color: var(--color-danger); }
.goal-clear:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
