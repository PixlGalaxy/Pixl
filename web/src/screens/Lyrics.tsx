import { MicVocal } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Card, Empty, PageSpinner, TileArtwork } from '../components/ui'
import { api } from '../lib/api'
import { cx } from '../lib/format'
import { usePosition } from '../lib/live'
import { useLynx } from '../lib/lynx'
import type { LyricLine, Lyrics } from '../lib/types'

type Load = { uid: string; lyrics: Lyrics | null } | { uid: string; error: true }

function SyncedLyrics({ lines }: { lines: LyricLine[] }) {
  const { t } = useTranslation()
  const { state, anchor, setAnchor, channelId, permissions, run } = useLynx()
  const position = usePosition(state, anchor)
  const box = useRef<HTMLDivElement>(null)
  const manualUntil = useRef(0)

  let current = -1
  for (let i = 0; i < lines.length; i++) {
    if ((lines[i]?.timeMs ?? Infinity) <= position) current = i
    else break
  }

  useEffect(() => {
    const el = box.current
    if (!el || current < 0 || Date.now() < manualUntil.current) return
    const line = el.querySelector<HTMLElement>(`[data-line="${current}"]`)
    if (!line) return
    const top = line.offsetTop - el.clientHeight / 2 + line.clientHeight / 2
    el.scrollTo({ top, behavior: 'smooth' })
  }, [current])

  const pauseAuto = () => {
    manualUntil.current = Date.now() + 4000
  }
  const canSeek = permissions.canControl && !!channelId

  return (
    <div
      ref={box}
      onWheel={pauseAuto}
      onTouchMove={pauseAuto}
      className="relative max-h-[calc(100vh-15rem)] min-h-64 overflow-y-auto px-4 py-6"
    >
      <ol className="space-y-3">
        {lines.map((l, i) => (
          <li key={`${l.timeMs}-${i}`} data-line={i}>
            <button
              type="button"
              disabled={!canSeek}
              title={canSeek ? t('lyrics.jump') : undefined}
              onClick={() => {
                if (!channelId) return
                manualUntil.current = 0
                setAnchor({ positionMs: l.timeMs, localAt: Date.now() })
                void run(() => api.seek(channelId, l.timeMs))
              }}
              className={cx(
                'block w-full text-left text-lg leading-snug font-semibold tracking-tight transition-colors disabled:cursor-default',
                i === current ? 'text-fg' : i < current ? 'text-muted/60' : 'text-muted',
                canSeek && 'hover:text-accent-500',
              )}
            >
              {l.line || <span className="text-muted/50">···</span>}
            </button>
          </li>
        ))}
      </ol>
    </div>
  )
}

export function LyricsScreen() {
  const { t } = useTranslation()
  const { state, channelId, loaded } = useLynx()
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

  if (!loaded) return <PageSpinner />
  const track = state?.current
  if (!track) {
    return (
      <Card title={t('lyrics.title')} icon={<MicVocal size={18} />} bodyClassName="">
        <Empty>{t('lyrics.nothingPlaying')}</Empty>
      </Card>
    )
  }

  const pending = !load || load.uid !== uid
  const lyrics = !pending && !('error' in load) ? load.lyrics : null
  const failed = !pending && 'error' in load
  const synced = lyrics?.synced && lyrics.synced.length > 0 ? lyrics.synced : null

  return (
    <Card
      title={t('lyrics.title')}
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
