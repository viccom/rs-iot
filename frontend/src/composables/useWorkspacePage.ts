import { onUnmounted, watchEffect } from 'vue'
import { useWorkspaceStore, type WorkspacePageContext } from '../stores/workspace'

export function useWorkspacePage(getPageContext: () => WorkspacePageContext) {
  const workspace = useWorkspaceStore()

  watchEffect(() => {
    workspace.setPageContext(getPageContext())
  })

  onUnmounted(() => {
    workspace.resetPageContext()
  })
}
