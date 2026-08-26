<script setup lang="ts">
// frontend/src/views/DeviceModels.vue
// 设备模型页（元数据唯一来源管理面，M-1）：四面板——
// ① 模型面板（devmodels：建/改/删 + 参数化地址模板 + 模板默认 props）；
// ② 设备注册表面板（devreg：建设备选模型 + 实例化参数 + per-tag 三态覆盖）；
// ③ 标签总览（全设备 effective 平铺：模板∪覆盖 的最终生效视图 + D2 glob
//    批量覆盖 + effective 后端对账）；
// ④ 发现队列（M-4：未建模标签列出 + 多选 adopt 一键建模（D2 起逐 tag 类型）
//    + 按设备清除）。
import { computed, nextTick, onMounted, ref } from 'vue'
import { useDevregStore, type DiscoveryItem } from '../stores/devreg'
import { useWorkspacePage } from '../composables/useWorkspacePage'
import {
  baseProps, extractParams, mergeTagProps, triStateText,
  validateDevice, validateModel,
  type DeviceEntry, type DeviceModel, type TagOverride, type TagTemplate,
} from '../lib/devmeta'
import {
  buildBatchProps, globMatch, reconcileTags,
  type DeadbandChoice, type EffectiveProps, type HistoryChoice, type TagDrift,
} from '../lib/tagops'
import ModelForm from '../components/devmeta/ModelForm.vue'
import DeviceForm from '../components/devmeta/DeviceForm.vue'
import HistoryQuery from '../components/devmeta/HistoryQuery.vue'

const store = useDevregStore()

// ---- 模型面板 ----
const selectedModelId = ref('')
const selectedModel = computed<DeviceModel | null>(
  () => store.models[selectedModelId.value] ?? null,
)
/** 参数声明摘要文本（{base} {db} 形态）。 */
const paramsSummary = computed(() => {
  const m = selectedModel.value
  if (!m || !m.params.length) return '（无）'
  return m.params.map(p => '{' + p + '}').join(' ')
})
const modelList = computed(() => Object.values(store.models).sort((a, b) => a.model_id.localeCompare(b.model_id)))

/** 选中模型的引用设备数（删除守卫提示）。 */
const refsOf = (modelId: string) =>
  Object.values(store.devices).filter(d => d.model === modelId)

const showModelForm = ref(false)
const modelFormMode = ref<'create' | 'update'>('create')
const modelFormValue = ref<DeviceModel | null>(null)
function onNewModel() {
  modelFormMode.value = 'create'
  modelFormValue.value = null
  showModelForm.value = true
}
function onEditModel() {
  if (!selectedModel.value) return
  modelFormMode.value = 'update'
  modelFormValue.value = JSON.parse(JSON.stringify(selectedModel.value)) as DeviceModel
  showModelForm.value = true
}
async function onDeleteModel() {
  const m = selectedModel.value
  if (!m || opBusy.value) return
  if (!confirm(`确定删除模型 ${m.model_id}？此操作不可撤销。`)) return
  opBusy.value = true
  const ok = await store.deleteModel(m.model_id)
  opBusy.value = false
  if (!ok) return
  selectedModelId.value = ''
  notify('模型已删除')
}

// ---- 设备面板 ----
const selectedDevid = ref('')
const selectedDevice = computed<DeviceEntry | null>(
  () => store.devices[selectedDevid.value] ?? null,
)
const deviceList = computed(() => Object.values(store.devices).sort((a, b) => a.devid.localeCompare(b.devid)))
const deviceModelOf = (d: DeviceEntry): DeviceModel | null => store.models[d.model] ?? null

const showDeviceForm = ref(false)
function onNewDevice() {
  showDeviceForm.value = true
}
function onEditDevice() {
  showDeviceForm.value = true
}
async function onDeleteDevice() {
  const d = selectedDevice.value
  if (!d || opBusy.value) return
  if (!confirm(`确定删除设备 ${d.devid}？将级联清理归属与实时数据（历史保留）。`)) return
  opBusy.value = true
  const ok = await store.deleteDevice(d.devid)
  opBusy.value = false
  if (!ok) return
  selectedDevid.value = ''
  notify('设备已删除（级联清理完成）')
}

