import { describe, expect, it } from 'vitest'
import { resolveInitialTheme, resolveInitialDensity, type ThemeName, type DensityName } from './theme'

describe('resolveInitialTheme', () => {
  it('defaults to dark theme when no saved preference exists', () => {
    expect(resolveInitialTheme(null)).toBe('dark')
  })

  it('restores a valid saved theme', () => {
    expect(resolveInitialTheme('dark')).toBe('dark')
    expect(resolveInitialTheme('light')).toBe('light')
  })

  it('falls back to dark when saved theme is invalid', () => {
    expect(resolveInitialTheme('amber' as ThemeName)).toBe('dark')
  })
})

describe('resolveInitialDensity', () => {
  it('defaults to cozy when no saved preference exists', () => {
    expect(resolveInitialDensity(null)).toBe('cozy')
  })

  it('restores compact when saved', () => {
    expect(resolveInitialDensity('compact')).toBe('compact')
  })

  it('falls back to cozy when saved value is invalid', () => {
    expect(resolveInitialDensity('huge' as DensityName)).toBe('cozy')
  })
})
