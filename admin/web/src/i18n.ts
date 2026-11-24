import i18next from 'i18next'
import { initReactI18next } from 'react-i18next'
import en from './locales/en.json'
import es from './locales/es.json'

export const LANGUAGES = [
  { code: 'en', label: 'English' },
  { code: 'es', label: 'Español' },
] as const

const KEY = 'lynx-admin:lang'

function initial(): string {
  try {
    const saved = localStorage.getItem(KEY)
    if (saved && LANGUAGES.some((l) => l.code === saved)) return saved
  } catch {
    // storage unavailable
  }
  return navigator.language.toLowerCase().startsWith('es') ? 'es' : 'en'
}

export function setLanguage(code: string) {
  try {
    localStorage.setItem(KEY, code)
  } catch {
    // storage unavailable
  }
  document.documentElement.lang = code
  void i18next.changeLanguage(code)
}

const lng = initial()
void i18next.use(initReactI18next).init({
  resources: { en: { translation: en }, es: { translation: es } },
  lng,
  fallbackLng: 'en',
  interpolation: { escapeValue: false },
  returnNull: false,
})
document.documentElement.lang = lng

export default i18next