// ---- 标签总览（全设备 effective 平铺 + 过滤）----
interface FlatRow {
  key: string          // <devid>.<tag>
  devid: string
  tag: string
  model: string
  dataType: string
  history: boolean
  deadband: number | null
  unit: string | null
  desc: string | null
  addr: string | null
  overridden: boolean
}
const flatRows = computed<FlatRow[]>(() => {
  const rows: FlatRow[] = []
  for (const dev of deviceList.value) {
    const model = deviceModelOf(dev)
    if (!model) continue // 悬空引用（防御，写入侧已拒）
    for (const t of model.tags) {
      const ov = dev.overrides[t.name]
      const eff = mergeTagProps(baseProps(t), ov)
      rows.push({
        key: `${dev.devid}.${t.name}`,
        devid: dev.devid,
        tag: t.name,
        model: model.model_id,
        dataType: t.data_type,
        history: eff.history,
        deadband: eff.deadband,
        unit: eff.unit,
        desc: eff.desc,
        addr: t.addr,
        overridden: !!ov,
      })
    }
  }
  return rows
})
const tagFilter = ref('')
/** 归档白名单过滤开关（仅看 effective history==true 的行；纯前端 filter，数据源不变）。 */
const historyOnly = ref(false)
/** 任一过滤激活（文本或白名单）——面板计数显示 "n / 总数"。 */
const tagFilterActive = computed(() => historyOnly.value || !!tagFilter.value.trim())
const filteredRows = computed(() => {
  let rows = historyOnly.value ? flatRows.value.filter(r => r.history) : flatRows.value
  const q = tagFilter.value.trim().toLowerCase()
  if (q) rows = rows.filter(r =>
    r.key.toLowerCase().includes(q) || r.model.toLowerCase().includes(q))
  return rows
})

// ---- 批量覆盖（D2-1：glob 批量写覆盖，POST /api/v1/tag/batch）----
const showBatchForm = ref(false)
const batchPattern = ref('')
const batchHistory = ref<HistoryChoice>('keep')
const batchDeadband = ref<DeadbandChoice>('keep')
const batchDeadbandValue = ref(0.5)
const batchBusy = ref(false)
const batchResult = ref<{ matched: number; updated: number } | null>(null)
/** 提交时点的预览计数快照（与后端 matched 对比提示差异）。 */
const batchLastPreview = ref(0)
/** 本地预览匹配计数（flatRows 按与后端同一 glob 算法匹配——仍以后端 matched 为准）。 */
const batchPreviewCount = computed(() => {
  const p = batchPattern.value.trim()
  if (!p) return 0
  return flatRows.value.filter(r => globMatch(p, r.key)).length
})
/** 至少携带一项变更才可提交（全"不变"提交无意义）。 */
const batchDirty = computed(
  () => batchHistory.value !== 'keep' || batchDeadband.value !== 'keep',
)

function onBatchToggle() {
  showBatchForm.value = !showBatchForm.value
  if (showBatchForm.value) batchResult.value = null
}

async function onBatchApply() {
  if (batchBusy.value || !batchDirty.value) return
  const p = batchPattern.value.trim()
  if (!p) return
  // 死区值守卫：负数非法；0.0 与无死区过滤等价（语义陷阱，rest-api.md §4.15）
  // ——显式关本表单不提供，如需走设备覆盖编辑。
  if (batchDeadband.value === 'set' && !(batchDeadbandValue.value > 0)) {
    notify('死区值须为正数——0.0 与无死区过滤等价（如需清除请选「恢复继承」）')
    return
  }
  batchBusy.value = true
  batchResult.value = null
  batchLastPreview.value = batchPreviewCount.value
  const r = await store.tagBatch({
    pattern: p,
    ...buildBatchProps(batchHistory.value, batchDeadband.value, batchDeadbandValue.value),
  })
  batchBusy.value = false
  if (!r) return
  batchResult.value = r
}

// ---- 历史查询（Task 5，P7/P8：head 切换按钮 + 行「查」入口预填展开）----
const showHistoryQuery = ref(false)
const hqDevId = ref('')
const hqTag = ref('')
const histZone = ref<HTMLElement | null>(null)

function onHistoryToggle() {
  showHistoryQuery.value = !showHistoryQuery.value
}

/** 行「查」入口：预填 dev/tag 并展开面板滚动到位（仅 history==true 行提供）。 */
function onHistoryRow(r: FlatRow) {
  hqDevId.value = r.devid
  hqTag.value = r.tag
  showHistoryQuery.value = true
  void nextTick(() => histZone.value?.scrollIntoView({ behavior: 'smooth', block: 'nearest' }))
}

// ---- 后端对账（D2-2：本地 effective 平铺 vs GET /api/v1/tags）----
const reconcileBusy = ref(false)
const reconcileSummary = ref('')
const reconcileDrifts = ref<TagDrift[]>([])
const reconcileDriftTotal = ref(0)

async function onReconcile() {
  if (reconcileBusy.value) return
  reconcileBusy.value = true
  reconcileSummary.value = ''
  reconcileDrifts.value = []
  const remote = await store.fetchEffectiveTags()
  reconcileBusy.value = false
  if (!remote) {
    notify('对账失败：后端 effective 视图拉取失败')
    return
  }
  const local: Record<string, EffectiveProps> = {}
  for (const r of flatRows.value)
    local[r.key] = { history: r.history, deadband: r.deadband, unit: r.unit, desc: r.desc }
  const drifts = reconcileTags(local, remote)
  reconcileDriftTotal.value = drifts.length
  if (!drifts.length) {
    reconcileSummary.value = `对账一致 ${flatRows.value.length} 条（后端 ${Object.keys(remote).length} 条）`
    return
  }
  reconcileDrifts.value = drifts.slice(0, 10)
  const driftedLocal = new Set(drifts.map(d => d.key))
  const consistent = flatRows.value.length - flatRows.value.filter(r => driftedLocal.has(r.key)).length
  reconcileSummary.value = `一致 ${consistent} 条 / 漂移 ${drifts.length} 条（以后端为准核对，可重新加载）`
}

