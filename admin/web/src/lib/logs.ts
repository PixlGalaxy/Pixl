import type { LogLevel, LogLine } from './types'

export type LogFilter = { levels: ReadonlySet<LogLevel>; target: string; text: string }

/** Lines matching the level set, the source (target prefix) and the text (case-insensitive). */
export function filterLines(lines: readonly LogLine[], f: LogFilter): LogLine[] {
  const text = f.text.trim().toLowerCase()
  return lines.filter(
    (l) =>
      f.levels.has(l.level) &&
      (!f.target || l.target === f.target || l.target.startsWith(`${f.target}::`)) &&
      (!text || l.text.toLowerCase().includes(text) || l.target.toLowerCase().includes(text)),
  )
}
