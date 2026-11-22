import { useCallback, useEffect, useRef, useState } from 'react'
import { api, getSession } from './api'
import type { AiState, GuildSettings, Permissions, PlayerState, ServerMessage } from './types'

const PING_MS = 25_000
const POLL_MS = 5_000
/** Cap for `now - positionAt` when a sample arrives, so client/server clock skew can't run the bar away. */
const MAX_SAMPLE_AGE_MS = 5_000

/** Position sample anchored to the local clock at the moment it arrived. */
export type PositionAnchor = { positionMs: number; localAt: number }

function anchorFrom(positionMs: number, positionAt: number): PositionAnchor {
  const now = Date.now()
  const age = Math.min(Math.max(now - positionAt, 0), MAX_SAMPLE_AGE_MS)
  return { positionMs: positionMs + age, localAt: now }
}

type Handlers = {
  onSettings: (s: GuildSettings) => void
  onPermissions: (p: Permissions) => void
}

/**
 * Live player state: WebSocket (`/ws`) with a 5 s `GET /api/player` poll while the socket is down
 * and the panel is visible.
 */
export function useLivePlayer(channelId: string | null, handlers: Handlers) {
  const [state, setState] = useState<PlayerState | null>(null)
  const [anchor, setAnchor] = useState<PositionAnchor>({ positionMs: 0, localAt: Date.now() })
  const [loaded, setLoaded] = useState(false)
  const [socketOpen, setSocketOpen] = useState(false)
  const handlersRef = useRef(handlers)
  handlersRef.current = handlers

  const applyState = useCallback((next: PlayerState | null) => {
    setState(next)
    setLoaded(true)
    if (next) setAnchor(anchorFrom(next.positionMs, next.positionAt))
  }, [])

  const refresh = useCallback(async () => {
    if (!channelId) {
      applyState(null)
      return
    }
    try {
      applyState(await api.player(channelId))
    } catch (error) {
      console.warn('[lynx] player refresh failed', error)
      setLoaded(true)
    }
  }, [channelId, applyState])

  // WebSocket with reconnect backoff.
  useEffect(() => {
    if (!channelId) return
    let ws: WebSocket | null = null
    let ping: ReturnType<typeof setInterval> | undefined
    let retry: ReturnType<typeof setTimeout> | undefined
    let attempts = 0
    let closed = false

    const open = () => {
      const session = getSession()
      if (!session || closed) return
      const proto = location.protocol === 'https:' ? 'wss' : 'ws'
      const params = new URLSearchParams({ session, channelId })
      try {
        ws = new WebSocket(`${proto}://${location.host}/ws?${params}`)
      } catch {
        schedule()
        return
      }
      ws.onopen = () => {
        attempts = 0
        setSocketOpen(true)
        ping = setInterval(() => ws?.readyState === WebSocket.OPEN && ws.send('{"type":"ping"}'), PING_MS)
      }
      ws.onmessage = (event) => {
        let msg: ServerMessage
        try {
          msg = JSON.parse(String(event.data)) as ServerMessage
        } catch {
          return
        }
        switch (msg.type) {
          case 'state':
            applyState(msg.state)
            break
          case 'position':
            setAnchor(anchorFrom(msg.positionMs, msg.positionAt))
            setState((s) => (s ? { ...s, positionMs: msg.positionMs, positionAt: msg.positionAt } : s))
            break
          case 'ai':
            setState((s) => (s ? { ...s, ai: msg.ai as AiState | null } : s))
            break
          case 'settings':
            handlersRef.current.onSettings(msg.settings)
            break
          case 'permissions':
            handlersRef.current.onPermissions(msg.permissions)
            break
        }
      }
      ws.onclose = () => {
        setSocketOpen(false)
        clearInterval(ping)
        schedule()
      }
    }
    const schedule = () => {
      if (closed) return
      attempts += 1
      retry = setTimeout(open, Math.min(30_000, 1_000 * 2 ** Math.min(attempts, 5)))
    }

    open()
    return () => {
      closed = true
      clearInterval(ping)
      clearTimeout(retry)
      if (ws) {
        ws.onclose = null
        ws.close()
      }
      setSocketOpen(false)
    }
  }, [channelId, applyState])

  // Initial load + polling fallback while the socket is down and the panel is visible.
  useEffect(() => {
    void refresh()
  }, [refresh])
  useEffect(() => {
    if (socketOpen) return
    const id = setInterval(() => {
      if (document.visibilityState === 'visible') void refresh()
    }, POLL_MS)
    const onVisible = () => document.visibilityState === 'visible' && void refresh()
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      clearInterval(id)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [socketOpen, refresh])

  return { state, setState, anchor, setAnchor, loaded, socketOpen, refresh }
}

/** Interpolated playback position, re-rendering ~4 times a second while playing. */
export function usePosition(state: PlayerState | null, anchor: PositionAnchor): number {
  const playing = state?.status === 'playing'
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!playing) return
    const id = setInterval(() => setNow(Date.now()), 250)
    return () => clearInterval(id)
  }, [playing])
  if (!state?.current) return 0
  const elapsed = playing ? Math.max(0, now - anchor.localAt) : 0
  const pos = anchor.positionMs + elapsed
  const dur = state.current.durationMs
  return dur != null ? Math.min(pos, dur) : pos
}

/** Re-renders every second; for countdowns. */
export function useNow(active: boolean, intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const id = setInterval(() => setNow(Date.now()), intervalMs)
    return () => clearInterval(id)
  }, [active, intervalMs])
  return now
}
