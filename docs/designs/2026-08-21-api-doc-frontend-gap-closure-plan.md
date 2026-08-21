# REST 文档与前端 UI 缺口补齐实施计划

> 日期：2026-08-21
> **状态**：implemented（2026-08-21 于 worktree `feat/api-doc-frontend-gaps` 执行完毕，6 task 全落地，已合并 master `736333f`）
> **实施**：2026-08-21，6 task（分支首末：T1 `3d23641` → T5 `52ca16d`，子代理逐 task 执行 + 主会话审查）
> **代码**：`docs/reference/rest-api.md`（5 端点补齐 + 可观测性段）+ `src/api/static/index.html`（apitest 按钮/操作行）+ `frontend/src/stores/{secrets,lua,monitor}.ts`（secret/stdlib/写命令三 store 扩展）+ `frontend/src/components/{monitor/SecretPanel,monitor/CommandPanel,lua/StdlibPanel}.vue` + `frontend/src/views/{Diagnostics,Lua,Control}.vue`（挂载）——Rust 逻辑零改动
> **测试**：vitest 113/113（原 102 + 新增 11：secrets 4 / lua stdlib 5 / sendCommand 2）；`npm run build` 0 error；`scripts/check_docs.py` OK；全部 TDD 红→绿
> **遗留**：三新 UI 组件未做浏览器渲染实测（store 契约由 vitest 锁定，建议 gateway 部署下浏览器过一遍）；client.ts router 依赖致 vitest 需绕行（裸 fetch/vi.mock 双模式并存，router 惰性化为后续清理候选）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 补齐 2026-08-21 审计发现的缺口——rest-api.md 缺 5 个端点文档、apitest 调试页缺新端点按钮、前端缺 secret 管理 / stdlib 同步 / 写命令三个 UI（含 vitest 测试）。

**Architecture:** 后端零改动（5 个端点均已实现且经两轮 e2e 实测）；本计划全部工作在文档（rest-api.md）、嵌入静态页（src/api/static/index.html，rust-embed）与 Vue 前端（stores + 组件挂现有页，不新增路由页）。前端新功能沿用既有模式：API 经 `/api/rsiot/api/v1/...` 前缀（gateway 代理，照 stores/lua.ts），store 用 pinia setup 风格，测试照 stores/monitor.test.ts 的桩模式。

**Tech Stack:** Markdown（rest-api.md）、原生 HTML/JS（apitest）、Vue 3 + pinia + TypeScript + vitest（前端）。

**范围裁定（审计结论修正与排除项）：**
- `lua/stats` 在文档 §4.6 的"已移除"说明是**有意的废弃标注**，不删除。
- **luaNorth 实例 UI 不在本计划**：`SOUTH_APP_CODES` 注释表明"实例管理聚焦南向采集"是有意设计；北向实例 UI 属产品决策扩展，不属缺口修复。北向实例经 REST/MCP 建立可用。
- 旧有运维面（device/user/group/accesskey 管理 UI）为改进前既有状态，不在本计划。

---

### Task 0: worktree 隔离

本计划跨 ≥3 commit、>5 文件，按仓惯例（AGENTS.md）用 worktree 隔离。

**Step 1: 建 worktree + 分支**

```bash
cd /e/Rs_Codes/rs-iot
git worktree add ../rs-iot-gap -b feat/api-doc-frontend-gaps
cd ../rs-iot-gap
```

后续所有任务在 `E:\Rs_Codes\rs-iot-gap` 中执行。

---

### Task 1: rest-api.md 补 5 端点文档 + 可观测性段同步

**Files:**
- Modify: `docs/reference/rest-api.md`

**Step 1: §4.1（系统）补 config 行**（在 §4.1 端点表 `inststatus` 行附近插入）

文档内容（契约源 `routes_sys.rs:66-74`）：

```markdown
| GET | `/api/v1/config` | — | 运行配置 JSON（**脱敏回显**：`jwt_secret`/`mqtt.username`/`mqtt.password` 遮蔽为 `"***"`，其余字段原样——诊断需看 lux 端口/data_dir 等） |
```

**Step 2: §4.2（实例 CRUD）补 roster 与 purge 两行**

契约源 `routes_inst.rs:27-45`（roster，M3 三面之一）与 `routes_inst.rs:205-223`（purge）：

