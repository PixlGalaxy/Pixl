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
  /**
   * Voice channel the panel follows: the one the user is connected to right now, wherever the
   * Activity was opened from. Null when the user is not in a voice channel (nothing is shown).
   * The socket's `channel` message and `GET /api/me/channel` keep it current.
   */
  channel: ChannelRef | null
  /** Channel the Activity was opened from (any kind), for display only. */
  launchChannel?: ChannelRef | null
  permissions: Permissions
  locale: string | null
}

export type ReasonKey = 'because_you_like' | 'similar_listeners' | 'your_favorite' | 'matches_prompt' | 'listener_favorite'
export type TrackReason = { key: ReasonKey; params: Record<string, string> }

export type Origin = 'youtube' | 'youtube_music' | 'spotify' | 'other'
export type Feedback = 'like' | 'dislike'

/** Platform of the list a queue entry came from (saved-list platforms plus yt-dlp sets). */
export type SourcePlatform = 'spotify' | 'youtube' | 'youtube_music' | 'apple_music' | 'soundcloud' | 'other'

/** The playlist / album a queue entry was added from. */
export type TrackSource = {
  name: string
  imageUrl: string | null
  platform: SourcePlatform
  /** Canonical list link (the key that identifies the list). */
  url: string
  /** 1-based position in the list. */
  index: number
  total: number
}

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
  /** The list it was added from; null for single songs and radio picks. */
  source?: TrackSource | null
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
  /** Snapshot version; grows on every change (across players). Older states are ignored. */
  version?: number
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
  /** The jam this channel is in, as the viewer sees it. */
  jam?: Jam | null
  /** Server clock (epoch ms) when the state was sent. */
  serverNow?: number
}

/** Who may edit the shared queue of a jam. */
export type JamEditRule = 'everyone' | 'host'
export type JamSettings = {
  /** Other voice channels of the host's server may join. */
  allowChannels: boolean
  /** Voice channels of other servers may join. */
  allowServers: boolean
  editableBy: JamEditRule
  /** No new channels may join. */
  closed: boolean
}
export type JamMember = {
  guildId: string
  channelId: string
  name: string
  guildName: string
  isHost: boolean
  /** People are in the channel. */
  listening: boolean
  /** The viewer's channel. */
  you: boolean
}
/**
 * A synchronized jam: several voice channels play one shared queue at the same position. Host
 * actions are only for people in the host channel; everyone else sees them read-only.
 */
export type Jam = {
  id: string
  hostGuildId: string
  hostChannelId: string
  hostName: string
  hostGuildName: string
  members: JamMember[]
  maxMembers: number
  settings: JamSettings
  /** Activity link with the jam's `custom_id`. */
  shareLink: string
  you: { member: boolean; isHost: boolean; canManage: boolean; canEdit: boolean }
}
/** `GET /jam/{id}`: the join prompt of a link. */
export type JamPreview = { jam: Jam; join: { allowed: boolean; code: string | null } }

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
  /** Text channel (or a voice channel chat) for every Lynx message; commands only work there. null = off. */
  musicChannel: string | null
  /** Managers ignore every channel rule. */
  managersExempt: boolean
}

export type GuildChannel = { id: string; name: string; parentName: string | null }
export type GuildChannels = { text: GuildChannel[]; voice: GuildChannel[] }

export type Member = { id: string; name: string; avatarUrl: string | null }

export type UserPrefs = {
  locale: string | null
  historyEnabled: boolean
  /** Panel theme; null = default (dark). */
  theme: 'light' | 'dark' | 'system' | null
  /** Accent palette id (see lib/appearance.ts); null = default (sky). */
  accent: string | null
}
export type Taste = {
  topGenres: Array<{ genre: string; weight: number }>
  topTracks: Array<{ title: string; artists: string[]; plays: number; url: string; artworkUrl: string | null }>
}
/** A song of the user's history tagged with one genre (`GET /me/taste/genre/{genre}`). */
export type GenreTrack = {
  /** Catalog key, used to remove it (`DELETE /me/history/{key}`). */
  key: string
  title: string
  artists: string[]
  artworkUrl: string | null
  url: string
  plays: number
  /** Unix seconds. */
  lastPlayed: number
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
  | 'in_jam'
  | 'not_in_jam'
  | 'jam_host_only'
  | 'jam_not_host'
  | 'jam_closed'
  | 'jam_full'
  | 'jam_already_in'
  | 'jam_channels_off'
  | 'jam_servers_off'
  | 'jam_ended'
  | 'jam_other'
  | 'refresh_blocked'

/** Where a saved playlist lives. */
export type Platform = 'spotify' | 'youtube' | 'youtube_music' | 'apple_music'

/** A playlist, album or artist page the user added (`GET /api/playlists`). */
export type SavedPlaylist = {
  id: string
  /** Empty when the platform gave no name. */
  name: string
  platform: Platform
  url: string
  imageUrl: string | null
  pinned: boolean
  /** Unix seconds. */
  addedAt: number
  usedAt: number
}

export type SavedPlaylists = {
  playlists: SavedPlaylist[]
  /** Most playlists kept per user. */
  max: number
  /** New playlists are saved for you (off when your listening history is off). */
  saving: boolean
  /** Every slot is pinned: new playlists are not saved. */
  full: boolean
}

export type PlaylistTrack = {
  title: string
  artists: string[]
  durationMs: number | null
  artworkUrl: string | null
  url: string
}

export type PlaylistTracks = {
  playlist: SavedPlaylist
  tracks: PlaylistTrack[]
  /** Unix seconds of the fetch the songs come from (shared cache, at most an hour old). */
  fetchedAt: number
  /** The refresh fetched the playlist again (false: answered from the cache, too soon). */
  refreshed: boolean
}

export type ServerMessage =
  | { type: 'state'; state: PlayerState | null; serverNow?: number }
  | { type: 'position'; positionMs: number; positionAt: number; serverNow?: number }
  | { type: 'ai'; ai: AiState | null }
  | { type: 'settings'; settings: GuildSettings }
  | { type: 'permissions'; permissions: Permissions }
  /** The followed voice channel changed (the user joined, left or moved); `state` follows. */
  | { type: 'channel'; channel: ChannelRef | null }
  /** The channel's jam changed (also sent right after `state`). */
  | { type: 'jam'; jam: Jam | null }
