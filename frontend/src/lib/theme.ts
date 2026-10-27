export type ThemeName = 'light' | 'dark'

export const THEME_STORAGE_KEY = 'rsiot_theme'

export function resolveInitialTheme(savedTheme: string | null): ThemeName {
  return savedTheme === 'dark' || savedTheme === 'light'
    ? savedTheme
    : 'light'
}

export function readStoredTheme(): ThemeName {
  if (typeof window === 'undefined') {
    return 'light'
  }

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

  document.documentElement.dataset.theme = theme
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
