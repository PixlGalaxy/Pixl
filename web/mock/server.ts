// Fake Lynx backend for `pnpm dev:mock`. Runs inside the Vite dev server only (never bundled).
// All songs, artists and lyric lines below are invented.
//
// Flags (add them to the page URL):
//   ?ai=0            features.ai = false
//   ?role=member     no control, can add      ?role=dj  control, not manager
//   ?empty=1         nothing playing          ?empty_channel=1  paused because the channel is empty
//   ?lyrics=plain|none                        ?vote=1  vote skip in progress
//   ?outside=1       show the "open from Discord" screen
//   ?lang=es         Discord locale reported at sign-in
//   ?pip=1 / ?grid=1 start in picture-in-picture / grid layout (mock SDK; keys `p` / `g` toggle)
//   ?voice=101|none  voice channel the user starts in (default 100, where the player is)
//   ?channel_id=100  launch channel (default 200, a text channel: the panel follows your voice channel)
// Keys in the browser (mock SDK): `v` moves you 100 -> 101 -> no voice -> 100 (socket `channel`
// message + new state), `x` expires every session (the panel signs in again silently).
import type { IncomingMessage, ServerResponse } from 'node:http'
import type { Plugin, ViteDevServer } from 'vite'
import { WebSocketServer, type WebSocket } from 'ws'
import type {
  AiPhase,
  AiState,
  GuildSettings,
  Member,
  Permissions,
  PlayerState,
  Track,
  TrackReason,
} from '../src/lib/types'
import { ACCENTS } from '../src/lib/appearance'

type Flags = {
  ai: boolean
  role: 'manager' | 'dj' | 'member'
  empty: boolean
  emptyChannel: boolean
  lyrics: 'synced' | 'plain' | 'none'
  vote: boolean
  voice: string | null
}

function parseFlags(search: string): Flags {
  const p = new URLSearchParams(search)
  const role = p.get('role')
  const lyrics = p.get('lyrics')
  return {
    ai: p.get('ai') !== '0',
    role: role === 'dj' || role === 'member' ? role : 'manager',
    empty: p.get('empty') === '1',
    emptyChannel: p.get('empty_channel') === '1',
    lyrics: lyrics === 'plain' || lyrics === 'none' ? lyrics : 'synced',
    vote: p.get('vote') === '1',
    voice: p.get('voice') === 'none' ? null : (p.get('voice') ?? '100'),
  }
}
// Sessions are `mock.<epoch>.<flags>`; `x` bumps the epoch so every older session gets 401.
let sessionEpoch = 1
let voiceInitialized = false
const encodeFlags = (search: string) => `mock.${sessionEpoch}.${Buffer.from(search || '?').toString('base64url')}`
function sessionValid(session: string | null | undefined) {
  return !!session && session.startsWith(`mock.${sessionEpoch}.`)
}
const decodeFlags = (session: string | null | undefined) =>
  parseFlags(
    session?.startsWith('mock.') ? Buffer.from(session.split('.')[2] ?? '', 'base64url').toString() : '',
  )

// Voice channels of the mock guild; the player lives in PLAYER_CHANNEL.
const PLAYER_CHANNEL = '100'
const CHANNEL_NAMES: Record<string, string> = { '100': 'Lounge', '101': 'Gaming', '102': 'Study Room', '200': 'general' }
const channelRef = (id: string | null) => (id ? { id, name: CHANNEL_NAMES[id] ?? id } : null)

// ---------------------------------------------------------------------------------------------
// Data

const USERS: Member[] = [
  { id: '1001', name: 'Pixl', avatarUrl: null },
  { id: '1002', name: 'Marta', avatarUrl: null },
  { id: '1003', name: 'Kenji', avatarUrl: null },
  { id: '1004', name: 'Lucia', avatarUrl: null },
  { id: '1005', name: 'Omar', avatarUrl: null },
  { id: '1006', name: 'Ines', avatarUrl: null },
]
const ME = USERS[0]!

