// frontend/src/stores/lua.test.ts
import { describe, it, expect, beforeEach, vi } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// lua.ts 静态 import { api } → api/client.ts import { router } → createWebHistory()
// 模块加载期读 window（node 测试环境无 window）。mock 掉 api 短路整条 import 链
// （vi.mock 提升至所有 import 之前，真实 client.ts 不加载）。同 agent.test.ts 模式：
// 桩 api 返回 Response 形状（{ok, status, json()}），mockImplementation 按路径分流。
vi.mock('../api/client', () => ({ api: vi.fn(async () => new Response('{}')) }))

import { api } from '../api/client'
import { useLuaStore } from './lua'

// 后端 stdlib_sync 成功响应真实形状（routes_lua.rs:695 + SyncReport in stdlib_sync.rs:79）。
const syncOk = (version: string, added: string[] = [], reloaded: unknown[] = []) =>
  new Response(
    JSON.stringify({
      status: 'ok',
      upstream: 'https://upstream.example',
      pin: null,
      report: { added, updated: [], removed: [], version },
      reloaded,
    }),
  )

describe('lua store · stdlib 清单/同步（阶段 A D1 前端面）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.mocked(api).mockClear()
    vi.mocked(api).mockImplementation(async () => new Response('{}'))
  })

  it('loadStdlib 解析 {version, files}（GET /lua/stdlib）', async () => {
    const s = useLuaStore()
    vi.mocked(api).mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          version: '1.2.0',
          files: [
            { name: 'algo.crc16', path: 'algo/crc16.lua', ver: '1.2.0', source: 'synced' },
            { name: 'util.hex', path: 'util/hex.lua', ver: null, source: 'seed' },
          ],
        }),
      ),
    )
    await s.loadStdlib()
    expect(vi.mocked(api).mock.calls[0][0]).toBe('/api/rsiot/api/v1/lua/stdlib')
    expect(vi.mocked(api).mock.calls[0][1]).toBeUndefined() // GET 无 body
    expect(s.stdlibVersion).toBe('1.2.0')
    expect(s.stdlibFiles).toHaveLength(2)
    expect(s.stdlibFiles[0]).toMatchObject({ name: 'algo.crc16', source: 'synced' })
    expect(s.error).toBe('')
  })

  it('loadStdlib 契约：无 index 时 version=null 归一为空串（UI 不显示 "null"）', async () => {
    const s = useLuaStore()
    vi.mocked(api).mockResolvedValueOnce(new Response(JSON.stringify({ version: null, files: [] })))
    await s.loadStdlib()
    expect(s.stdlibVersion).toBe('')
    expect(s.stdlibFiles).toEqual([])
  })

  it('syncStdlib POST stdlib/sync（无 pin 空 body {}）→ 置 stdlibLastSync 并重拉清单', async () => {
    const s = useLuaStore()
    const listResp = {
      version: '1.3.0',
      files: [{ name: 'algo.crc16', path: 'algo/crc16.lua', ver: '1.3.0', source: 'synced' }],
    }
    vi.mocked(api).mockImplementation(async (path: string) => {
      if (String(path).endsWith('/lua/stdlib/sync')) return syncOk('1.3.0', ['algo/crc16.lua'], [{ inst_id: 'mb-power' }])
      if (String(path).endsWith('/lua/stdlib')) return new Response(JSON.stringify(listResp))
      return new Response('{}')
    })
    expect(await s.syncStdlib()).toBe(true)
    // 请求契约：POST stdlib/sync，无 pin 时 body 为空对象 {}
    const syncCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/rsiot/api/v1/lua/stdlib/sync')
    expect(syncCall).toBeTruthy()
    expect(syncCall?.[1]?.method).toBe('POST')
    expect(JSON.parse(String(syncCall?.[1]?.body))).toEqual({})
    // 成功文案非空且含新版本号
    expect(s.stdlibLastSync).not.toBe('')
    expect(s.stdlibLastSync).toContain('1.3.0')
    // 成功后重拉清单（第二次调用 stdlib list 且消费其响应——版本/文件为新值）
    expect(s.stdlibVersion).toBe('1.3.0')
    expect(s.stdlibFiles).toHaveLength(1)
    expect(s.error).toBe('')
  })

  it('syncStdlib 带 pin 透传 body {pin}', async () => {
    const s = useLuaStore()
    vi.mocked(api).mockImplementation(async (path: string) => {
      if (String(path).endsWith('/lua/stdlib/sync')) return syncOk('1.2.0')
      if (String(path).endsWith('/lua/stdlib')) return new Response(JSON.stringify({ version: '1.2.0', files: [] }))
      return new Response('{}')
    })
    await s.syncStdlib('1.2.0')
    const syncCall = vi.mocked(api).mock.calls.find(([p]) => p === '/api/rsiot/api/v1/lua/stdlib/sync')
    expect(JSON.parse(String(syncCall?.[1]?.body))).toEqual({ pin: '1.2.0' })
  })

  it('syncStdlib 失败路径（HTTP 500）→ error 含 500 且不写成功文案', async () => {
    const s = useLuaStore()
    vi.mocked(api).mockResolvedValueOnce(new Response('upstream unreachable', { status: 500 }))
    expect(await s.syncStdlib()).toBe(false)
    expect(s.error).toContain('500')
    expect(s.stdlibLastSync).toBe('')
  })
})
