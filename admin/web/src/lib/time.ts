// Local date/time helpers for the notice form (`<input type="datetime-local">` works in local time;
// the API stores UTC epoch ms).
import type { Notice } from './types'

const pad = (n: number) => String(n).padStart(2, '0')

/** Epoch ms to the `YYYY-MM-DDTHH:MM` value of a datetime-local input, in local time. */
export function toLocalInput(ms: number): string {
  const d = new Date(ms)
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** A datetime-local value (local time) to epoch ms; null when empty or invalid. */
export function fromLocalInput(value: string): number | null {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2})?$/.test(value)) return null
  const ms = new Date(value).getTime()
  return Number.isFinite(ms) ? ms : null
}

/** What a notice looks like to users now. */
export function noticeState(n: Notice, now: number): 'active' | 'upcoming' | 'scheduled' | 'ended' | 'off' {
  if (!n.active) return 'off'
  if (now >= n.endsAt) return 'ended'
  if (now < n.showFrom) return 'scheduled'
  return now >= n.startsAt ? 'active' : 'upcoming'
}

/** `3 d 4 h`, `2 h 5 min`, `45 s`. */
export function formatUptime(secs: number): string {
  const d = Math.floor(secs / 86_400)
  const h = Math.floor((secs % 86_400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (d > 0) return `${d} d ${h} h`
  if (h > 0) return `${h} h ${m} min`
  if (m > 0) return `${m} min`
  return `${Math.max(0, Math.floor(secs))} s`
}

export function formatBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GiB`
  if (bytes >= 1024 ** 2) return `${Math.round(bytes / 1024 ** 2)} MiB`
  return `${Math.round(bytes / 1024)} KiB`
}

/** Full local date and time, e.g. `Tue, 25 Nov 2026, 03:00`. */
export function formatDateTime(ms: number, locale: string): string {
  return new Intl.DateTimeFormat(locale, {
    weekday: 'short',
    day: 'numeric',
    month: 'short',
    year: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  }).format(new Date(ms))
}
