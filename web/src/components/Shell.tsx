import { Headphones, Moon, Sun } from 'lucide-react'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { applyTheme, isDark } from '../lib/appearance'
import { onOpenAccount, onShowInQueue } from '../lib/events'
import { cx } from '../lib/format'
import { useLynx } from '../lib/lynx'
import { HelpScreen } from '../screens/Help'
import { LyricsScreen } from '../screens/Lyrics'
import { PlayerScreen } from '../screens/Player'
import { PlaylistsScreen } from '../screens/Playlists'
import { QueueScreen, UpNextPanel } from '../screens/Queue'
import { RadioScreen } from '../screens/Radio'
import { SettingsScreen } from '../screens/Settings'
import { AccountButton, AccountPanel, saveAppearance, useAppearance } from './Account'
import { LynxMark } from './Logo'
import { NoVoiceCard } from './NoVoice'

export type TabId = 'player' | 'queue' | 'playlists' | 'lyrics' | 'radio' | 'settings' | 'help'

const TABS: Array<{ id: TabId }> = [
  { id: 'player' },
  { id: 'queue' },
  { id: 'playlists' },
  { id: 'lyrics' },
  { id: 'radio' },
  { id: 'settings' },
  { id: 'help' },
]

/**
 * Initial tab. A fresh open always lands on Now playing: the tab is not saved across page loads.
 * Mock mode only (`pnpm dev:mock`): `?tab=queue|lyrics|radio|...` opens another tab, for screenshots.
 */
export function readTab(): TabId {
  if (import.meta.env.MODE === 'mock') {
    const wanted = new URLSearchParams(location.search).get('tab') as TabId | null
    if (wanted && TABS.some((t) => t.id === wanted)) return wanted
  }
  return 'player'
}

/** Quick light/dark switch; the full choice (light / dark / system, accent) is in the account view. */
function ThemeToggle() {
  const { t } = useTranslation()
  useAppearance()
  const dark = isDark()
  return (
    <button
      type="button"
      aria-label={dark ? t('topbar.lightTheme') : t('topbar.darkTheme')}
      title={dark ? t('topbar.lightTheme') : t('topbar.darkTheme')}
      onClick={() => {
        const next = dark ? 'light' : 'dark'
        applyTheme(next)
        void saveAppearance({ theme: next })
      }}
      className="inline-flex size-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface-2 hover:text-fg"
    >
      {dark ? <Sun size={16} /> : <Moon size={16} />}
    </button>
  )
}

/** Full panel. The tab lives in the parent so it survives the compact (PiP) view replacing the shell. */
export function Shell({ tab, onTab }: { tab: TabId; onTab: (id: TabId) => void }) {
  const { t } = useTranslation()
  const { state, permissions, aiEnabled, guildName, channelId, channelName: followed } = useLynx()

  const tabs = TABS.filter(
    (x) => (x.id !== 'radio' || aiEnabled) && (x.id !== 'settings' || permissions.isManager),
  )
  const active: TabId = tabs.some((x) => x.id === tab) ? tab : 'player'

  const setTab = onTab

  const [accountOpen, setAccountOpen] = useState(false)
  const closeAccount = useCallback(() => setAccountOpen(false), [])
  useEffect(() => onOpenAccount(() => setAccountOpen(true)), [])
  useEffect(() => onShowInQueue(() => setTab('queue')), [setTab])
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

  const channelName = followed ?? state?.channelName ?? null
  // Not in a voice channel: no player anywhere; Playlists (browsing), Settings and Help stay useful.
  const noVoice = !channelId && (active === 'player' || active === 'queue' || active === 'lyrics' || active === 'radio')
  // On wide screens the queue lives in a right column next to every tab except the queue itself.
  const showSide = active !== 'queue' && !!channelId

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
            <ThemeToggle />
            <AccountButton onClick={() => setAccountOpen(true)} />
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
                active === id ? 'text-accent-600 dark:text-accent-400' : 'text-muted hover:text-fg',
              )}
            >
              {t(`tabs.${id}`)}
              {active === id && <span className="absolute inset-x-2 bottom-0 h-0.5 rounded-full bg-accent-500" />}
            </button>
          ))}
        </nav>
      </header>

      <div className="mx-auto flex w-full max-w-6xl flex-1 gap-5 px-3 py-4 sm:px-4 sm:py-6">
        <main className="min-w-0 flex-1">
          {noVoice && <NoVoiceCard />}
          {!noVoice && active === 'player' && <PlayerScreen onOpenQueue={() => setTab('queue')} />}
          {!noVoice && active === 'queue' && <QueueScreen />}
          {active === 'playlists' && <PlaylistsScreen />}
          {!noVoice && active === 'lyrics' && <LyricsScreen />}
          {!noVoice && active === 'radio' && <RadioScreen />}
          {active === 'settings' && <SettingsScreen />}
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
      {accountOpen && <AccountPanel onClose={closeAccount} />}
    </div>
  )
}
