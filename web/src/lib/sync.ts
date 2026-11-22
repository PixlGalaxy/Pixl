// Pure rules that keep the panel's player state in sync with the server (unit tested in
// sync.test.ts, used by live.ts). No React here.
import type { PlayerState } from './types'

/** Cap for the age of a sample measured with the local clock (client/server skew can't run the bar away). */
export const MAX_LOCAL_AGE_MS = 5_000

/**
 * The one position sample everything derives from: `positionMs` at `at` (`performance.now()`, a
 * monotonic clock that keeps counting while the panel is hidden or in picture-in-picture). The
 * position is always `positionMs + (performance.now() - at)` while playing, never accumulated ticks.
 */
export type PositionAnchor = { positionMs: number; at: number }

/**
 * Anchor from a server sample. With `serverNow` (the server's clock when it sent the message) the
 * sample's age is exact whatever the client clock says, so it is not capped: a capped age made the
 * bar jump back when a state was built from a snapshot published minutes earlier. Without it, the
 * local clock is used and capped.
 */
export function anchorFrom(positionMs: number, positionAt: number, serverNow: number | undefined, nowPerf: number): PositionAnchor {
  const age =
    serverNow != null ? Math.max(serverNow - positionAt, 0) : Math.min(Math.max(Date.now() - positionAt, 0), MAX_LOCAL_AGE_MS)
  return { positionMs: positionMs + age, at: nowPerf }
}

/**
 * Whether a state from the server should replace the one shown. Snapshot versions only grow on the
 * server (across every player), so an older one in the same channel is a stale response that
 * arrived late (a slow `GET /player` after a newer socket message) and is dropped.
 */
export function acceptState(current: PlayerState | null, next: PlayerState | null): boolean {
  if (!current || !next) return true
  if (current.channelId !== next.channelId) return true
  if (current.version == null || next.version == null) return true
  return next.version >= current.version
}

/**
 * A `GET` refresh started when the socket had delivered `startedAt` states; it answers when it has
 * delivered `nowAt`. While the socket is open, any socket state received in between is newer than
 * the response, so the response is dropped.
 */
export function freshFetch(socketOpen: boolean, startedAt: number, nowAt: number): boolean {
  return !socketOpen || startedAt === nowAt
}

/** The 5 s polling fallback runs only while the socket is down and the panel is visible. */
export function shouldPoll(socketOpen: boolean, visible: boolean): boolean {
  return !socketOpen && visible
}
