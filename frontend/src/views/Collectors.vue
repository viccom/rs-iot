<script setup lang="ts">
// frontend/src/views/Collectors.vue
// 采集器管理：左实例名册（三类型分组 + 新建）+ 右运行详情（编辑/启停/删除）。
// M1：从只读卡片 → 可编辑实例（InstanceForm 整包读-改-写 POST /inst/set）。
import { computed, ref } from 'vue'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useMonitorStore } from '../stores/monitor'
import type { InstConfig, InstanceStat } from '../types'
import InstanceForm from '../components/monitor/InstanceForm.vue'

const monitor = useMonitorStore()

/** 实例类型分类（无独立 lua host_kind：app_code=="luaClient" → lua）。 */
function kindOf(inst: InstanceStat): 'builtin' | 'lua' | 'external' {
  if (inst.host_kind === 'external') return 'external'
  if (inst.app_code === 'luaClient') return 'lua'
  return 'builtin'
}

// 名册 = 配置全集 ∪ 运行态（monitor.roster）——未启动实例(auto_start=false)也可管理。
const builtinList = computed(() => monitor.roster.filter(i => kindOf(i) === 'builtin'))
const luaList = computed(() => monitor.roster.filter(i => kindOf(i) === 'lua'))
const externalList = computed(() => monitor.roster.filter(i => kindOf(i) === 'external'))

/** 当前选中实例的完整配置（编辑用）。 */
const selectedConfig = computed<InstConfig | null>(() => {
  if (!monitor.selectedInstance) return null
  return monitor.instanceConfigs[monitor.selectedInstance] ?? null
})
const selectedStat = computed(() =>
  monitor.roster.find(i => i.inst_id === monitor.selectedInstance) ?? null,
)

/** 实例编辑弹窗显隐 + 编辑目标。 */
const showForm = ref(false)
const editing = ref<InstConfig | null>(null)
const saving = ref(false)
const opError = ref('')
/** 操作进行中（启停/删除期间禁用全部操作按钮，防轮询间隙重复提交）。 */
const opBusy = ref(false)
/** 成功反馈（3s 自动清除；此前成功无任何提示，用户靠猜）。 */
const opNotice = ref('')
let noticeTimer: ReturnType<typeof setTimeout> | null = null

function notify(msg: string) {
  opNotice.value = msg
  if (noticeTimer) clearTimeout(noticeTimer)
  noticeTimer = setTimeout(() => { opNotice.value = '' }, 3000)
}

/** 打开新建表单。 */
function onNew() {
  editing.value = null
  showForm.value = true
}
/** 打开编辑表单（用选中实例的完整配置）。 */
function onEdit() {
  if (!selectedConfig.value) return
  editing.value = selectedConfig.value
  showForm.value = true
}
/** 保存（来自 InstanceForm）→ store.saveInstance 整包写。
 *  D 修复：period 改了且实例运行中 → 提示重启（period 无热加载，各插件 run 时一次性读取）。
 *  旧 period/运行态必须在 saveInstance **之前**捕获——saveInstance 内部重拉配置与状态，
 *  之后 selectedConfig/selectedStat 已是新值，对比恒相等（重启提示永不触发的时序 bug）。 */
async function onSave(cfg: InstConfig) {
  const oldPeriod = selectedConfig.value?.app_config?.period
  const wasRunning = selectedStat.value?.state === 'running'
  saving.value = true
  opError.value = ''
  const ok = await monitor.saveInstance(cfg)
  saving.value = false
  if (!ok) {
    opError.value = '保存失败（检查 inst_id 是否冲突或字段是否合法）'
    return
  }
  showForm.value = false
  notify('已保存')
  const newPeriod = cfg.app_config?.period
  if (oldPeriod !== undefined && oldPeriod !== newPeriod && wasRunning && monitor.selectedInstance) {
    if (confirm('采集周期已修改，需重启实例才生效。是否立即重启？')) {
      const stopped = await monitor.stopInstance(monitor.selectedInstance)
      if (stopped) {
        await monitor.startInstance(monitor.selectedInstance)
        notify('已重启（周期生效）')
      } else {
        opError.value = '重启失败（实例停止失败）'
      }
    }
  }
}
/** 删除选中实例（二次确认）。删后清选中（避免残留指向已删实例的幽灵选中）。 */
async function onDelete() {
  if (!monitor.selectedInstance || opBusy.value) return
  if (!confirm(`确定删除实例 ${monitor.selectedInstance}？此操作不可撤销。`)) return
  opBusy.value = true
  opError.value = ''
  const ok = await monitor.deleteInstance(monitor.selectedInstance)
  opBusy.value = false
  if (!ok) { opError.value = '删除失败'; return }
  monitor.selectInstance('')
  notify('已删除')
}
/** 清空实时数据（scope=rtdata：清缓存保留实例配置，轻量）。 */
async function onPurgeRtdata() {
  if (!confirm('确定清空所有设备的实时数据缓存？实例配置保留。')) return
  opError.value = ''
  const ok = await monitor.purge('rtdata')
  if (!ok) opError.value = '清空实时数据失败'
  else notify('已清空实时数据')
}
/** 重置全部（scope=all：删所有实例 + 级联数据，保留登录账号。高危）。 */
async function onPurgeAll() {
  if (!confirm('确定重置全部？将删除所有实例及其设备/数据（登录账号保留），且不可恢复！')) return
  opError.value = ''
  const ok = await monitor.purge('all')
  if (!ok) opError.value = '重置失败'
  else notify('已重置全部')
}
/** 启动/停止（busy 防重复提交 + 成功反馈）。 */
async function onStart() {
  if (!monitor.selectedInstance || opBusy.value) return
  opBusy.value = true
  opError.value = ''
  const ok = await monitor.startInstance(monitor.selectedInstance)
  opBusy.value = false
  opError.value = ok ? '' : '启动失败'
  if (ok) notify('启动指令已下发')
}
async function onStop() {
  if (!monitor.selectedInstance || opBusy.value) return
  opBusy.value = true
  opError.value = ''
  const ok = await monitor.stopInstance(monitor.selectedInstance)
  opBusy.value = false
  opError.value = ok ? '' : '停止失败'
  if (ok) notify('停止指令已下发')
}