async function onReloadDevreg() {
  if (opBusy.value) return
  opBusy.value = true
  await store.loadAll()
  opBusy.value = false
  notify('已按 devreg 重新加载')
}

// ---- 发现队列（M-4：未建模标签 → 人工确认 adopt 一键建模）----
/** 面板选中条目 key 集合（`<devid>.<tag>`）。 */
const pickedDisc = ref(new Set<string>())
const adoptModelId = ref('')
const adoptDataType = ref('f32')
/** adopt 合法数据类型（后端 AdoptTagReq data_type 为自由串，此为 UI 收敛清单）。 */
const ADOPT_TYPES = ['f32', 'f64', 'i32', 'i64', 'bool', 'string'] as const
/** 逐行类型选择（key = `<devid>.<tag>`；缺省回退全局 adoptDataType）。 */
const adoptTypes = ref<Record<string, string>>({})
const adoptBusy = ref(false)
const adoptNotice = ref('')

const rowTypeOf = (key: string): string => adoptTypes.value[key] ?? adoptDataType.value

function onRowTypeChange(key: string, ev: Event) {
  adoptTypes.value[key] = (ev.target as HTMLSelectElement).value
}

function applyTypeToPicked() {
  for (const k of pickedDisc.value) adoptTypes.value[k] = adoptDataType.value
}

function togglePick(key: string) {
  const s = new Set(pickedDisc.value)
  if (s.has(key)) s.delete(key)
  else s.add(key)
  pickedDisc.value = s
}
/** 按目标模型分组的待采纳建议（同 tag 名去重——REST adopt 同名跳过；
 *  值 = 该 tag 的逐行类型，首个选中行优先）。 */
const pickedByModel = computed(() => {
  const groups = new Map<string, Map<string, string>>()
  for (const item of store.discoveries) {
    const key = `${item.devid}.${item.tag}`
    if (!pickedDisc.value.has(key)) continue
    const dev = store.devices[item.devid]
    const target = adoptModelId.value || dev?.model || ''
    if (!target) continue
    if (!groups.has(target)) groups.set(target, new Map())
    const g = groups.get(target)!
    if (!g.has(item.tag)) g.set(item.tag, rowTypeOf(key))
  }
  return groups
})

async function onAdopt() {
  if (adoptBusy.value || !pickedByModel.value.size) return
  adoptBusy.value = true
  adoptNotice.value = ''
  const merged = { merged: 0, swept: 0 }
  for (const [modelId, tags] of pickedByModel.value) {
    const r = await store.adopt(modelId, [...tags.entries()].map(([name, dt]) => ({
      name, data_type: dt,
    })))
    if (!r) {
      adoptBusy.value = false
      return
    }
    merged.merged += r.merged
    merged.swept += r.swept
  }
  adoptBusy.value = false
  pickedDisc.value = new Set()
  adoptTypes.value = {}
  adoptNotice.value = `采纳完成：并入 ${merged.merged} 个 tag，队列清除 ${merged.swept} 条`
}

async function onClearDisc() {
  if (opBusy.value) return
  if (!confirm('清空发现队列？（未建模数据将重新计数发现）')) return
  opBusy.value = true
  await store.clearDiscoveries()
  opBusy.value = false
  pickedDisc.value = new Set()
  adoptTypes.value = {}
}

async function onClearDiscDevice(devId: string) {
  if (!confirm(`清除设备 ${devId} 的发现条目？`)) return
  await store.clearDiscoveries(devId)
  // 清理该设备行的孤儿类型选择
  for (const k of Object.keys(adoptTypes.value))
    if (k.startsWith(`${devId}.`)) delete adoptTypes.value[k]
}

// ---- 一次性迁移 ----
const migrating = ref(false)
const migrateSummary = ref('')
async function onMigrate() {
  if (migrating.value) return
  if (!confirm('扫描全部实例内联点表并迁移为隐式模型 + 设备注册表条目（幂等，已迁移跳过）？')) return
  migrating.value = true
  migrateSummary.value = ''
  const r = await store.migrateInline()
  migrating.value = false
  if (!r) return
  if (r.failures.length) {
    migrateSummary.value = `迁移完成（有失败）：模型 ${r.converted_models} / 设备 ${r.converted_devices} / 实例 ${r.converted_instances}；失败 ${r.failures.length} 条见控制台`
    console.warn('migrate failures:', r.failures)
  } else {
    migrateSummary.value = `迁移完成：模型 ${r.converted_models} / 设备 ${r.converted_devices} / 实例 ${r.converted_instances}（0 为已全部迁移）`
  }
  await store.loadAll()
}

// ---- 通用反馈 ----
const opBusy = ref(false)
const opNotice = ref('')
let noticeTimer: ReturnType<typeof setTimeout> | null = null
function notify(msg: string) {
  opNotice.value = msg
  if (noticeTimer) clearTimeout(noticeTimer)
  noticeTimer = setTimeout(() => { opNotice.value = '' }, 3000)
}

