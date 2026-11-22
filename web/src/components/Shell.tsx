import { Headphones, Languages, Moon, Sun } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { cx } from '../lib/format'
import i18n, { LANGUAGES, matchLanguage, setDocumentLanguage } from '../lib/i18n'
import { useLynx } from '../lib/lynx'
import { HelpScreen } from '../screens/Help'
import { LyricsScreen } from '../screens/Lyrics'
import { PlayerScreen } from '../screens/Player'
import { PrivacyScreen } from '../screens/Privacy'
import { QueueScreen, UpNextPanel } from '../screens/Queue'
import { RadioScreen } from '../screens/Radio'
import { SettingsScreen } from '../screens/Settings'
import { LynxMark } from './Logo'

export type TabId = 'player' | 'queue' | 'lyrics' | 'radio' | 'settings' | 'privacy' | 'help'

const TABS: Array<{ id: TabId }> = [
  { id: 'player' },
  { id: 'queue' },
  { id: 'lyrics' },
  { id: 'radio' },
  { id: 'settings' },
  { id: 'privacy' },
  { id: 'help' },
]

/** Initial tab: `#hash` deep link, then the last tab of this session. */
export function readTab(): TabId {
  // `#queue`, `#lyrics`... deep-link to a tab.
  const hash = location.hash.slice(1) as TabId
  if (TABS.some((t) => t.id === hash)) return hash
  try {
    const v = sessionStorage.getItem('lynx:tab') as TabId | null
    if (v && TABS.some((t) => t.id === v)) return v
  } catch {
    // storage blocked
  }
  return 'player'
}

function LanguageSelect() {
  const { t } = useTranslation()
  const { run } = useLynx()
  const current = matchLanguage(i18n.language) ?? 'en'
  return (
    <label className="relative inline-flex items-center" title={t('topbar.language')}>
      <Languages size={15} className="pointer-events-none absolute left-2.5 text-muted" />
      <select
        aria-label={t('topbar.language')}
        value={current}
        onChange={async (e) => {
          const code = e.target.value
          await i18n.changeLanguage(code)
          setDocumentLanguage(code)
          await run(() => api.updatePrefs({ locale: code }))
        }}
        className="h-8 appearance-none rounded-lg border border-line bg-surface pr-2.5 pl-8 text-xs font-medium shadow-xs focus:border-sky-500 focus:ring-2 focus:ring-sky-500/20 focus:outline-none"
      >
        {LANGUAGES.map((l) => (
          <option key={l.code} value={l.code}>
            {l.label}
          </option>
        ))}
      </select>
    </label>
  )
}

function ThemeToggle() {
  const { t } = useTranslation()
  const [dark, setDark] = useState(() => document.documentElement.classList.contains('dark'))
  return (
    <button
      type="button"
      aria-label={dark ? t('topbar.lightTheme') : t('topbar.darkTheme')}
      title={dark ? t('topbar.lightTheme') : t('topbar.darkTheme')}
      onClick={() => {
        const next = !dark
        document.documentElement.classList.toggle('dark', next)
        try {
          localStorage.setItem('lynx:theme', next ? 'dark' : 'light')
        } catch {
          // storage blocked
        }
        setDark(next)
      }}
      className="inline-flex size-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface-2 hover:text-fg"
    >
      {dark ? <Sun size={16} /> : <Moon size={16} />}
    </button>
  )
}

/** Saves the tab so a reload (or a trip through picture-in-picture) comes back to it. */
export function saveTab(id: TabId) {
  try {
    sessionStorage.setItem('lynx:tab', id)
  } catch {
    // storage blocked
  }
}

/** Full panel. The tab lives in the parent so it survives the compact (PiP) view replacing the shell. */
export function Shell({ tab, onTab }: { tab: TabId; onTab: (id: TabId) => void }) {
  const { t } = useTranslation()
  const { state, permissions, aiEnabled, guildName, launchChannelName } = useLynx()

  const tabs = TABS.filter(
    (x) => (x.id !== 'radio' || aiEnabled) && (x.id !== 'settings' || permissions.isManager),
  )
  const active: TabId = tabs.some((x) => x.id === tab) ? tab : 'player'

  const setTab = (id: TabId) => {
    onTab(id)
    saveTab(id)
  }

  const navRef = useRef<HTMLElement>(null)
  useEffect(() => {
    window.scrollTo({ top: 0 })
    // Narrow widths scroll the tab row sideways: keep the active tab in view.
    const nav = navRef.current
    const el = nav?.querySelector<HTMLElement>('[aria-current="page"]')
    if (nav && el && nav.scrollWidth > nav.clientWidth) {
      const left = el.offsetLeft - (nav.clientWidth - el.offsetWidth) / 2
      nav.scrollTo({ left: Math.max(0, left), behavior: 'smooth' })
    }
  }, [active])

  const channelName = state?.channelName || launchChannelName
  // On wide screens the queue lives in a right column next to every tab except the queue itself.
  const showSide = active !== 'queue'

  return (
    <div className="flex min-h-screen flex-col">
      <header className="sticky top-0 z-30 border-b border-line bg-surface/80 backdrop-blur-md">
        <div className="mx-auto flex h-12 max-w-6xl items-center gap-2 px-3 sm:px-4">
          <LynxMark className="size-7 text-fg" />
          <h1 className="text-base font-semibold tracking-tight">Lynx</h1>
          {(channelName || guildName) && (
            <>
              <div className="mx-1 h-6 w-px bg-line" />
              <div className="flex min-w-0 items-center gap-1.5 text-xs text-muted" title={channelName ?? guildName ?? ''}>
                <Headphones size={13} className="shrink-0" />
                <span className="truncate">{channelName ?? guildName}</span>
              </div>
            </>
          )}
          <div className="ml-auto flex shrink-0 items-center gap-1">
            <LanguageSelect />
            <ThemeToggle />
          </div>
        </div>
        <nav
          ref={navRef}
          className="no-scrollbar mx-auto flex h-11 max-w-6xl items-stretch overflow-x-auto px-1 sm:px-2"
          aria-label={t('topbar.sections')}
        >
          {tabs.map(({ id }) => (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              aria-current={active === id ? 'page' : undefined}
              className={cx(
                'relative inline-flex shrink-0 items-center justify-center px-3 text-sm font-medium whitespace-nowrap transition-colors',
                active === id ? 'text-sky-600 dark:text-sky-400' : 'text-muted hover:text-fg',
              )}
            >
              {t(`tabs.${id}`)}
              {active === id && <span className="absolute inset-x-2 bottom-0 h-0.5 rounded-full bg-sky-500" />}
            </button>
          ))}
        </nav>
      </header>

      <div className="mx-auto flex w-full max-w-6xl flex-1 gap-5 px-3 py-4 sm:px-4 sm:py-6">
        <main className="min-w-0 flex-1">
          {active === 'player' && <PlayerScreen onOpenQueue={() => setTab('queue')} />}
          {active === 'queue' && <QueueScreen />}
          {active === 'lyrics' && <LyricsScreen />}
          {active === 'radio' && <RadioScreen />}
          {active === 'settings' && <SettingsScreen />}
          {active === 'privacy' && <PrivacyScreen />}
          {active === 'help' && <HelpScreen />}
        </main>
        {showSide && (
          <aside className="hidden w-96 shrink-0 lg:block">
            <div className="sticky top-[7.25rem]">
              <UpNextPanel />
            </div>
          </aside>
        )}
      </div>
    </div>
  )
}
