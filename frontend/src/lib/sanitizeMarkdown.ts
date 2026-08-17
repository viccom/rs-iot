// frontend/src/lib/sanitizeMarkdown.ts
import DOMPurify from 'dompurify'

/**
 * AI 回复 markdown 文本消毒（XSS 防护，审查 Critical 修复）。
 *
 * incremark 对 markdown 里的 raw HTML 节点走 innerHTML 直插（零过滤）——审查实测
 * `<iframe srcdoc="<script>…">` 同源执行，可窃 localStorage 明文 JWT。进入
 * IncremarkContent 前用本函数剥掉注入载荷。
 *
 * DOMPurify 默认白名单已剥 script、on* 事件属性、javascript: URI；此处显式追加 FORBID：
 * - style/iframe/object/embed/form/link/meta（CSS 注入与同源脚本宿主）
 * - 属性 style/srcdoc（行内样式注入与 iframe 脚本载体）
 *
 * 副作用：markdown 源码里的 `<` 被转义为 `&lt;`——经 incremark 渲染后显示不变
 * （代码块/比较符号均如此）。流式期间每次 text append 消毒全文，消息级几 KB 成本可接受。
 */
export function sanitizeMarkdown(text: string): string {
  return DOMPurify.sanitize(text, {
    FORBID_TAGS: ['style', 'iframe', 'object', 'embed', 'form', 'link', 'meta'],
    FORBID_ATTR: ['style', 'srcdoc'],
  })
}
