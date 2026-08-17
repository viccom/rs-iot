<!-- frontend/src/components/chat/ProviderModal.vue
  LLM 供应商管理弹窗（初装配置入口）：列表 + 默认模型设置 + 新增/编辑/删除。
  背景：reasonix serve 无供应商 CRUD API（仅缺 key 补写），配置唯一权威源是
  REASONIX_HOME 的 config.toml + .env——由 gateway /api/agent/providers* 直管
  （toml_edit 原位改 + 经 warden 重启 reasonix 生效，serve 启动快照配置）。
  密钥只写不读：输入框留空 = 不修改现有密钥。参照 AskModal 视觉约定。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useAgentStore } from '../../stores/agent'
import type { LLMProvider } from '../../types'

const agent = useAgentStore()
const emit = defineEmits<{ close: [] }>()

const errorMsg = ref('')
/** 表单态：null = 列表视图；非空 = 新增（{}）/编辑（既有条目）。 */
const editing = ref<LLMProvider | 'new' | null>(null)
/** 重启等待态：gateway 已触发 warden 重启 reasonix，轮询 /models 直到恢复。 */
const restarting = ref(false)

/** 表单字段（模型经拉取勾选/手动添加，存 selected 数组）。 */
const form = ref({ name: '', kind: 'openai', base_url: '', api_key: '' })
const formBusy = ref(false)
/** 上游拉取的候选模型（空 = 未拉取）。 */
const fetchedModels = ref<string[]>([])
/** 已选模型 id（提交 payload 的 models）。 */
const selected = ref<string[]>([])
/** 手动添加输入框（部分供应商不开放列表接口）。 */
const customModel = ref('')
const fetching = ref(false)
const fetchError = ref('')

/** 勾选/取消一个模型。 */
function toggleModel(m: string) {
  selected.value = selected.value.includes(m) ? selected.value.filter(x => x !== m) : [...selected.value, m]
}
function addCustom() {
  const m = customModel.value.trim()
  if (m && !selected.value.includes(m)) selected.value = [...selected.value, m]
  customModel.value = ''
}
function removeModel(m: string) {
  selected.value = selected.value.filter(x => x !== m)
}
/** 已选但不在拉取列表里的（自定义/上游已下架）——chips 展示可移除。 */
const customSelected = computed(() => selected.value.filter(m => !fetchedModels.value.includes(m)))

/** 所有 provider/model 组合（默认模型下拉数据源）。 */
const modelOptions = computed(() =>
  agent.providers.flatMap(p => p.models.map(m => `${p.name}/${m}`)),
)

/** 编辑中是否为既有条目（名字锁定不可改——reasonix 以 name 定位条目）。 */
const isEditExisting = computed(() => editing.value !== null && editing.value !== 'new')

onMounted(async () => {
  try {
    await agent.loadProviders()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '加载供应商失败'
  }
})

function openAdd() {
  form.value = { name: '', kind: 'openai', base_url: '', api_key: '' }
  selected.value = []
  fetchedModels.value = []
  customModel.value = ''
  fetchError.value = ''
  editing.value = 'new'
  errorMsg.value = ''
}
function openEdit(p: LLMProvider) {
  form.value = { name: p.name, kind: p.kind || 'openai', base_url: p.base_url, api_key: '' }
  selected.value = [...p.models]
  fetchedModels.value = []
  customModel.value = ''
  fetchError.value = ''
  editing.value = p
  errorMsg.value = ''
}

/** 从上游拉模型列表（编辑场景自动用已存密钥；新增场景用输入框里的 key）。 */
async function onFetch() {
  fetchError.value = ''
  if (!form.value.base_url.trim()) {
    fetchError.value = '请先填写 API 地址'
    return
  }
  fetching.value = true
  try {
    fetchedModels.value = await agent.fetchUpstreamModels({
      base_url: form.value.base_url.trim(),
      kind: form.value.kind,
      api_key: form.value.api_key.trim() || undefined,
      // 编辑既有供应商：后端用 .env 已存密钥（api_key 输入框留空时）。
      name: isEditExisting.value ? form.value.name : undefined,
    })
    if (!fetchedModels.value.length) fetchError.value = '上游返回了空列表'
  } catch (e) {
    fetchError.value = e instanceof Error ? e.message : '获取失败'
  } finally {
    fetching.value = false
  }
}