useWorkspacePage(() => ({
  pageSummary: monitor.selectedInstance
    ? `正在管理采集器 ${monitor.selectedInstance}（可编辑参数/点表）`
    : '正在浏览所有采集器（内置/lua/外部）',
  facts: [
    `实例总数 ${monitor.roster.length}`,
    `内建 ${builtinList.value.length} / lua ${luaList.value.length} / 外部 ${externalList.value.length}`,
    `当前实例 ${monitor.selectedInstance || '未选中'}`,
    `当前实例状态 ${selectedStat.value?.state || '未知'}`,
  ],
}))
</script>

<template>
  <section class="collectors-page">
    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">实例名册</p>
          <h3>采集器列表</h3>
        </div>
        <div class="head-actions">
          <span class="count">{{ monitor.roster.length }} 个</span>
          <button class="btn-new" @click="onNew">+ 新建实例</button>
        </div>
      </div>

      <!-- 危险区（C6）：清空/重置与常规操作隔离——折叠展示，防止误触一步点掉全站数据。
           折叠态只占一行文字，点击展开后才露出两个红色按钮。 -->
      <details class="danger-zone">
        <summary>危险操作（清空实时数据 / 重置全部）</summary>
        <div class="danger-body">
          <button class="btn-ghost" title="清空实时数据缓存，保留实例配置" @click="onPurgeRtdata">清空实时数据</button>
          <button class="btn-danger" title="删除所有实例及数据（保留登录账号），不可恢复" @click="onPurgeAll">重置全部</button>
        </div>
      </details>

      <!-- 三类型分组列表 -->
      <div class="kind-list">
        <div v-if="builtinList.length" class="kind-group">
          <p class="kind-label">内置协议（{{ builtinList.length }}）</p>
          <div class="items">
            <button
              v-for="inst in builtinList" :key="inst.inst_id"
              class="item" :class="{ active: monitor.selectedInstance === inst.inst_id, [inst.state]: true }"
              @click="monitor.selectInstance(inst.inst_id)"
            >
              <span class="dot" />
              <span class="id">{{ inst.inst_id }}</span>
              <span class="code">{{ inst.app_code }}</span>
              <span class="state">{{ inst.state }}</span>
            </button>
          </div>
        </div>
        <div v-if="luaList.length" class="kind-group">
          <p class="kind-label">Lua 脚本（{{ luaList.length }}）</p>
          <div class="items">
            <button
              v-for="inst in luaList" :key="inst.inst_id"
              class="item lua" :class="{ active: monitor.selectedInstance === inst.inst_id, [inst.state]: true }"
              @click="monitor.selectInstance(inst.inst_id)"
            >
              <span class="dot" />
              <span class="id">{{ inst.inst_id }}</span>
              <span class="code">luaClient</span>
              <span class="state">{{ inst.state }}</span>
            </button>
          </div>
        </div>
        <div v-if="externalList.length" class="kind-group">
          <p class="kind-label">外部插件（{{ externalList.length }}）</p>
          <div class="items">
            <button
              v-for="inst in externalList" :key="inst.inst_id"
              class="item ext" :class="{ active: monitor.selectedInstance === inst.inst_id, [inst.state]: true }"
              @click="monitor.selectInstance(inst.inst_id)"
            >
              <span class="dot" />
              <span class="id">{{ inst.inst_id }}</span>
              <span class="code">{{ inst.app_code }}</span>
              <span class="state">{{ inst.state }}</span>
            </button>
          </div>
        </div>
        <p v-if="!monitor.roster.length" class="empty">暂无实例，点「+ 新建实例」创建</p>
      </div>
    </article>

    <article class="page-panel detail-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">运行详情</p>
          <h3>{{ selectedStat?.inst_id || '请选择一个采集器' }}</h3>
        </div>
        <span class="state-badge" :class="selectedStat?.state">
          {{ selectedStat?.state || '—' }}
        </span>
      </div>

      <div v-if="selectedStat" class="detail-body">
        <div class="detail-grid">
          <div class="detail-card"><span>插件</span><strong>{{ selectedStat.app_code }}</strong></div>
          <div class="detail-card"><span>宿主类型</span><strong>{{ selectedStat.host_kind }}</strong></div>
          <div class="detail-card"><span>健康</span><strong>{{ selectedStat.health?.status || 'unknown' }}</strong></div>
          <div class="detail-card"><span>最近失败</span><strong class="err">{{ selectedStat.health?.last_err || '无' }}</strong></div>
        </div>

        <!-- 运行参数预览（app_config 关键字段）-->
        <div v-if="selectedConfig" class="cfg-preview">
          <p class="preview-label">运行参数</p>
          <pre class="preview-json">{{ JSON.stringify(selectedConfig.app_config, null, 2) }}</pre>
        </div>

        <!-- 操作按钮 -->
        <div class="ops">
          <button class="op op-start" :disabled="selectedStat.state === 'running' || opBusy" @click="onStart">启动</button>
          <button class="op op-stop" :disabled="selectedStat.state === 'stopped' || opBusy" @click="onStop">停止</button>
          <button class="op op-edit" :disabled="opBusy" @click="onEdit">编辑</button>
          <button class="op op-del" :disabled="opBusy" @click="onDelete">删除</button>
        </div>
        <p v-if="opNotice" class="op-ok">{{ opNotice }}</p>
        <p v-if="opError" class="op-error">{{ opError }}</p>
      </div>

      <p v-else class="empty">左侧选择一个采集器后，这里会展示它的运行详情和操作入口。</p>
    </article>

    <InstanceForm v-if="showForm" :model-value="editing" @close="showForm = false" @save="onSave" />
  </section>
