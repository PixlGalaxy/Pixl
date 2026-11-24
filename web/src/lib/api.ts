import type {
  AuthResult,
  ChannelRef,
  Feedback,
  GenreTrack,
  GuildChannels,
  GuildSettings,
  Jam,
  JamPreview,
  JamSettings,
  LoopMode,
  Lyrics,
  Member,
  PlayerState,
  PlaylistTracks,
  PublicConfig,
  SavedPlaylist,
  SavedPlaylists,
  SearchResult,
  Taste,
  UserPrefs,
} from './types'
import { isGatewayStatus, reportBackendTrouble } from './health'

/** Error returned by the backend as `{ error: { code, message } }`, or a network failure (`code: 'network'`). */
export class ApiError extends Error {
  readonly code: string
  readonly status: number
  /** "Every Lynx bot is busy": invite link of the next bot to add, when there is one. */
  readonly invite: string | null
  /** Seconds until the action is accepted again (e.g. `refresh_blocked`), when the server says. */
  readonly retryAfter: number | null
  constructor(code: string, message: string, status: number, invite: string | null = null, retryAfter: number | null = null) {
    super(message)
    this.name = 'ApiError'
    this.code = code
    this.status = status
    this.invite = invite
    this.retryAfter = retryAfter
  }
}

let session: string | null = null
let onUnauthorized: (() => void) | null = null
let reauth: (() => Promise<boolean>) | null = null
let reauthRunning: Promise<boolean> | null = null

export function setSession(value: string | null) {
  session = value
}
export function getSession() {
  return session
}
/** Called when a request comes back 401 and signing in again silently did not work. */
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn
}
/** Silent sign-in (Discord authorize with prompt none + `/api/auth`); resolves true on success. */
export function setReauthHandler(fn: (() => Promise<boolean>) | null) {
  reauth = fn
}

/**
 * The session was refused: sign in again once (concurrent callers share the attempt). False when
 * that failed too, after telling the app (session expired screen).
 */
export async function renewSession(): Promise<boolean> {
  if (!reauth) {
    onUnauthorized?.()
    return false
  }
  if (!reauthRunning) {
    reauthRunning = reauth()
      .catch((error) => {
        console.warn('[lynx] silent sign-in failed', error)
        return false
      })
      .finally(() => {
        setTimeout(() => (reauthRunning = null), 0)
      })
  }
  const ok = await reauthRunning
  if (!ok) onUnauthorized?.()
  return ok
}

type Method = 'GET' | 'POST' | 'PUT' | 'DELETE'

/**
 * Prefix of everything served by the Rust backend. nginx in the image proxies `/backend/*` to it
 * (stripping the prefix); the panel itself is served at `/`. Both go through Discord's `/` URL mapping.
 */
export const BACKEND = '/backend'