type Seed = { title: string; artists: string[]; dur: number; genres: string[]; origin?: Track['origin'] }
const CATALOG: Seed[] = [
  { title: 'Night Drive', artists: ['Paper Lanterns'], dur: 214, genres: ['synthwave', 'electronic'] },
  { title: 'Glass Harbor', artists: ['Mira Vale'], dur: 187, genres: ['indie pop'] },
  { title: 'Static Bloom', artists: ['The Quiet Engines'], dur: 245, genres: ['indie rock', 'shoegaze'] },
  { title: 'Copper Skies', artists: ['Juno Reyes', 'Tidepool'], dur: 202, genres: ['house'] },
  { title: 'Low Tide Letters', artists: ['Ana Sorel'], dur: 263, genres: ['folk', 'acoustic'] },
  { title: 'Runway Lights', artists: ['Neon Orchard'], dur: 176, genres: ['dance pop'] },
  { title: 'Paper Moons', artists: ['Hollow Fields'], dur: 298, genres: ['dream pop'] },
  { title: 'Concrete Garden', artists: ['Kito Blane'], dur: 191, genres: ['hip hop', 'lo-fi'] },
  { title: 'Slow Satellites', artists: ['Echo Atlas'], dur: 322, genres: ['ambient'] },
  { title: 'Marble Hearts', artists: ['Velvet Arcade'], dur: 208, genres: ['alt rock'] },
  { title: 'Sundial', artists: ['Lua Marin'], dur: 233, genres: ['latin pop'], origin: 'spotify' },
  { title: 'Wired Forest', artists: ['Bitmoss'], dur: 199, genres: ['electronic', 'idm'] },
  { title: 'Harbor Lights (Live Session)', artists: ['Mira Vale'], dur: 251, genres: ['indie pop'], origin: 'youtube' },
  { title: 'Orange Hour', artists: ['Calla Rift'], dur: 184, genres: ['r&b', 'soul'] },
  { title: 'Midnight Ferry', artists: ['Paper Lanterns'], dur: 226, genres: ['synthwave'] },
  { title: 'Gravel Roads', artists: ['Dusty Compass'], dur: 207, genres: ['country', 'americana'] },
  { title: 'Polaroid Rain', artists: ['Hollow Fields'], dur: 241, genres: ['dream pop'] },
  { title: 'Signal Fire', artists: ['The Quiet Engines'], dur: 219, genres: ['indie rock'] },
]

const COLORS = ['#0ea5e9', '#f59e0b', '#10b981', '#ef4444', '#64748b', '#14b8a6', '#f97316', '#84cc16', '#e11d48', '#0284c7']

let uidCounter = 1
const startedAt = Date.now()
function makeTrack(seed: Seed, requester: Member, reason: TrackReason | null = null): Track {
  const index = CATALOG.indexOf(seed)
  return {
    uid: `t${uidCounter++}`,
    title: seed.title,
    artists: seed.artists,
    durationMs: seed.dur * 1000,
    artworkUrl: `/mock/art/${index}.svg`,
    url: `https://music.example.com/watch?v=mock${index}`,
    origin: seed.origin ?? 'youtube_music',
    requestedBy: requester,
    genres: seed.genres,
    reason,
    feedback: null,
  }
}

function artSvg(index: number): string {
  const seed = CATALOG[index] ?? CATALOG[0]!
  const color = COLORS[index % COLORS.length]!
  const initials = seed.title
    .split(/\s+/)
    .slice(0, 2)
    .map((w) => w[0])
    .join('')
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 300"><rect width="300" height="300" fill="${color}"/><circle cx="230" cy="70" r="120" fill="#000" opacity="0.12"/><text x="24" y="270" font-family="Inter, sans-serif" font-size="64" font-weight="700" fill="#fff" opacity="0.92">${initials}</text></svg>`
}

const SYNCED_LINES = [
  'Headlights paint the empty road',
  'Every sign is pointing home',
  'Radio hums a borrowed tune',
  'Underneath a paper moon',
  '',
  'Keep on driving, keep it slow',
  'There is nowhere else to go',
  'All the city lights below',
  'Fading out like falling snow',
  '',
  'Count the miles and count the stars',
  'Counting all the things we are',
  'Windows down and hands up high',
  'Chasing colors through the night',
  '',
  'Keep on driving, keep it slow',
  'There is nowhere else to go',
  'Night drive, night drive',
  'Hold the wheel and close your eyes',
]

function makeLyrics(durationMs: number) {
  const step = Math.floor((durationMs - 12_000) / SYNCED_LINES.length)
  return SYNCED_LINES.map((line, i) => ({ timeMs: 8_000 + i * step, line }))
}