onMounted(() => {
  void store.loadAll()
  void store.loadDiscoveries()
})

useWorkspacePage(() => ({
  pageSummary: selectedModelId.value
    ? `正在查看模型 ${selectedModelId.value} 的模板定义与引用设备`
    : '设备/标签元数据唯一出生地：模型模板 + 全局设备注册表',
  facts: [
    `模型 ${modelList.value.length} 个`,
    `设备 ${deviceList.value.length} 台`,
    `标签（effective）${flatRows.value.length} 个`,
    `归档白名单 ${flatRows.value.filter(r => r.history).length} 个`,
    `待建模发现 ${store.discoveries.length} 条`,
  ],
}))
</script>

<template>
  <section class="dm-page">
    <!-- 面板①：模型 -->
    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">物模型</p>
          <h3>设备模型</h3>
        </div>
        <div class="head-actions">
          <span class="count">{{ modelList.length }} 个</span>
          <button class="btn-new" @click="onNewModel">+ 新建模型</button>
        </div>
      </div>

      <details class="migrate-zone">
        <summary>一次性迁移：实例内联点表 → 隐式模型 + 设备注册表（幂等）</summary>
        <div class="migrate-body">
          <button class="btn-ghost" :disabled="migrating" @click="onMigrate">
            {{ migrating ? '迁移中…' : '执行迁移' }}
          </button>
          <span v-if="migrateSummary" class="migrate-summary">{{ migrateSummary }}</span>
        </div>
      </details>

      <div class="model-list">
        <button
          v-for="m in modelList" :key="m.model_id"
          class="model-item" :class="{ active: m.model_id === selectedModelId }"
          @click="selectedModelId = m.model_id"
        >
          <span class="mid">{{ m.model_id }}</span>
          <span class="mmeta">{{ m.tags.length }} tag · {{ m.params.length }} 参数 · 引用 {{ refsOf(m.model_id).length }} 台</span>
        </button>
        <p v-if="!modelList.length" class="empty">暂无模型。点「+ 新建模型」或执行一次性迁移。</p>
      </div>

      <div v-if="selectedModel" class="model-detail">
        <div class="detail-grid">
          <div class="detail-card"><span>厂商</span><strong>{{ selectedModel.vendor || '—' }}</strong></div>
          <div class="detail-card"><span>型号</span><strong>{{ selectedModel.model_no || '—' }}</strong></div>
          <div class="detail-card"><span>类别</span><strong>{{ selectedModel.category || '—' }}</strong></div>
          <div class="detail-card"><span>实例化参数</span><strong>{{ paramsSummary }}</strong></div>
        </div>
        <p class="table-label">标签模板（模板默认属性 = 同型号设备基线）</p>
        <table class="mini-table">
          <thead>
            <tr><th>tag</th><th>类型</th><th>参数化地址</th><th>读写</th><th>归档</th><th>死区</th><th>单位</th></tr>
          </thead>
          <tbody>
            <tr v-for="t in selectedModel.tags" :key="t.name">
              <td class="mono">{{ t.name }}</td>
              <td>{{ t.data_type }}</td>
              <td class="mono addr" :title="`占位符: ${(extractParams(t.addr ?? '') ?? []).join(', ') || '无'}`">{{ t.addr || '—' }}</td>
              <td>{{ t.rw === 'r' ? '只读' : '读写' }}</td>
              <td>{{ baseProps(t).history ? '✓' : '—' }}</td>
              <td>{{ baseProps(t).deadband ?? '—' }}</td>
              <td>{{ baseProps(t).unit || '—' }}</td>
            </tr>
          </tbody>
        </table>
        <div class="ops">
          <button class="op op-edit" @click="onEditModel">编辑模板</button>
          <button class="op op-del" :disabled="opBusy" @click="onDeleteModel">删除模型</button>
        </div>
      </div>
    </article>

    <!-- 面板②：设备注册表 -->
    <article class="page-panel">
      <div class="panel-head">
        <div>
          <p class="panel-label">全局实体</p>
          <h3>设备注册表</h3>
        </div>
        <div class="head-actions">
          <span class="count">{{ deviceList.length }} 台</span>
          <button class="btn-new" @click="onNewDevice" :disabled="!modelList.length">+ 建设备</button>
        </div>
      </div>

      <div class="device-list">
        <button
          v-for="d in deviceList" :key="d.devid"
          class="device-item" :class="{ active: d.devid === selectedDevid }"
          @click="selectedDevid = d.devid"
        >
          <span class="did">{{ d.devid }}</span>
          <span class="dmeta">{{ d.model }} · {{ Object.keys(d.overrides).length }} 项覆盖</span>
        </button>
        <p v-if="!deviceList.length" class="empty">暂无设备。先建模型，再点「+ 建设备」。</p>
      </div>

      <div v-if="selectedDevice && deviceModelOf(selectedDevice)" class="device-detail">
        <div class="detail-grid">
          <div class="detail-card"><span>模型</span><strong>{{ selectedDevice.model }}</strong></div>
          <div class="detail-card"><span>实例化参数</span><strong class="mono">{{ Object.entries(selectedDevice.params).map(([k, v]) => `${k}=${v}`).join(' · ') || '（无）' }}</strong></div>
          <div class="detail-card"><span>描述</span><strong>{{ selectedDevice.desc || '—' }}</strong></div>
        </div>
        <p class="table-label">effective 标签（模板默认 ∪ 覆盖；<em>覆盖</em> 列显示三态结果）</p>
        <table class="mini-table">
          <thead>
            <tr><th>tag</th><th>归档</th><th>死区</th><th>单位</th><th>描述</th><th>来源</th></tr>
          </thead>
          <tbody>
            <tr v-for="t in deviceModelOf(selectedDevice)!.tags" :key="t.name">
              <td class="mono">{{ t.name }}</td>
              <td>{{ triStateText(baseProps(t).history ? '✓' : '—', selectedDevice.overrides[t.name]?.history === undefined ? undefined : String(selectedDevice.overrides[t.name]?.history), '—') }}</td>
              <td>{{ triStateText(String(baseProps(t).deadband ?? '—'), selectedDevice.overrides[t.name]?.deadband, '（显式关）') }}</td>
              <td>{{ triStateText(baseProps(t).unit ?? '—', selectedDevice.overrides[t.name]?.unit ?? undefined, '（已清）') }}</td>
              <td>{{ triStateText(baseProps(t).desc ?? '—', selectedDevice.overrides[t.name]?.desc ?? undefined, '（已清）') }}</td>
              <td>
                <span class="src" :class="{ ov: !!selectedDevice.overrides[t.name] }">
                  {{ selectedDevice.overrides[t.name] ? '覆盖' : '模板' }}
                </span>
              </td>
            </tr>
          </tbody>
        </table>
        <div class="ops">
          <button class="op op-edit" @click="onEditDevice">编辑设备/覆盖</button>
          <button class="op op-del" :disabled="opBusy" @click="onDeleteDevice">删除设备</button>
        </div>
      </div>
    </article>

    <!-- 面板③：标签总览 -->
    <article class="page-panel wide">
      <div class="panel-head">
        <div>
          <p class="panel-label">effective 视图</p>
          <h3>标签总览（{{ filteredRows.length }}{{ tagFilterActive ? ` / ${flatRows.length}` : '' }}）</h3>
        </div>
        <div class="head-actions">
          <label class="hist-toggle"><input type="checkbox" v-model="historyOnly" />仅看归档白名单</label>
          <input v-model="tagFilter" class="filter" placeholder="过滤 <devid>.<tag> 或模型…" >
          <button class="btn-ghost" @click="onBatchToggle">{{ showBatchForm ? '收起批量' : '批量覆盖' }}</button>
          <button class="btn-ghost" :disabled="reconcileBusy" @click="onReconcile">
            {{ reconcileBusy ? '对账中…' : '后端对账' }}
          </button>
          <button class="btn-ghost" @click="onHistoryToggle">
            {{ showHistoryQuery ? '收起查询' : '历史查询' }}
          </button>
        </div>
      </div>

      <!-- D2-1：glob 批量覆盖表单（head-actions「批量覆盖」展开）-->
      <div v-if="showBatchForm" class="batch-zone">
        <div class="batch-row">
          <label class="disc-field batch-pattern">
            <span>glob 模式（匹配 <em>&lt;devid&gt;.&lt;tag&gt;</em> 全键，<em>*</em> 通配）</span>
            <input v-model="batchPattern" class="mono" placeholder="line1.*.temp*" />
          </label>
          <label class="disc-field">
            <span>归档白名单</span>
            <select v-model="batchHistory">
              <option value="keep">不变</option>
              <option value="on">开（入白名单）</option>
              <option value="off">关</option>
            </select>
          </label>
          <label class="disc-field">
            <span>死区</span>
            <select v-model="batchDeadband">
              <option value="keep">不变</option>
              <option value="set">设值…</option>
              <option value="inherit">恢复继承</option>
            </select>
          </label>
          <label v-if="batchDeadband === 'set'" class="disc-field">
            <span>死区值（正数）</span>
            <input v-model.number="batchDeadbandValue" type="number" min="0" step="0.1" />
          </label>
        </div>
        <p class="batch-hint">
          预览匹配 <strong>{{ batchPreviewCount }}</strong> 条（本地按同一 glob 语义计数，以后端返回为准）。
          「恢复继承」提交 <span class="mono">deadband=null</span>；显式关死区（0.0，与无死区过滤等价）本表单不提供——如需请走设备覆盖编辑。
        </p>
        <div class="batch-row">
          <button
            class="btn-adopt" :disabled="!batchPattern.trim() || !batchDirty || batchBusy"
            @click="onBatchApply"
          >{{ batchBusy ? '提交中…' : '提交批量覆盖' }}</button>
          <p v-if="batchResult" class="op-ok">
            后端匹配 {{ batchResult.matched }} 条 / 写入 {{ batchResult.updated }} 条<template v-if="batchResult.matched !== batchLastPreview">（注意：与预览 {{ batchLastPreview }} 条不符，以后端为准）</template>
          </p>
        </div>
      </div>

      <!-- D2-2：effective 后端对账结果（漂移时列出前 N 条 + 重新加载入口）-->
      <div v-if="reconcileSummary" class="reconcile-zone" :class="{ drift: reconcileDrifts.length }">
        <p :class="reconcileDrifts.length ? 'op-error' : 'op-ok'">{{ reconcileSummary }}</p>
        <table v-if="reconcileDrifts.length" class="mini-table">
          <thead>
            <tr><th>标签</th><th>属性</th><th>本地（devreg）</th><th>后端（GET /tags）</th></tr>
          </thead>
          <tbody>
            <tr v-for="(d, i) in reconcileDrifts" :key="`${d.key}.${d.field}.${i}`">
              <td class="mono">{{ d.key }}</td>
              <td>{{ d.field }}</td>
              <td class="mono">{{ d.local }}</td>
              <td class="mono">{{ d.remote }}</td>
            </tr>
          </tbody>
        </table>
        <p v-if="reconcileDriftTotal > reconcileDrifts.length" class="batch-hint">
          仅列前 {{ reconcileDrifts.length }} 条（共 {{ reconcileDriftTotal }} 条漂移）
        </p>
        <button v-if="reconcileDrifts.length" class="btn-ghost" :disabled="opBusy" @click="onReloadDevreg">
          以 devreg 重新加载
        </button>
      </div>
      <table class="mini-table flat">
        <thead>
          <tr><th>标签</th><th>模型</th><th>类型</th><th>地址模板</th><th>归档</th><th>死区</th><th>单位</th><th>来源</th><th>查询</th></tr>
        </thead>
        <tbody>
          <tr v-for="r in filteredRows" :key="r.key">
            <td class="mono">{{ r.key }}</td>
            <td>{{ r.model }}</td>
            <td>{{ r.dataType }}</td>
            <td class="mono addr">{{ r.addr || '—' }}</td>
            <td>{{ r.history ? '✓' : '—' }}</td>
            <td>{{ r.deadband ?? '—' }}</td>
            <td>{{ r.unit || '—' }}</td>
            <td><span class="src" :class="{ ov: r.overridden }">{{ r.overridden ? '覆盖' : '模板' }}</span></td>
            <td>
              <button v-if="r.history" class="link-query" @click="onHistoryRow(r)">查</button>
              <span v-else class="src">—</span>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-if="!filteredRows.length" class="empty">
        {{ flatRows.length && tagFilterActive ? '无匹配标签（调整过滤条件或关闭「仅看归档白名单」）。' : '无标签（先建模 + 建设备，或执行迁移）。' }}
      </p>

      <!-- 历史查询面板（Task 5）：head 按钮展开；行「查」预填直达 -->
      <div v-if="showHistoryQuery" ref="histZone" class="hist-query-zone">
        <HistoryQuery :dev-id="hqDevId" :tag="hqTag" />
      </div>
    </article>

    <!-- 面板④：发现队列（M-4：未建模标签 → adopt 一键建模）-->
    <article class="page-panel wide">
      <div class="panel-head">
        <div>
          <p class="panel-label">建模工作流入口</p>
          <h3>发现队列（{{ store.discoveries.length }}）</h3>
        </div>
        <div class="head-actions">
          <button class="btn-ghost" :disabled="!store.discoveries.length || opBusy" @click="onClearDisc">清空队列</button>
        </div>
      </div>
      <p v-if="!store.discoveries.length" class="empty">
        队列为空——出口数据全部已建模（或暂无脏数据）。数据流中出现未建模标签时会在此列出。
      </p>
      <template v-else>
        <div class="disc-toolbar">
          <label class="disc-field">
            <span>目标模型（缺省 = 条目设备所属模型）</span>
            <select v-model="adoptModelId">
              <option value="">（设备所属模型）</option>
              <option v-for="m in modelList" :key="m.model_id" :value="m.model_id">{{ m.model_id }}</option>
            </select>
          </label>
          <label class="disc-field">
            <span>批量设类型（应用到所选行）</span>
            <div class="type-apply">
              <select v-model="adoptDataType">
                <option v-for="t in ADOPT_TYPES" :key="t" :value="t">{{ t }}</option>
              </select>
              <button class="btn-ghost" :disabled="!pickedDisc.size" @click="applyTypeToPicked">应用</button>
            </div>
          </label>
          <button
            class="btn-adopt" :disabled="!pickedDisc.size || adoptBusy"
            @click="onAdopt"
          >{{ adoptBusy ? '采纳中…' : `采纳所选（${pickedByModel.size} 模型）` }}</button>
        </div>
        <table class="mini-table flat">
          <thead>
            <tr><th></th><th>标签</th><th>类型</th><th>所属模型</th><th>计数</th><th>最近值</th><th>最近时间</th><th></th></tr>
          </thead>
          <tbody>
            <tr v-for="d in store.discoveries" :key="`${d.devid}.${d.tag}`" :class="{ picked: pickedDisc.has(`${d.devid}.${d.tag}`) }">
              <td><input
                type="checkbox"
                :checked="pickedDisc.has(`${d.devid}.${d.tag}`)"
                @change="togglePick(`${d.devid}.${d.tag}`)"
              /></td>
              <td class="mono">{{ d.devid }}.{{ d.tag }}</td>
              <td>
                <select
                  class="row-type"
                  :value="rowTypeOf(`${d.devid}.${d.tag}`)"
                  @change="onRowTypeChange(`${d.devid}.${d.tag}`, $event)"
                >
                  <option v-for="t in ADOPT_TYPES" :key="t" :value="t">{{ t }}</option>
                </select>
              </td>
              <td>{{ store.devices[d.devid]?.model || '（未注册设备）' }}</td>
              <td>{{ d.count }}</td>
              <td class="mono">{{ d.sample_value }}</td>
              <td class="mono">{{ new Date(d.last_ts * 1000).toLocaleString() }}</td>
              <td><button class="link-del" @click="onClearDiscDevice(d.devid)">清此设备</button></td>
            </tr>
          </tbody>
        </table>
      </template>
      <p v-if="adoptNotice" class="op-ok">{{ adoptNotice }}</p>
    </article>

    <p v-if="store.error" class="op-error">{{ store.error }}</p>
    <p v-if="opNotice" class="op-ok">{{ opNotice }}</p>

    <ModelForm
      v-if="showModelForm"
      :mode="modelFormMode"
      :model-value="modelFormValue"
      @close="showModelForm = false"
      @saved="(id: string) => { selectedModelId = id }"
    />
    <DeviceForm
      v-if="showDeviceForm"
      :models="modelList"
      :model-value="selectedDevice"
      @close="showDeviceForm = false"
      @saved="(id: string) => { selectedDevid = id }"
    />
  </section>
