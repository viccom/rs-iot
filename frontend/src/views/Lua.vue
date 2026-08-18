<script setup lang="ts">
// frontend/src/views/Lua.vue
// lua 脚本管理（模块 M4c）：双栏——左脚本列表（三级树：脚本 → 关联实例 → 设备）/ 右编辑器 + 版本历史 + reload 关联实例。
// 后端：GET /lua/scripts（列表）、GET /lua/script（内容）、POST /lua/script/save（存版本）、
// GET /lua/script/versions（历史）、POST /lua/script/rollback（回滚）、POST /lua/reload（重载实例）。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { onBeforeRouteLeave, useRouter } from 'vue-router'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import { useLuaStore } from '../stores/lua'
import { useMonitorStore } from '../stores/monitor'
import { api } from '../api/client'
import { instDeviceView, relatedLuaInstances } from '../lib/instRelations'
import ScriptEditor from '../components/lua/ScriptEditor.vue'

const lua = useLuaStore()
const monitor = useMonitorStore()
const router = useRouter()

/** 新建脚本名输入（新建对话框简易态：列表下方输入框 + 按钮）。 */
const newName = ref('')
/** 当前编辑内容（v-model ScriptEditor）。 */
const editContent = computed({
  get: () => lua.current?.content ?? '',
  set: (v: string) => { if (lua.current) lua.current.content = v },
})

/** 展开设备子列表的实例 ID（三级树第三级，页内状态：null=全收起）。 */
const expandedInst = ref<string | null>(null)

/** 当前选中脚本的关联实例名单（lib 纯函数派生：inst_id/state/设备数/运行时设备）。 */
const relatedInstances = computed(() =>
  lua.current ? relatedLuaInstances(monitor.instanceConfigs, monitor.roster, lua.current.name) : [],
)

/** 某实例的设备行（配置 ∪ 运行时并集 + 来源标注，第三级数据）。 */
function devicesOf(instId: string) {
  const cfg = monitor.instanceConfigs[instId]
  if (!cfg) return []
  const runtimeDevs = relatedInstances.value.find(i => i.inst_id === instId)?.runtimeDevices ?? []
  return instDeviceView(cfg, monitor.rtdata, runtimeDevs)
}

/** 切换实例设备子列表展开态（再点收起）。 */
function toggleInstDevices(instId: string) {
  expandedInst.value = expandedInst.value === instId ? null : instId
}

/** 跳转采集器页并选中实例（[↗] 按钮：管理启停/编辑）。 */
function gotoCollectors(instId: string) {
  monitor.selectInstance(instId)
  void router.push('/collectors')
}

/** 跳转数据页并选中设备（设备行：看该设备实时点位）。
 *  同时聚焦来源实例——否则全局 selectedInstance 若是别的实例，数据页
 *  "实例 AND 设备"双重过滤会组合出空表（审查 P1 修复）。 */
function gotoTelemetry(devId: string, instId?: string) {
  monitor.selectDevice(devId)
  if (instId !== undefined) monitor.selectInstance(instId)
  void router.push('/telemetry')
}

/** 脚本列表项（含关联实例数，前端从 instanceConfigs 反查 app_config.script==name）。 */
const items = computed(() =>
  lua.scripts.map(name => ({
    name,
    instanceCount: countInstances(name),
  })),
)

/** 关联某脚本的 lua 实例数（app_config.script==name 的 luaClient 实例）。 */
function countInstances(name: string): number {
  return Object.values(monitor.instanceConfigs).filter(
    cfg => cfg.app_code === 'luaClient' && cfg.app_config?.script === name,
  ).length
}

/** 选中脚本：加载内容 + 历史（切脚本收起设备子列表，三级树回第二级）。 */
async function selectScript(name: string) {
  if (lua.dirty && lua.current && !confirm(`放弃对「${lua.current.name}」的未保存改动？`)) return
  expandedInst.value = null
  await lua.loadScript(name)
}

