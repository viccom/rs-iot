import { describe, expect, it } from 'vitest'
import { resolveInitialTheme, type ThemeName } from './theme'

describe('resolveInitialTheme', () => {
  it('defaults to light theme when no saved preference exists', () => {
    expect(resolveInitialTheme(null)).toBe('light')
  })

  it('restores a valid saved theme', () => {
    expect(resolveInitialTheme('dark')).toBe('dark')
    expect(resolveInitialTheme('light')).toBe('light')
  })

  it('falls back to light when saved theme is invalid', () => {
    expect(resolveInitialTheme('amber' as ThemeName)).toBe('light')
  })
})