/** 变更后的重启等待：轮询 /models 直到 serve 恢复（默认模型变更需重启生效）。 */
async function waitRestart() {
  restarting.value = true
  agent.models = []
  for (let i = 0; i < 12; i++) {
    await new Promise(r => setTimeout(r, 1500))
    try {
      await agent.loadModels()
      if (agent.models.length > 0) break
    } catch { /* serve 未就绪，继续轮询 */ }
  }
  restarting.value = false
}

async function onSave() {
  errorMsg.value = ''
  if (!form.value.name.trim() || !form.value.base_url.trim() || !selected.value.length) {
    errorMsg.value = '名称、API 地址必填，且至少选择一个模型'
    return
  }
  formBusy.value = true
  try {
    const res = await agent.saveProvider({
      name: form.value.name.trim(),
      kind: form.value.kind,
      base_url: form.value.base_url.trim(),
      models: selected.value,
      api_key: form.value.api_key.trim() || undefined,
    })
    editing.value = null
    if (!res.restarted) errorMsg.value = `已保存，但自动重启失败（${res.restart_error ?? '未知原因'}）——请在 warden 控制台手动重启 reasonix`
    else await waitRestart()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '保存失败'
  } finally {
    formBusy.value = false
  }
}

async function onDelete(p: LLMProvider) {
  if (!confirm(`确定删除供应商「${p.name}」？其 .env 密钥将一并删除，不可恢复。`)) return
  errorMsg.value = ''
  try {
    await agent.deleteProvider(p.name)
    await waitRestart()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '删除失败'
  }
}

async function onSetDefault(ref: string) {
  errorMsg.value = ''
  try {
    await agent.setDefaultModel(ref)
    await waitRestart()
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : '设置默认模型失败'
  }
}
</script>

