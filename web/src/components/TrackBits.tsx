import { Sparkles } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import type { TrackReason } from '../lib/types'
import { Tag } from './ui'

/** "Why this song" line for tracks picked by the AI radio. */
export function TrackReasonLine({ reason, compact }: { reason: TrackReason; compact?: boolean }) {
  const { t } = useTranslation()
  const text = t(`reason.${reason.key}`, { ...reason.params, defaultValue: t('reason.fallback') })
  if (compact) return <span className="truncate text-xs text-accent-600 dark:text-accent-400">{text}</span>
  return (
    <div className="flex min-w-0 items-center gap-2 text-xs">
      <Tag tone="sky">{t('reason.tag')}</Tag>
      <span className="min-w-0 truncate text-muted">{text}</span>
    </div>
  )
}

/** "AI" status pill for the Now playing header: same shape as the playing/paused pill, accent tint. */
export function AiChip() {
  const { t } = useTranslation()
  return (
    <span
      title={t('reason.selectedByAi')}
      className="inline-flex items-center gap-1.5 rounded-full bg-accent-500/10 px-2 py-0.5 text-xs font-medium text-accent-700 dark:text-accent-300"
    >
      <Sparkles size={11} className="shrink-0" />
      {t('reason.ai')}
    </span>
  )
}

/** Tiny inline "AI" tag after the artist of a track the AI radio picked (rows stay on one line). */
export function AiTag({ className = '' }: { className?: string }) {
  const { t } = useTranslation()
  return (
    <span
      title={t('reason.selectedByAi')}
      className={`ml-1 inline-block rounded-sm bg-accent-500/10 px-1 align-[1px] text-[9px] leading-[13px] font-semibold tracking-wide text-accent-700 dark:text-accent-300 ${className}`}
    >
      {t('reason.ai')}
    </span>
  )
}
