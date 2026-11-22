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
