import { RefreshCw, RotateCcw, Save } from 'lucide-react'
import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { adminApi } from '../lib/api'
import {
  fromDraft,
  formatInZone,
  MAX_DURATION_MINUTES,
  MAX_INTERVAL_MINUTES,
  MAX_LEAD_HOURS,
  MIN_INTERVAL_MINUTES,
  timeZones,
  toDraft,
  weekdayName,
  WEEKDAYS,
  type AutoUpdateDraft,
  type LeadChoice,
} from '../lib/autoUpdate'
import { useErrorText } from '../lib/errors'
import { formatDateTime } from '../lib/time'
import type { AutoUpdate, UpdateTemplate } from '../lib/types'
import { Banner, Button, Card, Field, inputCls, Pill, Spinner, Toggle } from '../components/ui'

const MAX_TITLE = 100
const MAX_MESSAGE = 500

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 py-1.5">
      <dt className="w-44 shrink-0 text-xs font-medium text-muted">{label}</dt>
      <dd className="min-w-0 text-sm break-words">{children}</dd>
    </div>
  )
}

function TemplateFields({
  label,
  value,
  onChange,
}: {
  label: string
  value: UpdateTemplate
  onChange: (v: UpdateTemplate) => void
}) {
  const { t } = useTranslation()
  return (
    <fieldset className="space-y-2 rounded-xl border border-line p-3">
      <legend className="px-1 text-xs font-medium text-muted">{label}</legend>
      <Field label={t('notices.title')} hint={t('notices.chars', { count: value.title.length, max: MAX_TITLE })}>
        <input
          className={inputCls}
          value={value.title}
          maxLength={MAX_TITLE}
          onChange={(e) => onChange({ ...value, title: e.target.value })}
        />
      </Field>
      <Field label={t('notices.message')} hint={t('notices.chars', { count: value.message.length, max: MAX_MESSAGE })}>
        <textarea
          className={`${inputCls} h-auto min-h-16 py-2`}
          value={value.message}
          maxLength={MAX_MESSAGE}
          rows={2}
          onChange={(e) => onChange({ ...value, message: e.target.value })}
        />
      </Field>
    </fieldset>
  )
}

