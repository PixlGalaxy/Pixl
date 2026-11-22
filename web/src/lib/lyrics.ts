import type { LyricLine } from './types'

/**
 * Index of the line playing at `positionMs`: the last line whose `timeMs` is at or before it, -1
 * before the first line. Lines are sorted by `timeMs` (LRCLIB order); binary search.
 */
export function activeLine(lines: readonly LyricLine[], positionMs: number): number {
  let lo = 0
  let hi = lines.length - 1
  let found = -1
  while (lo <= hi) {
    const mid = (lo + hi) >> 1
    if ((lines[mid]?.timeMs ?? Infinity) <= positionMs) {
      found = mid
      lo = mid + 1
    } else {
      hi = mid - 1
    }
  }
  return found
}
