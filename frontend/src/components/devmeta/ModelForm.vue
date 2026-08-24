<script setup lang="ts">
// frontend/src/components/devmeta/ModelForm.vue
// 模型编辑弹窗（新建/更新共用）：基本信息 + 参数声明 + 标签模板表格行内编辑。
// 提交前跑 validateModel 镜像校验（后端仍是权威，400 会回显）。
import { computed, reactive, ref, watch } from 'vue'
import { useDevregStore } from '../../stores/devreg'
import { validateModel, type DeviceModel, type TagTemplate } from '../../lib/devmeta'

const props = defineProps<{
  mode: 'create' | 'update'
  modelValue: DeviceModel | null
}>()
const emit = defineEmits<{ close: []; saved: [modelId: string] }>()

const store = useDevregStore()
const saving = ref(false)
const error = ref('')

function blank(): DeviceModel {
  return {
    model_id: '', vendor: null, model_no: null, category: null, desc: null,
    params: [], tags: [],
  }
}
function blankTag(): TagTemplate {
  return {
    name: '', data_type: 'f32', desc: null, unit: null,
    addr: null, rw: 'r', props: { history: false, deadband: null, desc: null, unit: null },
  }
}

const form = reactive<DeviceModel>(props.modelValue ? JSON.parse(JSON.stringify(props.modelValue)) as DeviceModel : blank())
/** 参数声明的编辑态（逗号分隔字符串 → 数组）。 */
const paramsText = ref(form.params.join(', '))

watch(paramsText, v => {
  form.params = v.split(/[,，\s]+/).map(s => s.trim()).filter(Boolean)
})

/** deadband 编辑态：字符串 ↔ number|null（空串 = 无死区）。 */
function deadbandText(t: TagTemplate): string {
  return t.props.deadband === null || t.props.deadband === undefined ? '' : String(t.props.deadband)
}
function setDeadband(t: TagTemplate, v: string) {
  const n = Number(v)
  t.props.deadband = v.trim() === '' || Number.isNaN(n) ? null : n
}

const errs = computed(() => (saving.value ? [] : validateModel(form)))

function addTag() { form.tags.push(blankTag()) }
function removeTag(i: number) { form.tags.splice(i, 1) }

async function onSubmit() {
  store.clearError()
  saving.value = true
  error.value = ''
  const payload = JSON.parse(JSON.stringify(form)) as DeviceModel
  // 规范化：空串 → null（wire 干净）
  for (const t of payload.tags) {
    t.desc = t.desc?.trim() || null
    t.unit = t.unit?.trim() || null
    t.addr = t.addr?.trim() || null
  }
  payload.vendor = payload.vendor?.trim() || null
  payload.model_no = payload.model_no?.trim() || null
  payload.category = payload.category?.trim() || null
  payload.desc = payload.desc?.trim() || null
  const ok = props.mode === 'create'
    ? await store.createModel(payload)
    : await store.updateModel(payload)
  saving.value = false
  if (!ok) {
    error.value = store.error || '保存失败'
    return
  }
  emit('saved', payload.model_id)
  emit('close')
}
</script>

