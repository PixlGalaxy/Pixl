import { Gauge, Search, Shield, UserPlus, Users, Vote, X } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Avatar, Button, Card, Empty, IconButton, PageSpinner, Segmented, SettingRow, Spinner, Toggle, inputCls } from '../components/ui'
import { api } from '../lib/api'
import { useLynx } from '../lib/lynx'
import type { GuildSettings, Member } from '../lib/types'

/** Number field that saves on blur / Enter, clamped to [min, max]. */
function NumberField({
  value,
  min,
  max,
  label,
  suffix,
  onSave,
}: {
  value: number
  min: number
  max: number
  label: string
  suffix?: string
  onSave: (v: number) => void
}) {
  const [draft, setDraft] = useState(String(value))
  useEffect(() => setDraft(String(value)), [value])
  const commit = () => {
    const n = Math.round(Number(draft))
    if (!Number.isFinite(n)) return setDraft(String(value))
    const clamped = Math.min(max, Math.max(min, n))
    setDraft(String(clamped))
    if (clamped !== value) onSave(clamped)
  }
  return (
    <div className="flex items-center gap-2">
      <input
        type="number"
        inputMode="numeric"
        min={min}
        max={max}
        value={draft}
        aria-label={label}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === 'Enter' && (e.currentTarget as HTMLInputElement).blur()}
        className={`${inputCls.replace('w-full', 'w-20')} text-right font-mono tabular-nums`}
      />
      <span className="w-4 text-xs text-muted">{suffix}</span>
    </div>
  )
}

