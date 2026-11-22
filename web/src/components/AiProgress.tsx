import { Check, Circle, CircleAlert, X } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { useNow } from '../lib/live'
import { useLynx } from '../lib/lynx'
import type { AiPhase } from '../lib/types'
import { Button, Spinner } from './ui'

export const AI_STEPS: AiPhase[] = ['queued', 'loading_model', 'reading_taste', 'finding_songs', 'thinking', 'resolving']

/** Calm step list for a running radio job, shown in the queue area and the Radio tab. */
export function AiProgress({ embedded }: { embedded?: boolean }) {
  const { t } = useTranslation()
  const { state, channelId, run, setState } = useLynx()
  const job = state?.ai?.job ?? null
  const running = !!job && AI_STEPS.includes(job.phase)
  const now = useNow(running)
  if (!job) return null

  const terminal = !running
  const idx = AI_STEPS.indexOf(job.phase)
  const elapsed = Math.max(0, Math.floor((now - job.startedAt) / 1000))

  const body = (
    <>
      <div className="flex items-center gap-3 px-4 py-3">
        <div className="min-w-0 flex-1">
          <h3 className="text-sm font-semibold">{terminal ? t(`ai.result.${job.phase}`) : t('ai.progressTitle')}</h3>
          <p className="text-xs text-muted">
            {terminal ? t(`ai.resultHint.${job.phase}`) : t('ai.progressHint', { seconds: elapsed })}
          </p>
        </div>
        {running && job.canCancel && channelId && (
          <Button
            size="xs"
            icon={<X size={14} />}
            onClick={() => {
              setState((s) => (s?.ai?.job ? { ...s, ai: { ...s.ai, job: { ...s.ai.job, phase: 'cancelled' } } } : s))
              void run(() => api.aiCancel(channelId, job.id))
            }}
          >
            {t('common.cancel')}
          </Button>
        )}
      </div>
      {running && (
        <ol className="space-y-2.5 border-t border-line px-4 py-3">
          {AI_STEPS.map((p, i) => {
            const done = i < idx
            const active = i === idx
            return (
              <li key={p} className="flex items-center gap-3 text-sm">
                <span className="flex size-4 shrink-0 items-center justify-center">
                  {done ? (
                    <Check size={14} className="text-emerald-500" />
                  ) : active ? (
                    <Spinner size={14} className="text-sky-500" />
                  ) : (
                    <Circle size={6} className="fill-current text-line" />
                  )}
                </span>
                <span className={active ? 'font-medium text-fg' : done ? 'text-muted' : 'text-muted/70'}>
                  {t(`ai.phase.${p}`)}
                </span>
              </li>
            )
          })}
        </ol>
      )}
      {job.phase === 'failed' && (
        <div className="flex items-center gap-2 border-t border-line px-4 py-3 text-xs text-red-600 dark:text-red-400">
          <CircleAlert size={13} /> {t('ai.failedHint')}
        </div>
      )}
    </>
  )

  if (embedded) return <div aria-live="polite">{body}</div>
  return (
    <section aria-live="polite" className="overflow-hidden rounded-2xl border border-line bg-surface shadow-sm">
      {body}
    </section>
  )
}
