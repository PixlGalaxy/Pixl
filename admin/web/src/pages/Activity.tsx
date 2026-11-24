import { RefreshCw, Table2 } from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Columns } from '../components/Chart'
import { Banner, Button, Card, Spinner } from '../components/ui'
import { adminApi } from '../lib/api'
import { useErrorText } from '../lib/errors'
import type { Activity, ActivityWindow, CounterName, NamedCount, TimingSummary } from '../lib/types'

const REFRESH_MS = 30_000
/** Counters with a chart, in this order. */
const CHARTED: CounterName[] = ['commands', 'apiRequests', 'tracksStarted', 'tracksFailed', 'warnings', 'errors']
/** Counters shown as plain numbers too. */
const NUMBERS: CounterName[] = [
  'commands',
  'buttons',
  'autocomplete',
  'apiRequests',
  'tracksStarted',
  'tracksFailed',
  'searches',
  'resolves',
  'resolvesCached',
  'rateLimited',
  'warnings',
  'errors',
]

function time(ms: number, locale: string) {
  return new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit' }).format(new Date(ms))
}

function NamedTable({ title, rows, name }: { title: string; rows: NamedCount[]; name?: (n: string) => string }) {
  const { t } = useTranslation()
  const total = rows.reduce((a, r) => a + r.count, 0)
  return (
    <Card title={title}>
      {rows.length === 0 ? (
        <p className="text-sm text-muted">{t('activity.none')}</p>
      ) : (
        <table className="w-full text-left text-sm">
          <thead className="text-xs text-muted">
            <tr>
              <th className="py-1 font-medium">{t('activity.name')}</th>
              <th className="py-1 text-right font-medium">{t('activity.count')}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.name} className="border-t border-line">
                <td className="py-1.5">
                  <span className="relative">
                    {/* Share of the total as a quiet bar behind the name. */}
                    <span
                      aria-hidden
                      className="absolute inset-y-0 -left-1 rounded bg-accent-500/10"
                      style={{ width: `calc(${Math.round((r.count / Math.max(1, total)) * 100)}% + 0.5rem)` }}
                    />
                    <span className="relative font-mono text-xs">{name ? name(r.name) : r.name}</span>
                  </span>
                </td>
                <td className="py-1.5 text-right font-mono text-xs tabular-nums">{r.count}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  )
}

function TimingCard({ title, s }: { title: string; s: TimingSummary }) {
  const { t } = useTranslation()
  const ms = (v: number | null) => (v === null ? '-' : v >= 10_000 ? `${(v / 1000).toFixed(1)} s` : `${v} ms`)
  return (
    <div className="rounded-xl bg-surface-2 px-3 py-2.5">
      <div className="text-xs font-medium text-muted">{title}</div>
      <div className="mt-1 flex items-baseline gap-4">
        <div>
          <div className="text-lg font-semibold tabular-nums">{ms(s.avgMs)}</div>
          <div className="text-xs text-muted">{t('activity.avg')}</div>
        </div>
        <div>
          <div className="text-lg font-semibold tabular-nums">{ms(s.p95Ms)}</div>
          <div className="text-xs text-muted">{t('activity.p95')}</div>
        </div>
        <div>
          <div className="text-lg font-semibold tabular-nums">{s.count}</div>
          <div className="text-xs text-muted">{t('activity.samples')}</div>
        </div>
      </div>
    </div>
  )
}

function WindowView({ w, serverNow }: { w: ActivityWindow; serverNow: number }) {
  const { t, i18n } = useTranslation()
  const [table, setTable] = useState(false)
  const locale = i18n.language
  const stepMs = w.stepMinutes * 60_000
  // Start of point i (epoch ms): the window ends with the current minute.
  const endMinuteStart = Math.floor(serverNow / 60_000) * 60_000
  const firstStart = endMinuteStart - (w.windowMinutes - 1) * 60_000
  const at = (i: number) => firstStart + i * stepMs
  const tip = (name: string) => (i: number, v: number) =>
    w.stepMinutes === 1
      ? `${name}: ${t('activity.perMinute', { count: v, time: time(at(i), locale) })}`
      : `${name}: ${t('activity.perStep', { count: v, from: time(at(i), locale), to: time(at(i) + stepMs, locale) })}`
  const points = w.series.commands.length
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4 lg:grid-cols-6">
        {NUMBERS.map((c) => (
          <div key={c} className="rounded-xl bg-surface-2 px-3 py-2">
            <div className="truncate text-xs text-muted" title={t(`activity.counters.${c}`)}>
              {t(`activity.counters.${c}`)}
            </div>
            <div className="text-lg font-semibold tabular-nums">{w.totals[c]}</div>
          </div>
        ))}
      </div>
      <Card
        actions={
          <Button variant="ghost" icon={<Table2 size={14} />} onClick={() => setTable((x) => !x)}>
            {table ? t('activity.hideTable') : t('activity.showTable')}
          </Button>
        }
        title={w.stepMinutes === 1 ? t('activity.lastHour') : t('activity.lastDay')}
      >
        <div className="grid gap-x-6 gap-y-5 md:grid-cols-2">
          {CHARTED.map((c) => {
            const name = t(`activity.counters.${c}`)
            return (
              <div key={c}>
                <div className="mb-1.5 flex items-baseline justify-between gap-2">
                  <span className="text-xs font-medium">{name}</span>
                  <span className="font-mono text-xs text-muted tabular-nums">{w.totals[c]}</span>
                </div>
                <Columns values={w.series[c]} label={`${name}, ${w.stepMinutes === 1 ? t('activity.lastHour') : t('activity.lastDay')}`} tooltip={tip(name)} />
              </div>
            )
          })}
        </div>
        {table && (
          <div className="mt-4 max-h-80 overflow-auto rounded-lg border border-line">
            <table className="w-full text-left text-xs">
              <thead className="sticky top-0 bg-surface text-muted">
                <tr>
                  <th className="px-2 py-1.5 font-medium">{t('activity.time')}</th>
                  {CHARTED.map((c) => (
                    <th key={c} className="px-2 py-1.5 text-right font-medium">
                      {t(`activity.counters.${c}`)}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {Array.from({ length: points }, (_, i) => points - 1 - i).map((i) => (
                  <tr key={i} className="border-t border-line">
                    <td className="px-2 py-1 font-mono">{time(at(i), locale)}</td>
                    {CHARTED.map((c) => (
                      <td key={c} className="px-2 py-1 text-right font-mono tabular-nums">
                        {w.series[c][i] ?? 0}
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <Card title={t('activity.timings')}>
        <div className="grid gap-2 sm:grid-cols-2">
          <TimingCard title={t('activity.resolveTime')} s={w.resolve} />
          <TimingCard title={t('activity.startTime')} s={w.start} />
        </div>
      </Card>
      <div className="grid gap-4 lg:grid-cols-3">
        <NamedTable title={t('activity.byCommand')} rows={w.commands} name={(n) => (n === 'other' ? n : `/${n}`)} />
        <NamedTable
          title={t('activity.byButton')}
          rows={w.buttons}
          name={(n) => t(`activity.buttons.${n}`, { defaultValue: n })}
        />
        <NamedTable title={t('activity.byRoute')} rows={w.api} />
      </div>
    </div>
  )
}

export function ActivityPage() {
  const { t } = useTranslation()
  const errorText = useErrorText()
  const [data, setData] = useState<Activity | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [range, setRange] = useState<'hour' | 'day'>('hour')

  const load = useCallback(async () => {
    setLoading(true)
    try {
      setData(await adminApi.activity())
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

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <p className="min-w-0 flex-1 text-sm text-muted">{t('activity.intro')}</p>
        <div className="inline-flex rounded-lg border border-line bg-surface p-0.5" role="tablist">
          {(['hour', 'day'] as const).map((r) => (
            <button
              key={r}
              type="button"
              role="tab"
              aria-selected={range === r}
              onClick={() => setRange(r)}
              className={`h-7 rounded-md px-3 text-xs font-medium transition-colors ${range === r ? 'bg-accent-600 text-white' : 'text-muted hover:text-fg'}`}
            >
              {r === 'hour' ? t('activity.lastHour') : t('activity.lastDay')}
            </button>
          ))}
        </div>
        <Button variant="ghost" icon={<RefreshCw size={14} />} loading={loading} onClick={() => void load()}>
          {t('activity.refresh')}
        </Button>
      </div>
      {error && <Banner tone="error">{error}</Banner>}
      {!data ? !error && <Spinner /> : <WindowView w={data[range]} serverNow={data.serverNow} />}
    </div>
  )
}