// ---------------------------------------------------------------------------------------------
// State

const s = {
  status: 'playing' as PlayerState['status'],
  current: makeTrack(CATALOG[0]!, ME) as Track | null,
  positionMs: 47_000,
  positionAt: Date.now(),
  upcoming: [] as Track[],
  history: [] as Track[],
  loop: 'off' as PlayerState['loop'],
  volume: 100,
  ai: null as AiState | null,
  settings: {
    controlMode: 'everyone',
    voteSkip: { enabled: false, mode: 'percent', value: 50 },
    maxQueueLength: 500,
    defaultVolume: 100,
    commandChannels: [],
    voiceChannels: [],
  } as GuildSettings,
  djs: [USERS[2]!] as Member[],
  prefs: {
    locale: null as string | null,
    historyEnabled: true,
    theme: null as 'light' | 'dark' | 'system' | null,
    accent: null as string | null,
  },
  aiTimer: null as ReturnType<typeof setTimeout> | null,
  /** Voice channel the mock user is in (the panel follows it). */
  voice: '100' as string | null,
}
for (let i = 1; i <= 12; i++) s.upcoming.push(makeTrack(CATALOG[i]!, USERS[i % USERS.length]!))
for (let i = 13; i < 18; i++) s.history.unshift(makeTrack(CATALOG[i]!, USERS[(i + 2) % USERS.length]!))

function currentPosition() {
  if (s.status !== 'playing') return s.positionMs
  return s.positionMs + (Date.now() - s.positionAt)
}
function setPosition(ms: number) {
  s.positionMs = ms
  s.positionAt = Date.now()
}

function permissionsFor(f: Flags): Permissions {
  if (f.role === 'member') return { isManager: false, isDj: false, canControl: false, canAdd: true }
  if (f.role === 'dj') return { isManager: false, isDj: true, canControl: true, canAdd: true }
  return { isManager: true, isDj: false, canControl: true, canAdd: true }
}

function snapshot(f: Flags): PlayerState | null {
  if (f.empty || !s.current || s.voice !== PLAYER_CHANNEL) return null
  const emptyChannel = f.emptyChannel
  return {
    guildId: '1',
    channelId: PLAYER_CHANNEL,
    channelName: CHANNEL_NAMES[PLAYER_CHANNEL]!,
    bot: { id: '900', name: 'Lynx', isMain: true },
    status: emptyChannel ? 'paused' : s.status,
    pausedReason: emptyChannel ? 'empty_channel' : s.status === 'paused' ? 'user' : null,
    emptyChannelDeadline: emptyChannel ? startedAt + 180_000 : null,
    current: s.current,
    positionMs: Math.round(currentPosition()),
    positionAt: Date.now(),
    upcoming: s.upcoming,
    history: s.history.slice(0, 20),
    loop: s.loop,
    volume: s.volume,
    ai: f.ai ? s.ai : null,
    skipVotes: f.vote ? { current: 2, required: 4 } : null,
    voteSkipEnabled: s.settings.voteSkip.enabled,
    serverNow: Date.now(),
  }
}
function advance() {
  if (!s.current) return
  if (s.loop === 'track') return setPosition(0)
  s.history.unshift(s.current)
  if (s.loop === 'queue') s.upcoming.push({ ...s.current, uid: `t${uidCounter++}` })
  s.current = s.upcoming.shift() ?? null
  setPosition(0)
  if (!s.current) s.status = 'idle'
}

