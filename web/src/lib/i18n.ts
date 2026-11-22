import i18next from 'i18next'
import { initReactI18next } from 'react-i18next'

/**
 * Panel languages. To add one: drop `src/locales/<code>.json` (same keys as en.json)
 * and add an entry here. The label is shown in the selector in its own language.
 */
export const LANGUAGES = [
  { code: 'en', label: 'English' },
  { code: 'es', label: 'Español' },
] as const

export type LanguageCode = (typeof LANGUAGES)[number]['code']
export const DEFAULT_LANGUAGE: LanguageCode = 'en'

const files = import.meta.glob<{ default: Record<string, unknown> }>('../locales/*.json', { eager: true })
const resources = Object.fromEntries(
  LANGUAGES.map(({ code }) => [code, { translation: files[`../locales/${code}.json`]?.default ?? {} }]),
)

/** Maps any locale (`es-ES`, `pt-BR`, `en-US`) to a supported language, or null. */
export function matchLanguage(locale: string | null | undefined): LanguageCode | null {
  if (!locale) return null
  const lower = locale.toLowerCase()
  const exact = LANGUAGES.find((l) => l.code === lower)
  if (exact) return exact.code
  const base = lower.split(/[-_]/)[0]
  return LANGUAGES.find((l) => l.code === base)?.code ?? null
}

export function setDocumentLanguage(code: string) {
  document.documentElement.lang = code
}

// Before login we only have the client's language (inside Discord it follows the Discord locale).
const initial = matchLanguage(navigator.language) ?? DEFAULT_LANGUAGE

void i18next.use(initReactI18next).init({
  resources,
  lng: initial,
  fallbackLng: DEFAULT_LANGUAGE,
  interpolation: { escapeValue: false },
  returnNull: false,
})
setDocumentLanguage(initial)

export default i18next
