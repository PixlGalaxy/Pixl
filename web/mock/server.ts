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
//   ?custom_id=jam-partyroom1  opened from a jam link: the join prompt of a jam hosted elsewhere
//   ?jam=host|member a jam already running (you host it, or your channel is a member)
//   ?tab=queue|lyrics|radio|...  tab the panel opens on (read by the panel itself, mock mode only);
//                    with ?pip=1 or the `p` key it picks the compact variant
//   ?ai_track=1      the current song was picked by the AI radio (AI chip, "Selected by AI")
//   ?radio=off|on|job  fixed radio state: off / on (no job) / a job finding songs (default: demo job cycling)
//   ?playlists=none|full  no saved playlists / every saved playlist pinned (Playlists tab)
// Keys in the browser (mock SDK): `v` moves you 100 -> 101 -> no voice -> 100 (socket `channel`
// message + new state), `x` expires every session (the panel signs in again silently), `o` takes
// the backend offline and back (/healthz and /api answer 502, sockets are dropped and refused).
import type { IncomingMessage, ServerResponse } from 'node:http'
import type { Plugin, ViteDevServer } from 'vite'
import { WebSocketServer, type WebSocket } from 'ws'
import type {
  AiPhase,
  AiState,
  GuildSettings,
  Jam,
  JamSettings,
  Member,
  Permissions,
  PlayerState,
  PlaylistTrack,
  SavedPlaylist,
  Track,
  TrackReason,
  TrackSource,
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
  jam: 'host' | 'member' | null
  aiTrack: boolean
  radio: 'off' | 'on' | 'job' | null
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
    jam: p.get('jam') === 'host' || p.get('jam') === 'member' ? (p.get('jam') as 'host' | 'member') : null,
    aiTrack: p.get('ai_track') === '1',
    radio: ['off', 'on', 'job'].includes(p.get('radio') ?? '') ? (p.get('radio') as 'off' | 'on' | 'job') : null,
  }
}
// Sessions are `mock.<epoch>.<flags>`; `x` bumps the epoch so every older session gets 401.
let sessionEpoch = 1
/** `o` key: pretend the Rust backend is down (what nginx answers then: 502). */
let backendOffline = false
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
function makeTrack(seed: Seed, requester: Member, reason: TrackReason | null = null, source: TrackSource | null = null): Track {
  const index = CATALOG.indexOf(seed)
  return {
    source,
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
    musicChannel: null,
    managersExempt: true,
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
// A couple of upcoming songs were picked by the AI radio (they carry a reason).
const MOCK_REASONS: Record<number, TrackReason> = {
  2: { key: 'because_you_like', params: { name: 'Paper Lanterns' } },
  4: { key: 'similar_listeners', params: {} },
}
// Lists the queue came from (Playing from / stacked covers of the Queue tab): the current song and
// a few upcoming ones come from a Spotify playlist, then an album and a SoundCloud set (no cover).
type MockList = Omit<TrackSource, 'index'>
const MOCK_LISTS: MockList[] = [
  { name: 'Late Night Drive', imageUrl: '/mock/art/14.svg', platform: 'spotify', url: 'https://open.spotify.com/playlist/mocklate', total: 64 },
  { name: 'Glass Harbor (Deluxe Edition)', imageUrl: '/mock/art/1.svg', platform: 'youtube_music', url: 'https://music.youtube.com/browse/MPREbmockglass', total: 12 },
  { name: 'Basement Tapes Vol. 2', imageUrl: null, platform: 'soundcloud', url: 'https://soundcloud.com/mock/sets/basement-tapes', total: 9 },
  { name: 'Focus Flow', imageUrl: '/mock/art/8.svg', platform: 'apple_music', url: 'https://music.apple.com/us/playlist/pl.mockfocus', total: 40 },
]
const fromList = (list: MockList, index: number): TrackSource => ({ ...list, index })
// Upcoming position -> [list, index in the list, requester] (positions 2 and 4 are radio picks).
const MOCK_SOURCES: Record<number, [number, number, Member]> = {
  1: [0, 19, ME],
  3: [0, 20, ME],
  5: [1, 3, USERS[1]!],
  6: [1, 4, USERS[1]!],
  7: [1, 5, USERS[1]!],
  9: [2, 1, USERS[3]!],
  10: [2, 2, USERS[3]!],
  11: [3, 7, USERS[4]!],
}
if (s.current) s.current.source = fromList(MOCK_LISTS[0]!, 18)
for (let i = 1; i <= 12; i++) {
  const src = MOCK_SOURCES[i]
  const by = src ? src[2] : USERS[i % USERS.length]!
  s.upcoming.push(makeTrack(CATALOG[i]!, by, MOCK_REASONS[i] ?? null, src ? fromList(MOCK_LISTS[src[0]]!, src[1]) : null))
}
for (let i = 13; i < 18; i++) s.history.unshift(makeTrack(CATALOG[i]!, USERS[(i + 2) % USERS.length]!))

// Listening history behind the taste card: every catalog song, a few played a lot.
type HistoryRow = { key: string; index: number; plays: number; lastPlayed: number }
const nowSecs = Math.floor(Date.now() / 1000)
let tasteHistory: HistoryRow[] = CATALOG.map((_, index) => ({
  key: `mock${index}`,
  index,
  plays: [42, 9, 18, 4, 7, 2, 23, 11, 5, 3, 6, 14, 1, 8, 31, 2, 16, 12][index] ?? 1,
  lastPlayed: nowSecs - ((index * 7919) % 40) * 3600 * 6 - index * 600,
}))
function historyTrack(h: HistoryRow) {
  const c = CATALOG[h.index]!
  return {
    key: h.key,
    title: c.title,
    artists: c.artists,
    artworkUrl: `/mock/art/${h.index}.svg`,
    url: `https://music.example.com/watch?v=mock${h.index}`,
    plays: h.plays,
    lastPlayed: h.lastPlayed,
  }
}
function tasteSummary() {
  const weights = new Map<string, number>()
  for (const h of tasteHistory) {
    for (const g of CATALOG[h.index]!.genres) weights.set(g, (weights.get(g) ?? 0) + h.plays)
  }
  const topGenres = [...weights]
    .map(([genre, weight]) => ({ genre, weight }))
    .sort((a, b) => b.weight - a.weight || a.genre.localeCompare(b.genre))
    .slice(0, 10)
  const topTracks = [...tasteHistory]
    .sort((a, b) => b.plays - a.plays || b.lastPlayed - a.lastPlayed)
    .slice(0, 10)
    .map(historyTrack)
    .map(({ title, artists, plays, url, artworkUrl }) => ({ title, artists, plays, url, artworkUrl }))
  return { topGenres, topTracks }
}

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
    current: f.aiTrack ? { ...s.current, reason: s.current.reason ?? { key: 'your_favorite', params: {} } } : s.current,
    positionMs: Math.round(currentPosition()),
    positionAt: Date.now(),
    upcoming: s.upcoming,
    history: s.history.slice(0, 20),
    loop: s.loop,
    volume: s.volume,
    ai: f.ai ? (f.radio ? mockRadio(f) : s.ai) : null,
    skipVotes: f.vote ? { current: 2, required: 4 } : null,
    voteSkipEnabled: s.settings.voteSkip.enabled,
    version: ++mockVersion,
    jam: jamView(f),
    serverNow: Date.now(),
  }
}