// AI job: walks through every phase, then queues a few picks.
const PHASES: AiPhase[] = ['queued', 'loading_model', 'reading_taste', 'finding_songs', 'thinking', 'resolving', 'ready']
function runAiJob(prompt?: string) {
  if (!s.ai) return
  const id = `job${Date.now()}`
  let i = 0
  const tick = () => {
    if (!s.ai?.job || s.ai.job.id !== id || s.ai.job.phase === 'cancelled') return
    i += 1
    const phase = PHASES[i]!
    s.ai.job = { ...s.ai.job, phase, canCancel: phase !== 'ready' }
    if (phase === 'ready') {
      const reasons: TrackReason[] = [
        { key: 'because_you_like', params: { name: 'Paper Lanterns' } },
        { key: 'similar_listeners', params: {} },
        { key: 'your_favorite', params: {} },
        prompt ? { key: 'matches_prompt', params: { prompt } } : { key: 'because_you_like', params: { name: 'Hollow Fields' } },
      ]
      for (const r of reasons) {
        const seed = CATALOG[Math.floor(Math.random() * CATALOG.length)]!
        s.upcoming.push(makeTrack(seed, { id: '900', name: 'Lynx Radio', avatarUrl: null }, r))
      }
      broadcast()
      s.aiTimer = setTimeout(() => {
        if (s.ai?.job?.id === id) {
          s.ai.job = null
          broadcast()
        }
      }, 6000)
      return
    }
    broadcastAi()
    s.aiTimer = setTimeout(tick, 2600)
  }
  s.ai.job = { id, phase: 'queued', startedAt: Date.now(), canCancel: true }
  s.aiTimer = setTimeout(tick, 2600)
}
// Start with a job in progress so the progress list is visible right away.
s.ai = { active: true, ownerId: ME.id, job: null }
runAiJob()
// Keep the demo cycling: restart the job a while after it finishes.
setInterval(() => {
  if (s.ai?.active && !s.ai.job) runAiJob()
}, 30_000)

setInterval(() => {
  if (s.status === 'playing' && s.current?.durationMs && currentPosition() >= s.current.durationMs) {
    advance()
    broadcast()
  }
}, 500)

// ---------------------------------------------------------------------------------------------
// WebSocket

const sockets = new Map<WebSocket, Flags>()
function send(ws: WebSocket, msg: unknown) {
  if (ws.readyState === ws.OPEN) ws.send(JSON.stringify(msg))
}
function broadcast() {
  for (const [ws, f] of sockets) send(ws, { type: 'state', state: snapshot(f) })
}
/** The user moved: every socket gets `channel`, then the state of that channel, then permissions. */
function broadcastChannel() {
  for (const [ws, f] of sockets) {
    send(ws, { type: 'channel', channel: channelRef(s.voice) })
    send(ws, { type: 'state', state: snapshot(f) })
    send(ws, { type: 'permissions', permissions: permissionsFor(f) })
  }
}
function broadcastAi() {
  for (const [ws, f] of sockets) send(ws, { type: 'ai', ai: f.ai ? s.ai : null })
}
setInterval(() => {
  for (const [ws, f] of sockets) {
    if (!snapshot(f)) continue
    send(ws, { type: 'position', positionMs: Math.round(currentPosition()), positionAt: Date.now() })
  }
}, 5000)

// ---------------------------------------------------------------------------------------------
// HTTP

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
  ) {
    super(code)
  }
}

async function readBody(req: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = []
  for await (const c of req) chunks.push(c as Buffer)
  if (chunks.length === 0) return {}
  try {
    return JSON.parse(Buffer.concat(chunks).toString())
  } catch {
    return {}
  }
}

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms))

function requireControl(f: Flags) {
  if (!permissionsFor(f).canControl) throw new HttpError(403, 'no_permission')
}

