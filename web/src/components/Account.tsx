import {
  Check,
  ExternalLink,
  History,
  Languages,
  Monitor,
  Moon,
  Palette,
  ShieldAlert,
  Sun,
  Trash2,
  X,
  type LucideIcon,
} from 'lucide-react'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import {
  ACCENTS,
  applyAccent,
  applyTheme,
  getAccent,
  getTheme,
  onAppearanceChange,
  THEMES,
  type AccentId,
  type ThemeMode,
} from '../lib/appearance'
import { openExternal } from '../lib/discord'
import { notifyTasteChanged } from '../lib/events'
import { cx, mediaUrl } from '../lib/format'
import i18n, { LANGUAGES, matchLanguage, setDocumentLanguage } from '../lib/i18n'
import { useLynx } from '../lib/lynx'
import type { User, UserPrefs } from '../lib/types'
import { ConfirmButton, IconButton, inputCls, Spinner, Toggle } from './ui'

/** Current theme mode and accent, re-rendering when either changes (also from the top-bar toggle). */
export function useAppearance(): { theme: ThemeMode; accent: AccentId } {
  const [, setTick] = useState(0)
  useEffect(() => onAppearanceChange(() => setTick((n) => n + 1)), [])
  return { theme: getTheme(), accent: getAccent() }
}

export function displayName(user: User) {
  return user.globalName || user.username
}

/** Round avatar with initials when there is no picture (or it fails to load). */
export function UserAvatar({ user, className = 'size-8' }: { user: User; className?: string }) {
  const [broken, setBroken] = useState(false)
  const src = mediaUrl(user.avatarUrl)
  const name = displayName(user)
  const initials =
    name
      .split(/\s+/)
      .filter(Boolean)
      .slice(0, 2)
      .map((w) => w[0]?.toUpperCase() ?? '')
      .join('') || '?'
  if (!src || broken) {
    return (
      <span
        className={cx(
          'inline-flex shrink-0 items-center justify-center rounded-full bg-accent-500/15 font-semibold text-accent-600 dark:text-accent-400',
          className,
        )}
      >
        <span className="text-[0.7em] leading-none">{initials}</span>
      </span>
    )
  }
  return <img src={src} alt="" onError={() => setBroken(true)} className={cx('shrink-0 rounded-full object-cover', className)} />
}

/** Save an appearance choice to the user's prefs (applied locally first, so it is instant). */
export async function saveAppearance(patch: { theme?: ThemeMode; accent?: AccentId }) {
  try {
    await api.updatePrefs(patch)
  } catch (error) {
    console.warn('[lynx] could not save appearance', error)
  }
}

const MODE_ICON: Record<ThemeMode, LucideIcon> = { light: Sun, dark: Moon, system: Monitor }

function Section({ title, icon, children }: { title: string; icon: ReactNode; children: ReactNode }) {
  return (
    <section className="border-t border-line px-4 py-4">
      <h3 className="mb-3 flex items-center gap-2 text-sm font-semibold">
        <span className="text-muted">{icon}</span>
        {title}
      </h3>
      <div className="space-y-4">{children}</div>
    </section>
  )
}

function Appearance() {
  const { t } = useTranslation()
  const { theme, accent } = useAppearance()
  return (
    <Section title={t('account.appearance')} icon={<Palette size={15} />}>
      <div>
        <div className="mb-2 text-xs font-medium text-muted">{t('account.themeMode')}</div>
        <div className="grid grid-cols-3 gap-2">
          {THEMES.map((m) => {
            const Icon = MODE_ICON[m]
            const selected = m === theme
            return (
              <button
                key={m}
                type="button"
                aria-pressed={selected}
                onClick={() => {
                  applyTheme(m)
                  void saveAppearance({ theme: m })
                }}
                className={cx(
                  'relative flex flex-col items-center gap-1.5 rounded-xl border-2 px-2 py-3 text-xs font-medium transition-colors',
                  selected
                    ? 'border-accent-500 bg-accent-500/10 text-fg'
                    : 'border-line text-muted hover:border-muted/50 hover:text-fg',
                )}
              >
                {selected && (
                  <span className="absolute -top-1.5 -right-1.5 flex size-4 items-center justify-center rounded-full bg-accent-500 text-white shadow">
                    <Check size={10} strokeWidth={3} />
                  </span>
                )}
                <Icon size={18} />
                {t(`account.mode.${m}`)}
              </button>
            )
          })}
        </div>
      </div>
      <div>
        <div className="mb-2 flex items-baseline justify-between gap-2 text-xs font-medium text-muted">
          <span>{t('account.accent')}</span>
          <span className="text-fg/80">{t(`account.accents.${accent}`)}</span>
        </div>
        <div className="grid grid-cols-6 gap-2 sm:grid-cols-8" role="radiogroup" aria-label={t('account.accent')}>
          {ACCENTS.map((a) => {
            const selected = a.id === accent
            const label = t(`account.accents.${a.id}`)
            return (
              <button
                key={a.id}
                type="button"
                role="radio"
                aria-checked={selected}
                aria-label={label}
                title={label}
                onClick={() => {
                  applyAccent(a.id)
                  void saveAppearance({ accent: a.id })
                }}
                className={cx(
                  'relative flex aspect-square items-center justify-center rounded-xl border-2 transition-colors',
                  selected ? '' : 'border-transparent hover:border-line',
                )}
                style={selected ? { borderColor: a.dots[1], backgroundColor: `${a.dots[1]}1a` } : undefined}
              >
                <span
                  className="size-6 rounded-full shadow-sm"
                  style={{ background: `linear-gradient(135deg, ${a.dots[0]}, ${a.dots[1]} 55%, ${a.dots[2]})` }}
                />
                {selected && (
                  <span className="absolute inset-0 flex items-center justify-center text-white">
                    <Check size={13} strokeWidth={3} />
                  </span>
                )}
              </button>
            )
          })}
        </div>
      </div>
    </Section>
  )
}

