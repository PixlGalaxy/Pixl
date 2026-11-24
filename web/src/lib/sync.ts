// Pure rules that keep the panel's player state in sync with the server (unit tested in
// sync.test.ts, used by live.ts). No React here.
import type { PlayerState, SeekHold } from './types'

/** Cap for the age of a sample measured with the local clock (client/server skew can't run the bar away). */
export const MAX_LOCAL_AGE_MS = 5_000

/**
 * The one position sample everything derives from: `positionMs` at `at` (`performance.now()`, a
 * monotonic clock that keeps counting while the panel is hidden or in picture-in-picture). The
 * position is always `positionMs + (performance.now() - at)` while playing, never accumulated ticks.
 */
export type PositionAnchor = {
  positionMs: number
  at: number
  /** The track the sample belongs to; a sample of another track never moves this track's bar. */
  uid?: string
}

/** Without any state or position message for this long while playing, the panel fetches one. */
export const RESYNC_AFTER_MS = 25_000

/**
 * Anchor from a server sample. With `serverNow` (the server's clock when it sent the message) the
 * sample's age is exact whatever the client clock says, so it is not capped: a capped age made the
 * bar jump back when a state was built from a snapshot published minutes earlier. Without it, the
 * local clock is used and capped.
 */
export function anchorFrom(
  positionMs: number,
  positionAt: number,
  serverNow: number | undefined,
  nowPerf: number,
  uid?: string,
): PositionAnchor {
  const age =
    serverNow != null ? Math.max(serverNow - positionAt, 0) : Math.min(Math.max(Date.now() - positionAt, 0), MAX_LOCAL_AGE_MS)
  return { positionMs: positionMs + age, at: nowPerf, ...(uid != null ? { uid } : {}) }
}

/** Pure: the position an anchor stands for at `now` (`performance.now()` time). */
export function positionAt(anchor: PositionAnchor, playing: boolean, now: number): number {
  return anchor.positionMs + (playing ? Math.max(0, now - anchor.at) : 0)
}

/**
 * Position shown for `state`'s current track at `now`: extrapolated from the anchor only while
 * it plays, clamped to the track (0 to its duration). An anchor of another track (a seek or a
 * position message of the previous one) shows 0 until the new track's own sample arrives.
 */
export function shownPosition(
  anchor: PositionAnchor,
  current: { uid: string; durationMs: number | null } | null,
  playing: boolean,
  now: number,
): number {
  if (!current) return 0
  if (anchor.uid != null && anchor.uid !== current.uid) return 0
  const pos = Math.max(0, positionAt(anchor, playing, now))
  return current.durationMs != null && current.durationMs > 0 ? Math.min(pos, current.durationMs) : pos
}

/**
 * Whether a socket `position` message applies to the state shown: one sent for another track
 * (it raced the track change) is dropped. Messages without a uid (older servers) apply.
 */
export function positionApplies(shownUid: string | null | undefined, messageUid: string | undefined): boolean {
  return messageUid == null || messageUid === shownUid
}

/**
 * Whether the panel should fetch a fresh state: it is visible, the track plays, and no state or
 * position arrived for `RESYNC_AFTER_MS` (the server sends a position every 10 s while playing,
 * so a quiet socket means messages were lost or the panel was throttled).
 */
export function needsResync(playing: boolean, visible: boolean, lastSyncAt: number, now: number): boolean {
  return playing && visible && now - lastSyncAt >= RESYNC_AFTER_MS
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

/**
 * A hold older than this is ignored (the server ends every hold after 5 s and says so; this only
 * covers a release message that never arrived).
 */
export const HOLD_STALE_MS = 7_000

/** Who locks the seek bar for user `me` at `now` (`performance.now()`): someone else's fresh hold. */
export function lockedBy(hold: SeekHold | null | undefined, me: string, now: number): { id: string; name: string } | null {
  const holder = hold?.holder
  if (!holder || holder.id === me) return null
  if (hold.at != null && now - hold.at >= HOLD_STALE_MS) return null
  return holder
}

/**
 * What a drag in progress does with a `seek_hold` message. `granted`: this panel already got the
 * hold. `granted` when the hold is now ours; `cancel` when someone else has it, the request was
 * refused, or our hold ended (the 5 s limit, a new track); `wait` while the request is pending.
 */
export function holdVerdict(
  msg: { holder: SeekHold['holder']; denied?: boolean },
  me: string,
  granted: boolean,
): 'granted' | 'cancel' | 'wait' {
  if (msg.holder?.id === me) return 'granted'
  if (msg.denied || msg.holder) return 'cancel'
  return granted ? 'cancel' : 'wait'
}

/** Our hold ended while we still dragged, after about the 5 s limit: "Released after 5 seconds". */
export function releasedByTimeout(grantedAt: number | null, now: number): boolean {
  return grantedAt != null && now - grantedAt >= 4_500
}