function DjCard() {
  const { t } = useTranslation()
  const { run, errorText } = useLynx()
  const [djs, setDjs] = useState<Member[] | null>(null)
  const [query, setQuery] = useState('')
  const [candidates, setCandidates] = useState<Member[] | null>(null)
  const [loadingCandidates, setLoadingCandidates] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const seq = useRef(0)

  const reload = () =>
    api.djs().then(
      (r) => setDjs(r.djs),
      (e) => setError(errorText(e)),
    )
  useEffect(() => {
    void reload()
  }, [])

  useEffect(() => {
    const id = ++seq.current
    setLoadingCandidates(true)
    const timer = setTimeout(
      () =>
        api.candidates(query.trim()).then(
          (r) => id === seq.current && setCandidates(r.members),
          () => id === seq.current && setCandidates([]),
        ).finally(() => id === seq.current && setLoadingCandidates(false)),
      query ? 300 : 0,
    )
    return () => clearTimeout(timer)
  }, [query])

  const djIds = new Set((djs ?? []).map((d) => d.id))
  const options = (candidates ?? []).filter((m) => !djIds.has(m.id))

  return (
    <Card title={t('settings.djs')} description={t('settings.djsHint')} icon={<Users size={18} />}>
      {error && <p className="text-xs text-red-600 dark:text-red-400">{error}</p>}
      {!djs ? (
        <div className="flex justify-center py-4 text-muted">
          <Spinner />
        </div>
      ) : djs.length === 0 ? (
        <p className="text-sm text-muted">{t('settings.noDjs')}</p>
      ) : (
        <ul className="-mx-4 -mt-1">
          {djs.map((d, i) => (
            <li key={d.id} className={`flex items-center gap-3 px-4 py-2 ${i > 0 ? 'border-t border-line' : ''}`}>
              <Avatar url={d.avatarUrl} name={d.name} className="size-7" />
              <span className="min-w-0 flex-1 truncate text-sm font-medium">{d.name}</span>
              <IconButton
                label={t('settings.removeDj', { name: d.name })}
                className="hover:!text-red-500"
                onClick={async () => {
                  setDjs((list) => list?.filter((x) => x.id !== d.id) ?? null)
                  await run(() => api.removeDj(d.id))
                  void reload()
                }}
              >
                <X size={16} />
              </IconButton>
            </li>
          ))}
        </ul>
      )}

      <div className="space-y-2 border-t border-line pt-4">
        <div className="text-xs font-medium text-muted">{t('settings.addDj')}</div>
        <div className="relative">
          <Search size={15} className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-muted" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('settings.searchMembers')}
            aria-label={t('settings.searchMembers')}
            className={`${inputCls} pl-9`}
          />
        </div>
        {loadingCandidates && !candidates ? (
          <div className="flex justify-center py-3 text-muted">
            <Spinner size={16} />
          </div>
        ) : options.length === 0 ? (
          <p className="py-2 text-xs text-muted">{t('settings.noCandidates')}</p>
        ) : (
          <ul className="overflow-hidden rounded-xl border border-line">
            {options.slice(0, 8).map((m, i) => (
              <li key={m.id} className={`flex items-center gap-3 px-3 py-2 ${i > 0 ? 'border-t border-line' : ''}`}>
                <Avatar url={m.avatarUrl} name={m.name} className="size-7" />
                <span className="min-w-0 flex-1 truncate text-sm">{m.name}</span>
                <Button
                  size="xs"
                  icon={<UserPlus size={13} />}
                  onClick={async () => {
                    const ok = await run(() => api.addDj(m.id), t('settings.djAdded', { name: m.name }))
                    if (ok !== null) void reload()
                  }}
                >
                  {t('settings.makeDj')}
                </Button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </Card>
  )
}

export function SettingsScreen() {
  const { t } = useTranslation()
  const { settings, setSettings, run } = useLynx()
  if (!settings) return <PageSpinner />

  const save = async (patch: Partial<GuildSettings>) => {
    const previous = settings
    setSettings({ ...settings, ...patch })
    const res = await run(() => api.updateSettings(patch))
    if (res === null) setSettings(previous)
    else if (res) setSettings(res)
  }
  const vs = settings.voteSkip

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-xl font-semibold tracking-tight">{t('settings.title')}</h2>
        <p className="text-sm text-muted">{t('settings.subtitle')}</p>
      </div>

      <Card title={t('settings.control')} description={t('settings.controlHint')} icon={<Shield size={18} />}>
        <SettingRow
          label={t('settings.controlMode')}
          description={settings.controlMode === 'everyone' ? t('settings.modeEveryoneHint') : t('settings.modeDjHint')}
        >
          <Segmented
            label={t('settings.controlMode')}
            value={settings.controlMode}
            onChange={(controlMode) => save({ controlMode })}
            options={[
              { value: 'everyone', label: t('settings.modeEveryone') },
              { value: 'dj', label: t('settings.modeDj') },
            ]}
          />
        </SettingRow>
      </Card>

      <DjCard />

      <Card title={t('settings.voteSkip')} description={t('settings.voteSkipHint')} icon={<Vote size={18} />}>
        <SettingRow label={t('settings.voteSkipEnabled')}>
          <Toggle
            label={t('settings.voteSkipEnabled')}
            checked={vs.enabled}
            onChange={(enabled) => save({ voteSkip: { ...vs, enabled } })}
          />
        </SettingRow>
        {vs.enabled && (
          <>
            <SettingRow label={t('settings.voteMode')} description={vs.mode === 'count' ? t('settings.voteCountHint') : t('settings.votePercentHint')}>
              <Segmented
                label={t('settings.voteMode')}
                value={vs.mode}
                onChange={(mode) =>
                  save({ voteSkip: { ...vs, mode, value: mode === 'percent' ? Math.min(100, Math.max(vs.value, 50)) : Math.min(vs.value, 25) } })
                }
                options={[
                  { value: 'count', label: t('settings.voteCount') },
                  { value: 'percent', label: t('settings.votePercent') },
                ]}
              />
            </SettingRow>
            <SettingRow label={vs.mode === 'count' ? t('settings.votesNeeded') : t('settings.percentNeeded')}>
              <NumberField
                label={vs.mode === 'count' ? t('settings.votesNeeded') : t('settings.percentNeeded')}
                value={vs.value}
                min={1}
                max={vs.mode === 'count' ? 25 : 100}
                suffix={vs.mode === 'percent' ? '%' : undefined}
                onSave={(value) => save({ voteSkip: { ...vs, value } })}
              />
            </SettingRow>
          </>
        )}
      </Card>

      <Card title={t('settings.limits')} description={t('settings.limitsHint')} icon={<Gauge size={18} />}>
        <SettingRow label={t('settings.maxQueue')} description={t('settings.maxQueueHint')}>
          <NumberField
            label={t('settings.maxQueue')}
            value={settings.maxQueueLength}
            min={1}
            max={5000}
            onSave={(maxQueueLength) => save({ maxQueueLength })}
          />
        </SettingRow>
        <SettingRow label={t('settings.defaultVolume')} description={t('settings.defaultVolumeHint')}>
          <NumberField
            label={t('settings.defaultVolume')}
            value={settings.defaultVolume}
            min={0}
            max={200}
            suffix="%"
            onSave={(defaultVolume) => save({ defaultVolume })}
          />
        </SettingRow>
      </Card>

      {settings && <Empty>{t('settings.savedAutomatically')}</Empty>}
    </div>
  )
}
