// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { durationMinutes, formatRelative, nextChange, noticePhase, pickNotice } from './notices.ts'
import type { PublicNotice } from './types.ts'

const n = (id: number, startsAt: number, minutes: number): PublicNotice => ({
  id,
  kind: 'maintenance',
  title: 'Upgrade',
  message: 'New host',
  startsAt,
  endsAt: startsAt + minutes * 60_000,
  phase: 'upcoming',
})

test('phases: upcoming, active, then gone at the end time', () => {
  const x = n(1, 10_000, 10)
  assert.equal(noticePhase(x, 0), 'upcoming')
  assert.equal(noticePhase(x, 10_000), 'active')
  assert.equal(noticePhase(x, 10_000 + 600_000 - 1), 'active')
  assert.equal(noticePhase(x, 10_000 + 600_000), null)
})

test('the soonest notice that has not ended or been dismissed wins', () => {
  const list = [n(1, 500_000, 10), n(2, 100_000, 10), n(3, -900_000, 5)]
  assert.equal(pickNotice(list, 0, new Set())?.id, 2)
  assert.equal(pickNotice(list, 0, new Set([2]))?.id, 1, 'dismissed per id')
  assert.equal(pickNotice(list, 2_000_000, new Set()), null, 'everything ended')
  assert.equal(pickNotice([], 0, new Set()), null)
  assert.equal(pickNotice([n(9, 5, 1), n(8, 5, 1)], 0, new Set())?.id, 8, 'ties: lower id')
})

test('the banner knows when to change by itself', () => {
  const list = [n(1, 60_000, 10), n(2, 30_000, 1)]
  assert.equal(nextChange(list, 0), 30_000)
  assert.equal(nextChange(list, 40_000), 20_000)
  assert.equal(nextChange([n(1, 0, 1)], 120_000), null)
})

test('durations and relative times', () => {
  assert.equal(durationMinutes(n(1, 0, 10)), 10)
  assert.equal(durationMinutes({ ...n(1, 0, 0), endsAt: 1 }), 1)
  assert.equal(formatRelative(2 * 3_600_000, 0, 'en'), 'in 2 hours')
  assert.equal(formatRelative(5 * 60_000, 0, 'en'), 'in 5 minutes')
  assert.equal(formatRelative(5 * 60_000, 0, 'es'), 'dentro de 5 minutos')
})