</template>

<style scoped>
.dm-page {
  display: grid;
  gap: 18px;
  grid-template-columns: minmax(380px, 1fr) minmax(380px, 1fr);
}
.page-panel.wide { grid-column: 1 / -1; }

.page-panel {
  border-radius: var(--radius-panel);
  padding: 22px;
  background: var(--panel-bg);
  border: 1px solid var(--panel-border);
  box-shadow: var(--shadow-panel);
  position: relative;
  overflow: hidden;
}
.panel-head {
  display: flex; justify-content: space-between; gap: 12px;
  align-items: end; margin-bottom: 14px;
}
.panel-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }
.head-actions { display: flex; align-items: center; gap: 10px; }
.count { font-size: 12px; color: var(--color-ink-dim); }
.btn-new {
  padding: 6px 14px; border-radius: 10px;
  border: 1px solid var(--color-primary); background: var(--color-primary);
  color: white; font-size: 12px; cursor: pointer;
}
.btn-new:disabled { opacity: 0.4; cursor: not-allowed; }
.btn-ghost {
  padding: 6px 14px; border-radius: 10px;
  border: 1px solid var(--card-border); background: var(--card-bg);
  color: var(--color-ink-secondary); font-size: 12px; cursor: pointer;
}
.panel-label {
  margin: 0 0 8px; font-size: 11px; letter-spacing: 0.18em;
  text-transform: uppercase; color: var(--color-ink-dim);
}

