// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { activeLine } from './lyrics.ts'

const lines = [0, 1000, 2500, 2500, 4000].map((timeMs, i) => ({ timeMs, line: `l${i}` }))

test('before the first line there is no active line', () => {
  assert.equal(activeLine(lines.slice(1), 999), -1)
  assert.equal(activeLine([], 5000), -1)
})

test('picks the last line at or before the position', () => {
  assert.equal(activeLine(lines, 0), 0)
  assert.equal(activeLine(lines, 999), 0)
  assert.equal(activeLine(lines, 1000), 1)
  assert.equal(activeLine(lines, 2499), 1)
  assert.equal(activeLine(lines, 2500), 3, 'equal timestamps: the later line')
  assert.equal(activeLine(lines, 3999), 3)
  assert.equal(activeLine(lines, 4000), 4)
  assert.equal(activeLine(lines, 1e9), 4)
})

test('matches a linear scan for every position', () => {
  const many = Array.from({ length: 50 }, (_, i) => ({ timeMs: i * 333 + (i % 3) * 7, line: String(i) }))
  for (let pos = -10; pos < 17_000; pos += 37) {
    let expected = -1
    for (let i = 0; i < many.length; i++) if (many[i]!.timeMs <= pos) expected = i
    assert.equal(activeLine(many, pos), expected, `position ${pos}`)
  }
})
