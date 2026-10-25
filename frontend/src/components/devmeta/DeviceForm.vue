<script setup lang="ts">
// frontend/src/components/devmeta/DeviceForm.vue
// 设备编辑弹窗（新建/编辑共用）：选模型 + 实例化参数（params 齐套）+
// per-tag 三态覆盖（undefined=继承 / 显式关 / 值覆盖；空串=清除描述与单位）。
import { computed, reactive, ref } from 'vue'
import { useDevregStore } from '../../stores/devreg'
import { validateDevice, type DeviceEntry, type DeviceModel, type TagOverride } from '../../lib/devmeta'

const props = defineProps<{
  models: DeviceModel[]
  modelValue: DeviceEntry | null
}>()
const emit = defineEmits<{ close: []; saved: [devid: string] }>()

const store = useDevregStore()
const saving = ref(false)
const error = ref('')

function blank(): DeviceEntry {
  return { devid: '', model: props.models[0]?.model_id ?? '', params: {}, overrides: {}, desc: null }
}
const form = reactive<DeviceEntry>(props.modelValue ? JSON.parse(JSON.stringify(props.modelValue)) as DeviceEntry : blank())

const selectedModel = computed<DeviceModel | null>(
  () => props.models.find(m => m.model_id === form.model) ?? null,
)

/** params 编辑副本（对象响应式增量更新困难，用扁平字符串字段）。 */
const paramValues = ref<Record<string, string>>({ ...form.params })
function setParam(k: string, v: string) {
  paramValues.value[k] = v
}
// 初始化：模型声明参数缺的补空串（表单可见）。
if (selectedModel.value)
  for (const p of selectedModel.value.params)
    if (!(p in paramValues.value)) paramValues.value[p] = ''

/** 覆盖编辑副本：tag → 三态字段（字符串编辑态，提交时规范化）。 */
interface OvEdit {
  enabled: boolean
  history: boolean | null   // null=未勾选（继承）不可表达——用 checkbox 三态转文字太绕，改用下拉
  deadband: string          // ''=继承 'off'=显式关 数值=覆盖
  desc: string              // ''=继承（占位显示模板值）；'<clear>' 语义不需要——空覆盖值即清除
  unit: string
}
const ovEdits = reactive<Record<string, OvEdit>>({})
function ovOf(tag: string): OvEdit {
  if (!ovEdits[tag]) {
    const src = form.overrides[tag]
    ovEdits[tag] = {
      enabled: !!src,
      history: src?.history ?? null,
      deadband: src?.deadband === undefined ? '' : src?.deadband === null ? 'off' : String(src.deadband),
      desc: src?.desc ?? '',
      unit: src?.unit ?? '',
    }
  }
  return ovEdits[tag]
}

const errs = computed(() => {
  if (!selectedModel.value) return ['请选择模型']
  return validateDevice({ ...form, params: paramValues.value }, selectedModel.value)
})

async function onSubmit() {
  if (!selectedModel.value) return
  store.clearError()
  saving.value = true
  error.value = ''
  const overrides: Record<string, TagOverride> = {}
  for (const t of selectedModel.value.tags) {
    const e = ovEdits[t.name]
    if (!e?.enabled) continue // 关闭覆盖 = 继承模板（该 tag 不进 overrides）
    const ov: TagOverride = {}
    if (e.history !== null) ov.history = e.history
    if (e.deadband !== '') {
      ov.deadband = e.deadband === 'off' ? null : Number(e.deadband)
    }
    if (e.desc !== '') ov.desc = e.desc
    if (e.unit !== '') ov.unit = e.unit
    if (Object.keys(ov).length) overrides[t.name] = ov
  }
  const params: Record<string, string> = {}
  for (const p of selectedModel.value.params) {
    const v = (paramValues.value[p] ?? '').trim()
    if (v) params[p] = v
  }
  const payload: DeviceEntry = {
    devid: form.devid.trim(),
    model: form.model,
    params,
    overrides,
    desc: form.desc?.trim() || null,
  }
  const ok = await store.saveDevice(payload)
  saving.value = false
  if (!ok) {
    error.value = store.error || '保存失败'
    return
  }
  emit('saved', payload.devid)
  emit('close')
}
</script>

