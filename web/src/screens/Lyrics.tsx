import { MicVocal } from 'lucide-react'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Card, Empty, PageSpinner, TileArtwork } from '../components/ui'
import { api } from '../lib/api'
import { cx } from '../lib/format'
import { localAnchor, usePosition } from '../lib/live'
import { activeLine } from '../lib/lyrics'
import { useLynx } from '../lib/lynx'
import type { LyricLine, Lyrics } from '../lib/types'

type Load = { uid: string; lyrics: Lyrics | null } | { uid: string; error: true }

/** Auto-scroll pauses this long after the user scrolls the lyrics by hand, then re-centers. */
const MANUAL_SCROLL_PAUSE_MS = 4000

function SyncedLyrics({ lines }: { lines: LyricLine[] }) {
  const { t } = useTranslation()
  const { state, anchor, setAnchor, channelId, permissions, run } = useLynx()
  const position = usePosition(state, anchor)
  const box = useRef<HTMLDivElement>(null)
  const manualUntil = useRef(0)
  const resume = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)

  // The single source of truth for both the highlight and the auto-scroll.
  const current = activeLine(lines, position)
  const currentRef = useRef(current)
  currentRef.current = current

  const center = useCallback((index: number, smooth: boolean) => {
    const el = box.current
    if (!el || index < 0) return
    const line = el.querySelector<HTMLElement>(`[data-line="${index}"]`)
    if (!line) return
    const top = line.offsetTop - el.clientHeight / 2 + line.clientHeight / 2
    el.scrollTo({ top, behavior: smooth ? 'smooth' : 'auto' })
  }, [])

  useEffect(() => {
    if (performance.now() < manualUntil.current) return
    center(current, true)
  }, [current, center])

  // Back from hidden / picture-in-picture: jump straight to the right line.
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState !== 'visible') return
      manualUntil.current = 0
      center(currentRef.current, false)
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      document.removeEventListener('visibilitychange', onVisible)
      clearTimeout(resume.current)
    }
  }, [center])

  const pauseAuto = () => {
    manualUntil.current = performance.now() + MANUAL_SCROLL_PAUSE_MS
    clearTimeout(resume.current)
    resume.current = setTimeout(() => {
      manualUntil.current = 0
      center(currentRef.current, true)
    }, MANUAL_SCROLL_PAUSE_MS)
  }
  const canSeek = permissions.canControl && !!channelId

  return (
    <div
      ref={box}
      onWheel={pauseAuto}
      onTouchMove={pauseAuto}
      onKeyDown={(e) => ['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' '].includes(e.key) && pauseAuto()}
      className="relative max-h-[calc(100vh-15rem)] min-h-64 overflow-y-auto px-4 py-6"
    >
      <ol className="space-y-1">
        {lines.map((l, i) => (
          <li key={`${l.timeMs}-${i}`} data-line={i}>
            <button
              type="button"
              disabled={!canSeek}
              aria-current={i === current ? 'true' : undefined}
              title={canSeek ? t('lyrics.jump') : undefined}
              onClick={() => {
                if (!channelId) return
                manualUntil.current = 0
                clearTimeout(resume.current)
                setAnchor(localAnchor(l.timeMs))
                void run(() => api.seek(channelId, l.timeMs))
              }}
              className={cx(
                '-mx-2 block w-[calc(100%+1rem)] rounded-lg px-2 py-1 text-left text-lg leading-snug font-semibold tracking-tight transition-colors disabled:cursor-default',
                i === current ? 'text-fg' : i < current ? 'text-muted/60' : 'text-muted',
                canSeek && 'hover:bg-surface-2',
              )}
            >
              {l.line || <span className={i === current ? undefined : 'text-muted/50'}>···</span>}
            </button>
          </li>
        ))}
      </ol>
    </div>
  )
}

export type TrackLyrics = { pending: boolean; failed: boolean; lyrics: Lyrics | null; synced: LyricLine[] | null }

/** Lyrics of the current song, fetched again whenever the song changes (full tab and compact view). */
export function useTrackLyrics(): TrackLyrics {
  const { state, channelId } = useLynx()
  const uid = state?.current?.uid ?? null
  const [load, setLoad] = useState<Load | null>(null)

  useEffect(() => {
    if (!uid || !channelId) return
    let cancelled = false
    api.lyrics(channelId).then(
      (lyrics) => !cancelled && setLoad({ uid, lyrics }),
      () => !cancelled && setLoad({ uid, error: true }),
    )
    return () => {
      cancelled = true
    }
  }, [uid, channelId])

  const pending = !load || load.uid !== uid
  const lyrics = !pending && !('error' in load) ? load.lyrics : null
  const failed = !pending && 'error' in load
  const synced = lyrics?.synced && lyrics.synced.length > 0 ? lyrics.synced : null
  return { pending, failed, lyrics, synced }
}

export function LyricsScreen() {
  const { t } = useTranslation()
  const { state, loaded } = useLynx()
  const { pending, failed, lyrics, synced } = useTrackLyrics()

  if (!loaded) return <PageSpinner />
  const track = state?.current
  if (!track) {
    return (
      <Card title={t('lyrics.title')} icon={<MicVocal size={18} />} help="info.lyrics.card" bodyClassName="">
        <Empty>{t('lyrics.nothingPlaying')}</Empty>
      </Card>
    )
  }

  return (
    <Card
      title={t('lyrics.title')}
      help="info.lyrics.card"
      description={
        <span className="flex min-w-0 items-baseline gap-1.5">
          <span className="truncate" title={[track.title, track.artists.join(', ')].filter(Boolean).join(' · ')}>
            <span className="font-medium text-fg/80">{track.title}</span>
            {track.artists.length > 0 && <> · {track.artists.join(', ')}</>}
          </span>
          {lyrics && (
            <span className="shrink-0 font-mono text-[11px]">· {t('lyrics.source', { source: 'LRCLIB' })}</span>
          )}
        </span>
      }
      media={<TileArtwork url={track.artworkUrl} fallback={<MicVocal size={18} />} />}
      actions={synced ? <span className="text-xs text-muted">{t('lyrics.synced')}</span> : undefined}
      bodyClassName=""
    >
      {pending ? (
        <PageSpinner />
      ) : failed ? (
        <Empty>{t('lyrics.error')}</Empty>
      ) : synced ? (
        <SyncedLyrics lines={synced} />
      ) : lyrics?.plain ? (
        <div className="max-h-[calc(100vh-15rem)] overflow-y-auto px-4 py-5">
          <p className="text-sm leading-relaxed whitespace-pre-line">{lyrics.plain}</p>
        </div>
      ) : (
        <Empty>{t('lyrics.none')}</Empty>
      )}
    </Card>
  )
}
