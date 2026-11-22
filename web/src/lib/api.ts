import type {
  AuthResult,
  Feedback,
  GuildChannels,
  GuildSettings,
  LoopMode,
  Lyrics,
  Member,
  PlayerState,
  PublicConfig,
  SearchResult,
  Taste,
  UserPrefs,
} from './types'

/** Error returned by the backend as `{ error: { code, message } }`, or a network failure (`code: 'network'`). */
export class ApiError extends Error {
  readonly code: string
  readonly status: number
  constructor(code: string, message: string, status: number) {
    super(message)
    this.name = 'ApiError'
    this.code = code
    this.status = status
  }
}

let session: string | null = null
let onUnauthorized: (() => void) | null = null

export function setSession(value: string | null) {
  session = value
}
export function getSession() {
  return session
}
/** Called once when any request comes back 401 (expired or revoked session). */
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn
}

type Method = 'GET' | 'POST' | 'PUT' | 'DELETE'

/**
 * Prefix of everything served by the Rust backend. nginx in the image proxies `/backend/*` to it
 * (stripping the prefix); the panel itself is served at `/`. Both go through Discord's `/` URL mapping.
 */
export const BACKEND = '/backend'

/** Relative `/backend/api/...` request. */
export async function request<T>(path: string, method: Method = 'GET', body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (body !== undefined) headers['Content-Type'] = 'application/json'
  if (session) headers.Authorization = `Bearer ${session}`

  let res: Response
  try {
    res = await fetch(`${BACKEND}/api${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  } catch (error) {
    throw new ApiError('network', error instanceof Error ? error.message : String(error), 0)
  }

  if (res.status === 401 && session) onUnauthorized?.()
  if (res.status === 204) return undefined as T

  const text = await res.text()
  let data: unknown = null
  if (text) {
    try {
      data = JSON.parse(text)
    } catch {
      data = null
    }
  }
  if (!res.ok) {
    const err = (data as { error?: { code?: string; message?: string } } | null)?.error
    throw new ApiError(err?.code ?? `http_${res.status}`, err?.message ?? res.statusText, res.status)
  }
  return data as T
}

const q = (params: Record<string, string>) => new URLSearchParams(params).toString()

/** Typed client for every route in plan/api.md. */
export const api = {
  // Auth
  publicConfig: () => request<PublicConfig>('/public'),
  auth: (code: string, guildId: string | null, channelId: string | null) =>
    request<AuthResult>('/auth', 'POST', { code, guildId, channelId }),

  // Player state
  player: (channelId: string) => request<PlayerState | null>(`/player?${q({ channelId })}`),

  // Player actions
  pause: (channelId: string) => request<void>('/player/pause', 'POST', { channelId }),
  resume: (channelId: string) => request<void>('/player/resume', 'POST', { channelId }),
  skip: (channelId: string) => request<void>('/player/skip', 'POST', { channelId }),
  previous: (channelId: string) => request<void>('/player/previous', 'POST', { channelId }),
  seek: (channelId: string, positionMs: number) => request<void>('/player/seek', 'POST', { channelId, positionMs }),
  volume: (channelId: string, volume: number) => request<void>('/player/volume', 'POST', { channelId, volume }),
  loop: (channelId: string, mode: LoopMode) => request<void>('/player/loop', 'POST', { channelId, mode }),
  shuffle: (channelId: string) => request<void>('/player/shuffle', 'POST', { channelId }),
  stop: (channelId: string) => request<void>('/player/stop', 'POST', { channelId }),

  // Queue
  add: (channelId: string, query: string) => request<{ added: number }>('/queue/add', 'POST', { channelId, query }),
  move: (channelId: string, uid: string, toIndex: number) =>
    request<void>('/queue/move', 'POST', { channelId, uid, toIndex }),
  remove: (channelId: string, uid: string) => request<void>('/queue/remove', 'POST', { channelId, uid }),
  clear: (channelId: string) => request<void>('/queue/clear', 'POST', { channelId }),
  feedback: (channelId: string, uid: string, value: Feedback | null) =>
    request<void>('/tracks/feedback', 'POST', { channelId, uid, value }),

  // AI radio
  aiStart: (channelId: string, prompt?: string) =>
    request<void>('/ai/start', 'POST', prompt ? { channelId, prompt } : { channelId }),
  aiStop: (channelId: string) => request<void>('/ai/stop', 'POST', { channelId }),
  aiCancel: (channelId: string, jobId: string) => request<void>('/ai/cancel', 'POST', { channelId, jobId }),

  // Search and lyrics
  search: (query: string) => request<{ results: SearchResult[] }>(`/search?${q({ q: query })}`),
  lyrics: (channelId: string) => request<Lyrics | null>(`/lyrics?${q({ channelId })}`),

  // Settings
  settings: () => request<GuildSettings>('/settings'),
  updateSettings: (patch: Partial<GuildSettings>) => request<GuildSettings | void>('/settings', 'PUT', patch),
  djs: () => request<{ djs: Member[] }>('/djs'),
  addDj: (userId: string) => request<void>('/djs', 'POST', { userId }),
  removeDj: (userId: string) => request<void>(`/djs/${encodeURIComponent(userId)}`, 'DELETE'),
  guildChannels: () => request<GuildChannels>('/guild/channels'),
  candidates: (query: string) => request<{ members: Member[] }>(`/members/candidates?${q({ q: query })}`),

  // User
  prefs: () => request<UserPrefs>('/me/prefs'),
  updatePrefs: (patch: Partial<UserPrefs>) => request<UserPrefs | void>('/me/prefs', 'PUT', patch),
  taste: () => request<Taste>('/me/taste'),
  deleteData: () => request<void>('/me/data', 'DELETE'),
}
