// frontend/src/stores/devreg.ts
// 设备模型注册表 store（M-1）：devmodels/devreg 双表 + 一次性迁移 + 发现队列
// （M-4：未建模标签进 discoveries，adopt 一键建模）。
// API 走 gateway 前缀 /api/rsiot（同 monitor.ts）；postJson 模式同 monitor
// （不 import api/client.ts——router 依赖会破坏 vitest）。
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { useAuthStore } from './auth'
import type { DeviceEntry, DeviceModel } from '../lib/devmeta'
import type { EffectiveProps } from '../lib/tagops'

/** 迁移结果（POST /migrate/inline-devices）。 */
export interface MigrateResult {
  converted_models: number
  converted_devices: number
  converted_instances: number
  failures: string[]
}

/** 后端 409 载荷：模型删除被引用 / 更新引用完整性冲突。 */
export interface ConflictInfo {
  status: string
  referenced_by?: string[]
  conflicts?: { devid: string; reason: string }[]
}

/** 发现队列条目（GET /discoveries；count 降序）。 */
export interface DiscoveryItem {
  devid: string
  tag: string
  first_ts: number
  last_ts: number
  count: number
  sample_value: string
}

/** adopt 结果摘要。 */
export interface AdoptResult {
  merged: number
  swept: number
}

/** adopt 请求的单 tag 建议。 */
export interface AdoptTagReq {
  name: string
  data_type: string
  addr?: string | null
  unit?: string | null
  desc?: string | null
}

/** `GET /api/v1/tags` effective 视图条目（TagProps 平铺 + 来源标注；
 *  形状源 src/api/routes_tag.rs EffectiveView——serde 无 skip 四属性恒在）。 */
export interface EffectiveTag extends EffectiveProps {
  owner_inst?: string
  /** "model"=纯模板派生 / "override"=含设备覆盖。 */
  _source: 'model' | 'override'
}

/** `POST /api/v1/tag/batch` 结果。 */
export interface TagBatchResult {
  matched: number
  updated: number
}

/** `POST /api/v1/tag/batch` body（pattern + 三态 props：字段缺省=不动，
 *  `null`=恢复继承模板——⚠️ tag 端口 deadband 口径与 devreg overrides 相反）。 */
export interface TagBatchBody {
  pattern: string
  history?: boolean
  deadband?: number | null
  desc?: string | null
  unit?: string | null
}

