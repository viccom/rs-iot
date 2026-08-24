<!-- frontend/src/components/monitor/InstanceForm.vue
  实例编辑表单（模态）。新建/修改采集器实例，含运行参数（app_config 表单化）+
  采集点表（devices/children 可视化增删改）。保存走 POST /inst/set 整包写
  （后端无单点端点——读-改-写覆盖）。参照 PermissionModal 的 mask/card 骨骼。
-->
<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import type { InstConfig, AppType, HostKind } from '../../types'
import { SOUTH_APP_CODES, schemaFor, defaultAppConfig } from '../../lib/pluginSchemas'
import { useDevregStore } from '../../stores/devreg'

const props = defineProps<{
  /** 编辑的实例配置；null 表示新建（用默认空配置）。 */
  modelValue: InstConfig | null
}>()
const emit = defineEmits<{
  /** 关闭弹窗。 */
  (e: 'close'): void
  /** 保存（父组件调 store.saveInstance）。 */
  (e: 'save', cfg: InstConfig): void
}>()

/** 工作副本（深拷贝，编辑不影响父组件原对象直到保存）。 */
const form = ref<InstConfig>(cloneOrEmpty(props.modelValue))

/** 兜底 JSON 编辑缓冲（无 schema 插件）：合法时同步进 form.app_config，非法时仅报错保留原文。
 *  只在 app_code 切换 / modelValue 重置这两个外部时机重排版（resetJsonText），用户输入
 *  过程中不 reformat——避免合法中间态触发 re-serialize 抢光标、非法中间态 JSON.parse 抛错。 */
const jsonText = ref(JSON.stringify(form.value.app_config, null, 2))
const jsonError = ref('')

watch(jsonText, v => {
  try {
    form.value.app_config = JSON.parse(v || '{}')
    jsonError.value = ''
  } catch (e) {
    jsonError.value = e instanceof Error ? e.message : 'JSON 解析失败'
  }
})

function resetJsonText() {
  jsonText.value = JSON.stringify(form.value.app_config, null, 2)
  jsonError.value = ''
}

/** modelValue 变化（切换编辑实例）时重置工作副本。 */
watch(() => props.modelValue, v => { form.value = cloneOrEmpty(v); resetJsonText() })

function cloneOrEmpty(src: InstConfig | null): InstConfig {
  if (src) return JSON.parse(JSON.stringify(src)) as InstConfig
  return {
    inst_id: '',
    inst_name: '',
    app_code: 'modbusClient',
    app_type: 'south',
    auto_start: true,
    app_config: defaultAppConfig('modbusClient'),
    devices: [],
    host_kind: 'builtin',
  }
}

/** 当前 app_code 的 schema（决定运行参数表单渲染）。 */
const fields = computed(() => schemaFor(form.value.app_code))

/** app_code 切换时，重置 app_config 为该插件默认值（避免残留旧插件字段）。 */
function onAppCodeChange() {
  form.value.app_config = defaultAppConfig(form.value.app_code)
  resetJsonText()
}

/** app_config 字段取值（string/number/bool 统一字符串化供 v-model，select 同 string；
 *  string[] 数组 join(',') 供逗号分隔输入框显示）。 */
function fieldValue(key: string, type?: string): string {
  const v = form.value.app_config[key]
  if (type === 'string[]' && Array.isArray(v)) return v.join(',')
  return v === undefined || v === null ? '' : String(v)
}
/** app_config 字段写入（按 schema 类型转换）。 */
function setField(key: string, type: string, raw: string) {
  if (type === 'number') {
    form.value.app_config[key] = raw === '' ? undefined : Number(raw)
  } else if (type === 'bool') {
    form.value.app_config[key] = raw === 'true'
  } else if (type === 'string[]') {
    // 逗号分隔 → 字符串数组（去空白去空段）；空输入 = 移除键（走后端缺省语义）。
    const arr = raw.split(',').map(s => s.trim()).filter(s => s !== '')
    form.value.app_config[key] = arr.length ? arr : undefined
  } else {
    form.value.app_config[key] = raw
  }
}

// ---- 设备绑定（M-2：实例只绑定 devreg 全局设备，点表来自设备模型）----

const devreg = useDevregStore()
onMounted(() => { void devreg.loadAll() })