.migrate-zone {
  margin-bottom: 12px; border: 1px dashed var(--color-primary);
  border-radius: 10px;
}
.migrate-zone summary {
  cursor: pointer; font-size: 12px; color: var(--color-primary);
  padding: 8px 12px; user-select: none; list-style: none;
}
.migrate-zone summary::-webkit-details-marker { display: none; }
.migrate-zone summary::before { content: "⇩ "; }
.migrate-body { display: flex; gap: 10px; align-items: center; padding: 8px 12px; flex-wrap: wrap; }
.migrate-summary { font-size: 12px; color: var(--color-ink-secondary); }

.model-list, .device-list {
  display: flex; flex-direction: column; gap: 8px;
  max-height: 34vh; overflow-y: auto; margin-bottom: 14px;
}
.model-item, .device-item {
  display: flex; flex-direction: column; gap: 4px;
  padding: 10px 12px; border-radius: 12px; cursor: pointer; font-size: 13px;
  border: 1px solid var(--card-border); background: var(--card-bg);
  text-align: left;
}
.model-item:hover, .device-item:hover { background: var(--color-surface-hover); }
.model-item.active, .device-item.active {
  background: linear-gradient(135deg, color-mix(in srgb, var(--color-primary) 18%, white), var(--card-bg));
  border-color: var(--color-divider-strong);
}
.mid, .did { font-family: Consolas, monospace; font-weight: 600; }
.mmeta, .dmeta { color: var(--color-ink-muted); font-size: 11px; }
.empty { color: var(--color-ink-dim); padding: 14px 0; font-size: 13px; }

