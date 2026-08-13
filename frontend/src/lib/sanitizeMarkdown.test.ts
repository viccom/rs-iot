// frontend/src/lib/sanitizeMarkdown.test.ts
// @vitest-environment jsdom  ← DOMPurify 依赖 window/Document（node 环境导出工厂无 sanitize）
import { describe, it, expect } from 'vitest'
import { sanitizeMarkdown } from './sanitizeMarkdown'

describe('sanitizeMarkdown', () => {
  it('剥掉 iframe 注入（审查 Critical 复现载荷：同源脚本宿主）', () => {
    // 锁的不变量：AI 回复中的 iframe 不得进入 incremark 渲染（同源执行可窃 JWT）。
    const evil = '<iframe srcdoc="<script>alert(document.domain)</script>">'
    expect(sanitizeMarkdown(evil)).not.toContain('<iframe')
    expect(sanitizeMarkdown(evil)).not.toContain('srcdoc')
  })

  it('剥掉 style/script/form 等注入标签', () => {
    const out = sanitizeMarkdown('<style>*{display:none}</style><script>alert(1)</script><form action="x">')
    expect(out).not.toContain('<style')
    expect(out).not.toContain('<script')
    expect(out).not.toContain('<form')
  })

  it('剥掉行内 style 与 on* 事件属性', () => {
    const out = sanitizeMarkdown('<img src="x" onerror="alert(1)" style="position:fixed">')
    expect(out).not.toContain('onerror')
    expect(out).not.toContain('style=')
  })

  it('保留正常 markdown 语法（消毒不破坏 markdown 渲染）', () => {
    const md = '# 标题\n\n- 列表项\n\n**加粗** 与 `行内代码`\n\n| a | b |\n|---|---|\n| 1 | 2 |'
    const out = sanitizeMarkdown(md)
    expect(out).toContain('# 标题')
    expect(out).toContain('- 列表项')
    expect(out).toContain('**加粗**')
    expect(out).toContain('| a | b |')
  })

  it('代码块内 < 转义后显示不变（&lt; 经 incremark 渲染还原为 <）', () => {
    const out = sanitizeMarkdown('```lua\nif a < b then\n```')
    // < 被转义为 &lt;（显示仍是 <），代码块语法保留。
    expect(out).toContain('&lt;')
    expect(out).toContain('```')
  })
})
