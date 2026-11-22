import { Pause, Play, SkipForward, ThumbsUp, Timer } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { cx, fmtTime } from '../lib/format'
import { useNow, usePosition } from '../lib/live'
import { useLynx } from '../lib/lynx'
import type { PlayerState } from '../lib/types'
import { AI_STEPS } from './AiProgress'
import { LynxMark } from './Logo'
import { Artwork, Spinner } from './ui'

// Sizes follow the window height so the same layout fits 240x135 up to ~400x225.
const ART = 'size-[clamp(40px,32vh,76px)] rounded-lg'
const BTN = 'size-[clamp(28px,20vh,40px)]'

/** "Radio: <phase>" while an AI radio job is running. */
function RadioLine({ state }: { state: PlayerState | null }) {
  const { t } = useTranslation()
  const job = state?.ai?.job
  if (!job || !AI_STEPS.includes(job.phase)) return null
  return (
    <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-sky-600 dark:text-sky-400" aria-live="polite">
      <Spinner size={11} className="shrink-0" />
      <span className="truncate">{t('pip.radio', { phase: t(`ai.phase.${job.phase}`) })}</span>
    </div>
  )
}

/** Amber chip with the time left before an empty channel clears the queue. */
function EmptyChannelChip({ deadline }: { deadline: number | null }) {
  const { t } = useTranslation()
  const now = useNow(deadline != null)
  const left = deadline != null ? Math.max(0, deadline - now) : null
  return (
    <span
      title={t('player.emptyChannelTitle')}
      className="inline-flex max-w-full min-w-0 items-center gap-1 self-start rounded-full border border-amber-500/40 bg-amber-500/10 px-1.5 py-px text-[10px] font-medium text-amber-700 dark:text-amber-300"
    >
      <Timer size={11} className="shrink-0" />
      <span className="truncate">
        {left != null ? (
          <>
            {t('pip.emptyCountdown')} <span className="font-mono tabular-nums">{fmtTime(left)}</span>
          </>
        ) : (
          t('pip.emptyPaused')
        )}
      </span>
    </span>
  )
}

