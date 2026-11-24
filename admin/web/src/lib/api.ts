// Admin API client (same origin, /api). The session is an HttpOnly cookie the page never sees.
// Every mutating request carries `X-Lynx-Admin: 1`, which the server requires (CSRF guard).
import type { Activity, HealthReport, LogLine, Notice, NoticeInput, Overview, Tweaks, TweaksInput } from './types'

export class AdminError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message)
  }
}

type Method = 'GET' | 'POST' | 'PUT' | 'DELETE'

let onUnauthorized: (() => void) | null = null
/** Called when a request finds the session gone (expired, logged out elsewhere, restart). */
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn
}

async function request<T>(path: string, method: Method = 'GET', body?: unknown): Promise<T> {
  let res: Response
  try {
    res = await fetch(`/api${path}`, {
      method,
      credentials: 'same-origin',
      cache: 'no-store',
      headers: {
        Accept: 'application/json',
        ...(method === 'GET' ? {} : { 'X-Lynx-Admin': '1' }),
        ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  } catch {
    throw new AdminError(0, 'network', 'network error')
  }
  const text = await res.text()
  let data: unknown = null
  try {
    data = text ? JSON.parse(text) : null
  } catch {
    data = null
  }
  if (!res.ok) {
    const err = (data as { error?: { code?: string; message?: string } } | null)?.error
    const code = err?.code ?? (res.status === 502 || res.status === 503 || res.status === 504 ? 'admin_unavailable' : 'http')
    if (res.status === 401 && code === 'unauthorized') onUnauthorized?.()
    throw new AdminError(res.status, code, err?.message ?? res.statusText)
  }
  return data as T
}

export const adminApi = {
  session: () => request<{ enabled: boolean; authenticated: boolean }>('/session'),
  login: (username: string, password: string) => request<{ ok: true }>('/login', 'POST', { username, password }),
  logout: () => request<{ ok: true }>('/logout', 'POST'),
  overview: () => request<Overview>('/overview'),
  activity: () => request<Activity>('/activity'),
  logs: (after = 0) => request<{ lines: LogLine[]; latest: number }>(`/logs?after=${after}`),
  notices: () => request<{ notices: Notice[]; serverNow: number }>('/notices'),
  createNotice: (n: NoticeInput) => request<{ notices: Notice[]; serverNow: number }>('/notices', 'POST', n),
  updateNotice: (id: number, n: NoticeInput) =>
    request<{ notices: Notice[]; serverNow: number }>(`/notices/${id}`, 'PUT', n),
  deleteNotice: (id: number) => request<{ notices: Notice[]; serverNow: number }>(`/notices/${id}`, 'DELETE'),
  tweaks: () => request<Tweaks>('/tweaks'),
  setTweaks: (t: TweaksInput) => request<Tweaks>('/tweaks', 'PUT', t),
  ytdlpUpdate: () => request<Tweaks>('/actions/ytdlp-update', 'POST'),
  clearStreamCache: () => request<{ removed: number }>('/actions/clear-stream-cache', 'POST'),
  clearLookupCache: () => request<{ removed: number }>('/actions/clear-lookup-cache', 'POST'),
}

export type { HealthReport }

/** Live log lines (Server-Sent Events; the session cookie goes along). Returns a close function. */
export function streamLogs(
  after: number,
  on: { line: (l: LogLine) => void; lagged: (n: number) => void; open: (yes: boolean) => void },
): () => void {
  const es = new EventSource(`/api/logs/stream?after=${after}`)
  es.addEventListener('line', (e) => {
    try {
      on.line(JSON.parse((e as MessageEvent<string>).data) as LogLine)
    } catch {
      // ignore a malformed line
    }
  })
  es.addEventListener('lagged', (e) => on.lagged(Number((e as MessageEvent<string>).data) || 0))
  es.onopen = () => on.open(true)
  es.onerror = () => on.open(false)
  return () => es.close()
}
