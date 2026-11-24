import { DatabaseZap, Download, Eraser } from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { adminApi } from '../lib/api'
import { useErrorText } from '../lib/errors'
import { formatDateTime } from '../lib/time'
import type { Flags, Tweaks } from '../lib/types'
import { Banner, Button, Card, Spinner, Toggle } from '../components/ui'

export function TweaksPage() {
  const { t, i18n } = useTranslation()
  const errorText = useErrorText()
  const [data, setData] = useState<Tweaks | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [info, setInfo] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setData(await adminApi.tweaks())
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }, [errorText])

  useEffect(() => {
    void load()
  }, [load])

  // Follow a running yt-dlp check until it finishes.
  const running = data?.ytdlp.job.running === true
  useEffect(() => {
    if (!running) return
    const id = setInterval(() => void load(), 2_000)
    return () => clearInterval(id)
  }, [running, load])

  const run = async (key: string, fn: () => Promise<void>) => {
    setBusy(key)
    setInfo(null)
    try {
      await fn()
      setError(null)
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(null)
    }
  }

  const setFlag = (k: keyof Flags, v: boolean) =>
    run(k, async () => {
      setData(await adminApi.setTweaks({ [k]: v }))
      setInfo(t('tweaks.saved'))
    })

  if (!data) return error ? <Banner tone="error">{error}</Banner> : <Spinner />
  const job = data.ytdlp.job
  return (
    <div className="space-y-4">
      <p className="text-sm text-muted">{t('tweaks.intro')}</p>
      {error && <Banner tone="error">{error}</Banner>}
      {info && <Banner tone="success">{info}</Banner>}
      <Card>
        <div className="space-y-4">
          <Toggle
            checked={data.flags.maintenanceMode}
            disabled={busy !== null}
            onChange={(v) => void setFlag('maintenanceMode', v)}
            label={t('tweaks.maintenance')}
            hint={t('tweaks.maintenanceHint')}
          />
          <Toggle
            checked={data.flags.ytdlpUpdatesPaused}
            disabled={busy !== null}
            onChange={(v) => void setFlag('ytdlpUpdatesPaused', v)}
            label={t('tweaks.pauseYtdlp')}
            hint={t('tweaks.pauseYtdlpHint')}
          />
        </div>
      </Card>
      <Card title={t('tweaks.ytdlpTitle')}>
        <p className="text-sm">
          {t('tweaks.ytdlpVersion')}: <span className="font-mono">{data.ytdlp.version ?? '-'}</span>
        </p>
        <div className="mt-3 flex flex-wrap items-center gap-3">
          <Button
            icon={<Download size={14} />}
            loading={running || busy === 'ytdlp'}
            onClick={() => void run('ytdlp', async () => setData(await adminApi.ytdlpUpdate()))}
          >
            {running ? t('tweaks.checking') : t('tweaks.checkNow')}
          </Button>
          {!running && job.outcome && (
            <span className="text-sm text-muted">
              {t(`tweaks.outcome.${job.outcome}`, { version: job.version ?? '' })}{' '}
              {job.finishedAt && t('tweaks.lastCheck', { time: formatDateTime(job.finishedAt, i18n.language) })}
            </span>
          )}
        </div>
      </Card>
      <Card title={t('tweaks.caches')}>
        <div className="space-y-4">
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <p className="flex-1 text-sm text-muted">{t('tweaks.clearStreamHint')}</p>
            <Button
              icon={<Eraser size={14} />}
              loading={busy === 'stream'}
              onClick={() =>
                void run('stream', async () => {
                  const r = await adminApi.clearStreamCache()
                  setInfo(t('tweaks.cleared', { count: r.removed }))
                })
              }
            >
              {t('tweaks.clearStream')}
            </Button>
          </div>
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <p className="flex-1 text-sm text-muted">{t('tweaks.clearLookupHint')}</p>
            <Button
              icon={<DatabaseZap size={14} />}
              loading={busy === 'lookup'}
              onClick={() => {
                if (!window.confirm(t('tweaks.confirmLookup'))) return
                void run('lookup', async () => {
                  const r = await adminApi.clearLookupCache()
                  setInfo(t('tweaks.cleared', { count: r.removed }))
                })
              }}
            >
              {t('tweaks.clearLookup')}
            </Button>
          </div>
        </div>
      </Card>
    </div>
  )
}
