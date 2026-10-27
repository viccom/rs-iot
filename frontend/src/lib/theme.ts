export type ThemeName = 'light' | 'dark'

// 统一主题键（与 hmi-studio 共享）：同源两 SPA 一边切换另一边跟随。
export const THEME_STORAGE_KEY = 'webhmi_theme'
// 旧键一次性迁移（rsiot_theme / hmi_theme → webhmi_theme）。
function migrateLegacyThemeKey() {
  if (typeof window === 'undefined') return
  if (!window.localStorage.getItem(THEME_STORAGE_KEY)) {
    const legacy =
      window.localStorage.getItem('rsiot_theme') ?? window.localStorage.getItem('hmi_theme')
    if (legacy) window.localStorage.setItem(THEME_STORAGE_KEY, legacy)
  }
  window.localStorage.removeItem('rsiot_theme')
  window.localStorage.removeItem('hmi_theme')
}

// 全局默认深色（工业深色取向，与 hmi-studio T16 对齐）。
export function resolveInitialTheme(savedTheme: string | null): ThemeName {
  return savedTheme === 'dark' || savedTheme === 'light'
    ? savedTheme
    : 'dark'
}

export function readStoredTheme(): ThemeName {
  if (typeof window === 'undefined') {
    return 'dark'
  }

  migrateLegacyThemeKey()
  return resolveInitialTheme(window.localStorage.getItem(THEME_STORAGE_KEY))
}

export function persistTheme(theme: ThemeName) {
  if (typeof window === 'undefined') {
    return
  }

  window.localStorage.setItem(THEME_STORAGE_KEY, theme)
}

export function applyTheme(theme: ThemeName) {
  if (typeof document === 'undefined') {
    return
  }

  // class 双挂：`dark` 驱动本项目 main.css 变量；`theme-dark` 喂 @incremark/theme
  // 的 `.theme-dark` 选择器（其 css 同时认 [data-theme="dark"]/.theme-dark）。
  document.documentElement.classList.toggle('dark', theme === 'dark')
  document.documentElement.classList.toggle('theme-dark', theme === 'dark')
}

// ---- 界面密度（舒适/紧凑，C2）——与主题同款模式：localStorage 持久化 +
// documentElement data 属性驱动 CSS。默认舒适（现状），紧凑为 opt-in。 ----

export type DensityName = 'cozy' | 'compact'

export const DENSITY_STORAGE_KEY = 'rsiot_density'

export function resolveInitialDensity(saved: string | null): DensityName {
  return saved === 'compact' ? 'compact' : 'cozy'
}

export function readStoredDensity(): DensityName {
  if (typeof window === 'undefined') {
    return 'cozy'
  }

  return resolveInitialDensity(window.localStorage.getItem(DENSITY_STORAGE_KEY))
}

export function persistDensity(density: DensityName) {
  if (typeof window === 'undefined') {
    return
  }

  window.localStorage.setItem(DENSITY_STORAGE_KEY, density)
}

export function applyDensity(density: DensityName) {
  if (typeof document === 'undefined') {
    return
  }

  document.documentElement.dataset.density = density
}