// ---------------------------------------------------------------------------------------------
// Jam (same shape as src/api/jam.rs). Ids are 10 lowercase letters or digits.

let mockVersion = 0
/** The jam linked by `?custom_id=jam-partyroom1`, hosted by a channel of another server. */
const LINKED_JAM_ID = 'partyroom1'
type MockJam = { id: string; host: 'me' | 'other'; members: Jam['members']; settings: JamSettings }
const jams = new Map<string, MockJam>()
let myJam: string | null = null
let jamInitialized = false

function otherHost(): Jam['members'][number] {
  return { guildId: '2', channelId: '500', name: 'Party Room', guildName: 'Night Owls', isHost: true, listening: true, you: false }
}
function meMember(host: boolean): Jam['members'][number] {
  return { guildId: '1', channelId: PLAYER_CHANNEL, name: CHANNEL_NAMES[PLAYER_CHANNEL]!, guildName: 'Galaxy Hangout', isHost: host, listening: true, you: true }
}
jams.set(LINKED_JAM_ID, {
  id: LINKED_JAM_ID,
  host: 'other',
  members: [otherHost(), { guildId: '2', channelId: '501', name: 'Chill Corner', guildName: 'Night Owls', isHost: false, listening: false, you: false }],
  settings: { allowChannels: true, allowServers: true, editableBy: 'everyone', closed: false },
})

