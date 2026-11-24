// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { formatUptime, fromLocalInput, noticeState, toLocalInput } from './time.ts'
import type { Notice } from './types.ts'

test('datetime-local values round-trip in local time', () => {
  const ms = new Date(2026, 10, 25, 3, 0).getTime()
  assert.equal(toLocalInput(ms), '2026-11-25T03:00')
  assert.equal(fromLocalInput('2026-11-25T03:00'), ms)
  assert.equal(fromLocalInput(''), null)
  assert.equal(fromLocalInput('tomorrow'), null)
})

test('notice states as users see them', () => {
  const n = { active: true, showFrom: 100, startsAt: 200, endsAt: 300 } as Notice
  assert.equal(noticeState(n, 50), 'scheduled')
  assert.equal(noticeState(n, 150), 'upcoming')
  assert.equal(noticeState(n, 250), 'active')
  assert.equal(noticeState(n, 300), 'ended')
  assert.equal(noticeState({ ...n, active: false }, 250), 'off')
})

test('uptime', () => {
  assert.equal(formatUptime(42), '42 s')
  assert.equal(formatUptime(3 * 60), '3 min')
  assert.equal(formatUptime(2 * 3600 + 5 * 60), '2 h 5 min')
  assert.equal(formatUptime(3 * 86_400 + 4 * 3600), '3 d 4 h')
})
