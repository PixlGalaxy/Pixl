// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { acceptState, anchorFrom, freshFetch, shouldPoll } from './sync.ts'
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