/** Settings and status of the automatic "scheduled update" notices. `onChange`: the notice list may have changed. */
export function AutoUpdateCard({ onChange }: { onChange: () => void }) {
  const { t, i18n } = useTranslation()
  const errorText = useErrorText()
  const [data, setData] = useState<AutoUpdate | null>(null)
  const [draft, setDraft] = useState<AutoUpdateDraft | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [info, setInfo] = useState<string | null>(null)
  const [busy, setBusy] = useState<'save' | 'check' | null>(null)
  const zones = useMemo(() => timeZones(), [])
  const localZone = Intl.DateTimeFormat().resolvedOptions().timeZone
  const locale = i18n.language

  const load = useCallback(
    async (resetDraft: boolean) => {
      try {
        const d = await adminApi.autoUpdate()
        setData(d)
        if (resetDraft) setDraft(toDraft(d.settings))
        setError(null)
      } catch (e) {
        setError(errorText(e))
      }
    },
    [errorText],
  )

  useEffect(() => {
    void load(true)
  }, [load])

  // Follow a running check until it finishes, then refresh the notice list.
  const checking = data?.status.checking === true
  useEffect(() => {
    if (!checking) return
    const id = setInterval(() => void load(false), 2_000)
    return () => {
      clearInterval(id)
      onChange()
    }
  }, [checking, load, onChange])

  const set = <K extends keyof AutoUpdateDraft>(k: K, v: AutoUpdateDraft[K]) =>
    setDraft((d) => (d ? { ...d, [k]: v } : d))

  const save = async (e: FormEvent) => {
    e.preventDefault()
    if (!draft) return
    const body = fromDraft(draft)
    if (typeof body === 'string') return setError(t(body))
    setBusy('save')
    setInfo(null)
    try {
      const d = await adminApi.setAutoUpdate(body)
      setData(d)
      setDraft(toDraft(d.settings))
      setError(null)
      setInfo(t('autoUpdate.saved'))
      onChange()
    } catch (err) {
      setError(errorText(err))
    } finally {
      setBusy(null)
    }
  }

  const checkNow = async () => {
    setBusy('check')
    setInfo(null)
    try {
      setData(await adminApi.checkAutoUpdate())
      setError(null)
    } catch (err) {
      setError(errorText(err))
    } finally {
      setBusy(null)
    }
  }

  if (!data || !draft)
    return <Card title={t('autoUpdate.title')}>{error ? <Banner tone="error">{error}</Banner> : <Spinner />}</Card>

  const st = data.status
  const unknown = <span className="text-muted">{t('autoUpdate.unknown')}</span>
  return (
    <Card
      title={t('autoUpdate.title')}
      actions={
        <Button
          icon={<RefreshCw size={14} />}
          loading={busy === 'check' || checking}
          onClick={() => void checkNow()}
        >
          {checking ? t('autoUpdate.checking') : t('autoUpdate.checkNow')}
        </Button>
      }
    >
      <div className="space-y-4">
        <p className="text-sm text-muted">{t('autoUpdate.intro')}</p>
        {error && <Banner tone="error">{error}</Banner>}
        {info && <Banner tone="success">{info}</Banner>}

        <section>
          <h3 className="mb-1 text-xs font-semibold tracking-wide text-muted uppercase">{t('autoUpdate.status')}</h3>
          <dl className="divide-y divide-line">
            <Row label={t('autoUpdate.running')}>
              {data.runningCommit ? <code className="font-mono">{data.runningCommit}</code> : unknown}
            </Row>
            <Row label={t('autoUpdate.published')}>
              <span className="inline-flex flex-wrap items-center gap-2">
                {st.publishedCommit ? <code className="font-mono">{st.publishedCommit}</code> : unknown}
                {st.updateAvailable === true && <Pill tone="accent">{t('autoUpdate.updateAvailable')}</Pill>}
                {st.updateAvailable === false && <Pill tone="ok">{t('autoUpdate.upToDate')}</Pill>}
              </span>
            </Row>
            <Row label={t('autoUpdate.checkedAt')}>
              {st.checkedAt ? formatDateTime(st.checkedAt, locale) : <span className="text-muted">{t('autoUpdate.never')}</span>}
            </Row>
            {st.lastError && (
              <Row label={t('autoUpdate.lastError')}>
                <span className="font-mono text-xs text-red-700 dark:text-red-300">{st.lastError}</span>
              </Row>
            )}
            <Row label={t('autoUpdate.nextWindow')}>
              {data.nextWindow ? (
                <span className="block space-y-0.5">
                  <span className="block">
                    {t('autoUpdate.local', { zone: localZone })}: {formatInZone(data.nextWindow, locale, localZone)}
                  </span>
                  <span className="block text-muted">
                    {t('autoUpdate.utc')}: {formatInZone(data.nextWindow, locale, 'UTC')}
                  </span>
                  {data.settings.timezone !== localZone && data.settings.timezone !== 'UTC' && (
                    <span className="block text-muted">
                      {data.settings.timezone}: {formatInZone(data.nextWindow, locale, data.settings.timezone)}
                    </span>
                  )}
                </span>
              ) : (
                unknown
              )}
            </Row>
          </dl>
          {!data.runningCommit && (
            <div className="mt-2">
              <Banner tone="warning">{t('autoUpdate.devBuild')}</Banner>
            </div>
          )}
        </section>

        <form onSubmit={(e) => void save(e)} className="space-y-3 border-t border-line pt-4">
          <Toggle
            checked={draft.enabled}
            onChange={(v) => set('enabled', v)}
            label={t('autoUpdate.enabled')}
            hint={t('autoUpdate.enabledHint')}
          />
          <Field label={t('autoUpdate.image')} hint={t('autoUpdate.imageHint')}>
            <input
              className={`${inputCls} font-mono`}
              value={draft.image}
              maxLength={300}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => set('image', e.target.value)}
            />
          </Field>

          <h3 className="pt-1 text-xs font-semibold tracking-wide text-muted uppercase">{t('autoUpdate.schedule')}</h3>
          <div className="grid gap-3 sm:grid-cols-4">
            <Field label={t('autoUpdate.frequency')}>
              <select
                className={inputCls}
                value={draft.frequency}
                onChange={(e) => set('frequency', e.target.value as AutoUpdateDraft['frequency'])}
              >
                <option value="daily">{t('autoUpdate.daily')}</option>
                <option value="weekly">{t('autoUpdate.weekly')}</option>
              </select>
            </Field>
            {draft.frequency === 'weekly' && (
              <Field label={t('autoUpdate.weekday')}>
                <select
                  className={inputCls}
                  value={draft.weekday}
                  onChange={(e) => set('weekday', e.target.value as AutoUpdateDraft['weekday'])}
                >
                  {WEEKDAYS.map((d) => (
                    <option key={d} value={d}>
                      {weekdayName(d, locale)}
                    </option>
                  ))}
                </select>
              </Field>
            )}
            <Field label={t('autoUpdate.time')}>
              <input type="time" className={inputCls} value={draft.time} onChange={(e) => set('time', e.target.value)} required />
            </Field>
            <Field label={t('autoUpdate.timezone')}>
              <input
                className={inputCls}
                value={draft.timezone}
                list="lynx-timezones"
                spellCheck={false}
                autoComplete="off"
                onChange={(e) => set('timezone', e.target.value)}
                required
              />
              <datalist id="lynx-timezones">
                {zones.map((z) => (
                  <option key={z} value={z} />
                ))}
              </datalist>
            </Field>
          </div>
          <p className="text-xs text-muted">{t('autoUpdate.timezoneHint')}</p>

          <div className="grid gap-3 sm:grid-cols-3">
            <Field label={t('autoUpdate.lead')}>
              <div className="flex gap-2">
                <select
                  className={draft.lead === 'custom' ? `${inputCls} w-auto` : inputCls}
                  value={draft.lead}
                  aria-label={t('autoUpdate.lead')}
                  onChange={(e) => set('lead', e.target.value as LeadChoice)}
                >
                  <option value="24">{t('autoUpdate.lead24')}</option>
                  <option value="72">{t('autoUpdate.lead72')}</option>
                  <option value="custom">{t('autoUpdate.leadCustom')}</option>
                </select>
                {draft.lead === 'custom' && (
                  <input
                    type="number"
                    min={1}
                    max={MAX_LEAD_HOURS}
                    className={inputCls}
                    value={draft.leadHours}
                    aria-label={t('autoUpdate.leadHours')}
                    title={t('autoUpdate.leadHours')}
                    onChange={(e) => set('leadHours', e.target.value)}
                  />
                )}
              </div>
            </Field>
            <Field label={t('autoUpdate.duration')}>
              <input
                type="number"
                min={1}
                max={MAX_DURATION_MINUTES}
                className={inputCls}
                value={draft.durationMinutes}
                onChange={(e) => set('durationMinutes', e.target.value)}
              />
            </Field>
            <Field label={t('autoUpdate.interval')} hint={t('autoUpdate.intervalHint')}>
              <input
                type="number"
                min={MIN_INTERVAL_MINUTES}
                max={MAX_INTERVAL_MINUTES}
                className={inputCls}
                value={draft.checkIntervalMinutes}
                onChange={(e) => set('checkIntervalMinutes', e.target.value)}
              />
            </Field>
          </div>

          <div className="flex flex-wrap items-center gap-2 pt-1">
            <h3 className="text-xs font-semibold tracking-wide text-muted uppercase">{t('autoUpdate.texts')}</h3>
            <Button
              variant="ghost"
              className="ml-auto"
              icon={<RotateCcw size={14} />}
              onClick={() => set('templates', data.defaults.templates)}
            >
              {t('autoUpdate.resetTexts')}
            </Button>
          </div>
          <p className="text-xs text-muted">{t('autoUpdate.textsHint')}</p>
          <div className="grid gap-3 lg:grid-cols-2">
            <TemplateFields
              label={t('autoUpdate.english')}
              value={draft.templates.en}
              onChange={(v) => set('templates', { ...draft.templates, en: v })}
            />
            <TemplateFields
              label={t('autoUpdate.spanish')}
              value={draft.templates.es}
              onChange={(v) => set('templates', { ...draft.templates, es: v })}
            />
          </div>

          <Button type="submit" variant="primary" icon={<Save size={14} />} loading={busy === 'save'}>
            {t('autoUpdate.save')}
          </Button>
        </form>
      </div>
    </Card>
  )
}
