import { Pencil, Plus, Trash2 } from 'lucide-react'
import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { adminApi } from '../lib/api'
import { useErrorText } from '../lib/errors'
import { formatDateTime, fromLocalInput, noticeState, toLocalInput } from '../lib/time'
import type { Notice, NoticeInput, NoticeKind } from '../lib/types'
import { Banner, Button, Card, Field, inputCls, Pill, Spinner, Toggle } from '../components/ui'

const MAX_TITLE = 100
const MAX_MESSAGE = 500
const MAX_MINUTES = 7 * 24 * 60
const KINDS: NoticeKind[] = ['maintenance', 'info', 'warning']

type Draft = {
  kind: NoticeKind
  title: string
  message: string
  start: string
  mode: 'duration' | 'end'
  minutes: string
  end: string
  showFrom: string
  active: boolean
}

function emptyDraft(): Draft {
  // Next full hour, 10 minutes long, announced right away.
  const start = Math.ceil((Date.now() + 60_000) / 3_600_000) * 3_600_000
  return {
    kind: 'maintenance',
    title: '',
    message: '',
    start: toLocalInput(start),
    mode: 'duration',
    minutes: '10',
    end: toLocalInput(start + 10 * 60_000),
    showFrom: '',
    active: true,
  }
}

function draftOf(n: Notice): Draft {
  const minutes = Math.round((n.endsAt - n.startsAt) / 60_000)
  return {
    kind: n.kind,
    title: n.title,
    message: n.message,
    start: toLocalInput(n.startsAt),
    mode: 'duration',
    minutes: String(minutes),
    end: toLocalInput(n.endsAt),
    showFrom: toLocalInput(n.showFrom),
    active: n.active,
  }
}

/** The request body, or the key of the first problem. */
function toInput(d: Draft): NoticeInput | string {
  const startsAt = fromLocalInput(d.start)
  if (startsAt === null) return 'notices.invalidStart'
  const base = { kind: d.kind, title: d.title.trim(), message: d.message.trim(), startsAt, active: d.active }
  const showFrom = d.showFrom ? fromLocalInput(d.showFrom) : null
  const extra = showFrom !== null ? { showFrom } : {}
  if (d.mode === 'duration') {
    const minutes = Number(d.minutes)
    if (!Number.isInteger(minutes) || minutes < 1 || minutes > MAX_MINUTES) return 'notices.invalidMinutes'
    return { ...base, ...extra, durationMinutes: minutes }
  }
  const endsAt = fromLocalInput(d.end)
  if (endsAt === null || endsAt <= startsAt) return 'notices.invalidEnd'
  return { ...base, ...extra, endsAt }
}

const STATE_TONE = { active: 'accent', upcoming: 'accent', scheduled: 'neutral', ended: 'neutral', off: 'neutral' } as const

