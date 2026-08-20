// frontend/src/stores/lua.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { LuaScript, LuaVersion } from '../types'
import { api } from '../api/client'

/**
 * lua 资产管理 store（脚本 scripts/<名>/main.lua + 站点共享库 scripts/_lib/<org>/...）。
 *
 * 后端端点（经 gateway `/api/rsiot/api/v1/lua/...`；`kind` 参数区分资产类型，
 * 缺省 script——阶段 A Q13「库」tab 复用同一编辑器与版本管理）：
 * - `GET /lua/scripts[?kind=lib]` → script：`{scripts: [...]}` | lib：`{libs: ["<org>/<路径>.lua", ...]}`
 * - `GET /lua/script?name=&kind=` → `{name, kind, content, size, mtime}`（回读）
 * - `POST /lua/script/save` `{name, content, kind?}` → 覆盖前自动存版本快照
 * - `GET /lua/script/versions?name=&kind=` → `{versions: [{ts, size}]}`（历史，降序）
 * - `POST /lua/script/rollback` `{name, ts, kind?}` → 复制 versions/<ts>.lua → 目标文件
 * - `POST /lua/reload?inst_id=<inst>` → 重载 lua 实例（stop+start；仅脚本关联实例，库走全局 watcher 自动 reload）
 *
 * dirty 状态由 ScriptEditor 双向绑定 content 时维护（保存前提示）。
 */
export const useLuaStore = defineStore('lua', () => {
  /** 当前资产类型 tab（script 脚本 | lib 站点共享库）。 */
  const kind = ref<'script' | 'lib'>('script')
  /** 脚本目录名列表（GET /lua/scripts，仅名字）。 */
  const scripts = ref<string[]>([])
  /** 共享库相对路径列表（GET /lua/scripts?kind=lib，如 "viccom/hex.lua"）。 */
  const libs = ref<string[]>([])
  /** 当前加载的资产（name + content；kind 标记来源 tab）。 */
  const current = ref<LuaScript | null>(null)
  /** 当前脚本的历史版本（降序）。 */
  const versions = ref<LuaVersion[]>([])
  /** 编辑器是否相对已保存内容有改动（保存前提示）。 */
  const dirty = ref(false)
  /** 操作进行中（禁用按钮防重复点击）。 */
  const loading = ref(false)
  /** 最近一次错误信息（save/rollback 失败等）。 */
  const error = ref('')

  /** 拉取资产列表（脚本 + 共享库并行；任一失败保留另一侧）。 */
  async function loadScripts() {
    loading.value = true
    error.value = ''
    const [sr, lr] = await Promise.all([
      api('/api/rsiot/api/v1/lua/scripts').catch(() => null),
      api('/api/rsiot/api/v1/lua/scripts?kind=lib').catch(() => null),
    ])
    try {
      if (sr?.ok) {
        const data = await sr.json()
        scripts.value = (data.scripts ?? []) as string[]
      }
      if (lr?.ok) {
        const data = await lr.json()
        libs.value = (data.libs ?? []) as string[]
      }
      if (!sr?.ok && !lr?.ok) error.value = '加载资产列表失败'
    } catch (e) {
      error.value = e instanceof Error ? e.message : '加载资产列表失败'
    } finally {
      loading.value = false
    }
  }

  /** 切换资产 tab（script/lib）：清当前编辑态，kind 驱动列表与后续读写。 */
  function switchKind(k: 'script' | 'lib') {
    if (kind.value === k) return
    kind.value = k
    reset()
  }

  /** 当前 kind 的查询串（lib 时附加；script 用缺省）。 */
  function kindQuery(): string {
    return kind.value === 'lib' ? '&kind=lib' : ''
  }

  /** 加载单个资产内容 + 历史版本（按当前 kind）。 */
  async function loadScript(name: string) {
    loading.value = true
    error.value = ''
    try {
      const [contentR, versionsR] = await Promise.all([
        api(`/api/rsiot/api/v1/lua/script?name=${encodeURIComponent(name)}${kindQuery()}`),
        api(`/api/rsiot/api/v1/lua/script/versions?name=${encodeURIComponent(name)}${kindQuery()}`),
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
        body: JSON.stringify({ name: current.value.name, content: current.value.content, kind: kind.value }),
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
        body: JSON.stringify({ name: current.value.name, ts, kind: kind.value }),
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
    kind,
    scripts,
    libs,
    current,
    versions,
    dirty,
    loading,
    error,
    loadScripts,
    switchKind,
    loadScript,
    saveScript,
    rollback,
    markDirty,
    reset,
  }
})
