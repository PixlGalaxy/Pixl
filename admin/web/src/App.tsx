import { Activity, Gauge, Languages, LogOut, Megaphone, Menu, Moon, ScrollText, SlidersHorizontal, Sun, X } from 'lucide-react'
import { useCallback, useEffect, useState, type FormEvent, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { LynxMark } from './components/Logo'
import { Banner, Button, Card, inputCls, Spinner } from './components/ui'
import { LANGUAGES, setLanguage } from './i18n'
import { AdminError, adminApi, setUnauthorizedHandler } from './lib/api'
import { useErrorText } from './lib/errors'
import { ActivityPage } from './pages/Activity'
import { LogsPage } from './pages/Logs'
import { NoticesPage } from './pages/Notices'
import { OverviewPage } from './pages/Overview'
import { TweaksPage } from './pages/Tweaks'

type Section = 'overview' | 'activity' | 'logs' | 'notices' | 'tweaks'
const SECTIONS: Array<{ id: Section; icon: ReactNode }> = [
  { id: 'overview', icon: <Gauge size={16} /> },
  { id: 'activity', icon: <Activity size={16} /> },
  { id: 'logs', icon: <ScrollText size={16} /> },
  { id: 'notices', icon: <Megaphone size={16} /> },
  { id: 'tweaks', icon: <SlidersHorizontal size={16} /> },
]
const THEME_KEY = 'lynx-admin:theme'

type Phase = { kind: 'loading' } | { kind: 'off' } | { kind: 'login'; note?: string } | { kind: 'ready' }

function readTheme(): boolean {
  try {
    return localStorage.getItem(THEME_KEY) !== 'light'
  } catch {
    return true
  }
}

function useTheme(): [boolean, () => void] {
  const [dark, setDark] = useState(readTheme)
  useEffect(() => {
    document.documentElement.classList.toggle('dark', dark)
  }, [dark])
  const toggle = useCallback(() => {
    setDark((d) => {
      try {
        localStorage.setItem(THEME_KEY, d ? 'light' : 'dark')
      } catch {
        // storage unavailable
      }
      return !d
    })
  }, [])
  return [dark, toggle]
}

/** Section from the URL hash (#logs), so a reload stays on the same page. */
function readSection(): Section {
  const h = location.hash.replace('#', '') as Section
  return SECTIONS.some((s) => s.id === h) ? h : 'overview'
}

function Tools({ onLogout }: { onLogout?: () => void }) {
  const { t, i18n } = useTranslation()
  const [dark, toggleTheme] = useTheme()
  const next = LANGUAGES.find((l) => l.code !== i18n.language) ?? LANGUAGES[0]
  return (
    <div className="flex items-center gap-1">
      <Button variant="ghost" icon={<Languages size={14} />} onClick={() => setLanguage(next.code)} title={t('app.language')}>
        {next.label}
      </Button>
      <Button
        variant="ghost"
        aria-label={dark ? t('app.lightTheme') : t('app.darkTheme')}
        title={dark ? t('app.lightTheme') : t('app.darkTheme')}
        onClick={toggleTheme}
        icon={dark ? <Sun size={14} /> : <Moon size={14} />}
      />
      {onLogout && (
        <Button variant="ghost" icon={<LogOut size={14} />} onClick={onLogout} title={t('app.logout')}>
          <span className="hidden sm:inline">{t('app.logout')}</span>
        </Button>
      )}
    </div>
  )
}

function Brand() {
  const { t } = useTranslation()
  return (
    <div className="flex items-center gap-2">
      <LynxMark className="size-7 text-fg" />
      <span className="text-base font-semibold tracking-tight">{t('app.title')}</span>
    </div>
  )
}

function Login({ note, onDone }: { note?: string; onDone: () => void }) {
  const { t } = useTranslation()
  const errorText = useErrorText()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    try {
      await adminApi.login(username, password)
      setPassword('')
      onDone()
    } catch (err) {
      setError(errorText(err))
    } finally {
      setBusy(false)
    }
  }
  return (
    <div className="mx-auto mt-16 max-w-sm px-3">
      <Card title={t('login.title')}>
        <p className="mb-4 text-sm text-muted">{t('login.subtitle')}</p>
        <form onSubmit={(e) => void submit(e)} className="space-y-3">
          {note && !error && <Banner tone="warning">{note}</Banner>}
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">{t('login.username')}</span>
            <input
              autoComplete="username"
              autoFocus
              className={inputCls}
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              required
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">{t('login.password')}</span>
            <input
              type="password"
              autoComplete="current-password"
              className={inputCls}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              required
            />
          </label>
          {error && <Banner tone="error">{error}</Banner>}
          <Button type="submit" variant="primary" loading={busy} className="w-full">
            {t('login.submit')}
          </Button>
        </form>
      </Card>
    </div>
  )
}