</template>

<style scoped>
.collectors-page {
  display: grid;
  gap: 18px;
  grid-template-columns: minmax(260px, 340px) minmax(0, 1fr);
}

.page-panel {
  border-radius: var(--radius-panel);
  padding: 22px;
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  box-shadow: var(--shadow-panel);
  position: relative;
  overflow: hidden;
}

.page-panel::before {
  content: "";
  position: absolute;
  inset: 0;
  background: linear-gradient(140deg, var(--panel-highlight), transparent 50%);
  pointer-events: none;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: end;
  margin-bottom: 16px;
  position: relative;
  z-index: 1;
}

.panel-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }

.head-actions { display: flex; flex-wrap: wrap; align-items: center; gap: 10px; }
.count { font-size: 12px; color: var(--color-ink-dim); }
.btn-new {
  padding: 6px 14px; border-radius: 10px;
  border: 1px solid var(--color-primary); background: var(--color-primary);
  color: white; font-size: 12px; cursor: pointer; white-space: nowrap;
}
.btn-new:hover { background: var(--color-primary-hover); }
.btn-ghost {
  padding: 6px 14px; border-radius: 10px;
  border: 1px solid var(--card-border); background: var(--card-bg);
  color: var(--color-ink-secondary); font-size: 12px; cursor: pointer; white-space: nowrap;
}
.btn-ghost:hover { border-color: var(--color-primary); color: var(--color-primary); }
.btn-danger {
  padding: 6px 14px; border-radius: 10px;
  border: 1px solid var(--color-danger); background: transparent;
  color: var(--color-danger); font-size: 12px; cursor: pointer; white-space: nowrap;
}
.btn-danger:hover { background: var(--color-danger); color: white; }

/* 危险区折叠（C6）：summary 常显一行，展开后才是按钮 */
.danger-zone {
  position: relative;
  z-index: 1;
  margin-bottom: 12px;
  border: 1px dashed var(--color-danger);
  border-radius: 10px;
  padding: 0;
}
.danger-zone summary {
  cursor: pointer;
  font-size: 12px;
  color: var(--color-danger);
  padding: 8px 12px;
  user-select: none;
  list-style: none;
}
.danger-zone summary::-webkit-details-marker { display: none; }
.danger-zone summary::before { content: "⚠ "; }
.danger-zone[open] summary { border-bottom: 1px dashed var(--color-danger); }
.danger-body {
  display: flex;
  gap: 8px;
  padding: 10px 12px;
  flex-wrap: wrap;
}

