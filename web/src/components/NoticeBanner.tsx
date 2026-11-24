import { TriangleAlert, Wrench, X, Info } from 'lucide-react'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import { useLynx } from '../lib/lynx'
import {
  durationMinutes,
  formatRelative,
  formatWhen,
  nextChange,
  noticePhase,
  pickNotice,
  readDismissed,
  saveDismissed,
} from '../lib/notices'

/** Re-render each minute (relative times) and right when a notice starts or ends. */
function useClock(delay: number | null): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const wait = Math.min(60_000, Math.max(1_000, delay ?? 60_000))
    const id = setTimeout(() => setNow(Date.now()), wait)
    return () => clearTimeout(id)
  }, [now, delay])
  return now
}

function durationText(t: TFunction, minutes: number): string {
  if (minutes < 60) return t('notice.minutes', { count: minutes })
  const h = Math.floor(minutes / 60)
  const m = minutes % 60
  return m === 0 ? t('notice.hours', { count: h }) : t('notice.hoursMinutes', { h, m })
}

/**
 * Operator notice (scheduled downtime, info, warning) at the very top of the content, under the tab
 * bar. Times are shown in the viewer's own time zone. Dismissed per notice (kept in localStorage).
 */
export function NoticeBanner() {
  const { t, i18n } = useTranslation()
  const { notices } = useLynx()
  const [dismissed, setDismissed] = useState<Set<number>>(readDismissed)
  const now = useClock(nextChange(notices.notices, Date.now()))
  const notice = pickNotice(notices.notices, now, dismissed)
  if (!notice) return null
  const phase = noticePhase(notice, now)
  const locale = i18n.language
  const Icon = notice.kind === 'maintenance' ? Wrench : notice.kind === 'warning' ? TriangleAlert : Info
  const label =
    notice.kind === 'maintenance'
      ? t(phase === 'active' ? 'notice.maintenanceActive' : 'notice.maintenance')
      : t(notice.kind === 'warning' ? 'notice.warning' : 'notice.info')
  const when =
    phase === 'active'
      ? t('notice.until', { time: formatWhen(notice.endsAt, locale), relative: formatRelative(notice.endsAt, now, locale) })
      : `${formatWhen(notice.startsAt, locale)} (${formatRelative(notice.startsAt, now, locale)})`
  const text = [notice.title, notice.message].map((s) => s.trim()).filter(Boolean).join(': ')

  const dismiss = () => {
    const next = new Set(dismissed)
    next.add(notice.id)
    saveDismissed(next)
    setDismissed(next)
  }

  return (
    <div
      role="status"
      className="mb-4 flex items-start gap-2.5 rounded-xl border border-orange-500/30 bg-orange-500/10 px-3 py-2.5 text-sm text-orange-900 dark:text-orange-100"
    >
      <Icon size={16} className="mt-0.5 shrink-0 text-orange-500" aria-hidden />
      <p className="min-w-0 flex-1 leading-5">
        <span className="font-semibold">{label}</span>
        <span className="px-1.5 text-orange-500">·</span>
        <span>{when}</span>
        {phase === 'upcoming' && (
          <>
            <span className="px-1.5 text-orange-500">·</span>
            <span>{t('notice.about', { duration: durationText(t, durationMinutes(notice)) })}</span>
          </>
        )}
        {text && (
          <>
            <span className="px-1.5 text-orange-500">-</span>
            <span className="break-words">{text}</span>
          </>
        )}
      </p>
      <button
        type="button"
        onClick={dismiss}
        aria-label={t('notice.dismiss')}
        title={t('notice.dismiss')}
        className="-m-1 inline-flex size-7 shrink-0 items-center justify-center rounded-lg text-orange-700 transition-colors hover:bg-orange-500/15 dark:text-orange-200"
      >
        <X size={14} />
      </button>
    </div>
  )
}
