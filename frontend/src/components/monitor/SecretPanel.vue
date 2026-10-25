<!-- frontend/src/components/monitor/SecretPanel.vue
  平台凭证面板（阶段 B Q4 前端面）：清单只显掩码（服务端 list_masked 永不回明文，
  值仅写入时可见）；写/删即时生效并重拉。删除带确认——引用它的脚本
  （ctx:secret_get）/北向连接（password: secret:<ref>）将取不到值。
-->
<script setup lang="ts">
import { ref } from 'vue'
import { useSecretsStore } from '../../stores/secrets'

const store = useSecretsStore()

const newRef = ref('')
const newValue = ref('')

/** 写入凭证：成功后清空表单（失败保内容便于改后重试——错误行由 store.error 呈现）。 */
async function submit() {
  if (!newRef.value || !newValue.value) return
  if (await store.set(newRef.value, newValue.value)) {
    newRef.value = ''
    newValue.value = ''
  }
}

/** 删除凭证（确认防误触——删除后引用方取值失败，需重写才能恢复）。 */
async function onRemove(ref: string) {
  if (!window.confirm(`删除凭证「${ref}」？引用它的脚本/北向连接将取不到值。`)) return
  await store.remove(ref)
}

// setup 即拉清单（面板常驻诊断页，无需手动激活）。
store.load()
</script>

<template>
  <div class="secret-panel">
    <div class="panel-head">
      <div>
        <p class="panel-label">平台凭证</p>
        <h3>Secret 管理</h3>
      </div>
      <span class="head-hint">值仅写入时可见；供脚本 ctx:secret_get 与北向 password: secret:&lt;ref&gt; 引用</span>
    </div>

    <p v-if="store.error" class="error">{{ store.error }}</p>
    <p v-if="store.loading" class="loading">加载中…</p>

    <ul v-if="store.items.length" class="list">
      <li v-for="it in store.items" :key="it.ref" class="row">
        <code class="ref">{{ it.ref }}</code>
        <span class="masked">{{ it.masked }}</span>
        <button class="del" @click="onRemove(it.ref)">删除</button>
      </li>
    </ul>
    <p v-else-if="!store.loading" class="empty">暂无凭证</p>

    <form class="form" @submit.prevent="submit">
      <input v-model="newRef" class="ipt" type="text" placeholder="secret:onenet_key" />
      <input v-model="newValue" class="ipt" type="password" placeholder="凭证值（仅写入时可见）" />
      <button type="submit" class="set-btn" :disabled="!newRef || !newValue">写入</button>
    </form>
  </div>
</template>

<style scoped>
.secret-panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
  position: relative;
  z-index: 1;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: end;
}
.panel-head h3 { margin: 0; font-family: "Bahnschrift", sans-serif; }
.panel-label {
  margin: 0 0 10px;
  font-size: 11px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: var(--color-ink-dim);
}
.head-hint {
  font-size: 12px;
  color: var(--color-ink-muted);
  font-family: Consolas, monospace;
  text-align: right;
}

.error {
  margin: 0;
  padding: 8px 12px;
  border-radius: 10px;
  font-size: 12px;
  color: var(--color-danger);
  background: color-mix(in srgb, var(--color-danger) 10%, transparent);
  border: 1px solid color-mix(in srgb, var(--color-danger) 26%, transparent);
}
.loading { margin: 0; font-size: 12px; color: var(--color-ink-muted); }

.list {
  margin: 0;
  padding: 0;
  list-style: none;
  border: 1px solid var(--card-border);
  border-radius: 14px;
  background: var(--card-bg);
  overflow: hidden;
}
.row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--color-divider);
}
.row:last-child { border-bottom: none; }
.row:hover { background: var(--table-row-hover); }
.ref {
  font-family: Consolas, monospace;
  font-size: 13px;
  color: var(--chip-ink);
  background: var(--chip-bg);
  border: 1px solid var(--chip-border);
  border-radius: 8px;
  padding: 2px 8px;
}
.masked {
  flex: 1;
  min-width: 0;
  font-family: Consolas, monospace;
  font-size: 12px;
  color: var(--color-ink-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.del {
  flex: none;
  padding: 5px 12px;
  border-radius: 8px;
  border: 1px solid var(--card-border);
  background: transparent;
  color: var(--color-ink-secondary);
  font-size: 12px;
  cursor: pointer;
}
.del:hover { color: var(--color-danger); border-color: var(--color-danger); }

.empty { margin: 0; padding: 18px; text-align: center; color: var(--color-ink-muted); font-size: 13px; }

.form {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) auto;
  gap: 10px;
}
.ipt {
  padding: 8px 12px;
  border-radius: 10px;
  font-size: 13px;
  border: 1px solid var(--chip-border);
  background: var(--card-bg);
  color: var(--color-ink);
  font-family: Consolas, monospace;
}
.ipt:focus { outline: none; border-color: var(--color-primary); }
.set-btn {
  padding: 8px 18px;
  border-radius: 10px;
  cursor: pointer;
  font-size: 13px;
  font-weight: 500;
  border: 1px solid var(--color-primary);
  background: var(--color-primary);
  color: white;
  white-space: nowrap;
}
.set-btn:disabled { opacity: 0.4; cursor: not-allowed; }
</style>
