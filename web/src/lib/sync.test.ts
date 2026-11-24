// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  acceptState,
  anchorFrom,
  freshFetch,
  HOLD_STALE_MS,
  holdVerdict,
  lockedBy,
  releasedByTimeout,
  needsResync,
  positionApplies,
  RESYNC_AFTER_MS,
  shouldPoll,
  shownPosition,
} from './sync.ts'
import type { PlayerState } from './types.ts'

const state = (channelId: string, version: number | undefined): PlayerState =>
  ({ channelId, version, positionMs: 0, positionAt: 0 }) as unknown as PlayerState

test('an older snapshot of the same channel is dropped', () => {
  assert.equal(acceptState(state('1', 5), state('1', 4)), false)
  assert.equal(acceptState(state('1', 5), state('1', 5)), true)
  assert.equal(acceptState(state('1', 5), state('1', 9)), true)
})

test('another channel, nothing shown, null or unversioned states always apply', () => {
  assert.equal(acceptState(state('1', 5), state('2', 1)), true)
  assert.equal(acceptState(null, state('1', 1)), true)
  assert.equal(acceptState(state('1', 5), null), true)
  assert.equal(acceptState(state('1', undefined), state('1', 1)), true)
})

test('the age of a server sample is not capped', () => {
  // Published 5 minutes before the message was sent: the anchor is 5 minutes further, not 1.
  const a = anchorFrom(6_000, 1_000_000, 1_300_000, 42)
  assert.deepEqual(a, { positionMs: 306_000, at: 42 })
  // A sample "from the future" (clock rounding) never moves the bar back.
  assert.equal(anchorFrom(6_000, 1_000_100, 1_000_000, 0).positionMs, 6_000)
})

test('without the server clock the local age is capped', () => {
  const a = anchorFrom(1_000, Date.now() - 60_000, undefined, 0)
  assert.equal(a.positionMs, 6_000)
})

test('a fetch that raced a socket state is dropped while the socket is open', () => {
  assert.equal(freshFetch(true, 3, 3), true)
  assert.equal(freshFetch(true, 3, 4), false)
  assert.equal(freshFetch(false, 3, 4), true, 'socket down: the fetch is all there is')
})

test('polling never runs while the socket is healthy', () => {
  assert.equal(shouldPoll(true, true), false)
  assert.equal(shouldPoll(true, false), false)
  assert.equal(shouldPoll(false, false), false)
  assert.equal(shouldPoll(false, true), true)
})

const track = (uid: string, durationMs: number | null = 255_000) => ({ uid, durationMs })

test('the shown position extrapolates only while playing and stays within the track', () => {
  const a = { positionMs: 10_000, at: 1_000, uid: 'a' }
  assert.equal(shownPosition(a, track('a'), true, 6_000), 15_000)
  assert.equal(shownPosition(a, track('a'), false, 60_000), 10_000, 'paused or loading: frozen')
  // 4:20 on a 4:15 track (the live report) can not be shown.
  assert.equal(shownPosition({ positionMs: 250_000, at: 0, uid: 'a' }, track('a'), true, 10_000), 255_000)
  assert.equal(shownPosition({ positionMs: 250_000, at: 0 }, track('a', null), true, 10_000), 260_000, 'live: no end')
  assert.equal(shownPosition(a, null, true, 6_000), 0)
})

test('clock skew between server and client does not move the bar', () => {
  // The server clock is 2 minutes ahead of the client: only server times are compared.
  const serverNow = Date.now() + 120_000
  const a = anchorFrom(30_000, serverNow - 200, serverNow, 500, 'a')
  assert.deepEqual(a, { positionMs: 30_200, at: 500, uid: 'a' })
  assert.equal(shownPosition(a, track('a'), true, 1_500), 31_200)
  // ...or behind it.
  const b = anchorFrom(30_000, Date.now() - 300_000, Date.now() - 300_000, 0, 'a')
  assert.equal(b.positionMs, 30_000)
})