```markdown
| GET | `/api/v1/inst/roster` | — | 实例名册 `[{<遮蔽 InstConfig>, "state": ...}]`——配置全集合并运行态；未运行标 `"stopped"`；运行态中配置没有的实例（config.toml/代码启动）追加仅含 `inst_id`+`state` 的条目。MCP `rsiot_list_instances` 同源（M3：回显经脱敏，不泄明文密码） |
| POST | `/api/v1/purge` | `{"scope":"all"}` | 级联清空全部实例配置 + 兜底清 `rtdata:*`/`stat:*`/`luakv:*` 残留 → `{"status":"purged","instances":N,...}`。**危险操作**（前端已接入二次确认） |
```

**Step 3: 新增 §4.12 活跃告警（`routes_alarm.rs`，D 阶段 Q12）**

插在 §4.11（secret）之后。契约源 `routes_alarm.rs` 模块头与 handler：

```markdown
### 4.12 活跃告警（`routes_alarm.rs`，D 阶段 Q12）

| 方法 | 端点 | 请求 | 响应 |
|---|---|---|---|
| GET | `/api/v1/alarms` | — | `[{key, level, source, message, ts, count}]` 按 ts 倒序；空表 `[]` |
| DELETE | `/api/v1/alarms?key=` | `?key=<告警键>` | 手动清除（运维兜底）→ `{"status":"ok","key":...}`；幂等（不存在仍 200） |

- **触发（fire）无 REST 端点**——判定归脚本（`ctx:fire_event`，阈值/迟滞在 Lua 内）；REST 面只读清单 + 手动清除。
- `DELETE` 清除时发 **Clear SSE 事件**（alarm 帧带 `action:"clear"`），前端面板视图收敛；条件帧沿用条目 level/message，`ts` 取当下。
- 前端消费：monitor store（30s 轮询 + SSE alarm 帧实时收敛）；写命令 `level ∈ {info,warn,error}`，`count` 为同键累计（同 key 1s 内重复 fire 不重发 SSE 但 count 仍累计）。

```bash
# 清单
curl -H "Authorization: $TOKEN" localhost:8790/api/v1/alarms
# 手动清除
curl -X DELETE -H "Authorization: $TOKEN" 'localhost:8790/api/v1/alarms?key=mb1.h21_high'
```
```

**Step 4: 新增 §4.13 SSE 实时事件流（`routes_events.rs`）**

契约源 `routes_events.rs:3-53`（帧格式、types 过滤）：

```markdown
### 4.13 SSE 实时事件流（`routes_events.rs`）

| 方法 | 端点 | 请求 | 响应 |
|---|---|---|---|
| GET | `/api/v1/events` | `?types=<逗号分隔>`（缺省全推） | `text/event-stream`：每事件一帧 `event: <kind>\ndata: <json>\n\n` |

- kind：`rtdata`（实时数据，C 阶段带值批量帧）/ `alarm`（告警 fire/clear，D 阶段）/ `log` / `packet`（报文 trace）/ `state`（实例状态）。
- alarm 帧 data：`{"kind":"alarm","action":"fire"|"clear","key","level","source","message","ts"}`（`source` 为宿主注入的实例 ID，不可伪造）。
- 鉴权走 cookie（EventSource 无法带自定义 header）；消费示例见前端 `stores/monitor.ts`。过载丢帧计数落 Prometheus `rs_iot_sse_lagged_total`。
```

**Step 5: §4.10 可观测性段与 §6 同步**

- §4.10 首句"可观测性已具备"清单追加：`+ 活跃告警清单（§4.12）与 SSE 事件流（§4.13）`。
- §6 已知限制中"错误格式不统一"等表述核对不受影响（ alarms/events 已有文档节后自然消除"文档未覆盖"状态，无需改 §6 文字；若 §6 有"告警无查询端点"类陈述则同步——以实际行文为准）。

**Step 6: 校验**

```bash
python scripts/check_docs.py
```
Expected: 退出码 0（无断链/规则违规）。再 `rg -c 'alarms|events|roster|purge' docs/reference/rest-api.md` 确认 ≥ 各 2 处。

**Step 7: Commit**