/** 新建脚本（仅本地占位，保存时真正写盘）。 */
async function createScript() {
  const name = newName.value.trim()
  if (!name || !/^[A-Za-z0-9_-]+$/.test(name)) {
    alert('脚本名只能含字母/数字/下划线/短横')
    return
  }
  if (lua.scripts.includes(name)) {
    alert('脚本已存在')
    return
  }
  newName.value = ''
  // 直接初始化 current（空内容），保存时 POST /lua/script/save 会建目录。
  lua.current = { name, content: '-- 新建 lua 脚本\nfunction collect()\n  -- ctx:request(frame, timeout)\n  return {}\nend\n', size: 0 }
  lua.versions = []
  lua.dirty = true
}

/** 保存。 */
async function save() {
  await lua.saveScript()
  await lua.loadScripts()  // 新脚本要刷新列表
}

/** 回滚到历史版本（二次确认）。 */
async function rollback(ts: number) {
  if (!confirm(`回滚到版本 ${new Date(ts * 1000).toLocaleString()}？当前内容会先存为新版本。`)) return
  await lua.rollback(ts)
}

/** reload 关联此脚本的 lua 实例。目标 = 展示名单（relatedInstances，与树形展示严格同源——
 *  对着名单点重载，不存在"展示 A 实际重载 B"的漂移）。逐条检查响应，如实汇报成功/失败。 */
async function reloadInstances() {
  if (!lua.current) return
  const targets = relatedInstances.value
  if (!targets.length) {
    alert(`无 luaClient 实例关联脚本「${lua.current.name}」`)
    return
  }
  let ok = 0
  let firstErr = ''
  for (const t of targets) {
    try {
      const r = await api(`/api/rsiot/api/v1/lua/reload?inst_id=${encodeURIComponent(t.inst_id)}`, { method: 'POST' })
      if (r.ok) ok++
      else if (!firstErr) firstErr = `${t.inst_id}: HTTP ${r.status}`
    } catch (e) {
      if (!firstErr) firstErr = `${t.inst_id}: ${e instanceof Error ? e.message : '请求失败'}`
    }
  }
  if (ok === targets.length) {
    alert(`已重载 ${ok} 个关联实例`)
  } else {
    alert(`重载 ${ok}/${targets.length} 个实例成功。首个失败：${firstErr}（实例需处于运行中才会重载）`)
  }
}

/** 版本时间戳格式化。 */
function fmtTs(ts: number): string {
  return new Date(ts * 1000).toLocaleString()
}

onMounted(async () => {
  await Promise.all([lua.loadScripts(), monitor.loadInstanceConfigs()])
  window.addEventListener('beforeunload', onBeforeUnload)
})

onUnmounted(() => {
  window.removeEventListener('beforeunload', onBeforeUnload)
})

/** 路由级未保存守卫（页内切脚本已有 confirm，此前切路由静默丢编辑内容）。 */
onBeforeRouteLeave(() => {
  if (lua.dirty && lua.current) {
    if (!confirm(`放弃对「${lua.current.name}」的未保存改动？`)) return false
  }
})

/** 刷新/关标签同样拦一道（dirty 时浏览器原生确认）。 */
function onBeforeUnload(e: BeforeUnloadEvent) {
  if (lua.dirty) {
    e.preventDefault()
    e.returnValue = ''
  }
}

useWorkspacePage(() => ({
  pageSummary: lua.current
    ? `正在编辑 lua 脚本「${lua.current.name}」${lua.dirty ? '（有未保存改动）' : ''}`
    : '正在浏览 lua 脚本库',
  facts: [
    `脚本总数 ${lua.scripts.length}`,
    `当前脚本 ${lua.current?.name || '未选中'}`,
    `历史版本 ${lua.versions.length}`,
    `操作中 ${lua.loading ? '是' : '否'}`,
  ],
}))
</script>

