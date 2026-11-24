import { Download, Pause, Play, Search, Trash2 } from 'lucide-react'
import { useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Banner, Button, inputCls, Pill } from '../components/ui'
import { adminApi, streamLogs } from '../lib/api'
import { useErrorText } from '../lib/errors'
import { filterLines, type LogFilter } from '../lib/logs'
import type { LogLevel, LogLine } from '../lib/types'

/** Lines kept in the browser (the server keeps its own ring). */
const KEEP = 5000
const LEVELS: LogLevel[] = ['error', 'warn', 'info', 'debug', 'trace']

const LEVEL_CLS: Record<LogLevel, string> = {
  error: 'text-red-600 dark:text-red-400',
  warn: 'text-amber-600 dark:text-amber-400',
  info: 'text-sky-600 dark:text-sky-400',
  debug: 'text-violet-600 dark:text-violet-400',
  trace: 'text-muted',
}

function stamp(ms: number) {
  const d = new Date(ms)
  const p = (n: number, w = 2) => String(n).padStart(w, '0')
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`
}

export function LogsPage() {
  const { t } = useTranslation()
  const errorText = useErrorText()
  const [lines, setLines] = useState<LogLine[]>([])
  const [error, setError] = useState<string | null>(null)
  const [online, setOnline] = useState(false)
  const [lagged, setLagged] = useState(0)
  const [paused, setPaused] = useState(false)
  const [filter, setFilter] = useState<LogFilter>({ levels: new Set(['error', 'warn', 'info', 'debug']), target: '', text: '' })
  const lastSeq = useRef(0)
  const clearedAt = useRef(0)
  const scroller = useRef<HTMLDivElement>(null)
  // Incoming lines are batched per animation frame: a burst never re-renders per line.
  const pending = useRef<LogLine[]>([])
  const frame = useRef(0)

  useEffect(() => {
    let close: (() => void) | null = null
    let cancelled = false
    const flush = () => {
      frame.current = 0
      const add = pending.current
      pending.current = []
      if (add.length) setLines((old) => [...old, ...add].slice(-KEEP))
    }
    const push = (l: LogLine) => {
      if (l.seq <= lastSeq.current || l.seq <= clearedAt.current) return
      lastSeq.current = l.seq
      pending.current.push(l)
      if (!frame.current) frame.current = requestAnimationFrame(flush)
    }
    adminApi
      .logs(0)
      .then((r) => {
        if (cancelled) return
        r.lines.forEach(push)
        close = streamLogs(lastSeq.current, {
          line: push,
          lagged: (n) => setLagged((x) => x + n),
          open: setOnline,
        })
      })
      .catch((e) => !cancelled && setError(errorText(e)))
    return () => {
      cancelled = true
      close?.()
      if (frame.current) cancelAnimationFrame(frame.current)
    }
  }, [errorText])

  const targets = useMemo(() => [...new Set(lines.map((l) => l.target))].sort(), [lines])
  const shown = useMemo(() => filterLines(lines, filter), [lines, filter])

  useEffect(() => {
    if (paused) return
    const el = scroller.current
    if (el) el.scrollTop = el.scrollHeight
  }, [shown, paused])

  const toggleLevel = (l: LogLevel) =>
    setFilter((f) => {
      const levels = new Set(f.levels)
      if (levels.has(l)) levels.delete(l)
      else levels.add(l)
      return { ...f, levels }
    })

  const download = () => {
    const text = shown.map((l) => `${new Date(l.ts).toISOString()} ${l.level.toUpperCase().padEnd(5)} ${l.target}: ${l.text}`).join('\n')
    const url = URL.createObjectURL(new Blob([text + '\n'], { type: 'text/plain' }))
    const a = document.createElement('a')
    a.href = url
    a.download = `lynx-logs-${new Date().toISOString().replace(/[:.]/g, '-')}.txt`
    a.click()
    setTimeout(() => URL.revokeObjectURL(url), 1000)
  }

  return (
    <div className="flex h-[calc(100vh-7rem)] min-h-[28rem] flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <p className="min-w-0 flex-1 text-sm text-muted">{t('logs.intro', { count: 2000 })}</p>
        <Pill tone={online ? 'ok' : 'degraded'}>{online ? t('logs.live') : t('logs.offline')}</Pill>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex flex-wrap gap-1">
          {LEVELS.map((l) => (
            <button
              key={l}
              type="button"
              aria-pressed={filter.levels.has(l)}
              onClick={() => toggleLevel(l)}
              className={`h-8 rounded-lg border px-2.5 text-xs font-medium transition-colors ${filter.levels.has(l) ? 'border-accent-500/40 bg-accent-500/10 text-fg' : 'border-line text-muted hover:text-fg'}`}
            >
              <span className={LEVEL_CLS[l]}>{t(`logs.levels.${l}`)}</span>
            </button>
          ))}
        </div>
        <select
          className={`${inputCls} h-8 w-auto max-w-56`}
          value={filter.target}
          onChange={(e) => setFilter((f) => ({ ...f, target: e.target.value }))}
          aria-label={t('logs.allTargets')}
        >
          <option value="">{t('logs.allTargets')}</option>
          {targets.map((x) => (
            <option key={x} value={x}>
              {x}
            </option>
          ))}
        </select>
        <label className="relative min-w-40 flex-1">
          <Search size={14} className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-muted" />
          <input
            className={`${inputCls} h-8 pl-8`}
            placeholder={t('logs.filterText')}
            value={filter.text}
            onChange={(e) => setFilter((f) => ({ ...f, text: e.target.value }))}
          />
        </label>
        <Button icon={paused ? <Play size={14} /> : <Pause size={14} />} onClick={() => setPaused((p) => !p)}>
          {paused ? t('logs.resume') : t('logs.pause')}
        </Button>
        <Button
          icon={<Trash2 size={14} />}
          onClick={() => {
            clearedAt.current = lastSeq.current
            setLines([])
            setLagged(0)
          }}
        >
          {t('logs.clear')}
        </Button>
        <Button icon={<Download size={14} />} onClick={download} disabled={shown.length === 0}>
          {t('logs.download')}
        </Button>
      </div>
      {error && <Banner tone="error">{error}</Banner>}
      {lagged > 0 && <Banner tone="warning">{t('logs.lagged', { count: lagged })}</Banner>}
      <div
        ref={scroller}
        className="min-h-0 flex-1 overflow-auto rounded-2xl border border-line bg-surface p-3 font-mono text-xs leading-5"
        onWheel={(e) => e.deltaY < 0 && !paused && setPaused(true)}
      >
        {shown.length === 0 ? (
          <p className="text-muted">{t('logs.empty')}</p>
        ) : (
          shown.map((l) => (
            <div key={l.seq} className="break-words whitespace-pre-wrap hover:bg-surface-2">
              <span className="text-muted">{stamp(l.ts)}</span>{' '}
              <span className={`inline-block w-11 font-semibold uppercase ${LEVEL_CLS[l.level]}`}>{l.level}</span>{' '}
              <span className="text-muted">{l.target}</span> <span>{l.text}</span>
            </div>
          ))
        )}
      </div>
      <div className="flex items-center justify-between text-xs text-muted">
        <span>{t('logs.shown', { shown: shown.length, total: lines.length })}</span>
        {paused && <span>{t('logs.paused')}</span>}
      </div>
    </div>
  )
}