export const useDevregStore = defineStore('devreg', () => {
  /** devmodels 全表（model_id → DeviceModel）。 */
  const models = ref<Record<string, DeviceModel>>({})
  /** devreg 全表（devid → DeviceEntry，含 effective 摘要）。 */
  const devices = ref<Record<string, DeviceEntry>>({})
  const loading = ref(false)
  const error = ref('')

  async function fetchJson(path: string): Promise<unknown | null> {
    try {
      const r = await fetch(path, {
        headers: { Authorization: `Bearer ${useAuthStore().token}` },
      })
      if (r.status === 401) {
        useAuthStore().clear()
        return null
      }
      if (!r.ok) return null
      return await r.json()
    } catch {
      return null
    }
  }

  async function postJson(path: string, body: unknown): Promise<Response | null> {
    try {
      const r = await fetch(path, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${useAuthStore().token}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify(body),
      })
      if (r.status === 401) {
        useAuthStore().clear()
        return null
      }
      return r
    } catch {
      return null
    }
  }

  /** 拉双表（进入页面/每次写操作后）。 */
  async function loadAll(): Promise<void> {
    loading.value = true
    error.value = ''
    const [m, d] = await Promise.all([
      fetchJson('/api/rsiot/api/v1/devmodels'),
      fetchJson('/api/rsiot/api/v1/devreg'),
    ])
    if (m) models.value = m as Record<string, DeviceModel>
    if (d) devices.value = d as Record<string, DeviceEntry>
    loading.value = false
  }

  /** 建模型（create-only：重名 409 → false）。 */
  async function createModel(m: DeviceModel): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/devmodel/set', m)
    if (!r) return false
    if (!r.ok) {
      error.value = r.status === 409 ? `模型 ${m.model_id} 已存在` : await r.text()
      return false
    }
    await loadAll()
    return true
  }

  /** 更新模型（引用完整性冲突 409 → false，error 带明细）。 */
  async function updateModel(m: DeviceModel): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/devmodel/update', m)
    if (!r) return false
    if (!r.ok) {
      if (r.status === 409) {
        try {
          const info = (await r.json()) as ConflictInfo
          const lines = (info.conflicts ?? []).map(c => `${c.devid}: ${c.reason}`)
          error.value = `更新被引用设备拒绝——${lines.join('；')}`
        } catch {
          error.value = '更新与引用设备冲突'
        }
      } else {
        error.value = await r.text()
      }
      return false
    }
    await loadAll()
    return true
  }

  /** 删模型（被引用 409 → false，error 带引用列表）。 */
  async function deleteModel(modelId: string): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/devmodel/delete', { model_id: modelId })
    if (!r) return false
    if (!r.ok) {
      if (r.status === 409) {
        try {
          const info = (await r.json()) as ConflictInfo
          error.value = `模型被设备引用（${(info.referenced_by ?? []).join(', ')}），先删设备`
        } catch {
          error.value = '模型被设备引用'
        }
      } else {
        error.value = await r.text()
      }
      return false
    }
    await loadAll()
    return true
  }

  /** 建设备（upsert；400 校验错 → false，error 带后端 reason）。 */
  async function saveDevice(d: DeviceEntry): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/devreg/set', d)
    if (!r) return false
    if (!r.ok) {
      error.value = r.status === 400 ? await r.text() : `保存失败 (${r.status})`
      return false
    }
    await loadAll()
    return true
  }

  /** 删设备（级联 dev2inst/rtdata）。 */
  async function deleteDevice(devid: string): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/devreg/delete', { devid })
    if (!r || !r.ok) {
      error.value = r ? `删除失败 (${r.status})` : '网络错误'
      return false
    }
    await loadAll()
    return true
  }

  /** 一次性迁移旧内联点表（幂等）。返回结果摘要或 null（失败）。 */
  async function migrateInline(): Promise<MigrateResult | null> {
    const r = await postJson('/api/rsiot/api/v1/migrate/inline-devices', {})
    if (!r || !r.ok) {
      error.value = r ? `迁移失败 (${r.status})` : '网络错误'
      return null
    }
    try {
      return (await r.json()) as MigrateResult
    } catch {
      return null
    }
  }

  // ---- 标签运营（D2：effective 后端对账 + glob 批量覆盖）----

  /** 拉后端 effective 全量视图（GET /tags 无过滤 = 全量；对账数据源）。 */
  async function fetchEffectiveTags(): Promise<Record<string, EffectiveTag> | null> {
    const v = await fetchJson('/api/rsiot/api/v1/tags')
    return v && typeof v === 'object' && !Array.isArray(v)
      ? (v as Record<string, EffectiveTag>)
      : null
  }

  /** glob 批量写覆盖（POST /tag/batch；单点失败后端不中断，updated ≤ matched）。
   *  成功后刷新双表；返回 {matched, updated} 或 null（失败，error 带文案）。 */
  async function tagBatch(body: TagBatchBody): Promise<TagBatchResult | null> {
    const r = await postJson('/api/rsiot/api/v1/tag/batch', body)
    if (!r || !r.ok) {
      error.value = r
        ? r.status === 400
          ? await r.text()
          : `批量覆盖失败 (${r.status})`
        : '网络错误'
      return null
    }
    try {
      const v = (await r.json()) as Partial<TagBatchResult>
      const out: TagBatchResult = { matched: v.matched ?? 0, updated: v.updated ?? 0 }
      await loadAll()
      return out
    } catch {
      return null
    }
  }

  // ---- 发现队列（M-4：未建模标签 → 人工确认 adopt 建模）----

  /** 发现队列（count 降序；空 = 全部已建模/无脏数据）。 */
  const discoveries = ref<DiscoveryItem[]>([])
  /** 拉队列（进入面板/adopt/clear 后）。 */
  async function loadDiscoveries(): Promise<void> {
    const v = await fetchJson('/api/rsiot/api/v1/discoveries')
    discoveries.value = Array.isArray(v) ? (v as DiscoveryItem[]) : []
  }
  /** 清除队列（devId 限定或全清）。 */
  async function clearDiscoveries(devId?: string): Promise<boolean> {
    const r = await postJson('/api/rsiot/api/v1/discovery/clear', devId ? { dev_id: devId } : {})
    if (!r || !r.ok) {
      error.value = r ? `清除失败 (${r.status})` : '网络错误'
      return false
    }
    await loadDiscoveries()
    return true
  }
  /** adopt 一键建模：tag 建议并入目标模型 + 队列对账清除；刷新双表与队列。 */
  async function adopt(modelId: string, tags: AdoptTagReq[]): Promise<AdoptResult | null> {
    const r = await postJson('/api/rsiot/api/v1/discovery/adopt', { model_id: modelId, tags })
    if (!r || !r.ok) {
      error.value = r
        ? r.status === 404
          ? `目标模型 ${modelId} 不存在`
          : r.status === 400 || r.status === 409
            ? await r.text()
            : `adopt 失败 (${r.status})`
        : '网络错误'
      return null
    }
    try {
      const v = (await r.json()) as Partial<AdoptResult>
      const out: AdoptResult = { merged: v.merged ?? 0, swept: v.swept ?? 0 }
      await Promise.all([loadAll(), loadDiscoveries()])
      return out
    } catch {
      return null
    }
  }

  function clearError() {
    error.value = ''
  }

  return {
    models,
    devices,
    loading,
    error,
    discoveries,
    loadAll,
    createModel,
    updateModel,
    deleteModel,
    saveDevice,
    deleteDevice,
    migrateInline,
    fetchEffectiveTags,
    tagBatch,
    loadDiscoveries,
    clearDiscoveries,
    adopt,
    clearError,
  }
})
