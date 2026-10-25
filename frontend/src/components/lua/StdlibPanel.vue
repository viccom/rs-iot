<!-- frontend/src/components/lua/StdlibPanel.vue
  stdlib 官方库面板（阶段 A D1 前端面）：清单只读展示（version/文件数/source 徽标），
  同步是覆盖性操作——window.confirm 二次确认；同步成功后 store 自动重拉清单，
  结果文案（stdlibLastSync）与错误（error）分两行呈现。样式同 SecretPanel.vue。
-->
<script setup lang="ts">
import { ref } from 'vue'
import { useLuaStore } from '../../stores/lua'

const store = useLuaStore()

/** 文件清单展开态（默认折叠——清单可能长，左栏底部空间有限）。 */
const expanded = ref(false)

/** 同步官方库（确认防误触——本地 stdlib 将被上游版本整体覆盖再热重载实例）。 */
async function onSync() {
  if (!window.confirm('将从上游源仓拉取并覆盖本地 stdlib（scripts/_stdlib），继续？')) return
  await store.syncStdlib()
}

// setup 即拉清单（面板常驻 Lua 页左栏底部，无需手动激活）。
store.loadStdlib()
</script>

<template>
  <div class="stdlib-panel">
    <div class="panel-head">
      <div>
        <p class="panel-label">官方库</p>
        <h3>stdlib 官方库</h3>
      </div>
      <span class="head-hint">{{ store.stdlibVersion || '未安装' }} · {{ store.stdlibFiles.length }} 文件</span>
    </div>

    <p v-if="store.error" class="error">{{ store.error }}</p>
    <p v-if="store.stdlibLastSync" class="sync-result">{{ store.stdlibLastSync }}</p>

    <div class="actions">
      <button class="refresh" :disabled="store.loading" @click="store.loadStdlib()">刷新清单</button>
      <button class="sync" :disabled="store.loading" @click="onSync">同步官方库</button>
    </div>

    <button v-if="store.stdlibFiles.length" class="toggle" @click="expanded = !expanded">
      文件清单（{{ store.stdlibFiles.length }}）
      <span class="caret" :class="{ open: expanded }">▸</span>
    </button>
    <ul v-if="expanded" class="list">
      <li v-for="(f, i) in store.stdlibFiles" :key="String(f.path ?? i)" class="row">
        <code class="name">{{ f.name }}</code>
        <span class="badge" :class="String(f.source)">{{ f.source }}</span>
        <span class="ver">{{ f.ver ?? '' }}</span>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.stdlib-panel {
  display: flex;
  flex-direction: column;
  gap: 10px;
  margin-top: 14px;
  padding-top: 12px;
  border-top: 1px solid var(--color-divider);
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
  white-space: nowrap;
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
.sync-result {
  margin: 0;
  padding: 8px 12px;
  border-radius: 10px;
  font-size: 12px;
  color: var(--color-success);
  background: color-mix(in srgb, var(--color-success) 10%, transparent);
  border: 1px solid color-mix(in srgb, var(--color-success) 26%, transparent);
}

.actions { display: flex; gap: 8px; }
.refresh, .sync {
  flex: 1;
  padding: 7px 12px;
  border-radius: 10px;
  cursor: pointer;
  font-size: 12px;
  border: 1px solid var(--card-border);
  background: var(--card-bg);
  color: var(--color-ink-secondary);
}
.refresh:hover { background: var(--color-surface-hover); }
.sync {
  border-color: var(--color-primary);
  background: var(--color-primary);
  color: white;
  font-weight: 500;
}
.refresh:disabled, .sync:disabled { opacity: 0.4; cursor: not-allowed; }

.toggle {
  align-self: flex-start;
  padding: 4px 10px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: var(--color-ink-muted);
  font-size: 12px;
  cursor: pointer;
}
.toggle:hover { background: var(--color-surface-hover); }
.caret { display: inline-block; margin-left: 4px; transition: transform 0.15s; }
.caret.open { transform: rotate(90deg); }

.list {
  margin: 0;
  padding: 0;
  list-style: none;
  border: 1px solid var(--card-border);
  border-radius: 14px;
  background: var(--card-bg);
  overflow: hidden;
  max-height: 300px;
  overflow-y: auto;
}
.row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 12px;
  border-bottom: 1px solid var(--color-divider);
  font-size: 12px;
}
.row:last-child { border-bottom: none; }
.row:hover { background: var(--table-row-hover); }
.name {
  flex: 1;
  min-width: 0;
  font-family: Consolas, monospace;
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.badge {
  flex: none;
  font-size: 10px;
  padding: 1px 6px;
  border-radius: 999px;
  white-space: nowrap;
}
.badge.synced {
  background: color-mix(in srgb, var(--color-primary) 14%, transparent);
  color: var(--color-primary);
}
.badge.seed { background: var(--chip-bg); color: var(--color-ink-muted); }
.ver { flex: none; font-size: 10px; color: var(--color-ink-muted); font-family: Consolas, monospace; }
</style>