function initJam(f: Flags) {
  if (jamInitialized) return
  jamInitialized = true
  if (f.jam === 'host') {
    jams.set('myjam00001', {
      id: 'myjam00001',
      host: 'me',
      members: [meMember(true), { guildId: '1', channelId: '101', name: 'Gaming', guildName: 'Galaxy Hangout', isHost: false, listening: true, you: false }],
      settings: { allowChannels: true, allowServers: false, editableBy: 'everyone', closed: false },
    })
    myJam = 'myjam00001'
  } else if (f.jam === 'member') {
    const j = jams.get(LINKED_JAM_ID)!
    j.members.push(meMember(false))
    myJam = LINKED_JAM_ID
  }
}

function toApiJam(j: MockJam, f: Flags): Jam {
  const host = j.members.find((m) => m.isHost) ?? j.members[0]!
  const member = j.members.some((m) => m.you) && s.voice === PLAYER_CHANNEL
  const isHost = member && j.host === 'me'
  return {
    id: j.id,
    hostGuildId: host.guildId,
    hostChannelId: host.channelId,
    hostName: host.name,
    hostGuildName: host.guildName,
    members: j.members.map((m) => ({ ...m, you: m.you && s.voice === PLAYER_CHANNEL })),
    maxMembers: 8,
    settings: j.settings,
    shareLink: `https://discord.com/activities/000000000000000000?custom_id=jam-${j.id}`,
    you: {
      member,
      isHost,
      canManage: isHost && permissionsFor(f).canControl,
      canEdit: member && (j.settings.editableBy === 'everyone' || isHost),
    },
  }
}
function jamView(f: Flags): Jam | null {
  initJam(f)
  const j = myJam ? jams.get(myJam) : undefined
  return j ? toApiJam(j, f) : null
}
function broadcastJam() {
  for (const [ws, f] of sockets) {
    send(ws, { type: 'state', state: snapshot(f) })
    send(ws, { type: 'jam', jam: snapshot(f)?.jam ?? null })
  }
}
function requireHost(f: Flags): MockJam {
  const j = myJam ? jams.get(myJam) : undefined
  if (!j) throw new HttpError(404, 'not_in_jam')
  if (j.host !== 'me') throw new HttpError(403, 'jam_not_host')
  requireControl(f)
  return j
}
/** `?radio=off|on|job`: fixed radio state for screenshots (instead of the cycling demo job). */
function mockRadio(f: Flags): AiState | null {
  if (f.radio === 'on') return { active: true, ownerId: ME.id, job: null }
  if (f.radio === 'job') {
    return { active: false, ownerId: ME.id, job: { id: 'mock-job', phase: 'finding_songs', startedAt: startedAt, canCancel: true } }
  }
  return null
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
    readonly retryAfter?: number,
  ) {
    super(code)
  }
}

// ---------------------------------------------------------------------------------------------
// Saved playlists (same rules as src/api/playlists.rs and src/store/playlists.rs)

const MAX_PLAYLISTS = 20
let playlistSeq = 100
let savedPlaylists: SavedPlaylist[] | null = null
const PLATFORMS: SavedPlaylist['platform'][] = ['spotify', 'youtube', 'youtube_music', 'apple_music']