<template>
  <div class="modal-mask" @click.self="emit('close')">
    <div class="modal">
      <div class="modal-head">
        <h3>{{ modelValue ? `编辑设备 ${form.devid}` : '新建设备（设备注册表）' }}</h3>
        <button class="x" @click="emit('close')">✕</button>
      </div>

      <div class="form-body">
        <div class="grid2">
          <label>
            <span>devid（全局唯一；= rtdata 的设备 ID，不含空白与 . * { } ,）</span>
            <input v-model="form.devid" class="mono" placeholder="vx_meter_01">
          </label>
          <label>
            <span>模型（决定标签模板与必填参数）</span>
            <select v-model="form.model">
              <option v-for="m in models" :key="m.model_id" :value="m.model_id">
                {{ m.model_id }}（{{ m.tags.length }} tag）
              </option>
            </select>
          </label>
          <label class="span2">
            <span>描述</span>
            <input v-model="form.desc" placeholder="1号配电房电表">
          </label>
        </div>

        <template v-if="selectedModel">
          <template v-if="selectedModel.params.length">
            <p class="table-label">实例化参数（模板地址 {name} 引用；全部必填）</p>
            <div class="params-row">
              <label v-for="p in selectedModel.params" :key="p" class="param">
                <span class="mono">{( p )}</span>
                <input
                  :value="paramValues[p] ?? ''" class="mono"
                  :placeholder="`如 100`" @input="setParam(p, ($event.target as HTMLInputElement).value)"
                >
              </label>
            </div>
          </template>

          <p class="table-label">标签覆盖（三态：不勾 = 继承模板；勾选后填值为覆盖，留空/关 = 显式清除）</p>
          <table class="mini-table">
            <thead>
              <tr>
                <th>tag</th><th>模板默认</th><th>覆盖</th><th>归档</th><th>死区</th><th>单位</th><th>描述</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="t in selectedModel.tags" :key="t.name">
                <td class="mono">{{ t.name }}</td>
                <td class="tmpl">
                  归档{{ t.props.history ? '✓' : '—' }} ·
                  死区{{ t.props.deadband ?? '—' }} ·
                  {{ t.props.unit || t.unit || '无单位' }}
                </td>
                <td><input v-model="ovOf(t.name).enabled" type="checkbox" class="cb"></td>
                <td>
                  <select v-if="ovOf(t.name).enabled" v-model="ovOf(t.name).history" class="cell w70">
                    <option :value="null">继承</option>
                    <option :value="true">开</option>
                    <option :value="false">关</option>
                  </select>
                  <span v-else class="dim">—</span>
                </td>
                <td>
                  <input
                    v-if="ovOf(t.name).enabled" v-model="ovOf(t.name).deadband"
                    class="cell mono w80" placeholder="继承；off=关；数值=覆盖"
                  >
                  <span v-else class="dim">—</span>
                </td>
                <td>
                  <input v-if="ovOf(t.name).enabled" v-model="ovOf(t.name).unit" class="cell w70" placeholder="继承">
                  <span v-else class="dim">—</span>
                </td>
                <td>
                  <input v-if="ovOf(t.name).enabled" v-model="ovOf(t.name).desc" class="cell" placeholder="继承">
                  <span v-else class="dim">—</span>
                </td>
              </tr>
            </tbody>
          </table>
        </template>

        <ul v-if="errs.length" class="errs">
          <li v-for="e in errs" :key="e">{{ e }}</li>
        </ul>
        <p v-if="error" class="errs-single">{{ error }}</p>
      </div>

      <div class="modal-foot">
        <button class="op-cancel" @click="emit('close')">取消</button>
        <button class="op-save" :disabled="saving || errs.length > 0 || !form.devid.trim()" @click="onSubmit">
          {{ saving ? '保存中…' : '保存设备' }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.modal-mask {
  position: fixed; inset: 0; background: rgba(15, 23, 42, 0.45);
  display: flex; align-items: center; justify-content: center; z-index: 60;
}
.modal {
  width: min(900px, 94vw); max-height: 90vh; overflow-y: auto;
  border-radius: 16px; background: var(--panel-bg);
  border: 1px solid var(--panel-border); box-shadow: var(--shadow-panel);
  padding: 20px 22px;
}
.modal-head { display: flex; justify-content: space-between; align-items: center; }
.modal-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }
.x { border: none; background: transparent; font-size: 14px; cursor: pointer; color: var(--color-ink-dim); }
.form-body { display: flex; flex-direction: column; gap: 12px; margin-top: 12px; }
.grid2 { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; }
.span2 { grid-column: 1 / -1; }
label { display: flex; flex-direction: column; gap: 4px; }
label span { font-size: 11px; color: var(--color-ink-dim); }
input, select {
  padding: 7px 10px; border-radius: 8px; font-size: 13px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: inherit;
}
.table-label { margin: 0; font-size: 11px; color: var(--color-ink-muted); }
.params-row { display: flex; gap: 12px; flex-wrap: wrap; }
.param { min-width: 200px; }
.mono { font-family: Consolas, monospace; }

.mini-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.mini-table th {
  text-align: left; font-weight: 500; color: var(--color-ink-dim);
  padding: 5px 6px; border-bottom: 1px solid var(--color-divider); font-size: 11px;
}
.mini-table td { padding: 4px 6px; border-bottom: 1px solid rgba(0,0,0,0.05); }
.tmpl { color: var(--color-ink-dim); font-size: 11px; white-space: nowrap; }
.cell { width: 100%; padding: 5px 8px !important; font-size: 12px; }
.w70 { max-width: 76px; } .w80 { max-width: 96px; }
.cb { accent-color: var(--color-primary); }
.dim { color: var(--color-ink-muted); font-size: 11px; }

.errs { margin: 0; padding-left: 18px; color: var(--color-danger); font-size: 12px; }
.errs li { margin: 2px 0; }
.errs-single { margin: 0; color: var(--color-danger); font-size: 12px; }

.modal-foot { display: flex; justify-content: flex-end; gap: 10px; margin-top: 16px; }
.op-cancel {
  padding: 8px 18px; border-radius: 8px; font-size: 13px; cursor: pointer;
  border: 1px solid var(--color-divider); background: transparent;
  color: var(--color-ink-secondary);
}
.op-save {
  padding: 8px 18px; border-radius: 8px; font-size: 13px; cursor: pointer;
  border: none; background: var(--color-primary); color: white; font-weight: 500;
}
.op-save:disabled { opacity: 0.4; cursor: not-allowed; }
</style>