function NoticeForm({
  initial,
  editing,
  onDone,
  onCancel,
}: {
  initial: Draft
  editing: number | null
  onDone: (list: Notice[]) => void
  onCancel: () => void
}) {
  const { t } = useTranslation()
  const errorText = useErrorText()
  const [d, setD] = useState<Draft>(initial)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const set = <K extends keyof Draft>(k: K, v: Draft[K]) => setD((x) => ({ ...x, [k]: v }))
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    const input = toInput(d)
    if (typeof input === 'string') return setError(t(input))
    setSaving(true)
    try {
      const res = editing === null ? await adminApi.createNotice(input) : await adminApi.updateNotice(editing, input)
      onDone(res.notices)
    } catch (err) {
      setError(errorText(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <form onSubmit={(e) => void submit(e)} className="space-y-3">
      <div className="grid gap-3 sm:grid-cols-[12rem_1fr]">
        <Field label={t('notices.kind')}>
          <select className={inputCls} value={d.kind} onChange={(e) => set('kind', e.target.value as NoticeKind)}>
            {KINDS.map((k) => (
              <option key={k} value={k}>
                {t(`notices.kinds.${k}`)}
              </option>
            ))}
          </select>
        </Field>
        <Field label={t('notices.title')} hint={t('notices.chars', { count: d.title.length, max: MAX_TITLE })}>
          <input
            className={inputCls}
            value={d.title}
            maxLength={MAX_TITLE}
            placeholder={t('notices.titleHint')}
            onChange={(e) => set('title', e.target.value)}
          />
        </Field>
      </div>
      <Field
        label={t('notices.message')}
        hint={`${t('notices.messageHint')} ${t('notices.chars', { count: d.message.length, max: MAX_MESSAGE })}`}
      >
        <textarea
          className={`${inputCls} h-auto min-h-20 py-2`}
          value={d.message}
          maxLength={MAX_MESSAGE}
          rows={3}
          onChange={(e) => set('message', e.target.value)}
        />
      </Field>
      <div className="grid gap-3 sm:grid-cols-3">
        <Field label={t('notices.start')}>
          <input type="datetime-local" className={inputCls} value={d.start} onChange={(e) => set('start', e.target.value)} required />
        </Field>
        <Field label={t('notices.length')}>
          <div className="flex gap-2">
            <select
              className={`${inputCls} w-auto`}
              value={d.mode}
              onChange={(e) => set('mode', e.target.value as Draft['mode'])}
              aria-label={t('notices.length')}
            >
              <option value="duration">{t('notices.byDuration')}</option>
              <option value="end">{t('notices.byEnd')}</option>
            </select>
            {d.mode === 'duration' ? (
              <input
                type="number"
                min={1}
                max={MAX_MINUTES}
                className={inputCls}
                value={d.minutes}
                aria-label={t('notices.minutes')}
                onChange={(e) => set('minutes', e.target.value)}
              />
            ) : (
              <input
                type="datetime-local"
                className={inputCls}
                value={d.end}
                aria-label={t('notices.end')}
                onChange={(e) => set('end', e.target.value)}
              />
            )}
          </div>
        </Field>
        <Field label={t('notices.showFrom')} hint={t('notices.showFromHint')}>
          <input type="datetime-local" className={inputCls} value={d.showFrom} onChange={(e) => set('showFrom', e.target.value)} />
        </Field>
      </div>
      <p className="text-xs text-muted">{t('notices.timezone', { zone })}</p>
      <Toggle checked={d.active} onChange={(v) => set('active', v)} label={t('notices.active')} hint={t('notices.activeHint')} />
      {error && <Banner tone="error">{error}</Banner>}
      <div className="flex gap-2">
        <Button type="submit" variant="primary" loading={saving}>
          {editing === null ? t('notices.create') : t('notices.save')}
        </Button>
        <Button variant="ghost" onClick={onCancel}>
          {t('notices.cancel')}
        </Button>
      </div>
    </form>
  )
}

export function NoticesPage() {
  const { t, i18n } = useTranslation()
  const errorText = useErrorText()
  const [list, setList] = useState<Notice[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [info, setInfo] = useState<string | null>(null)
  const [form, setForm] = useState<{ id: number | null; draft: Draft } | null>(null)
  const [now, setNow] = useState(() => Date.now())

  const load = useCallback(async () => {
    try {
      setList((await adminApi.notices()).notices)
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }, [errorText])

  useEffect(() => {
    void load()
    const id = setInterval(() => setNow(Date.now()), 30_000)
    return () => clearInterval(id)
  }, [load])

  const remove = async (n: Notice) => {
    if (!window.confirm(t('notices.confirmDelete'))) return
    try {
      setList((await adminApi.deleteNotice(n.id)).notices)
      setInfo(t('notices.deleted'))
      if (form?.id === n.id) setForm(null)
    } catch (e) {
      setError(errorText(e))
    }
  }

  const locale = i18n.language
  return (
    <div className="space-y-4">
      <p className="text-sm text-muted">{t('notices.intro')}</p>
      {error && <Banner tone="error">{error}</Banner>}
      {info && !form && <Banner tone="success">{info}</Banner>}
      {form ? (
        <Card title={form.id === null ? t('notices.new') : t('notices.edit')}>
          <NoticeForm
            key={form.id ?? 'new'}
            initial={form.draft}
            editing={form.id}
            onCancel={() => setForm(null)}
            onDone={(l) => {
              setList(l)
              setForm(null)
              setInfo(t('notices.saved'))
            }}
          />
        </Card>
      ) : (
        <Button
          variant="primary"
          icon={<Plus size={14} />}
          onClick={() => {
            setInfo(null)
            setForm({ id: null, draft: emptyDraft() })
          }}
        >
          {t('notices.new')}
        </Button>
      )}
      {list === null ? (
        !error && <Spinner />
      ) : list.length === 0 ? (
        <p className="text-sm text-muted">{t('notices.empty')}</p>
      ) : (
        <ul className="space-y-2">
          {list.map((n) => {
            const state = noticeState(n, now)
            return (
              <li key={n.id} className="rounded-2xl border border-line bg-surface p-4 shadow-xs">
                <div className="flex flex-wrap items-center gap-2">
                  <Pill tone={STATE_TONE[state]}>{t(`notices.states.${state}`)}</Pill>
                  <span className="text-xs font-medium text-muted">{t(`notices.kinds.${n.kind}`)}</span>
                  <div className="ml-auto flex gap-1">
                    <Button
                      variant="ghost"
                      icon={<Pencil size={14} />}
                      onClick={() => {
                        setInfo(null)
                        setForm({ id: n.id, draft: draftOf(n) })
                      }}
                    >
                      {t('notices.edit')}
                    </Button>
                    <Button variant="danger" icon={<Trash2 size={14} />} onClick={() => void remove(n)}>
                      {t('notices.delete')}
                    </Button>
                  </div>
                </div>
                {n.title && <h3 className="mt-2 text-sm font-semibold break-words">{n.title}</h3>}
                {n.message && <p className="mt-1 text-sm break-words whitespace-pre-line text-fg/90">{n.message}</p>}
                <p className="mt-2 text-xs text-muted">
                  {t('notices.until', { start: formatDateTime(n.startsAt, locale), end: formatDateTime(n.endsAt, locale) })}
                  <span className="px-1.5">·</span>
                  {t('notices.from', { time: formatDateTime(n.showFrom, locale) })}
                </p>
              </li>
            )
          })}
        </ul>
      )}
    </div>
  )
}
