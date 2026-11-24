// Shapes of the admin API (src/api/admin in the backend). Times are epoch ms.

export type Status = 'ok' | 'degraded' | 'down'

export type BotReport = {
  index: number
  role: 'main' | 'voice'
  name: string
  status: Status
  connected: boolean
  shards: { total: number; connected: number }
  gatewayPingMs: number | null
  guilds: number
  voiceSessions: number
  downSince: string | null
}

export type HealthReport = {
  status: Status
  version: string
  commit: string | null
  uptimeSecs: number
  checkedAt: string
  bots: BotReport[]
  services: {
    database: { ok: boolean; latencyMs: number | null }
    ytdlp: { ok: boolean; version: string | null }
    ollama: { ok: boolean } | null
  }
  players: { active: number; playing: number }
}

export type Flags = { maintenanceMode: boolean; ytdlpUpdatesPaused: boolean }

export type Overview = {
  health: HealthReport
  memoryBytes: number | null
  flags: Flags
  notices: { total: number; visible: number }
  serverNow: number
}

export type NoticeKind = 'maintenance' | 'info' | 'warning'

export type Notice = {
  id: number
  kind: NoticeKind
  title: string
  message: string
  startsAt: number
  endsAt: number
  showFrom: number
  active: boolean
  createdAt: number
  updatedAt: number
  /** What users see right now: null when hidden (inactive, not announced yet, ended). */
  phase: 'upcoming' | 'active' | null
  ended: boolean
}

export type NoticeInput = {
  kind: NoticeKind
  title: string
  message: string
  startsAt: number
  durationMinutes?: number
  endsAt?: number
  showFrom?: number
  active: boolean
}

export type YtdlpJob = {
  running: boolean
  finishedAt: number | null
  outcome: 'updated' | 'up_to_date' | 'failed' | null
  version: string | null
}

export type Tweaks = { flags: Flags; ytdlp: { version: string | null; job: YtdlpJob } }
export type TweaksInput = Partial<Flags>