function LanguagePicker() {
  const { t } = useTranslation()
  const current = matchLanguage(i18n.language) ?? 'en'
  return (
    <Section title={t('account.language')} icon={<Languages size={15} />}>
      <select
        aria-label={t('account.language')}
        value={current}
        onChange={async (e) => {
          const code = e.target.value
          await i18n.changeLanguage(code)
          setDocumentLanguage(code)
          try {
            await api.updatePrefs({ locale: code })
          } catch (error) {
            console.warn('[lynx] could not save language', error)
          }
        }}
        className={inputCls}
      >
        {LANGUAGES.map((l) => (
          <option key={l.code} value={l.code}>
            {l.label}
          </option>
        ))}
      </select>
    </Section>
  )
}

function Explained({ title, items }: { title: string; items: string[] }) {
  return (
    <div>
      <div className="text-xs font-semibold">{title}</div>
      <ul className="mt-1 list-disc space-y-0.5 pl-4 text-xs leading-relaxed text-muted marker:text-line">
        {items.map((x) => (
          <li key={x}>{x}</li>
        ))}
      </ul>
    </div>
  )
}

function Privacy({ prefs, setPrefs }: { prefs: UserPrefs; setPrefs: (p: UserPrefs) => void }) {
  const { t } = useTranslation()
  const { run } = useLynx()
  const list = (key: string) => t(key, { returnObjects: true }) as unknown as string[]
  return (
    <Section title={t('account.privacy')} icon={<History size={15} />}>
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-medium">{t('privacy.historyToggle')}</div>
          <div className="text-xs text-muted">{prefs.historyEnabled ? t('privacy.historyOn') : t('privacy.historyOff')}</div>
        </div>
        <Toggle
          label={t('privacy.historyToggle')}
          checked={prefs.historyEnabled}
          onChange={async (historyEnabled) => {
            setPrefs({ ...prefs, historyEnabled })
            const res = await run(() => api.updatePrefs({ historyEnabled }))
            if (res === null) setPrefs(prefs)
            else notifyTasteChanged()
          }}
        />
      </div>
      <div className="space-y-3 rounded-xl border border-line bg-surface-2/60 p-3">
        <Explained title={t('account.stored.title')} items={list('account.stored.items')} />
        <Explained title={t('account.used.title')} items={list('account.used.items')} />
        <Explained title={t('account.off.title')} items={list('account.off.items')} />
      </div>
    </Section>
  )
}

function Links() {
  const { t } = useTranslation()
  const { config } = useLynx()
  const links = (['privacy', 'terms'] as const).filter((k) => config.links[k])
  if (links.length === 0) return null
  return (
    <div className="flex flex-wrap gap-x-4 gap-y-1 border-t border-line px-4 py-3">
      {links.map((k) => (
        <button
          key={k}
          type="button"
          onClick={() => openExternal(config.links[k]!)}
          className="inline-flex items-center gap-1 text-xs font-medium text-accent-600 hover:underline dark:text-accent-400"
        >
          {t(`account.links.${k}`)}
          <ExternalLink size={11} />
        </button>
      ))}
    </div>
  )
}

