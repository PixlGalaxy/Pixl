// Operator notices (scheduled downtime...) shown as a banner under the tab bar. The server sends only
// the notices users may see right now; this file decides which one to show and formats it.
import type { PublicNotice } from './types'

const DISMISSED_KEY = 'lynx:dismissed-notices'
const MAX_DISMISSED = 50

/** Upcoming until it starts, active until it ends, then gone (the banner hides it on its own). */
export function noticePhase(n: PublicNotice, now: number): 'upcoming' | 'active' | null {
  if (now >= n.endsAt) return null
  return now >= n.startsAt ? 'active' : 'upcoming'
}

/** The notice for the banner: the soonest one that has not ended and was not dismissed. */
export function pickNotice(list: readonly PublicNotice[], now: number, dismissed: ReadonlySet<number>): PublicNotice | null {
  let best: PublicNotice | null = null
  for (const n of list) {
    if (dismissed.has(n.id) || noticePhase(n, now) === null) continue
    if (!best || n.startsAt < best.startsAt || (n.startsAt === best.startsAt && n.id < best.id)) best = n
  }
  return best
}

/** Milliseconds until the banner must change by itself (a notice starts or ends), or null. */
export function nextChange(list: readonly PublicNotice[], now: number): number | null {
  let next: number | null = null
  for (const n of list) {
    for (const t of [n.startsAt, n.endsAt]) {
      if (t > now && (next === null || t - now < next)) next = t - now
    }
  }
  return next
}

/** Whole minutes between start and end (rounded up, at least 1). */
export function durationMinutes(n: PublicNotice): number {
  return Math.max(1, Math.ceil((n.endsAt - n.startsAt) / 60_000))
}

/** `Tue 25 Nov, 03:00` in the viewer's time zone and language. */
export function formatWhen(ms: number, locale: string): string {
  const d = new Date(ms)
  const day = new Intl.DateTimeFormat(locale, { weekday: 'short', day: 'numeric', month: 'short' }).format(d)
  const time = new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit' }).format(d)
  return `${day}, ${time}`
}

/** `in 2 hours`, `in 5 minutes` (Intl relative time). */
export function formatRelative(ms: number, now: number, locale: string): string {
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: 'auto' })
  const minutes = Math.round((ms - now) / 60_000)
  if (Math.abs(minutes) < 60) return rtf.format(minutes, 'minute')
  const hours = Math.round(minutes / 60)
  if (Math.abs(hours) < 48) return rtf.format(hours, 'hour')
  return rtf.format(Math.round(hours / 24), 'day')
}

export function readDismissed(): Set<number> {
  try {
    const raw = localStorage.getItem(DISMISSED_KEY)
    const list = raw ? (JSON.parse(raw) as unknown) : []
    return new Set(Array.isArray(list) ? list.filter((x): x is number => typeof x === 'number') : [])
  } catch {
    return new Set()
  }
}

export function saveDismissed(ids: ReadonlySet<number>) {
  try {
    localStorage.setItem(DISMISSED_KEY, JSON.stringify([...ids].slice(-MAX_DISMISSED)))
  } catch {
    // storage unavailable inside some iframes: the dismissal lasts until the panel reloads
  }
}