.detail-grid { display: grid; gap: 10px; grid-template-columns: repeat(2, minmax(0, 1fr)); margin-bottom: 12px; }
.detail-card {
  display: grid; gap: 4px; padding: 12px; border-radius: var(--radius-card);
  background: var(--card-bg); border: 1px solid var(--card-border);
}
.detail-card span { color: var(--color-ink-dim); font-size: 11px; }
.detail-card strong { font-family: Consolas, monospace; font-size: 12px; word-break: break-all; }

.table-label { margin: 0 0 6px; font-size: 11px; color: var(--color-ink-muted); }
.table-label em { font-style: normal; color: var(--color-ink-dim); }
.mini-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.mini-table th {
  text-align: left; font-weight: 500; color: var(--color-ink-dim);
  padding: 6px 8px; border-bottom: 1px solid var(--color-divider);
  font-size: 11px; white-space: nowrap;
}
.mini-table td {
  padding: 6px 8px; border-bottom: 1px solid var(--color-divider-soft, rgba(0,0,0,0.05));
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 180px;
}
.mini-table td.addr { max-width: 220px; }
.mono { font-family: Consolas, monospace; }
.src { color: var(--color-ink-dim); font-size: 11px; }
.src.ov { color: var(--color-primary); font-weight: 600; }