<template>
  <div class="modal-mask" @click.self="emit('close')">
    <div class="modal">
      <div class="modal-head">
        <h3>{{ mode === 'create' ? '新建设备模型' : `编辑模型 ${form.model_id}` }}</h3>
        <button class="x" @click="emit('close')">✕</button>
      </div>

      <div class="form-body">
        <div class="grid2">
          <label>
            <span>model_id（唯一，不可含空白与 . * { } ,）</span>
            <input v-model="form.model_id" :disabled="mode === 'update'" class="mono" placeholder="vx_dtz341">
          </label>
          <label>
            <span>实例化参数（逗号分隔；地址模板用 {name} 引用）</span>
            <input v-model="paramsText" class="mono" placeholder="base, db">
          </label>
          <label><span>厂商</span><input v-model="form.vendor" placeholder="威胜"></label>
          <label><span>型号</span><input v-model="form.model_no" placeholder="DTZ34Z"></label>
          <label><span>类别</span><input v-model="form.category" placeholder="电表"></label>
          <label><span>描述</span><input v-model="form.desc" placeholder="三相电能表"></label>
        </div>

        <div class="tags-head">
          <p class="table-label">标签模板（{{ form.tags.length }}）——模板默认属性 = 同型号设备基线</p>
          <button class="btn-add" @click="addTag">+ 标签</button>
        </div>
        <table class="mini-table">
          <thead>
            <tr>
              <th>tag 名</th><th>类型</th><th>参数化地址</th><th>读写</th>
              <th>归档</th><th>死区</th><th>单位</th><th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(t, i) in form.tags" :key="i">
              <td><input v-model="t.name" class="cell mono" placeholder="ua"></td>
              <td><input v-model="t.data_type" class="cell mono w70" placeholder="f32"></td>
              <td><input v-model="t.addr" class="cell mono" placeholder="holding:{base}:f32"></td>
              <td>
                <select v-model="t.rw" class="cell w60">
                  <option value="r">只读</option>
                  <option value="rw">读写</option>
                </select>
              </td>
              <td><input v-model="t.props.history" type="checkbox" class="cb"></td>
              <td><input :value="deadbandText(t)" class="cell mono w70" placeholder="（无）" @input="setDeadband(t, ($event.target as HTMLInputElement).value)"></td>
              <td><input v-model="t.unit" class="cell w60" placeholder="V"></td>
              <td><button class="btn-rm" @click="removeTag(i)">✕</button></td>
            </tr>
          </tbody>
        </table>
        <p v-if="!form.tags.length" class="empty">尚未定义标签模板（模型至少含 1 个标签才有采集意义，可先保存空模板）。</p>

        <ul v-if="errs.length" class="errs">
          <li v-for="e in errs" :key="e">{{ e }}</li>
        </ul>
        <p v-if="error" class="errs-single">{{ error }}</p>
      </div>

      <div class="modal-foot">
        <button class="op-cancel" @click="emit('close')">取消</button>
        <button class="op-save" :disabled="saving || errs.length > 0" @click="onSubmit">
          {{ saving ? '保存中…' : (mode === 'create' ? '创建' : '保存更新') }}
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
  width: min(960px, 94vw); max-height: 90vh; overflow-y: auto;
  border-radius: 16px; background: var(--panel-bg);
  border: 1px solid var(--panel-border); box-shadow: var(--shadow-panel);
  padding: 20px 22px;
}
.modal-head { display: flex; justify-content: space-between; align-items: center; }
.modal-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }
.x {
  border: none; background: transparent; font-size: 14px; cursor: pointer;
  color: var(--color-ink-dim);
}
.form-body { display: flex; flex-direction: column; gap: 14px; margin-top: 12px; }
.grid2 { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; }
label { display: flex; flex-direction: column; gap: 4px; }
label span { font-size: 11px; color: var(--color-ink-dim); }
input, select {
  padding: 7px 10px; border-radius: 8px; font-size: 13px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: inherit;
}
input:disabled { opacity: 0.55; }

.tags-head { display: flex; justify-content: space-between; align-items: center; }
.table-label { margin: 0; font-size: 11px; color: var(--color-ink-muted); }
.btn-add {
  padding: 5px 12px; border-radius: 8px; font-size: 12px; cursor: pointer;
  border: 1px solid var(--color-primary); color: var(--color-primary); background: transparent;
}
.mini-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.mini-table th {
  text-align: left; font-weight: 500; color: var(--color-ink-dim);
  padding: 5px 6px; border-bottom: 1px solid var(--color-divider); font-size: 11px;
}
.mini-table td { padding: 4px 6px; border-bottom: 1px solid rgba(0,0,0,0.05); }
.cell { width: 100%; padding: 5px 8px !important; font-size: 12px; }
.w60 { max-width: 64px; } .w70 { max-width: 76px; }
.mono { font-family: Consolas, monospace; }
.cb { accent-color: var(--color-primary); }
.btn-rm {
  border: none; background: transparent; color: var(--color-danger);
  cursor: pointer; font-size: 12px; padding: 2px 6px;
}
.empty { color: var(--color-ink-dim); font-size: 12px; margin: 6px 0 0; }

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
