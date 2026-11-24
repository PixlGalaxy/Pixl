// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { filterLines } from './logs.ts'
import type { LogLine } from './types.ts'

const l = (seq: number, level: LogLine['level'], target: string, text: string): LogLine => ({ seq, ts: 0, level, target, text })

test('log filters: level, source and text', () => {
  const lines = [
    l(1, 'info', 'lynx::player::actor', 'track start: resolve=640ms'),
    l(2, 'warn', 'lynx::sources', 'yt-dlp self-update failed'),
    l(3, 'debug', 'serenity', 'heartbeat'),
  ]
  const all = new Set(['error', 'warn', 'info', 'debug', 'trace'] as const)
  assert.deepEqual(filterLines(lines, { levels: all, target: '', text: '' }).map((x) => x.seq), [1, 2, 3])
  assert.deepEqual(filterLines(lines, { levels: new Set(['warn']), target: '', text: '' }).map((x) => x.seq), [2])
  assert.deepEqual(filterLines(lines, { levels: all, target: 'lynx::player', text: '' }).map((x) => x.seq), [1])
  assert.deepEqual(filterLines(lines, { levels: all, target: 'lynx', text: '' }).map((x) => x.seq), [1, 2])
  assert.deepEqual(filterLines(lines, { levels: all, target: '', text: 'YT-DLP' }).map((x) => x.seq), [2])
})
