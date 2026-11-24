// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { authMessage, closeAction, WS_CLOSE } from './socket.ts'

test('the session goes in the first message', () => {
  assert.deepEqual(JSON.parse(authMessage('abc')), { type: 'auth', session: 'abc' })
})

test('a refused session or a socket that never answered renews the session', () => {
  assert.equal(closeAction(WS_CLOSE.unauthorized, false), 'renew')
  assert.equal(closeAction(WS_CLOSE.unauthorized, true), 'renew')
  assert.equal(closeAction(1006, false), 'renew')
  assert.equal(closeAction(WS_CLOSE.authTimeout, false), 'renew')
})

test('a replaced socket waits; other closes retry with backoff', () => {
  assert.equal(closeAction(WS_CLOSE.replaced, true), 'replaced')
  assert.equal(closeAction(1006, true), 'retry')
  assert.equal(closeAction(1011, true), 'retry')
})