<template>
  <section class="lua-page">
    <!-- 左：脚本列表 -->
    <article class="page-panel list-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">脚本库</p>
          <h3>lua 脚本</h3>
        </div>
        <span>{{ lua.scripts.length }} 个</span>
      </div>

      <div class="script-list">
        <div v-for="it in items" :key="it.name" class="script-block">
          <button
            class="script-item"
            :class="{ active: lua.current?.name === it.name }"
            @click="selectScript(it.name)"
          >
            <span class="name">{{ it.name }}</span>
            <span v-if="it.instanceCount" class="inst-count">{{ it.instanceCount }} 实例</span>
            <span v-else class="inst-count none">未关联</span>
          </button>

          <!-- 三级树第二级：选中脚本的关联实例名单（点行展开设备，[↗] 跳采集器页管理） -->
          <div v-if="lua.current?.name === it.name && relatedInstances.length" class="inst-list">
            <div
              v-for="inst in relatedInstances"
              :key="inst.inst_id"
              class="inst-row"
              :class="[inst.state]"
            >
              <button class="inst-main" @click="toggleInstDevices(inst.inst_id)">
                <span class="inst-dot" :title="inst.state"></span>
                <span class="inst-id">{{ inst.inst_id }}</span>
                <span class="inst-state">{{ inst.state }}</span>
                <span class="inst-devs">{{ inst.deviceCount }} 设备</span>
                <span
                  class="inst-caret"
                  :class="{ open: expandedInst === inst.inst_id }"
                >▸</span>
              </button>
              <button
                class="inst-goto"
                title="到采集器页管理此实例（启停/编辑/删除）"
                @click.stop="gotoCollectors(inst.inst_id)"
              >↗</button>

              <!-- 三级树第三级：实例设备（配置 ∪ 运行时上报 + 来源标注） -->
              <div v-if="expandedInst === inst.inst_id" class="dev-list">
                <button
                  v-for="d in devicesOf(inst.inst_id)"
                  :key="d.devId"
                  class="dev-row"
                  :class="{ nodata: !d.hasData }"
                  :title="d.source === 'runtime-only' ? '运行时上报（未在实例配置中声明）' : '实例配置声明'"
                  @click="gotoTelemetry(d.devId, inst.inst_id)"
                >
                  <span class="dev-id">{{ d.devId }}</span>
                  <span class="dev-tags">{{ d.tagCount }} tags</span>
                  <span v-if="d.source === 'runtime-only'" class="dev-badge rt">运行时</span>
                  <span v-if="!d.hasData" class="dev-badge off">无数据</span>
                </button>
                <p v-if="!devicesOf(inst.inst_id).length" class="dev-empty">无设备（配置与运行时均空）</p>
              </div>
            </div>
          </div>
          <p
            v-if="lua.current?.name === it.name && !relatedInstances.length"
            class="inst-none"
          >无 luaClient 实例引用此脚本</p>
        </div>
        <p v-if="!lua.scripts.length" class="empty">暂无脚本</p>
      </div>

      <!-- 新建脚本 -->
      <div class="new-row">
        <input v-model="newName" placeholder="新脚本名（[A-Za-z0-9_-]）" @keydown.enter="createScript" />
        <button class="btn-new" @click="createScript">新建</button>
      </div>
    </article>

    <!-- 右：编辑器 + 版本历史 -->
    <article class="page-panel editor-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">{{ lua.current ? '脚本编辑' : '请选择脚本' }}</p>
          <h3>{{ lua.current?.name || '—' }}</h3>
        </div>
        <div v-if="lua.current" class="head-actions">
          <span v-if="lua.dirty" class="dirty-flag">未保存</span>
          <button class="btn-reload" @click="reloadInstances">重载关联实例</button>
          <button class="btn-save" :disabled="lua.loading" @click="save">保存</button>
        </div>
      </div>

      <p v-if="lua.error" class="err-msg">{{ lua.error }}</p>

      <div v-if="lua.current" class="editor-body">
        <div class="editor-wrap">
          <ScriptEditor v-model="editContent" :readonly="lua.loading" @dirty="lua.markDirty()" />
        </div>

        <!-- 版本历史 -->
        <div class="versions">
          <p class="versions-label">历史版本（{{ lua.versions.length }}）</p>
          <div v-if="lua.versions.length" class="version-list">
            <div v-for="v in lua.versions" :key="v.ts" class="version-item">
              <div class="version-info">
                <span class="version-ts">{{ fmtTs(v.ts) }}</span>
                <span class="version-size">{{ v.size }} B</span>
              </div>
              <button class="btn-rollback" @click="rollback(v.ts)">回滚</button>
            </div>
          </div>
          <p v-else class="versions-empty">暂无历史版本（保存后会自动存档）</p>
        </div>
      </div>

      <p v-else class="empty">左侧选择一个脚本开始编辑，或新建脚本。</p>
    </article>
  </section>
