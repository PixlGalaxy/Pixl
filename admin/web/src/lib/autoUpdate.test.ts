// Run with `pnpm test` (Node's test runner; Node strips the types).
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { formatInZone, fromDraft, leadChoice, toDraft, weekdayName } from './autoUpdate.ts'
import type { AutoUpdateSettings } from './types.ts'

const settings: AutoUpdateSettings = {
  enabled: true,
  image: 'ghcr.io/pixlgalaxy/lynx:lynx',
  frequency: 'daily',
  weekday: 'sun',
  time: '04:00',
  timezone: 'UTC',
  leadHours: 24,
  durationMinutes: 5,
  checkIntervalMinutes: 60,
  templates: {
    en: { title: 'Scheduled update', message: 'About {minutes} minutes.' },
    es: { title: 'Actualización programada', message: 'Unos {minutes} minutos.' },
  },
}

test('lead time presets', () => {
  assert.equal(leadChoice(24), '24')
  assert.equal(leadChoice(72), '72')
  assert.equal(leadChoice(5), 'custom')
})

test('draft round-trips and picks the lead preset or the custom hours', () => {
  assert.deepEqual(fromDraft(toDraft(settings)), settings)
  const d = toDraft({ ...settings, leadHours: 72 })
  assert.equal(d.lead, '72')
  assert.equal((fromDraft({ ...d, leadHours: '999' }) as AutoUpdateSettings).leadHours, 72, 'the preset wins')
  const custom = fromDraft({ ...d, lead: 'custom', leadHours: '6' }) as AutoUpdateSettings
  assert.equal(custom.leadHours, 6)
  assert.equal((fromDraft({ ...d, image: '  ghcr.io/a/b:c ' }) as AutoUpdateSettings).image, 'ghcr.io/a/b:c')
})

test('bad numbers and times are caught before sending', () => {
  const d = toDraft(settings)
  assert.equal(fromDraft({ ...d, checkIntervalMinutes: '5' }), 'autoUpdate.invalidNumber')
  assert.equal(fromDraft({ ...d, durationMinutes: '0' }), 'autoUpdate.invalidNumber')
  assert.equal(fromDraft({ ...d, durationMinutes: '2.5' }), 'autoUpdate.invalidNumber')
  assert.equal(fromDraft({ ...d, lead: 'custom', leadHours: '0' }), 'autoUpdate.invalidNumber')
  assert.equal(fromDraft({ ...d, lead: 'custom', leadHours: '' }), 'autoUpdate.invalidNumber')
  assert.equal(fromDraft({ ...d, time: '4am' }), 'autoUpdate.invalidTime')
})

test('times in a zone and weekday names', () => {
  const ms = Date.UTC(2026, 10, 25, 4, 0)
  assert.match(formatInZone(ms, 'en-US', 'UTC'), /04:00/)
  assert.match(formatInZone(ms, 'en-US', 'America/Mexico_City'), /10:00|22:00/)
  assert.equal(weekdayName('mon', 'en-US'), 'Monday')
  assert.equal(weekdayName('sun', 'en-US'), 'Sunday')
  assert.equal(weekdayName('sat', 'es'), 'sábado')
})
