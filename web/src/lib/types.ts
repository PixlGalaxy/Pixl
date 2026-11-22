// Mirrors plan/api.md (the shared contract with the Rust backend). Snowflakes are strings.

export type PublicLinks = { landing?: string; terms?: string; privacy?: string; support?: string }
export type PublicConfig = { clientId: string; features: { ai: boolean }; links: PublicLinks }

export type User = { id: string; username: string; globalName: string | null; avatarUrl: string | null }
export type GuildInfo = { id: string; name: string; iconUrl: string | null }
export type Permissions = {
  isManager: boolean
  isDj: boolean
  canControl: boolean
  canAdd: boolean
}

export type ChannelRef = { id: string; name: string }

export type AuthResult = {
  accessToken: string
  session: string
  user: User
  guild: GuildInfo | null
  /** Voice channel the Activity was launched in (null outside a server voice channel). */
  channel?: ChannelRef | null
  permissions: Permissions
  locale: string | null
}

export type ReasonKey = 'because_you_like' | 'similar_listeners' | 'your_favorite' | 'matches_prompt'
export type TrackReason = { key: ReasonKey; params: Record<string, string> }

export type Origin = 'youtube' | 'youtube_music' | 'spotify' | 'other'
export type Feedback = 'like' | 'dislike'

export type Track = {
  uid: string
  title: string
  artists: string[]
  durationMs: number | null
  artworkUrl: string | null
  url: string
  origin: Origin
  requestedBy: { id: string; name: string; avatarUrl: string | null }
  genres: string[]
  reason: TrackReason | null
  feedback: Feedback | null
}

export type AiPhase =
  | 'queued'
  | 'loading_model'
  | 'reading_taste'
  | 'finding_songs'
  | 'thinking'
  | 'resolving'
  | 'ready'
  | 'failed'
  | 'cancelled'

export type AiJob = { id: string; phase: AiPhase; startedAt: number; canCancel: boolean }
export type AiState = { active: boolean; ownerId: string; job: AiJob | null }

export type LoopMode = 'off' | 'track' | 'queue'
export type PlayerStatus = 'playing' | 'paused' | 'loading' | 'idle'

export type PlayerState = {
  guildId: string
  channelId: string
  channelName: string
  bot: { id: string; name: string; isMain: boolean }
  status: PlayerStatus
  pausedReason: 'user' | 'empty_channel' | null
  emptyChannelDeadline: number | null
  current: Track | null
  positionMs: number
  positionAt: number
  upcoming: Track[]
  history: Track[]
  loop: LoopMode
  volume: number
  ai: AiState | null
  skipVotes: { current: number; required: number } | null
  voteSkipEnabled?: boolean
  /** Server clock (epoch ms) when the state was sent. */
  serverNow?: number
}

export type SearchResult = {
  title: string
  artists: string[]
  durationMs: number | null
  artworkUrl: string | null
  url: string
}

export type LyricLine = { timeMs: number; line: string }
export type Lyrics = { synced: LyricLine[] | null; plain: string | null; source: 'lrclib' }

export type ControlMode = 'everyone' | 'dj'
export type GuildSettings = {
  controlMode: ControlMode
  voteSkip: { enabled: boolean; mode: 'count' | 'percent'; value: number }
  maxQueueLength: number
  defaultVolume: number
  /** Text channels (or a voice channel chat) where music commands work. Empty = every channel. */
  commandChannels: string[]
  /** Voice channels where Lynx may play. Empty = every channel. */
  voiceChannels: string[]
}

export type GuildChannel = { id: string; name: string; parentName: string | null }
export type GuildChannels = { text: GuildChannel[]; voice: GuildChannel[] }

export type Member = { id: string; name: string; avatarUrl: string | null }

export type UserPrefs = { locale: string | null; historyEnabled: boolean }
export type Taste = {
  topGenres: Array<{ genre: string; weight: number }>
  topTracks: Array<{ title: string; artists: string[]; plays: number; url: string }>
}

export type ErrorCode =
  | 'not_in_channel'
  | 'no_permission'
  | 'not_found'
  | 'queue_full'
  | 'no_results'
  | 'rate_limited'
  | 'bot_unavailable'
  | 'invalid'
  | 'history_disabled'
  | 'channel_not_allowed'
  | 'unavailable'

export type ServerMessage =
  | { type: 'state'; state: PlayerState | null }
  | { type: 'position'; positionMs: number; positionAt: number; serverNow?: number }
  | { type: 'ai'; ai: AiState | null }
  | { type: 'settings'; settings: GuildSettings }
  | { type: 'permissions'; permissions: Permissions }