</template>

<style scoped>
.lua-page {
  display: grid;
  gap: 18px;
  grid-template-columns: minmax(240px, 300px) minmax(0, 1fr);
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
  position: absolute; inset: 0;
  background: linear-gradient(140deg, var(--panel-highlight), transparent 50%);
  pointer-events: none;
}

.panel-head {
  display: flex; justify-content: space-between; gap: 12px; align-items: end;
  margin-bottom: 16px; position: relative; z-index: 1;
}
.panel-head h3 { margin: 0; font-family: Consolas, monospace; }
.panel-label {
  margin: 0 0 10px; font-size: 11px; letter-spacing: 0.18em; text-transform: uppercase;
  color: var(--color-ink-dim);
}

/* 脚本列表 */
.list-panel { display: flex; flex-direction: column; }
.script-list {
  flex: 1; overflow-y: auto; display: flex; flex-direction: column; gap: 8px;
  position: relative; z-index: 1;
}
.script-item {
  display: flex; justify-content: space-between; align-items: center; gap: 8px;
  padding: 10px 12px; border-radius: 12px; cursor: pointer;
  border: 1px solid var(--card-border); background: var(--card-bg);
  text-align: left; font-size: 13px;
}
.script-item:hover { background: var(--color-surface-hover); }
.script-item.active {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 18%, white), var(--card-bg));
  border-color: var(--color-divider-strong);
}
.name { font-family: Consolas, monospace; }
.inst-count {
  font-size: 11px; padding: 3px 8px; border-radius: 999px;
  background: var(--chip-bg); color: var(--chip-ink); white-space: nowrap;
}
.inst-count.none { color: var(--color-ink-muted); }

/* ---- 三级树：脚本 → 实例 → 设备 ---- */
.script-block { display: flex; flex-direction: column; gap: 6px; }

