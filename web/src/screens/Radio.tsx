import { Radio, Square } from 'lucide-react'
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { AiProgress } from '../components/AiProgress'
import { TrackReasonLine } from '../components/TrackBits'
import { Artwork, Button, Card, Empty, Notice, labelCls, inputCls } from '../components/ui'
import { api } from '../lib/api'
import { useLynx } from '../lib/lynx'

const EXAMPLES = ['study', 'gym', 'evening', 'party'] as const

export function RadioScreen() {
  const { t } = useTranslation()
  const { state, user, permissions, channelId, run } = useLynx()
  const [prompt, setPrompt] = useState('')
  const [busy, setBusy] = useState(false)

  const ai = state?.ai ?? null
  const active = !!ai?.active
  const isOwner = ai?.ownerId === user.id
  const canStop = permissions.canControl || isOwner
  const jobRunning = !!ai?.job && !['ready', 'failed', 'cancelled'].includes(ai.job.phase)
  const picks = (state?.upcoming ?? []).filter((x) => x.reason).slice(0, 8)

  const start = async () => {
    if (!channelId) return
    setBusy(true)
    await run(() => api.aiStart(channelId, prompt.trim() || undefined))
    setBusy(false)
  }

  return (
    <div className="space-y-5">
      <Card
        title={t('ai.title')}
        description={t('ai.description')}
        icon={<Radio size={18} />}
        actions={
          active ? (
            <span className="inline-flex items-center gap-1.5 rounded-full bg-emerald-500/10 px-2 py-0.5 text-xs font-medium text-emerald-700 dark:text-emerald-300">
              <span className="size-1.5 animate-pulse rounded-full bg-emerald-500" />
              {t('ai.on')}
            </span>
          ) : (
            <span className="inline-flex items-center gap-1.5 rounded-full bg-surface-2 px-2 py-0.5 text-xs font-medium text-muted">
              <span className="size-1.5 rounded-full bg-zinc-400" />
              {t('ai.off')}
            </span>
          )
        }
      >
        {active ? (
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-sm text-muted">{isOwner ? t('ai.startedByYou') : t('ai.startedBySomeone')}</p>
            {canStop && channelId && (
              <Button variant="danger" size="sm" icon={<Square size={12} />} onClick={() => run(() => api.aiStop(channelId))}>
                {t('ai.stop')}
              </Button>
            )}
          </div>
        ) : !permissions.canAdd ? (
          <p className="text-sm text-muted">{t('ai.noPermission')}</p>
        ) : (
          <form
            className="space-y-3"
            onSubmit={(e) => {
              e.preventDefault()
              void start()
            }}
          >
            <div>
              <label htmlFor="ai-prompt" className={labelCls}>
                {t('ai.promptLabel')}
              </label>
              <input
                id="ai-prompt"
                value={prompt}
                maxLength={200}
                onChange={(e) => setPrompt(e.target.value)}
                placeholder={t('ai.promptPlaceholder')}
                className={inputCls}
              />
            </div>
            <div className="flex flex-wrap gap-1.5">
              {EXAMPLES.map((key) => (
                <button
                  key={key}
                  type="button"
                  onClick={() => setPrompt(t(`ai.examples.${key}`))}
                  className="rounded-md border border-line bg-surface-2 px-2 py-1 text-xs text-muted transition-colors hover:border-sky-500/40 hover:text-fg"
                >
                  {t(`ai.examples.${key}`)}
                </button>
              ))}
            </div>
            <div className="flex items-center justify-between gap-3">
              <p className="text-xs text-muted">{t('ai.startHint')}</p>
              <Button type="submit" variant="primary" size="md" icon={<Radio size={14} />} loading={busy} disabled={jobRunning || !channelId}>
                {t('ai.start')}
              </Button>
            </div>
          </form>
        )}
      </Card>

      {ai?.job && <AiProgress />}

      {!channelId && <Notice tone="amber">{t('ai.noChannel')}</Notice>}

      {active && (
        <Card title={t('ai.picksTitle')} description={t('ai.picksHint')} bodyClassName="">
          {picks.length === 0 ? (
            <Empty>{t('ai.noPicks')}</Empty>
          ) : (
            <ul className="-mt-px">
              {picks.map((tr) => (
                <li key={tr.uid} className="flex items-center gap-3 border-t border-line px-3 py-2.5">
                  <Artwork url={tr.artworkUrl} className="size-9 rounded-lg" iconSize={14} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-semibold">{tr.title}</div>
                    <div className="mt-0.5 truncate text-xs text-muted">{tr.artists.join(', ')}</div>
                    {tr.reason && (
                      <div className="mt-1">
                        <TrackReasonLine reason={tr.reason} />
                      </div>
                    )}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </Card>
      )}
    </div>
  )
}