```bash
git add docs/reference/rest-api.md
git commit -m "docs(api): 补齐审计缺口五端点——alarms/events SSE/inst roster/purge/config + 可观测性段同步"
```

---

### Task 2: apitest 嵌入页补新端点按钮

**Files:**
- Modify: `src/api/static/index.html`（GET 按钮区 + 仿 `startinst` 的带输入按钮 + 对应 JS 函数）

**Step 1: GET 快捷按钮区追加**（照现有 `call('/api/v1/insts')` 按钮形态，加在 users/usergroups 同 row 或新 row）

```html
<button onclick="call('/api/v1/alarms')">alarms</button>
<button onclick="call('/api/v1/rtdatas')">rtdatas</button>
<button onclick="call('/api/v1/secret/list')">secret/list</button>
<button onclick="call('/api/v1/lua/scripts')">lua/scripts</button>
<button onclick="call('/api/v1/lua/stdlib')">lua/stdlib</button>
```

**Step 2: 带输入操作按钮**（照 `startinst` 行模式加 row）

```html
<div class="row">
  secret ref: <input id="sref" style="width:140px"> value: <input id="sval" style="width:140px">
  <button onclick="secretSet()">secret/set</button>
  <button onclick="secretDel()">secret/del</button>
  alarm key: <input id="akey" style="width:140px"><button onclick="alarmClear()">alarm clear</button>
</div>
```

**Step 3: 对应 JS 函数**（加在 `stopinst()` 之后；`call()` 已支持 `opts` 透传 method/body——见 index.html:49）

```javascript
async function secretSet() { call('/api/v1/secret/set', { method: 'POST', body: JSON.stringify({ ref: document.getElementById('sref').value, value: document.getElementById('sval').value }) }); }
async function secretDel() { call('/api/v1/secret?ref=' + encodeURIComponent(document.getElementById('sref').value), { method: 'DELETE' }); }
async function alarmClear() { call('/api/v1/alarms?key=' + encodeURIComponent(document.getElementById('akey').value), { method: 'DELETE' }); }
```

**Step 4: 重新构建嵌入**（rust-embed 编译期嵌入）

```bash
cargo build -j 4
```
Expected: BUILD_EXIT=0。

**Step 5: 冒烟**（可选但推荐——起 e2e-lua-d 环境后浏览器/curl 验 `/apitest` 返回含新按钮）

```bash
curl -s -H "Authorization: $(cat /f/rsiot-field/work/e2e-lua-d/jwt.txt)" http://127.0.0.1:8790/apitest | grep -c 'alarms\|secret/list\|lua/stdlib'
```
Expected: ≥3（环境未起时可跳过，Task 6 统一回归）。

**Step 6: Commit**

```bash
git add src/api/static/index.html
git commit -m "feat(apitest): 调试页补新端点快捷按钮——alarms/rtdatas/secret/lua 系 + secret/告警操作行"
```

---

### Task 3: 前端 secret 管理 UI（B 阶段 Q4 补前端）

**Files:**
- Create: `frontend/src/stores/secrets.ts`
- Create: `frontend/src/stores/secrets.test.ts`
- Create: `frontend/src/components/monitor/SecretPanel.vue`
- Modify: `frontend/src/views/Diagnostics.vue`（挂面板，照 SystemStatus 位置追加一栏）

**Step 1: 写失败测试** `frontend/src/stores/secrets.test.ts`

fetch 桩模式（照 monitor.test.ts 的 vi.stubGlobal 思路）：

