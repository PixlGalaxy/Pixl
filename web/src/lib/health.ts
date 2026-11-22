// Backend life check. `GET /backend/healthz` every 5 s while the page is visible, and right away
// when a request fails like a dead backend (network error, 502/503/504) or the socket drops.
// Two failures in a row = offline; the first success after that = online again.
import { useEffect, useSyncExternalStore } from 'react'
import { BACKEND } from './api'

const INTERVAL_MS = 5_000
const TIMEOUT_MS = 4_000
const FAILURES_FOR_OFFLINE = 2

let online = true
let failures = 0
let checking: Promise<void> | null = null
let pollers = 0
let timer: ReturnType<typeof setInterval> | undefined
const listeners = new Set<() => void>()
const recoveredListeners = new Set<() => void>()

function setOnline(next: boolean) {
  if (next === online) return
  online = next
  for (const l of listeners) l()
  if (next) for (const l of recoveredListeners) l()
}

async function probe(): Promise<boolean> {
  const ctrl = new AbortController()
  const id = setTimeout(() => ctrl.abort(), TIMEOUT_MS)
  try {
    const res = await fetch(`${BACKEND}/healthz`, { cache: 'no-store', signal: ctrl.signal })
    return res.ok
  } catch {
    return false
  } finally {
    clearTimeout(id)
  }
}

/** One health check (concurrent callers share it). */
export function checkBackend(): Promise<void> {
  if (!checking) {
    checking = probe()
      .then((ok) => {
        if (ok) {
          failures = 0
          setOnline(true)
        } else {
          failures += 1
          if (failures >= FAILURES_FOR_OFFLINE) setOnline(false)
        }
      })
      .finally(() => {
        checking = null
      })
  }
  return checking
}

/**
 * Something looked like a dead backend: check now, and once more shortly after so two failures
 * show the overlay in about a second instead of waiting for the next poll.
 */
export function reportBackendTrouble() {
  if (!online) return
  void checkBackend().then(() => {
    if (online && failures > 0) setTimeout(() => void checkBackend(), 1_000)
  })
}

/** Status codes nginx answers with while the backend is down or restarting. */
export const isGatewayStatus = (status: number) => status === 502 || status === 503 || status === 504

/** Runs `fn` every time the backend comes back after being offline. */
export function onBackendRecovered(fn: () => void): () => void {
  recoveredListeners.add(fn)
  return () => recoveredListeners.delete(fn)
}

function subscribe(fn: () => void) {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

function startPolling() {
  pollers += 1
  if (pollers > 1) return
  const tick = () => document.visibilityState === 'visible' && void checkBackend()
  timer = setInterval(tick, INTERVAL_MS)
  document.addEventListener('visibilitychange', tick)
  stopPolling = () => {
    clearInterval(timer)
    document.removeEventListener('visibilitychange', tick)
  }
}
let stopPolling = () => {}

/** `{ online }`, polling while any component uses it. */
export function useBackendHealth(): { online: boolean } {
  useEffect(() => {
    startPolling()
    return () => {
      pollers -= 1
      if (pollers === 0) stopPolling()
    }
  }, [])
  return { online: useSyncExternalStore(subscribe, () => online) }
}