function DangerZone({ onDeleted }: { onDeleted: () => void }) {
  const { t } = useTranslation()
  const { run, aiEnabled } = useLynx()
  const items = t(aiEnabled ? 'account.danger.items' : 'account.danger.itemsNoAi', { returnObjects: true }) as unknown as string[]
  return (
    <section className="m-4 rounded-xl border border-red-500/30 bg-red-500/5 p-4">
      <h3 className="flex items-center gap-2 text-sm font-semibold text-red-600 dark:text-red-400">
        <ShieldAlert size={15} />
        {t('account.danger.title')}
      </h3>
      <p className="mt-1 text-xs text-muted">{t('account.danger.body')}</p>
      <ul className="mt-2 list-disc space-y-0.5 pl-4 text-xs text-muted marker:text-red-500/50">
        {items.map((x) => (
          <li key={x}>{x}</li>
        ))}
      </ul>
      <p className="mt-2 text-xs text-muted">{t('account.danger.kept')}</p>
      <div className="mt-3">
        <ConfirmButton
          label={t('privacy.delete')}
          question={t('privacy.deleteQuestion')}
          confirmLabel={t('privacy.deleteConfirm')}
          icon={<Trash2 size={12} />}
          onConfirm={async () => {
            const ok = await run(() => api.deleteData(), t('privacy.deleted'))
            if (ok !== null) onDeleted()
          }}
        />
      </div>
    </section>
  )
}

/**
 * Account view: who you are, appearance, language, privacy and the data wipe. A bottom sheet on
 * narrow widths, a panel under the account button on wide ones. Escape or the backdrop closes it.
 */
export function AccountPanel({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation()
  const { user, guildName, aiEnabled } = useLynx()
  const [prefs, setPrefs] = useState<UserPrefs | null>(null)
  const panel = useRef<HTMLDivElement>(null)

  useEffect(() => {
    api.prefs().then(setPrefs, () => setPrefs({ locale: null, historyEnabled: true, theme: null, accent: null }))
  }, [])
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose()
    window.addEventListener('keydown', onKey)
    panel.current?.focus()
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return (
    <div className="fixed inset-0 z-40">
      <div className="absolute inset-0 animate-[fade-in_.15s_ease-out] bg-black/50 backdrop-blur-sm" onClick={onClose} />
      <div
        ref={panel}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label={t('account.title')}
        className="absolute inset-x-0 bottom-0 flex max-h-[92vh] animate-[pop-in_.18s_ease-out] flex-col overflow-hidden rounded-t-2xl border border-line bg-surface shadow-2xl outline-none sm:inset-x-auto sm:top-14 sm:right-4 sm:bottom-auto sm:max-h-[calc(100vh-4.5rem)] sm:w-[420px] sm:rounded-2xl"
      >
        <div className="flex items-center gap-3 px-4 py-4">
          <UserAvatar user={user} className="size-12 text-lg" />
          <div className="min-w-0 flex-1">
            <div className="truncate font-semibold">{displayName(user)}</div>
            <div className="truncate text-xs text-muted">@{user.username}</div>
            {guildName && <div className="truncate text-xs text-muted">{t('account.server', { name: guildName })}</div>}
          </div>
          <IconButton label={t('account.close')} onClick={onClose}>
            <X size={18} />
          </IconButton>
        </div>
        <div className="overflow-y-auto overscroll-contain">
          <Appearance />
          <LanguagePicker />
          {aiEnabled &&
            (prefs ? (
              <Privacy prefs={prefs} setPrefs={setPrefs} />
            ) : (
              <div className="flex justify-center border-t border-line py-6 text-muted">
                <Spinner />
              </div>
            ))}
          {aiEnabled && <p className="border-t border-line px-4 py-3 text-xs text-muted">{t('privacy.tasteMoved')}</p>}
          {!aiEnabled && (
            <p className="border-t border-line px-4 py-4 text-xs text-muted">{t('privacy.noHistoryNote')}</p>
          )}
          <Links />
          <DangerZone onDeleted={notifyTasteChanged} />
        </div>
      </div>
    </div>
  )
}

/** Circular account button for the top bar. */
export function AccountButton({ onClick }: { onClick: () => void }) {
  const { t } = useTranslation()
  const { user } = useLynx()
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={t('account.open')}
      title={t('account.open')}
      className="ml-1 inline-flex size-8 items-center justify-center rounded-full ring-2 ring-transparent transition-shadow hover:ring-accent-500/40 focus-visible:ring-accent-500 focus-visible:outline-none"
    >
      <UserAvatar user={user} className="size-7 text-sm" />
    </button>
  )
}