<template>
  <div class="modal-mask" role="dialog" aria-modal="true">
    <div class="modal-card">
      <div class="head">
        <h3 class="title">LLM 供应商管理</h3>
        <button class="btn-icon" aria-label="关闭" @click="emit('close')">✕</button>
      </div>
      <p class="subtitle">配置写入 reasonix（config.toml + .env），保存后自动重启 AI 服务生效。</p>

      <p v-if="errorMsg" class="error" role="alert">{{ errorMsg }}</p>
      <p v-if="restarting" class="restarting">AI 服务重启中，稍候…（新配置启动时生效）</p>

      <!-- 列表视图 -->
      <template v-if="!editing">
        <div class="default-row">
          <label class="default-label" for="default-model">新会话默认模型</label>
          <select
            id="default-model"
            class="default-select"
            :value="agent.defaultModel"
            :disabled="restarting || agent.providerBusy"
            @change="onSetDefault(($event.target as HTMLSelectElement).value)"
          >
            <option v-if="!modelOptions.includes(agent.defaultModel)" :value="agent.defaultModel">
              {{ agent.defaultModel || '（未设置）' }}
            </option>
            <option v-for="m in modelOptions" :key="m" :value="m">{{ m }}</option>
          </select>
        </div>

        <div class="provider-list">
          <p v-if="!agent.providers.length" class="empty">暂无供应商——点「新增供应商」完成初次配置。</p>
          <div v-for="p in agent.providers" :key="p.name" class="provider-item">
            <div class="provider-main">
              <div class="provider-name">
                <span class="key-dot" :class="p.has_key ? 'ok' : 'miss'" :title="p.has_key ? '密钥已配置' : '密钥未配置（不可用）'" />
                <strong>{{ p.name }}</strong>
                <span class="kind-chip">{{ p.kind || 'openai' }}</span>
              </div>
              <div class="provider-meta">
                <span class="base-url" :title="p.base_url">{{ p.base_url }}</span>
                <span class="models-count">{{ p.models.length }} 个模型</span>
              </div>
            </div>
            <div class="provider-actions">
              <button class="btn-text" :disabled="restarting" @click="openEdit(p)">编辑</button>
              <button class="btn-text danger" :disabled="restarting" @click="onDelete(p)">删除</button>
            </div>
          </div>
        </div>

        <div class="actions">
          <button class="btn btn-later" @click="emit('close')">关闭</button>
          <button class="btn btn-answer" :disabled="restarting" @click="openAdd">新增供应商</button>
        </div>
      </template>

      <!-- 表单视图（新增/编辑） -->
      <template v-else>
        <div class="form">
          <label class="field">
            <span>名称（唯一标识，如 deepseek）</span>
            <input v-model="form.name" :disabled="isEditExisting" :placeholder="isEditExisting ? '' : 'my-provider'" />
          </label>
          <label class="field">
            <span>协议族</span>
            <select v-model="form.kind">
              <option value="openai">openai 兼容</option>
              <option value="anthropic">anthropic 兼容</option>
            </select>
          </label>
          <label class="field">
            <span>API 地址（base_url）</span>
            <input v-model="form.base_url" placeholder="https://api.example.com/v1" />
          </label>
          <div class="field">
            <span>模型（从 API 获取勾选，或手动添加）</span>
            <div class="models-toolbar">
              <button
                type="button"
                class="btn-fetch"
                :disabled="fetching || formBusy"
                @click="onFetch"
              >{{ fetching ? '获取中…' : '⟳ 从 API 获取模型列表' }}</button>
              <span v-if="isEditExisting && !form.api_key" class="hint">自动使用已保存的密钥</span>
            </div>
            <p v-if="fetchError" class="fetch-error">{{ fetchError }}</p>
            <div v-if="fetchedModels.length" class="models-list">
              <label v-for="m in fetchedModels" :key="m" class="model-check">
                <input
                  type="checkbox"
                  :checked="selected.includes(m)"
                  @change="toggleModel(m)"
                />{{ m }}
              </label>
            </div>
            <div class="custom-row">
              <input
                v-model="customModel"
                placeholder="手动添加模型 ID（供应商不开放列表接口时用）"
                @keydown.enter.prevent="addCustom"
              />
              <button type="button" class="btn-fetch" @click="addCustom">添加</button>
            </div>
            <div class="selected-chips">
              <span v-for="m in selected" :key="m" class="chip" :class="{ custom: customSelected.includes(m) }">
                {{ m }}<button type="button" aria-label="移除" @click="removeModel(m)">✕</button>
              </span>
              <span v-if="!selected.length" class="hint">尚未选择模型</span>
              <span v-else class="hint">已选 {{ selected.length }} 个</span>
            </div>
          </div>
          <label class="field">
            <span>API Key{{ isEditExisting ? '（留空 = 不修改现有密钥）' : '' }}</span>
            <input v-model="form.api_key" type="password" autocomplete="off" placeholder="sk-..." />
          </label>
        </div>
        <div class="actions">
          <button class="btn btn-later" :disabled="formBusy || restarting" @click="editing = null">取消</button>
          <button class="btn btn-answer" :disabled="formBusy || restarting" @click="onSave">
            {{ formBusy ? '保存中…' : '保存并重启生效' }}
          </button>
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.modal-mask {
  position: fixed; inset: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(15, 23, 42, 0.45);
  z-index: 1000;
}
.modal-card {
  width: 620px; max-width: calc(100vw - 32px);
  max-height: calc(100vh - 64px); overflow-y: auto;
  background: var(--color-surface-raised);
  border-radius: 14px;
  box-shadow: 0 10px 30px rgba(15, 23, 42, 0.2);
  padding: 20px;
  display: flex; flex-direction: column; gap: 14px;
}
.head { display: flex; align-items: center; justify-content: space-between; }
.title { margin: 0; font-size: 16px; font-weight: 600; color: var(--color-ink); }
.subtitle { margin: 0; font-size: 12px; color: var(--color-ink-muted); }
.error {
  margin: 0; padding: 8px 12px; font-size: 12px;
  color: var(--color-danger); background: rgba(220, 38, 38, 0.06);
  border: 1px solid rgba(220, 38, 38, 0.2); border-radius: 8px;
}
.restarting {
  margin: 0; padding: 8px 12px; font-size: 12px;
  color: var(--color-ink-secondary); background: var(--color-surface-hover);
  border-radius: 8px;
}

.default-row { display: flex; align-items: center; gap: 10px; }
.default-label { font-size: 13px; color: var(--color-ink-secondary); white-space: nowrap; }
.default-select {
  flex: 1; font-size: 12px; padding: 6px 8px;
  background: var(--card-bg); color: var(--color-ink);
  border: 1px solid var(--card-border); border-radius: 6px;
}