test('seek near the end of a track, then the next one starts: it shows its own position', () => {
  // The user drags A to 4:13 (optimistic local anchor for A)...
  const seek = { positionMs: 253_000, at: 0, uid: 'a' }
  assert.equal(shownPosition(seek, track('a'), true, 1_000), 254_000)
  // ...A ends and the state of B arrives (loading) before any sample of B: B is not at 4:14.
  assert.equal(shownPosition(seek, track('b'), false, 2_000), 0)
  // A position message of A that raced the track change is dropped.
  assert.equal(positionApplies('b', 'a'), false)
  assert.equal(positionApplies('b', 'b'), true)
  assert.equal(positionApplies('b', undefined), true, 'older servers send no uid')
  // B plays from its own sample.
  const b = anchorFrom(0, 10_000, 10_000, 3_000, 'b')
  assert.equal(shownPosition(b, track('b'), true, 5_000), 2_000)
})

test('a quiet panel fetches a fresh position while it plays and is visible', () => {
  assert.equal(needsResync(true, true, 0, RESYNC_AFTER_MS - 1), false, 'the 10 s server ticks keep it fresh')
  assert.equal(needsResync(true, true, 0, RESYNC_AFTER_MS), true)
  assert.equal(needsResync(true, false, 0, 10 * RESYNC_AFTER_MS), false, 'hidden: done when it becomes visible')
  assert.equal(needsResync(false, true, 0, 10 * RESYNC_AFTER_MS), false, 'paused: nothing moves')
})

const marta = { id: '2', name: 'Marta' }
const me = { id: '1', name: 'Pixl' }

test('the seek bar is locked by someone else, never by yourself', () => {
  assert.deepEqual(lockedBy({ holder: marta, until: 5_000, at: 100 }, '1', 200), marta)
  assert.equal(lockedBy({ holder: me, until: 5_000, at: 100 }, '1', 200), null)
  assert.equal(lockedBy({ holder: null, until: null }, '1', 200), null)
  assert.equal(lockedBy(null, '1', 200), null)
  assert.equal(lockedBy(undefined, '1', 200), null)
  // A hold without a receive time (straight from a state) still locks.
  assert.deepEqual(lockedBy({ holder: marta, until: 5_000 }, '1', 1e9), marta)
})

test('a hold whose release never arrived stops locking', () => {
  assert.deepEqual(lockedBy({ holder: marta, until: 0, at: 0 }, '1', HOLD_STALE_MS - 1), marta)
  assert.equal(lockedBy({ holder: marta, until: 0, at: 0 }, '1', HOLD_STALE_MS), null)
})

test('a drag keeps going only while the hold is ours', () => {
  // Pending request: an unrelated "free" update changes nothing; ours grants it.
  assert.equal(holdVerdict({ holder: null }, '1', false), 'wait')
  assert.equal(holdVerdict({ holder: me }, '1', false), 'granted')
  assert.equal(holdVerdict({ holder: me }, '1', true), 'granted', 'a repeat keeps it')
  // Someone else got there first, or the server refused (cooldown, permission).
  assert.equal(holdVerdict({ holder: marta }, '1', false), 'cancel')
  assert.equal(holdVerdict({ holder: null, denied: true }, '1', false), 'cancel')
  // Ours ended (5 s limit, new track): the drag is cancelled.
  assert.equal(holdVerdict({ holder: null }, '1', true), 'cancel')
  assert.equal(holdVerdict({ holder: marta }, '1', true), 'cancel')
})

test('the timeout notice shows only after about 5 seconds of holding', () => {
  assert.equal(releasedByTimeout(1_000, 6_000), true)
  assert.equal(releasedByTimeout(1_000, 2_000), false, 'a new track ended it early')
  assert.equal(releasedByTimeout(null, 9_000), false, 'never granted')
})