```typescript
import { describe, it, expect, beforeEach, vi, afterEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { useSecretsStore } from './secrets'

/** 统一 fetch 桩：按 URL 前缀路由响应，并记录调用供断言请求形状。 */
function stubFetch(resps: Record<string, { ok: boolean; status: number; body: unknown }>) {
  const calls: { url: string; init?: RequestInit }[] = []
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    calls.push({ url, init })
    const r = Object.entries(resps).find(([k]) => url.startsWith(k))?.[1]
    if (!r) throw new Error('unexpected url: ' + url)
    return { ok: r.ok, status: r.status, json: async () => r.body } as unknown as Response
  }))
  return calls
}

describe('secrets store', () => {
  beforeEach(() => setActivePinia(createPinia()))
  afterEach(() => vi.unstubAllGlobals())

  it('load 解析掩码清单（ref+masked，绝不期望明文）', async () => {
    const calls = stubFetch({ '/api/rsiot/api/v1/secret/list': { ok: true, status: 200, body: [{ ref: 'secret:onenet_key', masked: 'sk-****key' }] } })
    const s = useSecretsStore()
    await s.load()
    expect(s.items).toEqual([{ ref: 'secret:onenet_key', masked: 'sk-****key' }])
    expect(s.error).toBe('')
    expect(calls[0].url).toContain('/secret/list')
  })

  it('load 失败置 error 不抛', async () => {
    stubFetch({ '/api/rsiot/api/v1/secret/list': { ok: false, status: 500, body: {} } })
    const s = useSecretsStore()
    await s.load()
    expect(s.error).toContain('500')
    expect(s.items).toEqual([])
  })

  it('set 发 POST {ref,value} 到 secret/set', async () => {
    const calls = stubFetch({ '/api/rsiot/api/v1/secret/set': { ok: true, status: 200, body: { status: 'ok' } } })
    const s = useSecretsStore()
    const ok = await s.set('secret:k1', 'v1')
    expect(ok).toBe(true)
    const init = calls[0].init as { method: string; body: string }
    expect(init.method).toBe('POST')
    expect(JSON.parse(init.body)).toEqual({ ref: 'secret:k1', value: 'v1' })
  })

  it('remove 发 DELETE secret?ref=', async () => {
    const calls = stubFetch({ '/api/rsiot/api/v1/secret?': { ok: true, status: 200, body: { status: 'ok' } } })
    const s = useSecretsStore()
    const ok = await s.remove('secret:k1')
    expect(ok).toBe(true)
    expect(calls[0].url).toContain('/api/v1/secret?ref=' + encodeURIComponent('secret:k1'))
    expect((calls[0].init as { method: string }).method).toBe('DELETE')
  })
})
```

**Step 2: 跑测试确认失败**

```bash
cd frontend && npm run test -- src/stores/secrets.test.ts
```
Expected: FAIL（`Cannot find module './secrets'`）。

**Step 3: 实现 store** `frontend/src/stores/secrets.ts`

```typescript
// frontend/src/stores/secrets.ts
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '../api/client'

/** secret/list 掩码条目（后端 routes_secret.rs::list_masked——永不含明文）。 */
export interface SecretItem { ref: string; masked: string }

/**
 * 平台凭证管理 store（阶段 B Q4 的前端面，补审计缺口）。
 * 后端契约：GET secret/list → [{ref,masked}]；POST secret/set {ref,value}；
 * DELETE secret?ref=。写/删均有服务端审计日志（ref 级不落值）。
 */
export const useSecretsStore = defineStore('secrets', () => {
  const items = ref<SecretItem[]>([])
  const loading = ref(false)
  const error = ref('')

  async function load() {
    loading.value = true
    error.value = ''
    try {
      const r = await api('/api/rsiot/api/v1/secret/list')
      if (!r.ok) { error.value = `加载失败 HTTP ${r.status}`; return }
      items.value = (await r.json()) as SecretItem[]
    } catch (e) {
      error.value = e instanceof Error ? e.message : '加载失败'
    } finally { loading.value = false }
  }

  async function set(ref: string, value: string): Promise<boolean> {
    error.value = ''
    try {
      const r = await api('/api/rsiot/api/v1/secret/set', { method: 'POST', body: JSON.stringify({ ref, value }) })
      if (!r.ok) { error.value = `写入失败 HTTP ${r.status}`; return false }
      await load()
      return true
    } catch (e) {
      error.value = e instanceof Error ? e.message : '写入失败'
      return false
    }
  }

  async function remove(ref: string): Promise<boolean> {
    error.value = ''
    try {
      const r = await api('/api/rsiot/api/v1/secret?ref=' + encodeURIComponent(ref), { method: 'DELETE' })
      if (!r.ok) { error.value = `删除失败 HTTP ${r.status}`; return false }
      await load()
      return true
    } catch (e) {
      error.value = e instanceof Error ? e.message : '删除失败'
      return false
    }
  }

  return { items, loading, error, load, set, remove }
})
```

**Step 4: 跑测试确认通过**