function initPlaylists(search: string) {
  if (savedPlaylists) return
  const mode = new URLSearchParams(search).get('playlists')
  const now = Math.floor(Date.now() / 1000)
  const names = ['Late Night Drive', 'Focus Flow', 'Sunday Acoustic', 'Glass Harbor (Deluxe)', 'Gym Rotation', 'Old Favorites']
  savedPlaylists =
    mode === 'none'
      ? []
      : names.map((name, i) => ({
          id: String(i + 1),
          name,
          platform: PLATFORMS[i % PLATFORMS.length]!,
          url: `https://music.example.com/playlist?list=mock${i}`,
          imageUrl: `/mock/art/${(i * 2) % CATALOG.length}.svg`,
          pinned: mode === 'full' || i === 0,
          addedAt: now - (i + 1) * 86_400,
          usedAt: now - (i + 1) * 3_600,
        }))
}

function sortedPlaylists() {
  return [...(savedPlaylists ?? [])].sort((a, b) => Number(b.pinned) - Number(a.pinned) || b.usedAt - a.usedAt)
}

/** Queueing a playlist link saves it, like `/play` does (pinned ones are never evicted). */
function savePlaylist(url: string) {
  const list = savedPlaylists ?? []
  const now = Math.floor(Date.now() / 1000)
  const existing = list.find((p) => p.url === url)
  if (existing) {
    existing.usedAt = now
    return
  }
  if (list.length >= MAX_PLAYLISTS) {
    const oldest = list.filter((p) => !p.pinned).sort((a, b) => a.usedAt - b.usedAt)[0]
    if (!oldest) return
    list.splice(list.indexOf(oldest), 1)
  }
  playlistSeq += 1
  list.push({
    id: String(playlistSeq),
    name: `Added playlist ${playlistSeq - 100}`,
    platform: 'spotify',
    url,
    imageUrl: `/mock/art/${playlistSeq % CATALOG.length}.svg`,
    pinned: false,
    addedAt: now,
    usedAt: now,
  })
  savedPlaylists = list
}

const playlistFetchedAt = new Map<string, number>()
function playlistSongs(p: SavedPlaylist): PlaylistTrack[] {
  const n = 6 + (Number(p.id) % 7)
  return Array.from({ length: n }, (_, i) => {
    const index = (Number(p.id) * 3 + i) % CATALOG.length
    const c = CATALOG[index]!
    return {
      title: c.title,
      artists: c.artists,
      durationMs: c.dur * 1000,
      artworkUrl: `/mock/art/${index}.svg`,
      url: `https://music.example.com/watch?v=mock${index}`,
    }
  })
}

