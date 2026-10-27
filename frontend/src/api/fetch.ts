// frontend/src/api/fetch.ts
import { useAuthStore } from '../stores/auth'

/** 纯 fetch 封装（审计 B5）：JWT 头注入 + Content-Type 处理，**不 import
 *  router**——stores/组件可在 vitest 环境安全复用（client.ts 的 api() 因
 *  router.push 副作用不可单测，曾致 monitor/secrets/devreg/HistoryQuery
 *  四处复制 Bearer 逻辑）。401 处理语义刻意留给调用方（store 的
 *  handleUnauthorized 硬跳转与组件软提示不同，不在此统一）。
 *  FormData 不设 Content-Type：浏览器需自动生成含 boundary 的头。 */
export async function apiFetch(path: string, opts: RequestInit = {}) {
  const headers = new Headers(opts.headers)
  const auth = useAuthStore()
  if (auth.token) headers.set('Authorization', `Bearer ${auth.token}`)
  if (!headers.has('Content-Type') && opts.body && !(opts.body instanceof FormData)) {
    headers.set('Content-Type', 'application/json')
  }
  // 出口转 plain object：保持既有调用方/测试的可观察 headers 形态
  //（stores 原裸 fetch 传 plain object；Headers 实例会破坏 toMatchObject
  // 类断言与任何 headers 索引访问）。
  const plain: Record<string, string> = {}
  // Headers 迭代键恒小写——按 HTTP 惯例规范化首字母（Content-Type 等），
  // 保持与原裸 fetch 调用完全一致的可观察形态。
  headers.forEach((v, k) => {
    plain[k.split('-').map(seg => seg.charAt(0).toUpperCase() + seg.slice(1)).join('-')] = v
  })
  return fetch(path, { ...opts, headers: plain })
}
