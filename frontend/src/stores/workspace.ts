import { defineStore } from 'pinia'
import { ref } from 'vue'

export interface WorkspaceRouteContext {
  routeKey: string
  routeTitle: string
  routePath: string
}

export interface WorkspacePageContext {
  pageSummary: string
  facts: string[]
}

const EMPTY_ROUTE_CONTEXT: WorkspaceRouteContext = {
  routeKey: '',
  routeTitle: '',
  routePath: '',
}

const EMPTY_PAGE_CONTEXT: WorkspacePageContext = {
  pageSummary: '',
  facts: [],
}

export const useWorkspaceStore = defineStore('workspace', () => {
  const routeContext = ref<WorkspaceRouteContext>({ ...EMPTY_ROUTE_CONTEXT })
  const pageContext = ref<WorkspacePageContext>({ ...EMPTY_PAGE_CONTEXT })

  function setRouteContext(nextRouteContext: WorkspaceRouteContext) {
    routeContext.value = { ...nextRouteContext }
  }

  function setPageContext(nextPageContext: WorkspacePageContext) {
    pageContext.value = {
      pageSummary: nextPageContext.pageSummary,
      facts: [...nextPageContext.facts],
    }
  }

  function resetPageContext() {
    pageContext.value = { ...EMPTY_PAGE_CONTEXT }
  }

  return {
    routeContext,
    pageContext,
    setRouteContext,
    setPageContext,
    resetPageContext,
  }
})