```bash
npm run test -- src/stores/secrets.test.ts
```
Expected: 4 passed。

**Step 5: 组件** `frontend/src/components/monitor/SecretPanel.vue`（样式类照 SystemStatus.vue 的 panel 风格）

```vue
<script setup lang="ts">
import { ref } from 'vue'
import { storeToRefs } from 'pinia'
import { useSecretsStore } from '../../stores/secrets'

const store = useSecretsStore()
const { items, loading, error } = storeToRefs(store)
const newRef = ref('')
const newVal = ref('')

async function onSet() {
  if (!newRef.value.trim() || !newVal.value) return
  if (await store.set(newRef.value.trim(), newVal.value)) { newRef.value = ''; newVal.value = '' }
}
async function onDel(ref: string) {
  if (window.confirm(`确认删除凭证 ${ref}？引用它的脚本/北向连接将取不到值。`)) await store.remove(ref)
}
store.load()
</script>

<template>
  <div class="secret-panel">
    <p class="panel-label">平台凭证</p>
    <h3>Secret 管理</h3>
    <p class="hint">值仅写入时可见（lux secrets 表，回显恒掩码）；供脚本 <code>ctx:secret_get</code> 与北向 <code>password: secret:&lt;ref&gt;</code> 引用。</p>
    <p v-if="error" class="err">{{ error }}</p>
    <p v-if="loading">加载中…</p>
    <ul v-else class="secret-list">
      <li v-for="it in items" :key="it.ref">
        <code>{{ it.ref }}</code>
        <span class="masked">{{ it.masked }}</span>
        <button @click="onDel(it.ref)">删除</button>
      </li>
      <li v-if="!items.length" class="empty">暂无凭证</li>
    </ul>
    <div class="secret-form">
      <input v-model="newRef" placeholder="ref（如 secret:onenet_key）">
      <input v-model="newVal" type="password" placeholder="值（平台分配密钥）">
      <button :disabled="!newRef.trim() || !newVal" @click="onSet">写入</button>
    </div>
  </div>
</template>
```

样式段照仓内组件惯例补 `.secret-list/.secret-form/.masked/.err`（scoped，复用 CSS 变量 `--card-border`/`--color-ink-dim` 等）。

**Step 6: 挂 Diagnostics.vue**（SystemStatus 面板后追加 `<SecretPanel />`，import 补齐；布局 grid 位置照现有面板）

**Step 7: 构建验证**

```bash
npm run build
```
Expected: vue-tsc 0 error。

**Step 8: Commit**

```bash
git add frontend/src/stores/secrets.ts frontend/src/stores/secrets.test.ts frontend/src/components/monitor/SecretPanel.vue frontend/src/views/Diagnostics.vue
git commit -m "feat(frontend): secret 管理 UI（Q4 补前端面）——Diagnostics 页面板 + store + vitest"
```

---

### Task 4: 前端 stdlib 清单/同步 UI（A 阶段 D1 补前端）

**Files:**
- Modify: `frontend/src/stores/lua.ts`（扩展 stdlib 状态与动作）
- Create: `frontend/src/stores/lua.test.ts`（新文件——lua store 此前无测试，一并补脚本管理既有逻辑的最小覆盖）
- Create: `frontend/src/components/lua/StdlibPanel.vue`
- Modify: `frontend/src/views/Lua.vue`（左栏列表底部挂面板）

**Step 0: 核对 sync 响应契约**——读 `src/api/routes_lua.rs` 的 `stdlib_sync` 成功响应字段（版本号/文件数/是否变更），测试与 UI 显示以此为准（计划假定含 `status` 与 `version` 字段；`stdlib_list` 已确认为 `{version, files:[...]}`）。

**Step 1: 写失败测试** `frontend/src/stores/lua.test.ts`（照 Task 3 的 stubFetch helper，可提为 `frontend/src/lib/testFetchStub.ts` 共用——DRY）

覆盖用例：
1. `loadStdlib` 解析 `{version:'1.3.0', files:[...]}` → `stdlibVersion==='1.3.0'`、`stdlibFiles.length` 正确
2. `syncStdlib()` POST 无 body（或 `{}`）到 `stdlib/sync`，成功后置 `stdlibLastSync` 非空并刷新清单（第二次 fetch 到 list）
3. 失败路径：HTTP 500 → `error` 含 '500'

