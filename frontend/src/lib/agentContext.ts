export interface AgentContextBlockInput {
  routeTitle?: string
  routePath?: string
  pageSummary?: string
  selectedInstanceId?: string
  selectedDeviceId?: string
  facts?: string[]
}

export function buildAgentContextBlock(input: AgentContextBlockInput): string {
  const lines: string[] = []

  if (input.routeTitle) {
    lines.push(`当前页面: ${input.routeTitle}`)
  }
  if (input.routePath) {
    lines.push(`页面路径: ${input.routePath}`)
  }
  if (input.selectedInstanceId) {
    lines.push(`当前实例: ${input.selectedInstanceId}`)
  }
  if (input.selectedDeviceId) {
    lines.push(`当前设备: ${input.selectedDeviceId}`)
  }
  if (input.pageSummary) {
    lines.push(`页面摘要: ${input.pageSummary}`)
  }

  const visibleFacts = (input.facts ?? []).filter(Boolean)
  if (visibleFacts.length > 0) {
    lines.push('当前可见数据:')
    for (const fact of visibleFacts) {
      lines.push(`- ${fact}`)
    }
  }

  return lines.join('\n')
}
