import { Bot, Database, Download, Headphones, Music, Radio, RefreshCw, Server, Sparkles } from 'lucide-react'
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { Banner, Button, Card, Pill, Spinner } from '../components/ui'
import { adminApi } from '../lib/api'
import { useErrorText } from '../lib/errors'
import { formatBytes, formatDateTime, formatUptime } from '../lib/time'
import type { Overview } from '../lib/types'

const REFRESH_MS = 10_000

function Stat({ label, value, icon }: { label: string; value: string; icon?: ReactNode }) {
  return (
    <div className="min-w-0 rounded-xl bg-surface-2 px-3 py-2.5">
      <div className="flex items-center gap-1.5 text-xs text-muted">
        {icon}
        <span className="truncate">{label}</span>
      </div>
      <div className="mt-0.5 truncate text-lg font-semibold tabular-nums" title={value}>
        {value}
      </div>
    </div>
  )
}

function ServiceRow({ icon, name, detail, pill }: { icon: ReactNode; name: string; detail?: string | null; pill: ReactNode }) {
  return (
    <li className="flex items-center gap-2.5 py-2">
      <span className="text-muted">{icon}</span>
      <span className="min-w-0 flex-1">
        <span className="block text-sm font-medium">{name}</span>
        {detail && <span className="block truncate font-mono text-xs text-muted">{detail}</span>}
      </span>
      {pill}
    </li>
  )
}

export function OverviewPage() {
  const { t, i18n } = useTranslation()
  const errorText = useErrorText()
  const [data, setData] = useState<Overview | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      setData(await adminApi.overview())
      setError(null)
    } catch (e) {
      setError(errorText(e))
    } finally {
      setLoading(false)
    }
  }, [errorText])

  useEffect(() => {
    void load()
    const id = setInterval(() => document.visibilityState === 'visible' && void load(), REFRESH_MS)
    return () => clearInterval(id)
  }, [load])

  if (!data) return error ? <Banner tone="error">{error}</Banner> : <Spinner />
  const h = data.health
  const svc = h.services
  const locale = i18n.language
  const job = data.ytdlp.job
  const ytdlpDetail = [
    svc.ytdlp.version ?? data.ytdlp.version,
    data.ytdlp.updatesPaused ? t('overview.updatesPaused') : t('overview.updatesOn'),
    job.outcome ? t('overview.lastCheck', { outcome: t(`tweaks.outcome.${job.outcome}`, { version: job.version ?? '' }) }) : null,
  ]
    .filter(Boolean)
    .join(' · ')

  return (
    <div className="space-y-4">
      {error && <Banner tone="error">{error}</Banner>}
      {data.flags.maintenanceMode && <Banner tone="warning">{t('overview.maintenanceOn')}</Banner>}

      <Card
        title={
          <span className="flex items-center gap-2">
            Lynx <Pill tone={h.status}>{t(`status.${h.status}`)}</Pill>
          </span>
        }
        actions={
          <>
            <span className="hidden text-xs text-muted sm:inline">
              {t('overview.snapshotAt', { time: formatDateTime(data.snapshotAt, locale) })}
            </span>
            <Button variant="ghost" icon={<RefreshCw size={14} />} loading={loading} onClick={() => void load()}>
              {t('overview.refresh')}
            </Button>
          </>
        }
      >
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <Stat label={t('overview.version')} value={`v${h.version}`} />
          <Stat label={t('overview.commit')} value={h.commit ?? t('overview.unknown')} />
          <Stat label={t('overview.uptime')} value={formatUptime(h.uptimeSecs)} />
          <Stat label={t('overview.memory')} value={data.memoryBytes != null ? formatBytes(data.memoryBytes) : t('overview.unknown')} />
        </div>
      </Card>

      <Card title={t('overview.totals')}>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <Stat icon={<Server size={13} />} label={t('overview.servers')} value={String(data.totals.servers)} />
          <Stat icon={<Radio size={13} />} label={t('overview.playersActive')} value={String(data.totals.playersActive)} />
          <Stat icon={<Music size={13} />} label={t('overview.playersPlaying')} value={String(data.totals.playersPlaying)} />
          <Stat icon={<Headphones size={13} />} label={t('overview.listeners')} value={String(data.totals.listeners)} />
        </div>
      </Card>

      <section>
        <h2 className="mb-2 px-1 text-sm font-semibold tracking-tight">{t('overview.bots')}</h2>
        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
          {h.bots.map((b, i) => (
            <div key={b.index} className="rounded-2xl border border-line bg-surface p-4 shadow-xs">
              <div className="flex items-start gap-2">
                <span className="mt-0.5 rounded-lg bg-surface-2 p-1.5 text-muted">
                  <Bot size={16} />
                </span>
                <div className="min-w-0 flex-1">
                  <div className="text-sm font-semibold">{data.labels[i] ?? `#${b.index}`}</div>
                  <div className="truncate text-xs text-muted" title={b.name}>
                    {b.name}
                  </div>
                </div>
                <Pill tone={b.status}>{t(`status.${b.status}`)}</Pill>
              </div>
              <dl className="mt-3 grid grid-cols-2 gap-x-3 gap-y-1.5 text-xs">
                <dt className="text-muted">{t('overview.shards')}</dt>
                <dd className="text-right font-mono tabular-nums">
                  {b.shards.connected}/{b.shards.total}
                </dd>
                <dt className="text-muted">{t('overview.ping')}</dt>
                <dd className="text-right font-mono tabular-nums">{b.gatewayPingMs != null ? `${b.gatewayPingMs} ms` : '-'}</dd>
                <dt className="text-muted">{t('overview.guilds')}</dt>
                <dd className="text-right font-mono tabular-nums">{b.guilds}</dd>
                <dt className="text-muted">{t('overview.voiceSessions')}</dt>
                <dd className="text-right font-mono tabular-nums">{b.voiceSessions}</dd>
              </dl>
              {b.downSince && (
                <p className="mt-2 text-xs text-red-600 dark:text-red-400">
                  {t('overview.downSince', { time: formatDateTime(Date.parse(b.downSince), locale) })}
                </p>
              )}
            </div>
          ))}
        </div>
      </section>

      <Card title={t('overview.services')}>
        <ul className="divide-y divide-line">
          <ServiceRow
            icon={<Database size={16} />}
            name={t('overview.database')}
            detail={svc.database.latencyMs != null ? `${svc.database.latencyMs} ms` : null}
            pill={<Pill tone={svc.database.ok ? 'ok' : 'down'}>{t(svc.database.ok ? 'status.ok' : 'status.down')}</Pill>}
          />
          <ServiceRow
            icon={<Download size={16} />}
            name={t('overview.ytdlp')}
            detail={ytdlpDetail}
            pill={<Pill tone={svc.ytdlp.ok ? 'ok' : 'degraded'}>{t(svc.ytdlp.ok ? 'status.ok' : 'status.degraded')}</Pill>}
          />
          <ServiceRow
            icon={<Sparkles size={16} />}
            name={t('overview.ollama')}
            pill={
              svc.ollama ? (
                <Pill tone={svc.ollama.ok ? 'ok' : 'degraded'}>{t(svc.ollama.ok ? 'status.ok' : 'status.degraded')}</Pill>
              ) : (
                <Pill tone="neutral">{t('overview.notConfigured')}</Pill>
              )
            }
          />
        </ul>
      </Card>

      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        <Stat label={t('overview.noticesShown')} value={String(data.notices.visible)} />
        <Stat label={t('overview.noticesStored')} value={String(data.notices.total)} />
      </div>
    </div>
  )
}