function Nav({ active, onPick }: { active: Section; onPick: (s: Section) => void }) {
  const { t } = useTranslation()
  return (
    <nav className="space-y-0.5" aria-label={t('app.menu')}>
      {SECTIONS.map(({ id, icon }) => (
        <button
          key={id}
          type="button"
          onClick={() => onPick(id)}
          aria-current={active === id ? 'page' : undefined}
          className={`flex h-9 w-full items-center gap-2.5 rounded-lg px-3 text-sm font-medium transition-colors ${active === id ? 'bg-accent-500/10 text-accent-700 dark:text-accent-300' : 'text-muted hover:bg-surface-2 hover:text-fg'}`}
        >
          {icon}
          {t(`tabs.${id}`)}
        </button>
      ))}
    </nav>
  )
}

export default function AdminApp() {
  const { t } = useTranslation()
  const [phase, setPhase] = useState<Phase>({ kind: 'loading' })
  const [section, setSection] = useState<Section>(readSection)
  const [menuOpen, setMenuOpen] = useState(false)

  const check = useCallback(async () => {
    setPhase({ kind: 'loading' })
    try {
      const s = await adminApi.session()
      setPhase(s.authenticated ? { kind: 'ready' } : { kind: 'login' })
    } catch (e) {
      setPhase(
        e instanceof AdminError && e.code !== 'network' && e.code !== 'admin_unavailable' ? { kind: 'login' } : { kind: 'off' },
      )
    }
  }, [])

  useEffect(() => {
    void check()
    setUnauthorizedHandler(() => setPhase({ kind: 'login', note: t('errors.unauthorized') }))
    return () => setUnauthorizedHandler(null)
  }, [check, t])

  const pick = (s: Section) => {
    setSection(s)
    setMenuOpen(false)
    history.replaceState(null, '', `#${s}`)
    window.scrollTo({ top: 0 })
  }

  const logout = async () => {
    try {
      await adminApi.logout()
    } finally {
      setPhase({ kind: 'login' })
    }
  }

  if (phase.kind !== 'ready') {
    return (
      <div className="min-h-screen">
        <header className="border-b border-line bg-surface/80 backdrop-blur-md">
          <div className="mx-auto flex h-14 max-w-6xl items-center px-4">
            <Brand />
            <div className="ml-auto">
              <Tools />
            </div>
          </div>
        </header>
        {phase.kind === 'loading' && (
          <div className="mt-16 flex justify-center">
            <Spinner />
          </div>
        )}
        {phase.kind === 'off' && (
          <div className="mx-auto mt-16 max-w-md px-3">
            <Card title={t('off.title')}>
              <p className="mb-4 text-sm text-muted">{t('off.body')}</p>
              <Button onClick={() => void check()}>{t('off.retry')}</Button>
            </Card>
          </div>
        )}
        {phase.kind === 'login' && <Login note={phase.note} onDone={() => setPhase({ kind: 'ready' })} />}
      </div>
    )
  }

  return (
    <div className="min-h-screen lg:flex">
      {/* Sidebar on wide screens. */}
      <aside className="sticky top-0 hidden h-screen w-60 shrink-0 flex-col border-r border-line bg-surface px-3 py-4 lg:flex">
        <div className="mb-5 px-2">
          <Brand />
        </div>
        <Nav active={section} onPick={pick} />
        <div className="mt-auto px-2 text-xs text-muted">
          <span className="font-mono">v{__APP_VERSION__}</span>
        </div>
      </aside>
      <div className="min-w-0 flex-1">
        <header className="sticky top-0 z-30 border-b border-line bg-surface/80 backdrop-blur-md">
          <div className="mx-auto flex h-14 max-w-6xl items-center gap-2 px-3 sm:px-5">
            <button
              type="button"
              className="inline-flex size-8 items-center justify-center rounded-lg text-muted hover:bg-surface-2 hover:text-fg lg:hidden"
              aria-label={t('app.menu')}
              aria-expanded={menuOpen}
              onClick={() => setMenuOpen((o) => !o)}
            >
              {menuOpen ? <X size={16} /> : <Menu size={16} />}
            </button>
            <div className="lg:hidden">
              <Brand />
            </div>
            <h1 className="hidden text-base font-semibold tracking-tight lg:block">{t(`tabs.${section}`)}</h1>
            <div className="ml-auto">
              <Tools onLogout={() => void logout()} />
            </div>
          </div>
          {menuOpen && (
            <div className="border-t border-line px-3 py-2 lg:hidden">
              <Nav active={section} onPick={pick} />
            </div>
          )}
        </header>
        <main className="mx-auto max-w-6xl px-3 py-4 sm:px-5 sm:py-6">
          <h1 className="mb-3 text-lg font-semibold tracking-tight lg:hidden">{t(`tabs.${section}`)}</h1>
          {section === 'overview' && <OverviewPage />}
          {section === 'activity' && <ActivityPage />}
          {section === 'logs' && <LogsPage />}
          {section === 'notices' && <NoticesPage />}
          {section === 'tweaks' && <TweaksPage />}
        </main>
      </div>
    </div>
  )
}