**Step 2: 跑测试确认失败** `npm run test -- src/stores/lua.test.ts` → FAIL（stdlib 状态不存在）。

**Step 3: 实现**——`lua.ts` 追加：

```typescript
/** 本地 stdlib 清单（GET lua/stdlib：{version, files}）。 */
const stdlibVersion = ref('')
const stdlibFiles = ref<{ name: string; [k: string]: unknown }[]>([])
/** 最近一次同步结果描述（成功文案；失败走 error）。 */
const stdlibLastSync = ref('')

async function loadStdlib() {
  const r = await api('/api/rsiot/api/v1/lua/stdlib').catch(() => null)
  if (r?.ok) {
    const data = await r.json()
    stdlibVersion.value = data.version ?? ''
    stdlibFiles.value = data.files ?? []
  } else {
    error.value = '加载 stdlib 清单失败'
  }
}

async function syncStdlib(pin?: string): Promise<boolean> {
  error.value = ''
  const r = await api('/api/rsiot/api/v1/lua/stdlib/sync', {
    method: 'POST',
    body: JSON.stringify(pin ? { pin } : {}),
  }).catch(() => null)
  if (!r?.ok) { error.value = `stdlib 同步失败 HTTP ${r?.status ?? 'network'}`; return false }
  const data = await r.json().catch(() => ({}))
  stdlibLastSync.value = `已同步 v${data.version ?? '?'}（${data.files?.length ?? stdlibFiles.value.length} 文件）`
  await loadStdlib()
  return true
}
```

（return 列表补 `stdlibVersion, stdlibFiles, stdlibLastSync, loadStdlib, syncStdlib`；若 Step 0 契约与此不符以代码为准调整。）

**Step 4: 跑测试确认通过**。

**Step 5: 组件** `StdlibPanel.vue`（左栏底部）：显示 `stdlibVersion` + 文件数 + 展开 files 列表（名称），操作：刷新清单、同步（同步前 `window.confirm`——提示会拉取上游并覆盖本地 stdlib）。挂 `Lua.vue` 左栏 `.kind-tabs` 所列容器末尾。

**Step 6: `npm run build`** → 0 error。

**Step 7: Commit**

```bash
git add frontend/src/stores/lua.ts frontend/src/stores/lua.test.ts frontend/src/components/lua/StdlibPanel.vue frontend/src/views/Lua.vue
git commit -m "feat(frontend): stdlib 清单/同步 UI（A 阶段 D1 补前端面）——Lua 页面板 + store 扩展 + vitest"
```

---

### Task 5: 前端写命令 UI（command/send 补前端）

**Files:**
- Modify: `frontend/src/stores/monitor.ts`（追加 `sendCommand`）
- Modify: `frontend/src/stores/monitor.test.ts`（追加 describe）
- Create: `frontend/src/components/monitor/CommandPanel.vue`
- Modify: `frontend/src/views/Control.vue`（command-panel 之后挂）

**Step 1: 写失败测试**（monitor.test.ts 追加）

```typescript
describe('sendCommand', () => {
  it('发 POST command/send，data 为字符串化 JSON（cmd=write）', async () => {
    const calls = stubFetch({ '/api/rsiot/api/v1/command/send': { ok: true, status: 200, body: { status: 'ok' } } })
    const s = useMonitorStore()
    const r = await s.sendCommand('mb-native', 'mbn', '{"address":"holding:10:u16","value":1}')
    expect(r.ok).toBe(true)
    const init = calls[0].init as { method: string; body: string }
    expect(init.method).toBe('POST')
    expect(JSON.parse(init.body)).toEqual({
      instance: 'mb-native', devid: 'mbn', cmd: 'write',
      data: '{"address":"holding:10:u16","value":1}',
    })
  })

  it('非法 JSON data 前置校验失败不发请求', async () => {
    const calls = stubFetch({})
    const s = useMonitorStore()
    const r = await s.sendCommand('i', 'd', '{bad json')
    expect(r.ok).toBe(false)
    expect(r.error).toContain('JSON')
    expect(calls.length).toBe(0)
  })
})
```