/** devreg 设备清单（下拉候选；含 model 便于识别）。 */
const registryDevices = computed(() =>
  Object.values(devreg.devices).sort((a, b) => a.devid.localeCompare(b.devid)))

/** 新增绑定（默认选第一台在册设备；无设备时占位空，校验会拦）。 */
function addDevice() {
  form.value.devices.push({ devid: registryDevices.value[0]?.devid ?? '', bind: {} })
}
/** 删除绑定。 */
function removeDevice(i: number) {
  form.value.devices.splice(i, 1)
}
/** slave_id 绑定参数写入（空 = 删键走后端缺省；DeviceBinding.bind 值统一字符串）。 */
function setBindSlave(b: { bind: Record<string, string> }, raw: string) {
  if (raw === '') delete b.bind.slave_id
  else b.bind.slave_id = raw
}

/** 选中设备的模型标签数预览（绑定行下展示点表来源概览）。 */
function deviceTagsPreview(devid: string): string {
  const dev = devreg.devices[devid]
  if (!dev) return '（未在注册表）'
  const model = devreg.models[dev.model]
  const n = model?.tags.length ?? 0
  const params = Object.entries(dev.params).map(([k, v]) => `${k}=${v}`).join(' ')
  return `${dev.model} · ${n} tag${params ? ' · ' + params : ''}`
}

/** 是否新建实例（inst_id 可编辑）。 */
const isNew = computed(() => !props.modelValue)

/** 绑定校验错误（空串=合法）：devid 为空/重复/不在注册表。 */
const devicesError = computed(() => {
  const ids = new Set<string>()
  if (!registryDevices.value.length && form.value.devices.length)
    return '设备注册表为空——先到「设备模型」页建设备或执行迁移'
  for (const b of form.value.devices) {
    const id = b.devid.trim()
    if (!id) return '存在未选择设备的绑定条目'
    if (!devreg.devices[id]) return `设备 ${id} 不在注册表（先建设备）`
    if (ids.has(id)) return `重复绑定设备：${id}`
    ids.add(id)
  }
  return ''
})

/** 保存校验：inst_id 非空 + 点表合法（devId 非空不重复）+ app_code 必填字段 + JSON 兜底态无语法错误。 */
function canSave(): boolean {
  if (!form.value.inst_id.trim()) return false
  if (devicesError.value) return false
  if (!fields.value.length && jsonError.value) return false
  for (const f of fields.value) {
    if (f.required && !form.value.app_config[f.key]) return false
  }
  return true
}

/** 提交保存。 */
function submit() {
  if (!canSave()) return
  emit('save', form.value)
}
</script>