.inst-list {
  display: flex; flex-direction: column; gap: 6px;
  padding-left: 12px; border-left: 2px solid var(--chip-border);
  margin-left: 8px;
  /* 实例多时不无限撑长左栏（与 .versions 的 480px 上限同款约束）。 */
  max-height: 420px; overflow-y: auto;
}
.inst-row {
  display: flex; align-items: stretch; gap: 4px;
  border: 1px solid var(--card-border); border-radius: 10px;
  background: var(--card-bg); overflow: hidden;
}
.inst-main {
  flex: 1; display: flex; align-items: center; gap: 8px;
  padding: 8px 10px; border: none; background: transparent; cursor: pointer;
  text-align: left; font-size: 12px; min-width: 0;
}
.inst-dot { width: 8px; height: 8px; border-radius: 50%; flex: none; background: var(--color-ink-muted); }
.inst-row.running .inst-dot { background: var(--color-success); }
.inst-row.failed .inst-dot { background: var(--color-danger); }
.inst-row.starting .inst-dot, .inst-row.stopping .inst-dot { background: var(--color-warning); }
.inst-id { font-family: Consolas, monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.inst-state {
  font-size: 10px; color: var(--color-ink-muted);
  letter-spacing: 0.06em; text-transform: uppercase; flex: none;
}
.inst-devs { font-size: 10px; color: var(--color-ink-muted); flex: none; }
.inst-caret { margin-left: auto; color: var(--color-ink-muted); transition: transform 0.15s; flex: none; }
.inst-caret.open { transform: rotate(90deg); }
.inst-goto {
  flex: none; width: 34px; border: none; border-left: 1px solid var(--card-border);
  background: transparent; color: var(--color-ink-muted); cursor: pointer; font-size: 14px;
}
.inst-goto:hover { color: var(--color-primary); background: var(--color-surface-hover); }

.dev-list {
  display: flex; flex-direction: column; gap: 4px;
  padding: 6px 0 6px 18px; border-top: 1px dashed var(--card-border);
}
.dev-row {
  display: flex; align-items: center; gap: 8px;
  padding: 5px 8px; border: none; border-radius: 8px;
  background: transparent; cursor: pointer; text-align: left; font-size: 12px;
}
.dev-row:hover { background: var(--color-surface-hover); }
.dev-row.nodata { opacity: 0.6; }
.dev-id { font-family: Consolas, monospace; }
.dev-tags { font-size: 10px; color: var(--color-ink-muted); }
.dev-badge {
  font-size: 10px; padding: 1px 6px; border-radius: 999px; white-space: nowrap;
}
.dev-badge.rt {
  background: color-mix(in srgb, var(--color-primary) 14%, transparent);
  color: var(--color-primary);
}
.dev-badge.off { background: var(--chip-bg); color: var(--color-ink-muted); }
.dev-empty { margin: 0; padding: 4px 8px; font-size: 11px; color: var(--color-ink-muted); }
.inst-none { margin: 0 0 0 20px; font-size: 11px; color: var(--color-ink-muted); }

.new-row { display: flex; gap: 6px; margin-top: 12px; position: relative; z-index: 1; }
.new-row input {
  flex: 1; padding: 8px 10px; border-radius: 8px; font-size: 12px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: var(--color-ink);
}
.btn-new {
  padding: 8px 14px; border-radius: 8px; font-size: 12px; cursor: pointer;
  border: 1px solid var(--color-primary); background: var(--color-primary); color: white; white-space: nowrap;
}

/* 编辑器 */
.editor-panel { display: flex; flex-direction: column; }
.head-actions { display: flex; align-items: center; gap: 10px; }
.dirty-flag {
  padding: 4px 10px; border-radius: 999px; font-size: 11px;
  background: color-mix(in srgb, var(--color-warning) 20%, transparent); color: var(--color-warning);
}
.btn-reload, .btn-save {
  padding: 8px 16px; border-radius: 8px; font-size: 13px; font-weight: 500; cursor: pointer;
  border: 1px solid transparent;
}
.btn-reload { background: var(--color-surface-hover); color: var(--color-ink-secondary); border-color: var(--color-divider); }
.btn-save { background: var(--color-primary); color: white; }
.btn-save:disabled { opacity: 0.5; cursor: not-allowed; }

.err-msg { color: var(--color-danger); font-size: 12px; margin: 0 0 10px; position: relative; z-index: 1; }

.editor-body {
  flex: 1; display: grid; grid-template-columns: 1fr 240px; gap: 14px;
  min-height: 420px; position: relative; z-index: 1;
}
.editor-wrap { min-height: 420px; overflow: hidden; }

.versions { display: flex; flex-direction: column; gap: 8px; max-height: 480px; overflow-y: auto; }
.versions-label { margin: 0; font-size: 11px; color: var(--color-ink-muted); letter-spacing: 0.08em; text-transform: uppercase; }
.version-list { display: flex; flex-direction: column; gap: 6px; }
.version-item {
  display: flex; justify-content: space-between; align-items: center; gap: 8px;
  padding: 8px 10px; border-radius: 8px;
  border: 1px solid var(--card-border); background: var(--card-bg);
}
.version-info { display: flex; flex-direction: column; gap: 2px; }
.version-ts { font-size: 12px; color: var(--color-ink-secondary); }
.version-size { font-size: 10px; color: var(--color-ink-muted); }
.btn-rollback {
  padding: 4px 10px; border-radius: 6px; font-size: 11px; cursor: pointer;
  border: 1px solid var(--color-divider); background: var(--color-surface-hover); color: var(--color-ink-secondary);
}
.btn-rollback:hover { background: var(--color-surface-active); }
.versions-empty { font-size: 12px; color: var(--color-ink-muted); margin: 8px 0; }

.empty { color: var(--color-ink-dim); padding: 20px 0; font-size: 13px; }

@media (max-width: 960px) {
  .lua-page, .editor-body { grid-template-columns: 1fr; }
}
</style>
