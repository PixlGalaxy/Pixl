import { useCallback, useEffect, useRef, useState } from 'react'
import { api, BACKEND, getSession } from './api'
import { useLayoutMode } from './layout'
import type { AiState, ChannelRef, GuildSettings, Permissions, PlayerState, ServerMessage } from './types'

const PING_MS = 25_000
const POLL_MS = 5_000
/** Cap for the age of a sample measured with the local clock (client/server skew can't run the bar away). */
const MAX_LOCAL_AGE_MS = 5_000
/** Cap for the age measured on the server clock (`serverNow - positionAt`, no skew involved). */
const MAX_SERVER_AGE_MS = 60_000

/**
 * The one position sample everything derives from: `positionMs` at `at` (`performance.now()`, a
 * monotonic clock that keeps counting while the panel is hidden or in picture-in-picture). The
 * position is always `positionMs + (performance.now() - at)` while playing, never accumulated ticks.
 */
export type PositionAnchor = { positionMs: number; at: number }

/**
 * Anchor from a server sample. The sample's age comes from the server's own clock (`serverNow`) when
 * the message has it, so a skewed client clock does not matter; otherwise from the local clock, capped.
 */
export function anchorFrom(positionMs: number, positionAt: number, serverNow?: number): PositionAnchor {
  const age =
    serverNow != null
      ? Math.min(Math.max(serverNow - positionAt, 0), MAX_SERVER_AGE_MS)
      : Math.min(Math.max(Date.now() - positionAt, 0), MAX_LOCAL_AGE_MS)
  return { positionMs: positionMs + age, at: performance.now() }
}

/** Anchor for a position the user just picked (seek), before the server confirms it. */
export function localAnchor(positionMs: number): PositionAnchor {
  return { positionMs, at: performance.now() }
}

type Handlers = {
  onSettings: (s: GuildSettings) => void
  onPermissions: (p: Permissions) => void
}

/**
 * Live player state of the voice channel the user is in. The server decides that channel: the
 * socket (`/ws`) pushes `channel` when the user joins, leaves or moves, then the new `state`. While
 * the socket is down and the panel is visible, a 5 s poll of `GET /api/me/channel` + `GET /api/player`
 * does the same. A socket that cannot open (refused session) triggers a request, which renews the
 * session silently, and the next attempt uses the new one.
 */
export function useLivePlayer(initialChannel: ChannelRef | null, handlers: Handlers) {
  const [state, setState] = useState<PlayerState | null>(null)
  const [channel, setChannel] = useState<ChannelRef | null>(initialChannel)
  const [anchor, setAnchor] = useState<PositionAnchor>(() => localAnchor(0))
  const [loaded, setLoaded] = useState(false)
  const [socketOpen, setSocketOpen] = useState(false)
  const handlersRef = useRef(handlers)
  handlersRef.current = handlers

  const applyState = useCallback((next: PlayerState | null, serverNow?: number) => {
    setState(next)
    setLoaded(true)
    if (next) setAnchor(anchorFrom(next.positionMs, next.positionAt, serverNow ?? next.serverNow))
  }, [])

  const applyChannel = useCallback((next: ChannelRef | null) => {
    setChannel((prev) => (prev?.id === next?.id && prev?.name === next?.name ? prev : next))
  }, [])

  const refresh = useCallback(async () => {
    try {
      const { channel: now } = await api.meChannel()
      applyChannel(now)
      applyState(now ? await api.player(now.id) : null)
    } catch (error) {
      console.warn('[lynx] player refresh failed', error)
      setLoaded(true)
    }
  }, [applyChannel, applyState])

  // WebSocket with reconnect backoff. One socket for the whole session: the server switches channels.
  useEffect(() => {
    let ws: WebSocket | null = null
    let ping: ReturnType<typeof setInterval> | undefined
    let retry: ReturnType<typeof setTimeout> | undefined
    let attempts = 0
    let closed = false

    const open = () => {
      const session = getSession()
      if (!session || closed) return
      const proto = location.protocol === 'https:' ? 'wss' : 'ws'
      const params = new URLSearchParams({ session })
      let opened = false
      try {
        ws = new WebSocket(`${proto}://${location.host}${BACKEND}/ws?${params}`)
      } catch {
        schedule()
        return
      }
      ws.onopen = () => {
        opened = true
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
          case 'channel':
            applyChannel(msg.channel)
            break
          case 'state':
            applyState(msg.state, msg.serverNow)
            break
          case 'position':
            setAnchor(anchorFrom(msg.positionMs, msg.positionAt, msg.serverNow))
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
        // The handshake failed (the browser hides the status): a plain request finds out whether the
        // session was refused and renews it, so the next attempt carries the new one.
        if (!opened) void refresh()
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
  }, [applyChannel, applyState, refresh])

  // Fresh state (and a fresh position anchor) on load and whenever Discord switches between the
  // focused panel and picture-in-picture / grid: timers are throttled while minimized. A socket
  // (re)connect needs nothing here, the server sends the full state on connect.
  const layout = useLayoutMode()
  useEffect(() => {
    void refresh()
  }, [refresh, layout])
  // Coming back to a hidden panel: same.
  useEffect(() => {
    const onVisible = () => document.visibilityState === 'visible' && void refresh()
    document.addEventListener('visibilitychange', onVisible)
    return () => document.removeEventListener('visibilitychange', onVisible)
  }, [refresh])
  // Polling fallback while the socket is down and the panel is visible.
  useEffect(() => {
    if (socketOpen) return
    const id = setInterval(() => {
      if (document.visibilityState === 'visible') void refresh()
    }, POLL_MS)
    return () => clearInterval(id)
  }, [socketOpen, refresh])

  return { state, setState, channel, anchor, setAnchor, loaded, socketOpen, refresh }
}

/** Pure: the position an anchor stands for at `now` (`performance.now()` time). */
export function positionAt(anchor: PositionAnchor, playing: boolean, now: number): number {
  return anchor.positionMs + (playing ? Math.max(0, now - anchor.at) : 0)
}

/**
 * Interpolated playback position, recomputed from the anchor on every render. While playing it
 * re-renders on animation frames (about 10 per second), plus a 1 s timer because frames stop while
 * the panel is hidden or throttled in picture-in-picture.
 */
export function usePosition(state: PlayerState | null, anchor: PositionAnchor): number {
  const playing = state?.status === 'playing'
  const [, setTick] = useState(0)
  useEffect(() => {
    if (!playing) return
    let raf = 0
    let last = 0
    const frame = (t: number) => {
      if (t - last >= 100) {
        last = t
        setTick((n) => n + 1)
      }
      raf = requestAnimationFrame(frame)
    }
    raf = requestAnimationFrame(frame)
    const id = setInterval(() => setTick((n) => n + 1), 1000)
    return () => {
      cancelAnimationFrame(raf)
      clearInterval(id)
    }
  }, [playing])
  if (!state?.current) return 0
  const pos = positionAt(anchor, playing, performance.now())
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