.provider-list { display: flex; flex-direction: column; gap: 8px; }
.empty { margin: 8px 0; font-size: 13px; color: var(--color-ink-muted); text-align: center; }
.provider-item {
  display: flex; align-items: center; gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--color-divider); border-radius: 10px;
  background: var(--color-surface-hover);
}
.provider-main { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px; }
.provider-name { display: flex; align-items: center; gap: 6px; }
.provider-name strong { font-size: 13px; color: var(--color-ink); }
.kind-chip {
  font-size: 11px; padding: 1px 6px; border-radius: 999px;
  background: var(--chip-bg); color: var(--chip-ink);
}
.key-dot { width: 8px; height: 8px; border-radius: 999px; flex-shrink: 0; }
.key-dot.ok { background: #22c55e; }
.key-dot.miss { background: #ef4444; }
.provider-meta { display: flex; gap: 8px; font-size: 12px; color: var(--color-ink-muted); }
.base-url { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.models-count { flex-shrink: 0; }
.provider-actions { display: flex; gap: 4px; flex-shrink: 0; }
.btn-text {
  background: transparent; border: none; font-size: 12px;
  color: var(--color-ink-muted); padding: 2px 6px; cursor: pointer; border-radius: 4px;
}
.btn-text:hover:not(:disabled) { background: var(--color-surface-hover); color: var(--color-ink); }
.btn-text.danger:hover:not(:disabled) { color: var(--color-danger); }
.btn-text:disabled { opacity: 0.5; cursor: not-allowed; }

.form { display: flex; flex-direction: column; gap: 10px; }
.field { display: flex; flex-direction: column; gap: 4px; font-size: 12px; color: var(--color-ink-secondary); }
.field input, .field select, .field textarea {
  font-size: 13px; padding: 8px 10px; font-family: inherit;
  border: 1px solid var(--card-border); border-radius: 8px;
  background: var(--card-bg); color: var(--color-ink);
}
.field input:disabled, .field textarea:disabled { background: var(--color-surface-hover); }

.models-toolbar { display: flex; align-items: center; gap: 8px; }
.btn-fetch {
  font-size: 12px; padding: 6px 12px; cursor: pointer;
  background: var(--card-bg); color: var(--color-ink-secondary);
  border: 1px solid var(--card-border); border-radius: 8px;
}
.btn-fetch:hover:not(:disabled) { border-color: var(--color-primary); color: var(--color-primary); }
.btn-fetch:disabled { opacity: 0.5; cursor: not-allowed; }
.hint { font-size: 11px; color: var(--color-ink-muted); }
.fetch-error {
  margin: 0; font-size: 12px; color: var(--color-danger);
  word-break: break-all; max-height: 72px; overflow-y: auto;
}
.models-list {
  max-height: 180px; overflow-y: auto;
  border: 1px solid var(--color-divider); border-radius: 8px;
  padding: 6px 10px; display: flex; flex-direction: column; gap: 2px;
  background: var(--card-bg);
}
.model-check {
  display: flex; align-items: center; gap: 8px;
  font-size: 12px; color: var(--color-ink); cursor: pointer; padding: 2px 0;
}
.model-check input { accent-color: var(--color-primary); }
.custom-row { display: flex; gap: 6px; }
.custom-row input { flex: 1; font-size: 12px; padding: 6px 10px; border: 1px solid var(--card-border); border-radius: 8px; background: var(--card-bg); color: var(--color-ink); }
.selected-chips { display: flex; flex-wrap: wrap; gap: 4px; align-items: center; }
.chip {
  display: inline-flex; align-items: center; gap: 4px;
  font-size: 11px; padding: 2px 6px; border-radius: 999px;
  background: color-mix(in srgb, var(--color-primary) 10%, var(--card-bg));
  color: var(--color-ink-secondary);
}
.chip.custom { background: var(--color-surface-hover); }
.chip button {
  border: none; background: transparent; cursor: pointer;
  font-size: 10px; color: var(--color-ink-muted); padding: 0 2px;
}
.chip button:hover { color: var(--color-danger); }

.actions { display: flex; gap: 8px; justify-content: flex-end; }
.btn {
  padding: 9px 18px; font-size: 13px; font-weight: 500;
  border: 1px solid transparent; border-radius: 8px; cursor: pointer;
}
.btn-later {
  background: var(--color-surface-hover); color: var(--color-ink-secondary);
  border-color: var(--color-divider);
}
.btn-answer { background: var(--color-primary); color: white; }
.btn-answer:disabled { opacity: 0.45; cursor: not-allowed; }
</style>
