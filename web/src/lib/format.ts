import { BACKEND } from './api'

/** `m:ss` or `h:mm:ss`. */
export function fmtTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return '--:--'
  const total = Math.floor(ms / 1000)
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  const ss = String(s).padStart(2, '0')
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`
}

/** Readable message for anything thrown (the Discord SDK rejects with plain `{ code, message }` objects). */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  if (typeof error === 'string') return error
  if (error && typeof error === 'object') {
    const e = error as { message?: unknown; code?: unknown }
    if (typeof e.message === 'string' && e.message) {
      return e.code != null ? `${e.message} (${String(e.code)})` : e.message
    }
    try {
      return JSON.stringify(error)
    } catch {
      return 'Unknown error'
    }
  }
  return 'Unknown error'
}

/**
 * Only mapped origins load inside the Discord iframe. The backend already rewrites URLs, this is a
 * safety net: Discord CDN -> `/backend/cdn/...`, any other absolute URL -> `/backend/img?url=...`.
 */
export function mediaUrl(url: string | null | undefined): string | null {
  if (!url) return null
  if (url.startsWith('/')) return url
  if (url.startsWith('https://cdn.discordapp.com/')) {
    return url.replace('https://cdn.discordapp.com/', `${BACKEND}/cdn/`)
  }
  if (/^https?:\/\//.test(url)) return `${BACKEND}/img?url=${encodeURIComponent(url)}`
  return url
}

export const cx = (...c: Array<string | false | null | undefined>) => c.filter(Boolean).join(' ')
