import { Languages, LogOut, Moon, Sun } from 'lucide-react'
import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { LynxMark } from '../components/Logo'
import { AdminError, adminApi, setUnauthorizedHandler } from './api'
import { useErrorText } from './errors'
import { LANGUAGES, setLanguage } from './i18n'
import { NoticesPage } from './Notices'
import { OverviewPage } from './Overview'
import { TweaksPage } from './Tweaks'
import { Banner, Button, Card, inputCls, Spinner } from './ui'

type Tab = 'overview' | 'notices' | 'tweaks'
const TABS: Tab[] = ['overview', 'notices', 'tweaks']
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

function TopBar({ onLogout }: { onLogout?: () => void }) {
  const { t, i18n } = useTranslation()
  const [dark, toggleTheme] = useTheme()
  const next = LANGUAGES.find((l) => l.code !== i18n.language) ?? LANGUAGES[0]
  return (
    <div className="flex h-12 items-center gap-2">
      <LynxMark className="size-7 text-fg" />
      <h1 className="text-base font-semibold tracking-tight">{t('app.title')}</h1>
      <div className="ml-auto flex items-center gap-1">
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
          <Button variant="ghost" icon={<LogOut size={14} />} onClick={onLogout}>
            {t('app.logout')}
          </Button>
        )}
      </div>
    </div>
  )
}

function Login({ note, onDone }: { note?: string; onDone: () => void }) {
  const { t } = useTranslation()
  const errorText = useErrorText()
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    try {
      await adminApi.login(password)
      setPassword('')
      onDone()
    } catch (err) {
      setError(errorText(err))
    } finally {
      setBusy(false)
    }
  }
  return (
    <div className="mx-auto mt-16 max-w-sm">
      <Card title={t('login.title')}>
        <p className="mb-4 text-sm text-muted">{t('login.subtitle')}</p>
        <form onSubmit={(e) => void submit(e)} className="space-y-3">
          {note && !error && <Banner tone="warning">{note}</Banner>}
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-muted">{t('login.password')}</span>
            <input
              type="password"
              autoComplete="current-password"
              autoFocus
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

export default function AdminApp() {
  const { t } = useTranslation()
  const [phase, setPhase] = useState<Phase>({ kind: 'loading' })
  const [tab, setTab] = useState<Tab>('overview')

  const check = useCallback(async () => {
    setPhase({ kind: 'loading' })
    try {
      const s = await adminApi.session()
      setPhase(s.authenticated ? { kind: 'ready' } : { kind: 'login' })
    } catch (e) {
      setPhase(e instanceof AdminError && e.code !== 'network' && e.code !== 'admin_unavailable' ? { kind: 'login' } : { kind: 'off' })
    }
  }, [])

  useEffect(() => {
    void check()
    setUnauthorizedHandler(() => setPhase({ kind: 'login', note: t('errors.unauthorized') }))
    return () => setUnauthorizedHandler(null)
  }, [check, t])

  const logout = async () => {
    try {
      await adminApi.logout()
    } finally {
      setPhase({ kind: 'login' })
    }
  }

  return (
    <div className="min-h-screen">
      <header className="sticky top-0 z-30 border-b border-line bg-surface/80 backdrop-blur-md">
        <div className="mx-auto max-w-5xl px-3 sm:px-4">
          <TopBar onLogout={phase.kind === 'ready' ? () => void logout() : undefined} />
          {phase.kind === 'ready' && (
            <nav className="no-scrollbar flex h-11 items-stretch overflow-x-auto" aria-label={t('app.title')}>
              {TABS.map((id) => (
                <button
                  key={id}
                  type="button"
                  onClick={() => setTab(id)}
                  aria-current={tab === id ? 'page' : undefined}
                  className={`relative inline-flex shrink-0 items-center px-3 text-sm font-medium whitespace-nowrap transition-colors ${tab === id ? 'text-accent-600 dark:text-accent-400' : 'text-muted hover:text-fg'}`}
                >
                  {t(`tabs.${id}`)}
                  {tab === id && <span className="absolute inset-x-2 bottom-0 h-0.5 rounded-full bg-accent-500" />}
                </button>
              ))}
            </nav>
          )}
        </div>
      </header>
      <main className="mx-auto max-w-5xl px-3 py-4 sm:px-4 sm:py-6">
        {phase.kind === 'loading' && <Spinner />}
        {phase.kind === 'off' && (
          <div className="mx-auto mt-16 max-w-md">
            <Card title={t('off.title')}>
              <p className="mb-4 text-sm text-muted">{t('off.body')}</p>
              <Button onClick={() => void check()}>{t('off.retry')}</Button>
            </Card>
          </div>
        )}
        {phase.kind === 'login' && <Login note={phase.note} onDone={() => setPhase({ kind: 'ready' })} />}
        {phase.kind === 'ready' && (
          <>
            {tab === 'overview' && <OverviewPage />}
            {tab === 'notices' && <NoticesPage />}
            {tab === 'tweaks' && <TweaksPage />}
          </>
        )}
      </main>
    </div>
  )
}