async function handle(req: IncomingMessage, url: URL, f: Flags, session: string | null): Promise<unknown> {
  const method = req.method ?? 'GET'
  const path = url.pathname.replace(/^\/api/, '')
  const body = method === 'POST' || method === 'PUT' ? await readBody(req) : {}
  const changed = () => {
    broadcast()
    return undefined
  }

  if (path === '/public') {
    return {
      clientId: '000000000000000000',
      features: { ai: f.ai },
      links: {
        landing: 'https://lynx.example.com',
        terms: 'https://lynx.example.com/terms',
        privacy: 'https://lynx.example.com/privacy',
        support: 'https://lynx.example.com/support',
      },
    }
  }
  // Mock-only controls, driven by the keys in mock/discord-sdk.ts.
  if (method === 'POST' && path === '/mock/voice') {
    const order = ['100', '101', null]
    s.voice = order[(order.indexOf(s.voice) + 1) % order.length] ?? null
    broadcastChannel()
    return { voice: s.voice }
  }
  if (method === 'POST' && path === '/mock/expire') {
    sessionEpoch += 1
    for (const ws of sockets.keys()) ws.close()
    return { epoch: sessionEpoch }
  }
  if (path === '/auth') {
    if (!voiceInitialized) {
      s.voice = f.voice
      voiceInitialized = true
    }
    const launch = (body.channelId as string | null) ?? null
    return {
      accessToken: 'mock-access-token',
      session: encodeFlags(new URL(req.headers.referer ?? 'http://x/').search),
      user: { id: ME.id, username: 'pixl', globalName: ME.name, avatarUrl: null },
      guild: { id: '1', name: 'Galaxy Hangout', iconUrl: null },
      channel: channelRef(s.voice),
      launchChannel: channelRef(launch),
      permissions: permissionsFor(f),
      locale: new URL(req.headers.referer ?? 'http://x/').searchParams.get('lang') ?? 'en-US',
    }
  }
  if (!sessionValid(session)) throw new HttpError(401, 'unauthorized')

  // Like the real API: player, queue, radio and lyrics routes only take the user's voice channel.
  if (/^\/(player|queue|tracks|ai|lyrics)/.test(path)) {
    const channel = String(body.channelId ?? url.searchParams.get('channelId') ?? '')
    if (!s.voice || channel !== s.voice) throw new HttpError(403, 'not_in_channel')
  }

  switch (`${method} ${path}`) {
    case 'GET /player':
      return snapshot(f)
    case 'POST /player/pause':
      requireControl(f)
      setPosition(currentPosition())
      s.status = 'paused'
      return changed()
    case 'POST /player/resume':
      requireControl(f)
      setPosition(s.positionMs)
      s.status = 'playing'
      return changed()
    case 'POST /player/skip':
      if (!permissionsFor(f).canControl && !s.settings.voteSkip.enabled) throw new HttpError(403, 'no_permission')
      advance()
      return changed()
    case 'POST /player/previous': {
      requireControl(f)
      const prev = s.history.shift()
      if (!prev) throw new HttpError(404, 'not_found')
      if (s.current) s.upcoming.unshift(s.current)
      s.current = prev
      setPosition(0)
      return changed()
    }
    case 'POST /player/seek':
      requireControl(f)
      setPosition(Number(body.positionMs) || 0)
      return changed()
    case 'POST /player/volume':
      requireControl(f)
      s.volume = Math.max(0, Math.min(100, Number(body.volume) || 0))
      return changed()
    case 'POST /player/loop':
      requireControl(f)
      s.loop = body.mode as PlayerState['loop']
      return changed()
    case 'POST /player/shuffle':
      requireControl(f)
      s.upcoming.sort(() => Math.random() - 0.5)
      return changed()
    case 'POST /player/stop':
      requireControl(f)
      s.upcoming = []
      s.current = null
      s.status = 'idle'
      if (s.ai) s.ai = { ...s.ai, active: false, job: null }
      return changed()
    case 'POST /queue/add': {
      const query = String(body.query ?? '').trim()
      if (!query || query.toLowerCase() === 'error') throw new HttpError(404, 'no_results')
      const voice = s.settings.voiceChannels
      if (!permissionsFor(f).isManager && voice.length > 0 && !voice.includes('100')) {
        throw new HttpError(403, 'channel_not_allowed')
      }
      if (s.upcoming.length >= s.settings.maxQueueLength) throw new HttpError(409, 'queue_full')
      const match = /mock(\d+)/.exec(query)
      const seed = (match && CATALOG[Number(match[1])]) || CATALOG[Math.floor(Math.random() * CATALOG.length)]!
      const added = query.includes('playlist') ? 5 : 1
      for (let i = 0; i < added; i++) s.upcoming.push(makeTrack(i === 0 ? seed : CATALOG[(i * 3) % CATALOG.length]!, ME))
      if (!s.current) {
        advance()
        s.status = 'playing'
      }
      broadcast()
      return { added }
    }
    case 'POST /queue/move': {
      requireControl(f)
      const from = s.upcoming.findIndex((t) => t.uid === body.uid)
      if (from < 0) throw new HttpError(404, 'not_found')
      const [t] = s.upcoming.splice(from, 1)
      s.upcoming.splice(Math.max(0, Math.min(Number(body.toIndex), s.upcoming.length)), 0, t!)
      return changed()
    }
    case 'POST /queue/remove': {
      const t = s.upcoming.find((x) => x.uid === body.uid)
      if (!t) throw new HttpError(404, 'not_found')
      if (!permissionsFor(f).canControl && t.requestedBy.id !== ME.id) throw new HttpError(403, 'no_permission')
      s.upcoming = s.upcoming.filter((x) => x.uid !== body.uid)
      return changed()
    }
    case 'POST /queue/clear':
      requireControl(f)
      s.upcoming = []
      return changed()
    case 'POST /tracks/feedback':
      if (s.current && s.current.uid === body.uid) {
        s.current = { ...s.current, feedback: (body.value as Track['feedback']) ?? null }
      }
      return changed()
    case 'POST /ai/start':
      s.ai = { active: true, ownerId: ME.id, job: null }
      runAiJob(typeof body.prompt === 'string' ? body.prompt : undefined)
      return changed()
    case 'POST /ai/stop':
      s.ai = null
      return changed()
    case 'POST /ai/cancel':
      if (s.ai?.job) s.ai.job = { ...s.ai.job, phase: 'cancelled', canCancel: false }
      setTimeout(() => {
        if (s.ai?.job?.phase === 'cancelled') {
          s.ai.job = null
          broadcast()
        }
      }, 3000)
      return changed()
    case 'GET /search': {
      const q = (url.searchParams.get('q') ?? '').toLowerCase()
      await delay(250)
      if (q === 'nothing') return { results: [] }
      const scored = CATALOG.map((c, i) => ({ c, i, hit: `${c.title} ${c.artists.join(' ')}`.toLowerCase().includes(q) }))
      const ordered = [...scored.filter((x) => x.hit), ...scored.filter((x) => !x.hit)].slice(0, 8)
      return {
        results: ordered.map(({ c, i }) => ({
          title: c.title,
          artists: c.artists,
          durationMs: c.dur * 1000,
          artworkUrl: `/mock/art/${i}.svg`,
          url: `https://music.example.com/watch?v=mock${i}`,
        })),
      }
    }
    case 'GET /lyrics': {
      await delay(200)
      if (!s.current || f.lyrics === 'none') return null
      if (f.lyrics === 'plain') return { synced: null, plain: SYNCED_LINES.join('\n'), source: 'lrclib' }
      return { synced: makeLyrics(s.current.durationMs ?? 200_000), plain: SYNCED_LINES.join('\n'), source: 'lrclib' }
    }
    case 'GET /settings':
      return s.settings
    case 'PUT /settings':
      if (!permissionsFor(f).isManager) throw new HttpError(403, 'no_permission')
      s.settings = { ...s.settings, ...(body as Partial<GuildSettings>) }
      for (const ws of sockets.keys()) send(ws, { type: 'settings', settings: s.settings })
      return s.settings
    case 'GET /guild/channels':
      if (!permissionsFor(f).isManager) throw new HttpError(403, 'no_permission')
      return {
        text: [
          { id: '200', name: 'general', parentName: 'Text Channels' },
          { id: '201', name: 'music-requests', parentName: 'Text Channels' },
          { id: '202', name: 'off-topic', parentName: null },
        ],
        voice: [
          { id: '100', name: 'Lounge', parentName: 'Voice Channels' },
          { id: '101', name: 'Gaming', parentName: 'Voice Channels' },
          { id: '102', name: 'Study Room', parentName: 'Quiet' },
        ],
      }
    case 'GET /djs':
      return { djs: s.djs }
    case 'POST /djs': {
      const m = USERS.find((u) => u.id === body.userId)
      if (!m) throw new HttpError(404, 'not_found')
      if (!s.djs.some((d) => d.id === m.id)) s.djs.push(m)
      return undefined
    }
    case 'GET /members/candidates': {
      const q = (url.searchParams.get('q') ?? '').toLowerCase()
      return { members: USERS.filter((u) => u.id !== ME.id && u.name.toLowerCase().includes(q)) }
    }
    case 'GET /me/channel':
      return { channel: channelRef(s.voice) }
    case 'GET /me/prefs':
      return s.prefs
    case 'PUT /me/prefs':
      // Same checks as the API (src/api/me.rs).
      if (body.theme != null && !['light', 'dark', 'system'].includes(String(body.theme))) {
        throw new HttpError(400, 'invalid')
      }
      if (body.accent != null && !ACCENTS.some((a) => a.id === body.accent)) throw new HttpError(400, 'invalid')
      s.prefs = { ...s.prefs, ...(body as Partial<typeof s.prefs>) }
      return s.prefs
    case 'GET /me/taste':
      await delay(200)
      return {
        topGenres: [
          { genre: 'synthwave', weight: 0.92 },
          { genre: 'dream pop', weight: 0.71 },
          { genre: 'indie rock', weight: 0.55 },
          { genre: 'electronic', weight: 0.48 },
          { genre: 'lo-fi', weight: 0.3 },
          { genre: 'folk', weight: 0.18 },
        ],
        topTracks: CATALOG.slice(0, 6).map((c, i) => ({
          title: c.title,
          artists: c.artists,
          plays: 42 - i * 6,
          url: `https://music.example.com/watch?v=mock${i}`,
        })),
      }
    case 'DELETE /me/data':
      return undefined
  }
  const djMatch = /^\/djs\/(\w+)$/.exec(path)
  if (method === 'DELETE' && djMatch) {
    s.djs = s.djs.filter((d) => d.id !== djMatch[1])
    return undefined
  }
  throw new HttpError(404, 'not_found')
}

