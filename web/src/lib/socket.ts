// Pure helpers of the live socket (`/ws`), kept free of imports so `pnpm test` runs them directly.

/** Close codes of `/ws` (see the backend's `api::ws`). */
export const WS_CLOSE = { unauthorized: 4401, forbidden: 4403, authTimeout: 4408, replaced: 4429 } as const

/** First message on a new socket: the session (never in the URL, proxies log URLs). */
export function authMessage(session: string): string {
  return JSON.stringify({ type: 'auth', session })
}

/**
 * What a closed socket means. `renew`: the session was refused or the socket closed before the
 * server answered (a request renews the session); `replaced`: closed for a newer socket of the same
 * user; `retry`: anything else (backoff).
 */
export function closeAction(code: number, opened: boolean): 'renew' | 'replaced' | 'retry' {
  if (code === WS_CLOSE.replaced) return 'replaced'
  if (code === WS_CLOSE.unauthorized || !opened) return 'renew'
  return 'retry'
}
