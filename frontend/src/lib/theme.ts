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