function Progress({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const { anchor } = useLynx()
  const position = usePosition(state, anchor)
  const duration = state.current?.durationMs ?? null
  const pct = duration ? Math.min(100, (position / duration) * 100) : 0
  return (
    <div className="flex items-center gap-2 font-mono text-[10px] leading-none text-muted tabular-nums">
      <span className="shrink-0">{fmtTime(position)}</span>
      {duration != null ? (
        <>
          <div
            role="progressbar"
            aria-label={t('player.seek')}
            aria-valuemin={0}
            aria-valuemax={duration}
            aria-valuenow={Math.round(position)}
            className="h-1 min-w-0 flex-1 overflow-hidden rounded-full bg-line"
          >
            <div className="h-full rounded-full bg-sky-500" style={{ width: `${pct}%` }} />
          </div>
          <span className="shrink-0">{fmtTime(duration)}</span>
        </>
      ) : (
        <span className="rounded-md border border-red-500/30 bg-red-500/10 px-1 py-px font-sans text-[9px] font-semibold tracking-wide text-red-600 uppercase dark:text-red-400">
          {t('player.live')}
        </span>
      )}
    </div>
  )
}

const iconBtn = `inline-flex ${BTN} items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface-2 hover:text-fg focus-visible:outline-2 focus-visible:outline-sky-500 disabled:pointer-events-none disabled:opacity-40`

function Controls({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const { channelId, permissions, settings, aiEnabled, run, setState } = useLynx()
  const track = state.current
  if (!channelId || !track) return null

  const can = permissions.canControl
  const voteSkip = !can && !!settings?.voteSkip.enabled
  const playing = state.status === 'playing'
  const liked = track.feedback === 'like'
  const showLike = aiEnabled
  if (!can && !voteSkip && !showLike) return null

  const pauseLabel = playing ? t('player.pause') : t('player.play')
  const nextLabel = voteSkip ? t('player.voteSkip') : t('player.next')
  const likeLabel = liked ? t('player.liked') : t('player.like')
  return (
    <div className="flex items-center justify-center gap-[clamp(4px,3vw,12px)]">
      {showLike && (
        <button
          type="button"
          aria-label={likeLabel}
          title={likeLabel}
          aria-pressed={liked}
          onClick={() => {
            const value = liked ? null : 'like'
            setState((s) => (s?.current?.uid === track.uid ? { ...s, current: { ...s.current, feedback: value } } : s))
            void run(() => api.feedback(channelId, track.uid, value))
          }}
          className={cx(iconBtn, liked && '!text-emerald-500')}
        >
          <ThumbsUp size={16} fill={liked ? 'currentColor' : 'none'} />
        </button>
      )}
      {can && (
        <button
          type="button"
          aria-label={pauseLabel}
          title={pauseLabel}
          disabled={state.status === 'loading'}
          onClick={() => {
            const next = playing ? 'paused' : 'playing'
            setState((s) => (s ? { ...s, status: next, pausedReason: next === 'paused' ? 'user' : null } : s))
            void run(() => (playing ? api.pause(channelId) : api.resume(channelId)))
          }}
          className={`inline-flex ${BTN} items-center justify-center rounded-full bg-sky-600 text-white shadow-sm shadow-sky-600/25 transition-colors hover:bg-sky-500 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sky-500 disabled:pointer-events-none disabled:opacity-50`}
        >
          {playing ? <Pause size={16} fill="currentColor" /> : <Play size={16} fill="currentColor" className="translate-x-px" />}
        </button>
      )}
      {(can || voteSkip) && (
        <button
          type="button"
          aria-label={nextLabel}
          title={nextLabel}
          onClick={() => run(() => api.skip(channelId), voteSkip ? t('player.voteSent') : undefined)}
          className={iconBtn}
        >
          <SkipForward size={16} />
        </button>
      )}
    </div>
  )
}

function NowPlaying({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const track = state.current!
  const emptyChannel = state.pausedReason === 'empty_channel'
  const artists = track.artists.join(', ') || t('player.unknownArtist')
  return (
    <>
      <div className="flex min-h-0 items-center gap-2.5">
        <Artwork url={track.artworkUrl} className={ART} iconSize={18} />
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <div className="truncate text-[13px] leading-tight font-semibold" title={track.title}>
            {track.title}
          </div>
          <div className="truncate text-[11px] leading-tight text-muted" title={artists}>
            {artists}
          </div>
          {emptyChannel ? <EmptyChannelChip deadline={state.emptyChannelDeadline} /> : <RadioLine state={state} />}
        </div>
      </div>
      <div className="flex flex-col gap-[clamp(4px,3vh,10px)]">
        <Progress state={state} />
        <Controls state={state} />
      </div>
    </>
  )
}

function NothingPlaying({ state }: { state: PlayerState | null }) {
  const { t } = useTranslation()
  const radio = <RadioLine state={state} />
  const job = state?.ai?.job
  const radioRunning = !!job && AI_STEPS.includes(job.phase)
  return (
    <div className="flex flex-1 items-center justify-center gap-3">
      <LynxMark className="size-[clamp(28px,22vh,44px)] text-muted" />
      <div className="min-w-0">
        <div className="truncate text-[13px] font-semibold">{t('player.nothingTitle')}</div>
        {radioRunning ? radio : <div className="truncate text-[11px] text-muted">{t('pip.nothingHint')}</div>}
      </div>
    </div>
  )
}

/**
 * Compact view for Discord's picture-in-picture window and grid tile: artwork, title, progress and
 * the few controls the user may use. Same live state as the full panel; no scrolling.
 */
export function PipView() {
  const { t } = useTranslation()
  const { state, loaded } = useLynx()

  let body
  if (!loaded) {
    body = (
      <div className="flex flex-1 animate-pulse items-center gap-2.5">
        <div className={`${ART} shrink-0 bg-surface-2`} />
        <div className="flex-1 space-y-1.5">
          <div className="h-3 w-3/4 rounded bg-surface-2" />
          <div className="h-2.5 w-1/2 rounded bg-surface-2" />
        </div>
      </div>
    )
  } else if (!state?.current) {
    body = <NothingPlaying state={state} />
  } else {
    body = <NowPlaying state={state} />
  }

  return (
    <main
      aria-label={t('pip.label')}
      className="fixed inset-0 flex flex-col justify-center gap-[clamp(8px,6vh,16px)] overflow-hidden bg-bg p-[clamp(6px,5vh,12px)] select-none"
    >
      {body}
    </main>
  )
}
