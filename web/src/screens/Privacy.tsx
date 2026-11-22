import { Database, History, Trash2, TrendingUp } from 'lucide-react'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Card, ConfirmButton, Empty, PageSpinner, SettingRow, Spinner, Toggle } from '../components/ui'
import { api } from '../lib/api'
import { openExternal } from '../lib/discord'
import { useLynx } from '../lib/lynx'
import type { Taste, UserPrefs } from '../lib/types'

function TasteCard({ reloadKey }: { reloadKey: number }) {
  const { t } = useTranslation()
  const [taste, setTaste] = useState<Taste | null>(null)
  const [failed, setFailed] = useState(false)
  useEffect(() => {
    setTaste(null)
    api.taste().then(setTaste, () => setFailed(true))
  }, [reloadKey])

  const max = Math.max(1e-9, ...(taste?.topGenres ?? []).map((g) => g.weight))
  return (
    <Card title={t('privacy.taste')} description={t('privacy.tasteHint')} icon={<TrendingUp size={18} />}>
      {failed ? (
        <p className="text-sm text-muted">{t('errors.generic')}</p>
      ) : !taste ? (
        <div className="flex justify-center py-4 text-muted">
          <Spinner />
        </div>
      ) : taste.topGenres.length === 0 && taste.topTracks.length === 0 ? (
        <p className="text-sm text-muted">{t('privacy.tasteEmpty')}</p>
      ) : (
        <>
          <div className="space-y-2.5">
            <h3 className="text-[10px] font-semibold tracking-wide text-muted uppercase">{t('privacy.topGenres')}</h3>
            {taste.topGenres.slice(0, 8).map((g) => (
              <div key={g.genre} className="flex items-center gap-3">
                <span className="w-28 shrink-0 truncate text-sm">{g.genre}</span>
                <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-line">
                  <div
                    className="h-full rounded-full bg-sky-500 transition-[width] duration-700 ease-out"
                    style={{ width: `${(g.weight / max) * 100}%` }}
                  />
                </div>
              </div>
            ))}
          </div>
          <div>
            <h3 className="mb-1 text-[10px] font-semibold tracking-wide text-muted uppercase">{t('privacy.topTracks')}</h3>
            <ol className="-mx-4">
              {taste.topTracks.slice(0, 10).map((tr, i) => (
                <li key={tr.url} className="flex items-center gap-3 border-t border-line px-4 py-2 first:border-t-0">
                  <span className="w-4 text-right font-mono text-[11px] text-muted tabular-nums">{i + 1}</span>
                  <button type="button" onClick={() => openExternal(tr.url)} className="min-w-0 flex-1 text-left hover:text-sky-600 dark:hover:text-sky-400">
                    <div className="truncate text-sm font-medium">{tr.title}</div>
                    <div className="truncate text-xs text-muted">{tr.artists.join(', ')}</div>
                  </button>
                  <span className="font-mono text-[11px] text-muted tabular-nums">{t('privacy.plays', { count: tr.plays })}</span>
                </li>
              ))}
            </ol>
          </div>
        </>
      )}
    </Card>
  )
}

export function PrivacyScreen() {
  const { t } = useTranslation()
  const { aiEnabled, run, config } = useLynx()
  const [prefs, setPrefs] = useState<UserPrefs | null>(null)
  const [reloadKey, setReloadKey] = useState(0)

  useEffect(() => {
    api.prefs().then(setPrefs, () => setPrefs({ locale: null, historyEnabled: true }))
  }, [])

  if (!prefs) return <PageSpinner />

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-xl font-semibold tracking-tight">{t('privacy.title')}</h2>
        <p className="text-sm text-muted">{aiEnabled ? t('privacy.subtitle') : t('privacy.subtitleNoAi')}</p>
      </div>

      {aiEnabled && (
        <Card title={t('privacy.history')} description={t('privacy.historyHint')} icon={<History size={18} />}>
          <SettingRow
            label={t('privacy.historyToggle')}
            description={prefs.historyEnabled ? t('privacy.historyOn') : t('privacy.historyOff')}
          >
            <Toggle
              label={t('privacy.historyToggle')}
              checked={prefs.historyEnabled}
              onChange={async (historyEnabled) => {
                setPrefs({ ...prefs, historyEnabled })
                const res = await run(() => api.updatePrefs({ historyEnabled }))
                if (res === null) setPrefs(prefs)
              }}
            />
          </SettingRow>
        </Card>
      )}

      {aiEnabled && <TasteCard reloadKey={reloadKey} />}

      <Card title={t('privacy.data')} description={t('privacy.dataHint')} icon={<Database size={18} />}>
        <SettingRow label={t('privacy.delete')} description={aiEnabled ? t('privacy.deleteHint') : t('privacy.deleteHintNoAi')}>
          <ConfirmButton
            label={t('privacy.deleteButton')}
            question={t('privacy.deleteQuestion')}
            confirmLabel={t('privacy.deleteConfirm')}
            icon={<Trash2 size={12} />}
            onConfirm={async () => {
              const ok = await run(() => api.deleteData(), t('privacy.deleted'))
              if (ok !== null) setReloadKey((k) => k + 1)
            }}
          />
        </SettingRow>
        {config.links.privacy && (
          <button
            type="button"
            onClick={() => config.links.privacy && openExternal(config.links.privacy)}
            className="text-xs font-medium text-sky-600 hover:underline dark:text-sky-400"
          >
            {t('privacy.policy')}
          </button>
        )}
      </Card>
      {!aiEnabled && <Empty>{t('privacy.noHistoryNote')}</Empty>}
    </div>
  )
}