function sendJson(res: ServerResponse, status: number, data: unknown) {
  if (data === undefined) {
    res.statusCode = 204
    res.end()
    return
  }
  res.statusCode = status
  res.setHeader('Content-Type', 'application/json')
  res.end(JSON.stringify(data))
}

export function mockApi(): Plugin {
  return {
    name: 'lynx-mock-api',
    apply: 'serve',
    configureServer(server: ViteDevServer) {
      // The real image serves the backend under /backend/ (nginx strips the prefix); do the same here.
      server.middlewares.use((req, _res, next) => {
        if (req.url?.startsWith('/backend/')) req.url = req.url.slice('/backend'.length)
        next()
      })
      // Pretend Discord loaded us: add the iframe params unless ?outside=1.
      server.middlewares.use((req, res, next) => {
        const url = new URL(req.url ?? '/', 'http://localhost')
        if (url.pathname === '/' && !url.searchParams.has('frame_id') && !url.searchParams.has('outside')) {
          url.searchParams.set('frame_id', 'mock-frame')
          url.searchParams.set('instance_id', 'mock-instance')
          url.searchParams.set('guild_id', '1')
          url.searchParams.set('channel_id', url.searchParams.get('channel_id') ?? '200')
          res.statusCode = 302
          res.setHeader('Location', `/${url.search}`)
          res.end()
          return
        }
        next()
      })

      server.middlewares.use('/mock/art', (req, res) => {
        const index = Number(/(\d+)/.exec(req.url ?? '')?.[1] ?? 0)
        res.setHeader('Content-Type', 'image/svg+xml')
        res.end(artSvg(index))
      })

      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? '/', 'http://localhost')
        if (!url.pathname.startsWith('/api/')) return next()
        const auth = req.headers.authorization
        const session = auth?.startsWith('Bearer ') ? auth.slice(7) : null
        const flags = session ? decodeFlags(session) : parseFlags(new URL(req.headers.referer ?? 'http://x/').search)
        await delay(80)
        try {
          sendJson(res, 200, await handle(req, url, flags, session))
        } catch (error) {
          if (error instanceof HttpError) {
            sendJson(res, error.status, { error: { code: error.code, message: `Mock error: ${error.code}` } })
          } else {
            sendJson(res, 500, { error: { code: 'internal', message: String(error) } })
          }
        }
      })

      const wss = new WebSocketServer({ noServer: true })
      server.httpServer?.on('upgrade', (req, socket, head) => {
        const url = new URL(req.url ?? '/', 'http://localhost')
        if (url.pathname !== '/backend/ws') return
        const session = url.searchParams.get('session')
        if (!sessionValid(session)) {
          socket.write('HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n')
          socket.destroy()
          return
        }
        wss.handleUpgrade(req, socket, head, (ws) => {
          const flags = decodeFlags(session)
          sockets.set(ws, flags)
          send(ws, { type: 'channel', channel: channelRef(s.voice) })
          send(ws, { type: 'state', state: snapshot(flags) })
          send(ws, { type: 'permissions', permissions: permissionsFor(flags) })
          ws.on('close', () => sockets.delete(ws))
        })
      })
    },
  }
}