.filter {
  padding: 6px 12px; border-radius: 8px; font-size: 12px;
  border: 1px solid var(--card-border); background: var(--card-bg);
  color: inherit; min-width: 220px;
}

/* 面板③归档白名单过滤开关 */
.hist-toggle {
  display: inline-flex; align-items: center; gap: 6px; white-space: nowrap;
  font-size: 12px; color: var(--color-ink-secondary); cursor: pointer; user-select: none;
}
.hist-toggle input { cursor: pointer; accent-color: var(--color-primary); }

/* 面板③批量覆盖（D2-1）与后端对账（D2-2） */
.batch-zone, .reconcile-zone {
  margin-bottom: 12px; padding: 12px; border-radius: 10px;
  border: 1px dashed var(--color-primary); background: var(--card-bg);
}
.reconcile-zone.drift { border-color: var(--color-danger); }
.reconcile-zone .op-error, .reconcile-zone .op-ok { margin: 0 0 8px; }
.reconcile-zone .btn-ghost { margin-top: 8px; }
.batch-row { display: flex; gap: 12px; align-items: end; flex-wrap: wrap; }
.batch-pattern input { width: 240px; }
.batch-hint {
  margin: 8px 0 0; font-size: 12px; color: var(--color-ink-muted); line-height: 1.6;
}
.batch-hint em { font-style: normal; color: var(--color-ink-dim); }
.batch-hint .mono { font-size: 11px; }

/* 发现队列面板 */
.disc-toolbar {
  display: flex; gap: 12px; align-items: end; flex-wrap: wrap; margin-bottom: 10px;
}
.disc-field { display: flex; flex-direction: column; gap: 4px; font-size: 11px; }
.disc-field span { color: var(--color-ink-dim); }
.disc-field select, .disc-field input {
  padding: 6px 10px; border-radius: 8px; font-size: 12px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: inherit;
}
.disc-field input { width: 180px; }
.btn-adopt {
  padding: 7px 16px; border-radius: 8px; font-size: 12px;
  border: 1px solid var(--color-primary); background: var(--color-primary);
  color: white; cursor: pointer;
}
.btn-adopt:disabled { opacity: 0.4; cursor: not-allowed; }
.type-apply { display: flex; gap: 6px; align-items: center; }
.row-type {
  padding: 4px 8px; border-radius: 6px; font-size: 11px; font-family: Consolas, monospace;
  border: 1px solid var(--card-border); background: var(--card-bg); color: inherit;
  cursor: pointer;
}
tr.picked td { background: color-mix(in srgb, var(--color-primary) 8%, transparent); }
.link-del {
  background: transparent; border: none; cursor: pointer;
  color: var(--color-ink-dim); font-size: 11px; text-decoration: underline;
}
.link-del:hover { color: var(--color-danger); }

/* 面板③历史查询（Task 5）：行「查」入口 + 查询面板挂载位 */
.link-query {
  background: transparent; border: none; cursor: pointer;
  color: var(--color-primary); font-size: 11px; text-decoration: underline;
}
.link-query:hover { text-decoration: none; opacity: 0.8; }
.hist-query-zone { margin-top: 12px; }

.ops { display: flex; gap: 8px; margin-top: 12px; flex-wrap: wrap; }
.op {
  padding: 8px 18px; border-radius: 8px; font-size: 13px; font-weight: 500;
  border: 1px solid transparent; cursor: pointer;
}
.op:disabled { opacity: 0.4; cursor: not-allowed; }
.op-edit { background: var(--color-primary); color: white; }
.op-del { background: transparent; color: var(--color-danger); border-color: var(--color-danger); }
.op-error { color: var(--color-danger); font-size: 12px; margin: 4px 0 0; }
.op-ok { color: var(--color-success); font-size: 12px; margin: 4px 0 0; }

@media (max-width: 1100px) { .dm-page { grid-template-columns: 1fr; } }
</style>
