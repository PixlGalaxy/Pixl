import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { PipView } from './components/PipView'
import { ReconnectingOverlay } from './components/Reconnecting'
import { readTab, Shell, type TabId } from './components/Shell'
import { api, setReauthHandler, setUnauthorizedHandler } from './lib/api'
import { applySaved } from './lib/appearance'
import { connect, isInsideDiscord, LoginError, reauthenticate, type Connection, type LoginStep } from './lib/discord'
import { errorMessage } from './lib/format'
import { useBackendHealth } from './lib/health'
import i18n, { DEFAULT_LANGUAGE, matchLanguage, setDocumentLanguage } from './lib/i18n'
import { useCompactLayout } from './lib/layout'
import { LynxProvider } from './lib/lynx'
import { ConnectingScreen, NoGuildScreen, OutsideDiscordScreen, SessionExpiredScreen } from './screens/Gate'

type Phase =
  | { kind: 'connecting'; step: LoginStep }
  | { kind: 'failed'; step: LoginStep | null; message: string }
  | { kind: 'ready'; conn: Connection }
  | { kind: 'expired' }

/** Language, theme and accent saved for the user (they follow the user across devices). */
async function applyInitialPrefs(conn: Connection) {
  let saved: string | null = null
  try {
    const prefs = await api.prefs()
    saved = prefs.locale
    applySaved(prefs.theme, prefs.accent)
  } catch (error) {
    console.warn('[lynx] prefs', error)
  }
  const lang = matchLanguage(saved) ?? matchLanguage(conn.auth.locale) ?? DEFAULT_LANGUAGE
  await i18n.changeLanguage(lang)
  setDocumentLanguage(lang)
}

/** Full shell when focused; the compact view in picture-in-picture or a grid tile. */
function Panel() {
  const compact = useCompactLayout()
  const [tab, setTab] = useState<TabId>(readTab)
  const { online } = useBackendHealth()
  return (
    <>
      <div inert={!online}>{compact ? <PipView /> : <Shell tab={tab} onTab={setTab} />}</div>
      {!online && <ReconnectingOverlay compact={compact} />}
    </>
  )
}

export default function App() {
  const inside = isInsideDiscord()
  const [phase, setPhase] = useState<Phase>({ kind: 'connecting', step: 'config' })
  const started = useRef(false)
  useTranslation()

  useEffect(() => {
    // StrictMode runs effects twice in dev; the OAuth code is single-use, so authorize only once.
    if (!inside || started.current) return
    started.current = true
    // A refused session first signs in again silently; the expired screen is the last resort.
    setReauthHandler(reauthenticate)
    setUnauthorizedHandler(() => setPhase({ kind: 'expired' }))
    connect((step) => setPhase({ kind: 'connecting', step }))
      .then(async (conn) => {
        await applyInitialPrefs(conn)
        setPhase({ kind: 'ready', conn })
      })
      .catch((error) => {
        setPhase({
          kind: 'failed',
          step: error instanceof LoginError ? error.step : null,
          message: errorMessage(error),
        })
      })
  }, [inside])

  if (!inside) return <OutsideDiscordScreen />
  if (phase.kind === 'connecting') return <ConnectingScreen step={phase.step} />
  if (phase.kind === 'failed') return <ConnectingScreen step={phase.step} error={phase.message} />
  if (phase.kind === 'expired') return <SessionExpiredScreen />
  if (!phase.conn.auth.guild) return <NoGuildScreen />
  return (
    <LynxProvider conn={phase.conn}>
      <Panel />
    </LynxProvider>
  )
}
