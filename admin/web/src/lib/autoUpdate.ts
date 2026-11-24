// Form helpers of the automatic update notices card (Notices page).
import type { AutoUpdateSettings, Weekday } from './types'

export const WEEKDAYS: Weekday[] = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun']
export const MAX_LEAD_HOURS = 14 * 24
export const MAX_DURATION_MINUTES = 24 * 60
export const MIN_INTERVAL_MINUTES = 10
export const MAX_INTERVAL_MINUTES = 24 * 60

export type LeadChoice = '24' | '72' | 'custom'

/** The lead time preset a value matches (24 hours, 3 days) or `custom`. */
export function leadChoice(hours: number): LeadChoice {
  return hours === 24 ? '24' : hours === 72 ? '72' : 'custom'
}

/** The form: numbers stay text while they are typed. */
export type AutoUpdateDraft = Omit<AutoUpdateSettings, 'leadHours' | 'durationMinutes' | 'checkIntervalMinutes'> & {
  lead: LeadChoice
  leadHours: string
  durationMinutes: string
  checkIntervalMinutes: string
}

export function toDraft(s: AutoUpdateSettings): AutoUpdateDraft {
  return {
    ...s,
    lead: leadChoice(s.leadHours),
    leadHours: String(s.leadHours),
    durationMinutes: String(s.durationMinutes),
    checkIntervalMinutes: String(s.checkIntervalMinutes),
  }
}

const inRange = (v: number, min: number, max: number) => Number.isInteger(v) && v >= min && v <= max

/** The request body, or the translation key of the first problem (the server checks the rest). */
export function fromDraft(d: AutoUpdateDraft): AutoUpdateSettings | string {
  const { lead, ...rest } = d
  const leadHours = lead === 'custom' ? Number(d.leadHours) : Number(lead)
  const durationMinutes = Number(d.durationMinutes)
  const checkIntervalMinutes = Number(d.checkIntervalMinutes)
  if (
    !inRange(leadHours, 1, MAX_LEAD_HOURS) ||
    !inRange(durationMinutes, 1, MAX_DURATION_MINUTES) ||
    !inRange(checkIntervalMinutes, MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES)
  )
    return 'autoUpdate.invalidNumber'
  if (!/^\d{1,2}:\d{2}$/.test(d.time.trim())) return 'autoUpdate.invalidTime'
  return {
    ...rest,
    image: d.image.trim(),
    time: d.time.trim(),
    timezone: d.timezone.trim(),
    leadHours,
    durationMinutes,
    checkIntervalMinutes,
  }
}

/** Date and time in a given IANA zone, with the zone's short name. */
export function formatInZone(ms: number, locale: string, timeZone: string): string {
  try {
    return new Intl.DateTimeFormat(locale, {
      weekday: 'short',
      day: 'numeric',
      month: 'short',
      year: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
      timeZone,
      timeZoneName: 'short',
    }).format(new Date(ms))
  } catch {
    return new Date(ms).toISOString()
  }
}

/** Localized weekday names (2024-01-01 was a Monday). */
export function weekdayName(day: Weekday, locale: string): string {
  const i = WEEKDAYS.indexOf(day)
  return new Intl.DateTimeFormat(locale, { weekday: 'long', timeZone: 'UTC' }).format(new Date(Date.UTC(2024, 0, 1 + i)))
}

/** IANA zones the browser knows (for suggestions), empty when it cannot list them. */
export function timeZones(): string[] {
  try {
    return Intl.supportedValuesOf('timeZone')
  } catch {
    return []
  }
}
