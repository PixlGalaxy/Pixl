import { RefreshCw } from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { adminApi } from './api'
import { useErrorText } from './errors'
import { formatBytes, formatDateTime, formatUptime } from './time'
import type { Overview } from './types'
import { Banner, Button, Card, Pill, Spinner } from './ui'

const REFRESH_MS = 15_000

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0 rounded-xl bg-surface-2 px-3 py-2">
      <div className="text-xs text-muted">{label}</div>
      <div className="truncate text-sm font-medium" title={value}>
        {value}
      </div>
    </div>
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
          <Button variant="ghost" icon={<RefreshCw size={14} />} loading={loading} onClick={() => void load()}>
            {t('overview.refresh')}
          </Button>
        }
      >
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-5">
          <Stat label={t('overview.version')} value={`v${h.version}`} />
          <Stat label={t('overview.commit')} value={h.commit ?? t('overview.unknown')} />
          <Stat label={t('overview.uptime')} value={formatUptime(h.uptimeSecs)} />
          <Stat
            label={t('overview.memory')}
            value={data.memoryBytes != null ? formatBytes(data.memoryBytes) : t('overview.unknown')}
          />
          <Stat label={t('overview.checkedAt')} value={formatDateTime(Date.parse(h.checkedAt), i18n.language)} />
        </div>
      </Card>

      <Card title={t('overview.bots')}>
        <div className="-mx-1 overflow-x-auto">
          <table className="w-full min-w-[36rem] text-left text-sm">
            <thead className="text-xs text-muted">
              <tr>
                <th className="px-1 py-1.5 font-medium">{t('overview.bot')}</th>
                <th className="px-1 py-1.5 font-medium">{t('overview.role')}</th>
                <th className="px-1 py-1.5 font-medium" />
                <th className="px-1 py-1.5 font-medium">{t('overview.shards')}</th>
                <th className="px-1 py-1.5 font-medium">{t('overview.ping')}</th>
                <th className="px-1 py-1.5 font-medium">{t('overview.guilds')}</th>
                <th className="px-1 py-1.5 font-medium">{t('overview.voiceSessions')}</th>
              </tr>
            </thead>
            <tbody>
              {h.bots.map((b) => (
                <tr key={b.index} className="border-t border-line">
                  <td className="px-1 py-2 font-medium">
                    <span className="mr-1 font-mono text-xs text-muted">#{b.index}</span>
                    {b.name}
                  </td>
                  <td className="px-1 py-2">{t(b.role === 'main' ? 'overview.main' : 'overview.voice')}</td>
                  <td className="px-1 py-2">
                    <Pill tone={b.status}>{t(`status.${b.status}`)}</Pill>
                  </td>
                  <td className="px-1 py-2 font-mono text-xs">
                    {b.shards.connected}/{b.shards.total}
                  </td>
                  <td className="px-1 py-2 font-mono text-xs">{b.gatewayPingMs != null ? `${b.gatewayPingMs} ms` : '-'}</td>
                  <td className="px-1 py-2">{b.guilds}</td>
                  <td className="px-1 py-2">{b.voiceSessions}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Card>

      <div className="grid gap-4 md:grid-cols-2">
        <Card title={t('overview.services')}>
          <ul className="space-y-2 text-sm">
            <li className="flex items-center gap-2">
              <span className="flex-1">{t('overview.database')}</span>
              {svc.database.latencyMs != null && <span className="font-mono text-xs text-muted">{svc.database.latencyMs} ms</span>}
              <Pill tone={svc.database.ok ? 'ok' : 'down'}>{t(svc.database.ok ? 'status.ok' : 'status.down')}</Pill>
            </li>
            <li className="flex items-center gap-2">
              <span className="flex-1">{t('overview.ytdlp')}</span>
              {svc.ytdlp.version && <span className="font-mono text-xs text-muted">{svc.ytdlp.version}</span>}
              <Pill tone={svc.ytdlp.ok ? 'ok' : 'degraded'}>{t(svc.ytdlp.ok ? 'status.ok' : 'status.degraded')}</Pill>
            </li>
            <li className="flex items-center gap-2">
              <span className="flex-1">{t('overview.ollama')}</span>
              {svc.ollama ? (
                <Pill tone={svc.ollama.ok ? 'ok' : 'degraded'}>{t(svc.ollama.ok ? 'status.ok' : 'status.degraded')}</Pill>
              ) : (
                <Pill tone="neutral">{t('overview.notConfigured')}</Pill>
              )}
            </li>
          </ul>
        </Card>
        <Card title={t('overview.players')}>
          <div className="grid grid-cols-2 gap-2">
            <Stat label={t('overview.active')} value={String(h.players.active)} />
            <Stat label={t('overview.playing')} value={String(h.players.playing)} />
            <Stat label={t('overview.noticesShown')} value={String(data.notices.visible)} />
            <Stat label={t('overview.noticesStored')} value={String(data.notices.total)} />
          </div>
        </Card>
      </div>
    </div>
  )
}
