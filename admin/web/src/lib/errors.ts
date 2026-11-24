import { useCallback } from 'react'
import { useTranslation } from 'react-i18next'
import { AdminError } from './api'

/** Translated text for an API error: a known code, the server's message for validation errors, or generic. */
export function useErrorText() {
  const { t } = useTranslation()
  return useCallback(
    (e: unknown) => {
      if (e instanceof AdminError) {
        const known = t(`errors.${e.code}`, { defaultValue: '' })
        if (known) return known
        if (e.code === 'invalid' && e.message) return e.message
      }
      return t('errors.generic')
    },
    [t],
  )
}
