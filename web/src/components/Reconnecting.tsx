import { useTranslation } from 'react-i18next'
import { CompactGate } from '../screens/Gate'
import { LynxMark } from './Logo'
import { StatusLink } from './StatusLink'
import { Spinner } from './ui'

/**
 * Shown over the panel while the backend does not answer. The top bar stays visible; the content
 * below is dimmed and blurred, and the whole page takes no clicks (the shell is also `inert`).
 */
export function ReconnectingOverlay({ compact }: { compact: boolean }) {
  const { t } = useTranslation()
  if (compact) return <CompactGate state="busy" text={t('health.compact')} />
  return (
    <div className="fixed inset-0 z-50 animate-[fade-in_.2s_ease-out]" role="alertdialog" aria-live="assertive" aria-label={t('health.title')}>
      {/* Header (h-12 + tab row h-11): visible, not clickable. */}
      <div className="absolute inset-x-0 top-0 h-[5.75rem]" />
      <div className="absolute inset-x-0 top-[5.75rem] bottom-0 flex items-center justify-center bg-bg/70 px-4 backdrop-blur-sm">
        <section className="w-full max-w-sm animate-[pop-in_.2s_ease-out] rounded-2xl border border-line bg-surface p-6 text-center shadow-2xl">
          <LynxMark className="mx-auto mb-4 size-14 text-fg" />
          <div className="flex items-center justify-center gap-2">
            <Spinner size={16} className="text-accent-500" />
            <h1 className="font-semibold">{t('health.title')}</h1>
          </div>
          <p className="mt-1.5 text-sm text-muted">{t('health.body')}</p>
          <StatusLink className="mt-5 border-t border-line pt-4" />
        </section>
      </div>
    </div>
  )
}