.panel-label {
  margin: 0 0 10px;
  font-size: 11px; letter-spacing: 0.18em; text-transform: uppercase;
  color: var(--color-ink-dim);
}

.kind-list { position: relative; z-index: 1; max-height: 60vh; overflow-y: auto; }
.kind-group { margin-bottom: 14px; }
.kind-label {
  margin: 0 0 8px; font-size: 11px; letter-spacing: 0.1em;
  color: var(--color-ink-muted); text-transform: uppercase;
}
.items { display: flex; flex-direction: column; gap: 8px; }
.item {
  display: grid; grid-template-columns: auto 1fr auto auto; gap: 8px; align-items: center;
  padding: 10px 12px; border-radius: 12px; cursor: pointer; font-size: 13px;
  border: 1px solid var(--card-border); background: var(--card-bg);
  text-align: left;
}
.item:hover { background: var(--color-surface-hover); border-color: var(--color-divider); }
.item.active {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 18%, white), var(--card-bg));
  border-color: var(--color-divider-strong);
}
.item.lua .code { color: #7c3aed; }
.item.ext .code { color: #0891b2; }
.dot { width: 8px; height: 8px; border-radius: 50%; }
.running .dot { background: var(--color-success); }
.failed .dot { background: var(--color-danger); }
.stopped .dot { background: var(--color-ink-muted); }
/* 过渡态（starting/stopping）：黄色闪烁，与 stopped 灰点区分（C5） */
.starting .dot, .stopping .dot {
  background: var(--color-warning);
  animation: dot-pulse 1s ease-in-out infinite;
}
@keyframes dot-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.25; }
}
.id { font-family: Consolas, monospace; }
.code { color: var(--color-ink-muted); font-size: 11px; }
.state {
  padding: 3px 8px; border-radius: 999px;
  background: var(--soft-overlay); color: var(--color-ink-dim);
  font-size: 10px; letter-spacing: 0.06em; text-transform: uppercase;
}
.empty { color: var(--color-ink-dim); padding: 16px 0; font-size: 13px; }

.state-badge {
  padding: 5px 12px; border-radius: 999px; font-size: 12px;
  background: var(--soft-overlay); color: var(--color-ink-dim);
}
.state-badge.running { background: color-mix(in srgb, var(--color-success) 20%, transparent); color: var(--color-success); }
.state-badge.failed { background: color-mix(in srgb, var(--color-danger) 20%, transparent); color: var(--color-danger); }
.state-badge.starting, .state-badge.stopping {
  background: color-mix(in srgb, var(--color-warning) 20%, transparent);
  color: var(--color-warning);
}

.detail-body { position: relative; z-index: 1; display: flex; flex-direction: column; gap: 16px; }
.detail-grid { display: grid; gap: 12px; grid-template-columns: repeat(2, minmax(0, 1fr)); }
.detail-card {
  display: grid; gap: 6px; padding: 14px; border-radius: var(--radius-card);
  background: var(--card-bg); border: 1px solid var(--card-border);
}
.detail-card span { color: var(--color-ink-dim); font-size: 11px; }
.detail-card strong { font-family: Consolas, monospace; font-size: 13px; }
.detail-card .err { color: var(--color-danger); font-size: 12px; word-break: break-all; }

.cfg-preview { display: flex; flex-direction: column; gap: 6px; }
.preview-label { margin: 0; font-size: 11px; color: var(--color-ink-muted); }
.preview-json {
  margin: 0; padding: 12px; border-radius: 10px;
  background: var(--card-bg); border: 1px solid var(--card-border);
  font-family: Consolas, monospace; font-size: 12px; color: var(--color-ink-secondary);
  max-height: 200px; overflow: auto; white-space: pre-wrap;
}

.ops { display: flex; gap: 8px; flex-wrap: wrap; }
.op {
  padding: 8px 18px; border-radius: 8px; font-size: 13px; font-weight: 500;
  border: 1px solid transparent; cursor: pointer;
}
.op:disabled { opacity: 0.4; cursor: not-allowed; }
.op-start { background: var(--color-success); color: white; }
.op-stop { background: var(--color-surface-hover); color: var(--color-ink-secondary); border-color: var(--color-divider); }
.op-edit { background: var(--color-primary); color: white; }
.op-del { background: transparent; color: var(--color-danger); border-color: var(--color-danger); }
.op-error { color: var(--color-danger); font-size: 12px; margin: 4px 0 0; }
.op-ok { color: var(--color-success); font-size: 12px; margin: 4px 0 0; }

@media (max-width: 960px) {
  .collectors-page, .detail-grid { grid-template-columns: 1fr; }
}
</style>