<template>
  <div class="modal-mask" role="dialog" aria-modal="true" @click.self="emit('close')">
    <div class="modal-card">
      <header class="head">
        <h3>{{ isNew ? '新建采集器实例' : `编辑 ${form.inst_id}` }}</h3>
        <button class="btn-icon" aria-label="关闭" @click="emit('close')">✕</button>
      </header>

      <div class="body">
        <!-- 基础字段 -->
        <section class="sec">
          <h4>基础信息</h4>
          <div class="grid2">
            <label class="field">
              <span>实例 ID</span>
              <input v-model="form.inst_id" :disabled="!isNew" placeholder="如 mb-tcp-2002" />
            </label>
            <label class="field">
              <span>显示名</span>
              <input v-model="form.inst_name" placeholder="可选" />
            </label>
            <label class="field">
              <span>插件类型</span>
              <select v-model="form.app_code" :disabled="!isNew" @change="onAppCodeChange">
                <option v-for="c in SOUTH_APP_CODES" :key="c" :value="c">{{ c }}</option>
              </select>
            </label>
            <label class="field">
              <span>方向</span>
              <select v-model="form.app_type">
                <option value="south">南向采集</option>
                <option value="north">北向发布</option>
              </select>
            </label>
            <label class="field">
              <span>宿主类型</span>
              <select v-model="form.host_kind">
                <option value="builtin">内置</option>
                <option value="external">外部（sidecar）</option>
              </select>
            </label>
            <label class="field check">
              <input type="checkbox" v-model="form.auto_start" />
              <span>启动时自动拉起</span>
            </label>
          </div>
        </section>

        <!-- 运行参数（按 schema 表单化）-->
        <section class="sec">
          <h4>运行参数</h4>
          <div v-if="fields.length" class="grid2">
            <label v-for="f in fields" :key="f.key" class="field">
              <span>{{ f.label }}<em v-if="f.required">*</em></span>
              <input
                v-if="f.type === 'string' || f.type === 'number' || f.type === 'string[]'"
                :type="f.type === 'number' ? 'number' : 'text'"
                :value="fieldValue(f.key, f.type)"
                :placeholder="f.hint"
                @input="setField(f.key, f.type, ($event.target as HTMLInputElement).value)"
              />
              <select
                v-else-if="f.type === 'select'"
                :value="fieldValue(f.key)"
                @change="setField(f.key, f.type, ($event.target as HTMLSelectElement).value)"
              >
                <option v-for="o in f.options" :key="o" :value="o">{{ o }}</option>
              </select>
              <label v-else class="check-inline">
                <input
                  type="checkbox"
                  :checked="fieldValue(f.key) === 'true'"
                  @change="setField(f.key, f.type, ($event.target as HTMLInputElement).checked ? 'true' : 'false')"
                />
                <span>{{ f.hint || '' }}</span>
              </label>
            </label>
          </div>
          <!-- 无 schema 的插件回退 JSON 文本框（缓冲编辑：非法 JSON 只报错不丢内容） -->
          <template v-else>
            <textarea v-model="jsonText" class="json-area" rows="6" placeholder="{}" />
            <p v-if="jsonError" class="json-err">JSON 语法错误：{{ jsonError }}（保存已禁用，修正后自动恢复）</p>
          </template>
        </section>

        <!-- 设备绑定（M-2：点表权威在设备模型注册表，实例只绑定）-->
        <section class="sec">
          <div class="sec-head">
            <h4>设备绑定（点表来自设备模型）</h4>
            <button class="btn-add" @click="addDevice" :disabled="!registryDevices.length">+ 绑定设备</button>
          </div>
          <div v-for="(b, di) in form.devices" :key="di" class="device-block">
            <div class="device-head">
              <select v-model="b.devid" class="dev-id">
                <option value="" disabled>选择设备…</option>
                <option v-for="d in registryDevices" :key="d.devid" :value="d.devid">
                  {{ d.devid }}（{{ d.model }}）
                </option>
              </select>
              <label v-if="form.app_code === 'modbusClient'" class="slave">
                <span>slave_id</span>
                <input
                  :value="b.bind.slave_id ?? ''"
                  type="number" placeholder="默认"
                  @input="setBindSlave(b, ($event.target as HTMLInputElement).value)"
                >
              </label>
              <button class="btn-del" @click="removeDevice(di)">解绑</button>
            </div>
            <p class="bind-preview">{{ deviceTagsPreview(b.devid) }}</p>
          </div>
          <p v-if="!form.devices.length" class="empty-hint">暂无绑定。设备与点表在「设备模型」页管理（模型模板 → 建设备 → 此处绑定）。</p>
          <p v-if="devicesError" class="json-err">{{ devicesError }}（保存已禁用）</p>
        </section>
      </div>

      <footer class="actions">
        <button class="btn btn-cancel" @click="emit('close')">取消</button>
        <button class="btn btn-save" :disabled="!canSave()" @click="submit">保存</button>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.modal-mask {
  position: fixed; inset: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(15, 23, 42, 0.5);
  z-index: 1000;
}
.modal-card {
  width: 760px; max-width: calc(100vw - 32px);
  max-height: calc(100vh - 64px);
  display: flex; flex-direction: column;
  background: var(--color-surface-raised);
  border-radius: 14px;
  box-shadow: 0 12px 40px rgba(15, 23, 42, 0.25);
}
.head {
  display: flex; justify-content: space-between; align-items: center;
  padding: 16px 20px;
  border-bottom: 1px solid var(--color-divider);
}
.head h3 { margin: 0; font-size: 16px; font-weight: 600; color: var(--color-ink); }
.btn-icon {
  background: transparent; border: none; cursor: pointer;
  font-size: 16px; color: var(--color-ink-muted); padding: 4px 8px; border-radius: 4px;
}
.btn-icon:hover { background: var(--color-surface-hover); color: var(--color-ink); }
.body { flex: 1; overflow-y: auto; padding: 16px 20px; }
.sec { margin-bottom: 20px; }
.sec h4 {
  margin: 0 0 10px; font-size: 13px; font-weight: 600;
  color: var(--color-ink-secondary);
}
.sec-head { display: flex; justify-content: space-between; align-items: center; margin-bottom: 10px; }
.grid2 {
  display: grid; grid-template-columns: 1fr 1fr; gap: 10px 14px;
}
.field { display: flex; flex-direction: column; gap: 4px; font-size: 12px; }
.field > span { color: var(--color-ink-muted); }
.field em { color: var(--color-danger); font-style: normal; margin-left: 2px; }
.field input, .field select {
  padding: 8px 10px; border-radius: 8px;
  border: 1px solid var(--card-border); background: var(--card-bg);
  color: var(--color-ink); font-size: 13px;
}
.field input:disabled { background: var(--color-surface-hover); color: var(--color-ink-muted); }
.field.check { flex-direction: row; align-items: center; gap: 6px; }
.check-inline { display: inline-flex; align-items: center; gap: 6px; font-size: 12px; color: var(--color-ink-muted); }
.json-area {
  width: 100%; font-family: Consolas, monospace; font-size: 12px;
  padding: 10px; border-radius: 8px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: var(--color-ink);
  resize: vertical;
}
.json-err { margin: 6px 0 0; font-size: 11px; color: var(--color-danger); word-break: break-all; }
.device-block {
  padding: 12px; margin-bottom: 12px;
  border: 1px solid var(--card-border); border-radius: 10px;
  background: var(--card-bg);
}
.device-head {
  display: flex; gap: 8px; align-items: center; margin-bottom: 8px;
}
.device-head input { flex: 1; padding: 6px 8px; border-radius: 6px; border: 1px solid var(--card-border); font-size: 12px; }
.dev-id { font-family: Consolas, monospace; max-width: 160px; }
.slave { display: flex; align-items: center; gap: 4px; font-size: 11px; color: var(--color-ink-muted); }
.slave input { max-width: 70px; }
.child-table { width: 100%; border-collapse: collapse; font-size: 12px; margin-top: 6px; }
.child-table th { text-align: left; padding: 4px 6px; color: var(--color-ink-muted); font-size: 11px; font-weight: 500; }
.child-table td { padding: 3px 4px; }
.child-table input, .child-table select {
  width: 100%; padding: 4px 6px; border-radius: 5px;
  border: 1px solid var(--card-border); background: var(--card-bg); color: var(--color-ink); font-size: 12px;
}
.btn-add {
  padding: 5px 12px; border-radius: 8px; border: 1px solid var(--color-primary);
  background: transparent; color: var(--color-primary); font-size: 12px; cursor: pointer;
}
.btn-add-sm {
  margin-top: 6px; padding: 3px 10px; border-radius: 6px;
  border: 1px dashed var(--card-border); background: transparent;
  color: var(--color-ink-muted); font-size: 11px; cursor: pointer;
}
.btn-del, .btn-del-sm {
  padding: 4px 8px; border-radius: 6px; border: 1px solid var(--color-danger);
  background: transparent; color: var(--color-danger); font-size: 11px; cursor: pointer;
}
.btn-del-sm { padding: 2px 6px; border: none; }
.empty-hint { color: var(--color-ink-muted); font-size: 12px; padding: 8px 0; }
.bind-preview { margin: 4px 0 0; font-size: 11px; color: var(--color-ink-muted); font-family: Consolas, monospace; }
.actions {
  display: flex; gap: 8px; justify-content: flex-end;
  padding: 14px 20px; border-top: 1px solid var(--color-divider);
}
.btn {
  padding: 8px 20px; border-radius: 8px; font-size: 13px; font-weight: 500;
  border: 1px solid transparent; cursor: pointer;
}
.btn-cancel {
  background: var(--color-surface-hover); color: var(--color-ink-secondary);
  border-color: var(--color-divider);
}
.btn-save { background: var(--color-primary); color: white; }
.btn-save:disabled { opacity: 0.45; cursor: not-allowed; }
.btn-save:hover:not(:disabled) { background: var(--color-primary-hover); }
</style>
