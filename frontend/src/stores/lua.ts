// frontend/src/stores/lua.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { LuaScript, LuaVersion } from '../types'
import { api } from '../api/client'

/**
 * lua 脚本管理 store。
 *
 * 后端端点（经 gateway `/api/rsiot/api/v1/lua/...`）：
 * - `GET /lua/scripts` → `{scripts: ["devA", ...]}`（仅目录名）
 * - `GET /lua/script?name=<n>` → `{name, content, size, mtime}`（回读内容）
 * - `POST /lua/script/save` `{name, content}` → 写 main.lua（覆盖前自动存版本快照）
 * - `GET /lua/script/versions?name=<n>` → `{versions: [{ts, size}]}`（历史，降序）
 * - `POST /lua/script/rollback` `{name, ts}` → 复制 versions/<ts>.lua → main.lua
 * - `POST /lua/reload?inst_id=<inst>` → 重载 lua 实例（stop+start）
 *
 * dirty 状态由 ScriptEditor 双向绑定 content 时维护（保存前提示）。
 */
export const useLuaStore = defineStore('lua', () => {
  /** 脚本目录名列表（GET /lua/scripts，仅名字）。 */
  const scripts = ref<string[]>([])
  /** 当前加载的脚本（name + content）。 */
  const current = ref<LuaScript | null>(null)
  /** 当前脚本的历史版本（降序）。 */
  const versions = ref<LuaVersion[]>([])
  /** 编辑器是否相对已保存内容有改动（保存前提示）。 */
  const dirty = ref(false)
  /** 操作进行中（禁用按钮防重复点击）。 */
  const loading = ref(false)
  /** 最近一次错误信息（save/rollback 失败等）。 */
  const error = ref('')

  /** 拉取脚本列表。 */
  async function loadScripts() {
    loading.value = true
    error.value = ''
    try {
      const r = await api('/api/rsiot/api/v1/lua/scripts')
      if (r.ok) {
        const data = await r.json()
        scripts.value = (data.scripts ?? []) as string[]
      }
    } catch (e) {
      error.value = e instanceof Error ? e.message : '加载脚本列表失败'
    } finally {
      loading.value = false
    }
  }

  /** 加载单个脚本内容 + 历史版本。 */
  async function loadScript(name: string) {
    loading.value = true
    error.value = ''
    try {
      const [contentR, versionsR] = await Promise.all([
        api(`/api/rsiot/api/v1/lua/script?name=${encodeURIComponent(name)}`),
        api(`/api/rsiot/api/v1/lua/script/versions?name=${encodeURIComponent(name)}`),
      ])
      if (contentR.ok) {
        current.value = (await contentR.json()) as LuaScript
      } else {
        error.value = '读取脚本内容失败'
      }
      if (versionsR.ok) {
        const data = await versionsR.json()
        versions.value = (data.versions ?? []) as LuaVersion[]
      }
      dirty.value = false
    } catch (e) {
      error.value = e instanceof Error ? e.message : '加载脚本失败'
    } finally {
      loading.value = false
    }
  }

  /** 保存当前脚本（POST /lua/script/save，后端覆盖前自动存版本）。保存后重拉版本列表。 */
  async function saveScript(): Promise<boolean> {
    if (!current.value) return false
    loading.value = true
    error.value = ''
    try {
      const r = await api('/api/rsiot/api/v1/lua/script/save', {
        method: 'POST',
        body: JSON.stringify({ name: current.value.name, content: current.value.content }),
      })
      if (!r.ok) {
        error.value = `保存失败 (${r.status})`
        return false
      }
      dirty.value = false
      // 保存后重拉版本列表（新版本已存）。
      await loadScript(current.value.name)
      return true
    } catch (e) {
      error.value = e instanceof Error ? e.message : '保存失败'
      return false
    } finally {
      loading.value = false
    }
  }

  /** 回滚到指定历史版本（POST /lua/script/rollback）。回滚后重拉内容 + 版本。 */
  async function rollback(ts: number): Promise<boolean> {
    if (!current.value) return false
    loading.value = true
    error.value = ''
    try {
      const r = await api('/api/rsiot/api/v1/lua/script/rollback', {
        method: 'POST',
        body: JSON.stringify({ name: current.value.name, ts }),
      })
      if (!r.ok) {
        error.value = `回滚失败 (${r.status})`
        return false
      }
      await loadScript(current.value.name)
      return true
    } catch (e) {
      error.value = e instanceof Error ? e.message : '回滚失败'
      return false
    } finally {
      loading.value = false
    }
  }

  /** 标记编辑器内容已改动（ScriptEditor input 时调用）。 */
  function markDirty() {
    dirty.value = true
  }

  /** 清空当前脚本状态（切换脚本 / 离开页面）。 */
  function reset() {
    current.value = null
    versions.value = []
    dirty.value = false
    error.value = ''
  }

  return {
    scripts,
    current,
    versions,
    dirty,
    loading,
    error,
    loadScripts,
    loadScript,
    saveScript,
    rollback,
    markDirty,
    reset,
  }
})