// Refresh limiter: one honored refresh every 5 s, 10 requests within 5 s block for 5 minutes.
const refreshState = { last: 0, hits: [] as number[], blockedUntil: 0 }
function checkRefresh(now: number): 'allow' | 'ignore' | number {
  if (refreshState.blockedUntil > now) return Math.ceil((refreshState.blockedUntil - now) / 1000)
  if (refreshState.blockedUntil) {
    refreshState.blockedUntil = 0
    refreshState.hits = []
  }
  refreshState.hits = refreshState.hits.filter((t) => now - t < 5000).concat(now)
  if (refreshState.hits.length >= 10) {
    refreshState.hits = []
    refreshState.blockedUntil = now + 300_000
    return 300
  }
  if (now - refreshState.last < 5000) return 'ignore'
  refreshState.last = now
  return 'allow'
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
      ...mockNotices(),
    }
  }
  if (path === '/notices') return mockNotices()
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

  // Like the real API: player, queue, radio, lyrics and jam routes only take the user's voice channel.
  const jamRoute = path === '/jam' || /^\/jam\/(create|join|leave|settings|kick|close|end)$/.test(path)
  if (/^\/(player|queue|tracks|ai|lyrics)/.test(path) || jamRoute) {
    const channel = String(body.channelId ?? url.searchParams.get('channelId') ?? '')
    if (!s.voice || channel !== s.voice) throw new HttpError(403, 'not_in_channel')
  }
  // The jam's host-only rule for shared-queue edits (volume and stop stay per channel).
  if (/^\/(player\/(pause|resume|skip|previous|seek|loop|shuffle)|queue\/)/.test(path)) {
    const jam = snapshot(f)?.jam
    if (jam?.you.member && !jam.you.canEdit) throw new HttpError(403, 'jam_host_only')
  }
  initJam(f)
  switch (`${method} ${path}`) {
    case 'GET /jam':
      return { jam: jamView(f) }
    case 'POST /jam/create': {
      if (!snapshot(f)) throw new HttpError(404, 'not_found')
      if (!myJam) {
        const id = `mine${String(Date.now()).slice(-6)}`
        jams.set(id, {
          id,
          host: 'me',
          members: [meMember(true)],
          settings: { allowChannels: true, allowServers: false, editableBy: 'everyone', closed: false },
        })
        myJam = id
        broadcastJam()
      }
      const jam = jamView(f)!
      return { jamId: jam.id, shareLink: jam.shareLink, jam }
    }
    case 'POST /jam/join': {
      const j = jams.get(String(body.jamId ?? ''))
      if (!j) throw new HttpError(404, 'jam_ended')
      if (myJam === j.id) throw new HttpError(409, 'jam_already_in')
      if (myJam) throw new HttpError(409, 'jam_other')
      if (j.settings.closed) throw new HttpError(403, 'jam_closed')
      if (!snapshot(f)) throw new HttpError(503, 'bot_unavailable')
      await delay(400)
      j.members = j.members.filter((m) => !m.you).concat(meMember(false))
      myJam = j.id
      broadcastJam()
      return { jam: jamView(f) }
    }
    case 'POST /jam/leave': {
      const j = myJam ? jams.get(myJam) : undefined
      if (!j) throw new HttpError(404, 'not_in_jam')
      requireControl(f)
      j.members = j.members.filter((m) => !m.you)
      if (j.host === 'me') jams.delete(j.id)
      myJam = null
      broadcastJam()
      return undefined
    }
    case 'POST /jam/settings':
    case 'POST /jam/close': {
      const j = requireHost(f)
      const patch = path === '/jam/close' ? { closed: true } : (body as Partial<JamSettings>)
      for (const k of ['allowChannels', 'allowServers', 'closed'] as const) {
        if (typeof patch[k] === 'boolean') j.settings[k] = patch[k]
      }
      if (patch.editableBy === 'everyone' || patch.editableBy === 'host') j.settings.editableBy = patch.editableBy
      broadcastJam()
      return { settings: j.settings }
    }
    case 'POST /jam/kick': {
      const j = requireHost(f)
      const target = String(body.memberChannelId ?? '')
      if (j.members.find((m) => m.channelId === target)?.isHost) throw new HttpError(400, 'invalid')
      const before = j.members.length
      j.members = j.members.filter((m) => m.channelId !== target)
      if (j.members.length === before) throw new HttpError(404, 'not_found')
      broadcastJam()
      return undefined
    }
    case 'POST /jam/end': {
      const j = requireHost(f)
      jams.delete(j.id)
      myJam = null
      broadcastJam()
      return undefined
    }
  }
  const jamMatch = /^\/jam\/([a-z0-9-]+)$/.exec(path)
  if (method === 'GET' && jamMatch) {
    const id = jamMatch[1]!.replace(/^jam-/, '')
    const j = jams.get(id)
    if (!/^[a-z0-9]{10}$/.test(id)) throw new HttpError(400, 'invalid')
    if (!j) throw new HttpError(404, 'jam_ended')
    const code = !s.voice
      ? 'not_in_channel'
      : myJam === j.id
        ? 'jam_already_in'
        : myJam
          ? 'jam_other'
          : j.settings.closed
            ? 'jam_closed'
            : null
    await delay(200)
    return { jam: toApiJam(j, f), join: { allowed: code === null, code } }
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
      s.volume = Math.max(10, Math.min(100, Number(body.volume) || 0))
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
      if (!(permissionsFor(f).isManager && s.settings.managersExempt) && voice.length > 0 && !voice.includes('100')) {
        throw new HttpError(403, 'channel_not_allowed')
      }
      if (s.upcoming.length >= s.settings.maxQueueLength) throw new HttpError(409, 'queue_full')
      const match = /mock(\d+)/.exec(query)
      const seed = (match && CATALOG[Number(match[1])]) || CATALOG[Math.floor(Math.random() * CATALOG.length)]!
      const added = query.includes('playlist') ? 5 : 1
      if (query.includes('playlist')) savePlaylist(query)
      const list: MockList | null =
        added > 1 ? { name: 'Pasted playlist', imageUrl: `/mock/art/${uidCounter % CATALOG.length}.svg`, platform: 'spotify', url: query, total: added } : null
      for (let i = 0; i < added; i++) {
        s.upcoming.push(makeTrack(i === 0 ? seed : CATALOG[(i * 3) % CATALOG.length]!, ME, null, list && fromList(list, i + 1)))
      }
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
      if (s.settings.musicChannel === '') s.settings.musicChannel = null
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
      return tasteSummary()
    case 'DELETE /me/data':
      tasteHistory = []
      savedPlaylists = []
      return undefined
    case 'POST /playlists/save': {
      initPlaylists(new URL(req.headers.referer ?? 'http://x/').search)
      if (!s.prefs.historyEnabled) throw new HttpError(403, 'history_disabled')
      const url = String(body.url ?? '')
      const known = MOCK_LISTS.find((l) => l.url === url)
      if (!url || known?.platform === 'soundcloud') throw new HttpError(400, 'invalid')
      const list = savedPlaylists ?? []
      if (!list.some((p) => p.url === url) && list.length >= MAX_PLAYLISTS && list.every((p) => p.pinned)) {
        throw new HttpError(409, 'playlists_full')
      }
      savePlaylist(url)
      const saved = (savedPlaylists ?? []).find((p) => p.url === url)!
      if (known) {
        saved.name = known.name
        saved.imageUrl = known.imageUrl
        if (known.platform !== 'other') saved.platform = known.platform
      }
      return { playlist: saved }
    }
    case 'GET /playlists': {
      initPlaylists(new URL(req.headers.referer ?? 'http://x/').search)
      await delay(150)
      const pinned = (savedPlaylists ?? []).filter((p) => p.pinned).length
      const saving = s.prefs.historyEnabled
      return { playlists: sortedPlaylists(), max: MAX_PLAYLISTS, saving, full: saving && pinned >= MAX_PLAYLISTS }
    }
  }
  // Saved playlists: /playlists/{id}[/pin|/tracks|/refresh|/add]
  const playlistMatch = /^\/playlists\/(\w+)(?:\/(pin|tracks|refresh|add))?$/.exec(path)
  if (playlistMatch) {
    initPlaylists(new URL(req.headers.referer ?? 'http://x/').search)
    const [, id, action] = playlistMatch
    const p = (savedPlaylists ?? []).find((x) => x.id === id)
    if (!p) throw new HttpError(404, 'not_found')
    const songs = () => {
      const fetchedAt = playlistFetchedAt.get(p.id) ?? Math.floor(Date.now() / 1000) - 600
      return { playlist: p, tracks: playlistSongs(p), fetchedAt }
    }
    switch (`${method} ${action ?? ''}`) {
      case 'DELETE ':
        savedPlaylists = (savedPlaylists ?? []).filter((x) => x.id !== p.id)
        return undefined
      case 'POST pin':
        p.pinned = body.pinned === true
        return undefined
      case 'GET tracks':
        await delay(350)
        return { ...songs(), refreshed: false }
      case 'POST refresh': {
        const decision = checkRefresh(Date.now())
        if (typeof decision === 'number') throw new HttpError(429, 'refresh_blocked', decision)
        if (decision === 'allow') {
          await delay(700)
          playlistFetchedAt.set(p.id, Math.floor(Date.now() / 1000))
        }
        return { ...songs(), refreshed: decision === 'allow' }
      }
      case 'POST add': {
        const channel = String(body.channelId ?? '')
        if (!s.voice || channel !== s.voice) throw new HttpError(403, 'not_in_channel')
        const voice = s.settings.voiceChannels
        if (!(permissionsFor(f).isManager && s.settings.managersExempt) && voice.length > 0 && !voice.includes('100')) {
          throw new HttpError(403, 'channel_not_allowed')
        }
        const all = playlistSongs(p)
        const picked = typeof body.trackUrl === 'string' ? all.filter((t) => t.url === body.trackUrl).slice(0, 1) : all
        if (picked.length === 0) throw new HttpError(404, 'not_found')
        const room = Math.max(0, s.settings.maxQueueLength - s.upcoming.length)
        if (room === 0) throw new HttpError(409, 'queue_full')
        const list: MockList = { name: p.name, imageUrl: p.imageUrl, platform: p.platform, url: p.url, total: all.length }
        for (const t of picked.slice(0, room)) {
          const seed = CATALOG.find((c) => c.title === t.title) ?? CATALOG[0]!
          s.upcoming.push(makeTrack(seed, ME, null, fromList(list, all.indexOf(t) + 1)))
        }
        if (!s.current) {
          advance()
          s.status = 'playing'
        }
        if (!body.trackUrl) p.usedAt = Math.floor(Date.now() / 1000)
        broadcast()
        return { added: Math.min(picked.length, room) }
      }
    }
  }
  // Same rules as src/api/me.rs: AI on, history on, a known genre.
  const genreMatch = /^\/me\/taste\/genre\/([^/]+)$/.exec(path)
  const forgetMatch = /^\/me\/history\/([^/]+)$/.exec(path)
  if ((method === 'GET' && genreMatch) || (method === 'DELETE' && forgetMatch)) {
    if (!f.ai) throw new HttpError(404, 'not_found')
    if (!s.prefs.historyEnabled) throw new HttpError(403, 'history_disabled')
  }
  if (method === 'GET' && genreMatch) {
    const genre = decodeURIComponent(genreMatch[1]!).toLowerCase()
    if (!CATALOG.some((c) => c.genres.includes(genre))) throw new HttpError(400, 'invalid')
    const limit = Math.min(50, Math.max(1, Number(url.searchParams.get('limit') ?? 50) || 50))
    const offset = Math.max(0, Number(url.searchParams.get('offset') ?? 0) || 0)
    const all = tasteHistory
      .filter((h) => CATALOG[h.index]!.genres.includes(genre))
      .sort((a, b) => b.plays - a.plays || b.lastPlayed - a.lastPlayed)
    await delay(150)
    return { tracks: all.slice(offset, offset + limit).map(historyTrack), total: all.length }
  }
  if (method === 'DELETE' && forgetMatch) {
    const key = decodeURIComponent(forgetMatch[1]!)
    const before = tasteHistory.length
    tasteHistory = tasteHistory.filter((h) => h.key !== key)
    if (tasteHistory.length === before) throw new HttpError(404, 'not_found')
    await delay(250)
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

      server.middlewares.use((req, res, next) => {
        const path = new URL(req.url ?? '/', 'http://localhost').pathname
        if (path === '/mock-offline' && req.method === 'POST') {
          backendOffline = !backendOffline
          if (backendOffline) for (const ws of sockets.keys()) ws.terminate()
          console.log(`[mock] backend ${backendOffline ? 'offline' : 'online'}`)
          return sendJson(res, 200, { offline: backendOffline })
        }
        if (path === '/healthz') {
          res.statusCode = backendOffline ? 502 : 200
          return res.end(backendOffline ? 'Bad Gateway' : 'ok')
        }
        if (backendOffline && path.startsWith('/api/')) {
          res.statusCode = 502
          return res.end('Bad Gateway')
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
            const retry = error.retryAfter === undefined ? {} : { retryAfter: error.retryAfter }
            sendJson(res, error.status, { error: { code: error.code, message: `Mock error: ${error.code}`, ...retry } })
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
        if (backendOffline) {
          socket.write('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n')
          socket.destroy()
          return
        }
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

/** A scheduled downtime two hours from now (the banner under the tab bar). */
function mockNotices() {
  const start = Math.ceil((Date.now() + 2 * 3_600_000) / 600_000) * 600_000
  return {
    notices: [
      {
        id: 1,
        kind: 'maintenance',
        title: 'Server upgrade',
        message: 'Lynx moves to a faster machine.',
        startsAt: start,
        endsAt: start + 10 * 60_000,
        phase: 'upcoming',
      },
    ],
    maintenance: false,
  }
}