/** Relative `/backend/api/...` request. A 401 signs in again silently and retries once. */
export async function request<T>(path: string, method: Method = 'GET', body?: unknown, retried = false): Promise<T> {
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
    reportBackendTrouble()
    throw new ApiError('network', error instanceof Error ? error.message : String(error), 0)
  }
  if (isGatewayStatus(res.status)) reportBackendTrouble()

  if (res.status === 401 && session && path !== '/auth' && !retried) {
    if (await renewSession()) return request<T>(path, method, body, true)
  }
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
    const err = (data as { error?: { code?: string; message?: string; invite?: string; retryAfter?: number } } | null)
      ?.error
    throw new ApiError(
      err?.code ?? `http_${res.status}`,
      err?.message ?? res.statusText,
      res.status,
      err?.invite ?? null,
      typeof err?.retryAfter === 'number' ? err.retryAfter : null,
    )
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
  /** `expectedUid`: the current track shown; when another one plays by then nothing is skipped (a double click skips one track). */
  skip: (channelId: string, expectedUid?: string) => request<void>('/player/skip', 'POST', { channelId, expectedUid }),
  previous: (channelId: string) => request<void>('/player/previous', 'POST', { channelId }),
  seek: (channelId: string, positionMs: number) => request<void>('/player/seek', 'POST', { channelId, positionMs }),
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

  // Saved playlists (per user)
  playlists: () => request<SavedPlaylists>('/playlists'),
  pinPlaylist: (id: string, pinned: boolean) =>
    request<void>(`/playlists/${encodeURIComponent(id)}/pin`, 'POST', { pinned }),
  removePlaylist: (id: string) => request<void>(`/playlists/${encodeURIComponent(id)}`, 'DELETE'),
  /** Songs of a saved playlist, from the shared cache (at most an hour old) or a new fetch. */
  playlistTracks: (id: string) => request<PlaylistTracks>(`/playlists/${encodeURIComponent(id)}/tracks`),
  /** Fetch it again. Limited by the server: once every 5 s; spamming blocks it for 5 minutes (`refresh_blocked`). */
  refreshPlaylist: (id: string) => request<PlaylistTracks>(`/playlists/${encodeURIComponent(id)}/refresh`, 'POST'),
  /** Queue the whole playlist, or one of its songs (`trackUrl`), like the add box would. */
  addPlaylist: (channelId: string, id: string, trackUrl?: string) =>
    request<{ added: number }>(
      `/playlists/${encodeURIComponent(id)}/add`,
      'POST',
      trackUrl ? { channelId, trackUrl } : { channelId },
    ),

  /** Save a list (a queue entry's `source.url`) to your playlists, like queueing it would. */
  savePlaylist: (url: string) => request<{ playlist: SavedPlaylist }>('/playlists/save', 'POST', { url }),

  // AI radio
  aiStart: (channelId: string, prompt?: string) =>
    request<void>('/ai/start', 'POST', prompt ? { channelId, prompt } : { channelId }),
  aiStop: (channelId: string) => request<void>('/ai/stop', 'POST', { channelId }),
  aiCancel: (channelId: string, jobId: string) => request<void>('/ai/cancel', 'POST', { channelId, jobId }),

  // Jam
  jam: (channelId: string) => request<{ jam: Jam | null }>(`/jam?${q({ channelId })}`),
  jamPreview: (jamId: string) => request<JamPreview>(`/jam/${encodeURIComponent(jamId)}`),
  jamCreate: (channelId: string) =>
    request<{ jamId: string; shareLink: string; jam: Jam }>('/jam/create', 'POST', { channelId }),
  jamJoin: (channelId: string, jamId: string) => request<{ jam: Jam | null }>('/jam/join', 'POST', { channelId, jamId }),
  jamLeave: (channelId: string) => request<void>('/jam/leave', 'POST', { channelId }),
  jamSettings: (channelId: string, patch: Partial<JamSettings>) =>
    request<{ settings: JamSettings }>('/jam/settings', 'POST', { channelId, ...patch }),
  jamKick: (channelId: string, guildId: string, memberChannelId: string) =>
    request<void>('/jam/kick', 'POST', { channelId, guildId, memberChannelId }),
  jamClose: (channelId: string) => request<{ settings: JamSettings }>('/jam/close', 'POST', { channelId }),
  jamEnd: (channelId: string) => request<void>('/jam/end', 'POST', { channelId }),

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
  /** Songs of your history tagged with a genre, most played first. */
  tasteGenre: (genre: string, limit: number, offset: number) =>
    request<{ tracks: GenreTrack[]; total: number }>(
      `/me/taste/genre/${encodeURIComponent(genre)}?${q({ limit: String(limit), offset: String(offset) })}`,
    ),
  /** Removes a song from your history and dislikes it (the radio never plays it for you). */
  forgetTrack: (key: string) => request<void>(`/me/history/${encodeURIComponent(key)}`, 'DELETE'),
  /** Voice channel the panel follows (polling fallback of the socket's `channel` message). */
  meChannel: () => request<{ channel: ChannelRef | null }>('/me/channel'),
  /** The wipe also signs out every session of the user: sign in again right away (silently). */
  deleteData: async () => {
    await request<void>('/me/data', 'DELETE')
    await renewSession()
  },
  /** Sign this session out (`DELETE /api/auth`); its socket closes too. */
  signOut: async () => {
    await request<void>('/auth', 'DELETE')
    setSession(null)
  },
}
