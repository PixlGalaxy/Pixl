// Panel appearance: theme mode (light / dark / system) and accent palette. The server keeps both in
// the user's prefs (`/api/me/prefs`) so they follow the user across devices; localStorage is only a
// first-paint cache read by public/theme.js before React loads.

export const THEMES = ['light', 'dark', 'system'] as const
export type ThemeMode = (typeof THEMES)[number]

/**
 * Accent palettes, the same set as WebinCloud's skins (one per Tailwind color family). Ids match the
 * `[data-accent]` blocks in index.css and the API's list (src/api/me.rs). `dots` = 300/500/700.
 */
export const ACCENTS = [
  { id: 'sky', dots: ['#7dd3fc', '#0ea5e9', '#0369a1'] },
  { id: 'indigo', dots: ['#a5b4fc', '#6366f1', '#4338ca'] },
  { id: 'ocean', dots: ['#67e8f9', '#06b6d4', '#0e7490'] },
  { id: 'sunset', dots: ['#fdba74', '#f97316', '#c2410c'] },
  { id: 'forest', dots: ['#6ee7b7', '#10b981', '#047857'] },
  { id: 'grape', dots: ['#c4b5fd', '#8b5cf6', '#6d28d9'] },
  { id: 'crimson', dots: ['#fda4af', '#f43f5e', '#be123c'] },
  { id: 'solar', dots: ['#fcd34d', '#f59e0b', '#b45309'] },
  { id: 'mint', dots: ['#5eead4', '#14b8a6', '#0f766e'] },
  { id: 'blossom', dots: ['#f0abfc', '#d946ef', '#a21caf'] },
  { id: 'ares', dots: ['#fca5a5', '#ef4444', '#b91c1c'] },
  { id: 'poseidon', dots: ['#93c5fd', '#3b82f6', '#1d4ed8'] },
  { id: 'zeus', dots: ['#fde047', '#eab308', '#a16207'] },
  { id: 'jade', dots: ['#86efac', '#22c55e', '#15803d'] },
  { id: 'verdigris', dots: ['#bef264', '#84cc16', '#4d7c0f'] },
  { id: 'nebula', dots: ['#d8b4fe', '#a855f7', '#7e22ce'] },
  { id: 'sakura', dots: ['#f9a8d4', '#ec4899', '#be185d'] },
  { id: 'slate', dots: ['#cbd5e1', '#64748b', '#334155'] },
  { id: 'mono', dots: ['#d1d5db', '#6b7280', '#374151'] },
  { id: 'graphite', dots: ['#d4d4d8', '#71717a', '#3f3f46'] },
  { id: 'ash', dots: ['#d4d4d4', '#737373', '#404040'] },
  { id: 'terracotta', dots: ['#d6d3d1', '#78716c', '#44403c'] },
  { id: 'olive', dots: ['#c2df90', '#8cbe37', '#608226'] },
  { id: 'maroon', dots: ['#d8979d', '#b1434c', '#7a2e35'] },
] as const
export type AccentId = (typeof ACCENTS)[number]['id']
export const DEFAULT_ACCENT: AccentId = 'sky'
/** Discord is dark for almost everyone: dark until the user picks something else. */
export const DEFAULT_THEME: ThemeMode = 'dark'

export const isThemeMode = (v: unknown): v is ThemeMode => THEMES.includes(v as ThemeMode)
export const isAccentId = (v: unknown): v is AccentId => ACCENTS.some((a) => a.id === v)

const THEME_KEY = 'lynx:theme'
const ACCENT_KEY = 'lynx:accent'

function store(key: string, value: string) {
  try {
    localStorage.setItem(key, value)
  } catch {
    // storage blocked inside some iframes
  }
}
function load(key: string): string | null {
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

const listeners = new Set<() => void>()
/** Re-render hook support: called after the theme or the accent changed. */
export function onAppearanceChange(fn: () => void): () => void {
  listeners.add(fn)
  return () => listeners.delete(fn)
}
const notify = () => listeners.forEach((fn) => fn())

const media = typeof window !== 'undefined' ? window.matchMedia?.('(prefers-color-scheme: dark)') : undefined
let mode: ThemeMode = isThemeMode(load(THEME_KEY)) ? (load(THEME_KEY) as ThemeMode) : DEFAULT_THEME
let accent: AccentId = isAccentId(load(ACCENT_KEY)) ? (load(ACCENT_KEY) as AccentId) : DEFAULT_ACCENT

function paintTheme() {
  const dark = mode === 'system' ? (media?.matches ?? true) : mode === 'dark'
  document.documentElement.classList.toggle('dark', dark)
}
media?.addEventListener?.('change', () => {
  if (mode === 'system') {
    paintTheme()
    notify()
  }
})

export function getTheme(): ThemeMode {
  return mode
}
export function getAccent(): AccentId {
  return accent
}
export function isDark(): boolean {
  return document.documentElement.classList.contains('dark')
}

export function applyTheme(next: ThemeMode) {
  mode = next
  store(THEME_KEY, next)
  paintTheme()
  notify()
}

export function applyAccent(next: AccentId) {
  accent = next
  store(ACCENT_KEY, next)
  if (next === DEFAULT_ACCENT) delete document.documentElement.dataset.accent
  else document.documentElement.dataset.accent = next
  notify()
}

/** Apply what the server has for the user (sign-in); unknown or missing values keep the cached ones. */
export function applySaved(theme: string | null | undefined, accentId: string | null | undefined) {
  if (isThemeMode(theme)) applyTheme(theme)
  if (isAccentId(accentId)) applyAccent(accentId)
}