（stubFetch 若 Task 4 已提为 `lib/testFetchStub.ts` 则 import 复用。注意 monitor.ts 现有路径是 `/api/v1/...` 直连形态——sendCommand 沿用 monitor.ts 的既有前缀保持页内一致。）

**Step 2: 跑测试确认失败**。

**Step 3: 实现**——monitor.ts 追加：

```typescript
/** 写命令下发（POST command/send）。data 为插件自定义 JSON 文本（modbus 地址串 /
 *  opcua node_id 形态见 rest-api.md §4.7；lua 实例走脚本 on_write）。前置校验 data
 *  是合法 JSON（常见手误早拦，不发坏请求）。 */
async function sendCommand(instance: string, devid: string, data: string): Promise<{ ok: boolean; error?: string; status?: number }> {
  try { JSON.parse(data) } catch { return { ok: false, error: 'data 不是合法 JSON' } }
  const r = await api('/api/v1/command/send', {
    method: 'POST',
    body: JSON.stringify({ instance, devid, cmd: 'write', data }),
  }).catch(() => null)
  if (!r?.ok) return { ok: false, error: `HTTP ${r?.status ?? 'network'}`, status: r?.status }
  return { ok: true, status: r.status }
}
```

**Step 4: 跑测试确认通过**。

**Step 5: 组件** `CommandPanel.vue`：实例下拉（`monitor.instances`，显示 inst_id+app_code）、devid 输入、data textarea（placeholder 给 modbus/opcua 两形态示例）、发送按钮（确认弹窗）、结果/错误行。挂 `Control.vue` 的 command-panel 文章之后（`<article class="page-panel">` 新增，或并入既有 command-panel 下方并列）。

**Step 6: `npm run build`** → 0 error。

**Step 7: Commit**

```bash
git add frontend/src/stores/monitor.ts frontend/src/stores/monitor.test.ts frontend/src/components/monitor/CommandPanel.vue frontend/src/views/Control.vue
git commit -m "feat(frontend): 写命令下发 UI——Control 页 CommandPanel + sendCommand（data JSON 前置校验）+ vitest"
```

---

### Task 6: 全量回归 + 合并

**Step 1: 前端全量**

```bash
cd frontend && npm run test 2>&1 | tail -5
```
Expected: 全部 passed（既有 102 + 新增 ≥10），0 failed。

```bash
npm run build
```
Expected: vue-tsc 0 error。

**Step 2: 文档校验 + 后端确认零改动**

```bash
python scripts/check_docs.py
git diff master --stat -- src/ crates/
```
Expected: docs 校验过；`src/` 仅 `api/static/index.html`（嵌入页），`crates/` 零改动。

**Step 3: 冒烟（可选，环境在 F:\rsiot-field）**：起 e2e-lua-d 环境后浏览器走三个新 UI 各一轮（secret 写入→清单掩码回显；stdlib 清单显示版本；command 发 holding 写→8090 验值）。若执行会话无浏览器能力则记录为待人工冒烟项。

**Step 4: 合并回 master（用户确认后）**

```bash
cd /e/Rs_Codes/rs-iot
git merge --no-ff feat/api-doc-frontend-gaps -m "merge: REST 文档与前端 UI 缺口补齐（5 端点文档/apitest 按钮/secret·stdlib·命令 UI）"
git worktree remove ../rs-iot-gap && git branch -d feat/api-doc-frontend-gaps
```

---

## 验收清单

- [ ] rest-api.md 含 alarms/events/roster/purge/config 五节，check_docs.py 过
- [ ] apitest 新按钮可点（alarms/rtdatas/secret/lua + 操作行）
- [ ] Diagnostics 页可写/删/列 secret（回显恒掩码）
- [ ] Lua 页可看 stdlib 版本/清单并触发同步（带确认）
- [ ] Control 页可发写命令（非法 JSON 前置拦截；modbus/opcua 形态可发）
- [ ] vitest 全量绿（新增 ≥10 用例）；npm run build 0 error
- [ ] 后端 Rust 逻辑零改动（仅静态页嵌入）

## 显式不做（记录在案）

- luaNorth 实例 UI（产品决策：实例管理有意聚焦南向；北向经 REST/MCP 可建）
- lua/stats 文档条目删除（是有意废弃标注）
- device/user/group/accesskey 管理 UI（改进前既有状态）
