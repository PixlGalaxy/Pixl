import { Pause, Play, Radio, SkipForward, Square, ThumbsUp, Timer } from 'lucide-react'
import { useState, useSyncExternalStore } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { cx, fmtTime } from '../lib/format'
import { useCompactInteractive } from '../lib/layout'
import { useNow, usePosition } from '../lib/live'
import { activeLine } from '../lib/lyrics'
import { useLynx } from '../lib/lynx'
import type { LyricLine, PlayerState, Track } from '../lib/types'
import { useTrackLyrics } from '../screens/Lyrics'
import { AI_STEPS } from './AiProgress'
import { LynxMark } from './Logo'
import type { TabId } from './Shell'
import { AiTag } from './TrackBits'
import { Artwork, Button, Spinner } from './ui'

// Sizes follow the window height so the same layout fits 240x135 up to ~400x225.
const ART = 'size-[clamp(40px,32vh,76px)] rounded-lg'
const BTN = 'size-[clamp(28px,20vh,40px)]'

/** "Radio: <phase>" while an AI radio job is running. */
function RadioLine({ state }: { state: PlayerState | null }) {
  const { t } = useTranslation()
  const job = state?.ai?.job
  if (!job || !AI_STEPS.includes(job.phase)) return null
  return (
    <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-accent-600 dark:text-accent-400" aria-live="polite">
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
            <div className="h-full rounded-full bg-accent-500" style={{ width: `${pct}%` }} />
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

const iconBtn = `inline-flex ${BTN} items-center justify-center rounded-lg text-muted transition-colors hover:bg-surface-2 hover:text-fg focus-visible:outline-2 focus-visible:outline-accent-500 disabled:pointer-events-none disabled:opacity-40`

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
          className={`inline-flex ${BTN} items-center justify-center rounded-full bg-accent-600 text-white shadow-sm shadow-accent-600/25 transition-colors hover:bg-accent-500 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-500 disabled:pointer-events-none disabled:opacity-50`}
        >
          {playing ? <Pause size={16} fill="currentColor" /> : <Play size={16} fill="currentColor" className="translate-x-px" />}
        </button>
      )}
      {(can || voteSkip) && (
        <button
          type="button"
          aria-label={nextLabel}
          title={nextLabel}
          onClick={() => run(() => api.skip(channelId, state.current?.uid), voteSkip ? t('player.voteSent') : undefined)}
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
            {track.reason && <AiTag />}
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
  const { channelId } = useLynx()
  const radio = <RadioLine state={state} />
  const job = state?.ai?.job
  const radioRunning = !!job && AI_STEPS.includes(job.phase)
  return (
    <div className="flex flex-1 items-center justify-center gap-3">
      <LynxMark className="size-[clamp(28px,22vh,44px)] text-muted" />
      <div className="min-w-0">
        <div className="truncate text-[13px] font-semibold">
          {channelId ? t('player.nothingTitle') : t('noVoice.title')}
        </div>
        {radioRunning ? (
          radio
        ) : (
          <div className="truncate text-[11px] text-muted">{channelId ? t('pip.nothingHint') : t('noVoice.pip')}</div>
        )}
      </div>
    </div>
  )
}

// ---------------------------------------------------------------------------------------------
// Tab-specific compact views: the compact window shows what the user was looking at in the panel.

const SMALL_ART = 'size-[clamp(20px,15vh,32px)] rounded-md'

/** Window height, for the variants that show as many rows as fit. */
function useViewportHeight(): number {
  return useSyncExternalStore(
    (l) => {
      window.addEventListener('resize', l)
      return () => window.removeEventListener('resize', l)
    },
    () => window.innerHeight,
    () => 180,
  )
}

/** One-line now playing: small artwork, title · artist, and the empty-channel countdown. */
function TrackLine({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const track = state.current!
  const artists = track.artists.join(', ') || t('player.unknownArtist')
  return (
    <div className="flex min-w-0 shrink-0 items-center gap-2">
      <Artwork url={track.artworkUrl} className={SMALL_ART} iconSize={12} />
      <div className="min-w-0 flex-1 truncate text-[12px] leading-tight" title={`${track.title} · ${artists}`}>
        <span className="font-semibold">{track.title}</span>
        <span className="text-muted"> · {artists}</span>
        {track.reason && <AiTag />}
      </div>
      {state.pausedReason === 'empty_channel' && (
        <span className="flex shrink-0">
          <EmptyChannelChip deadline={state.emptyChannelDeadline} />
        </span>
      )}
    </div>
  )
}

/** Muted message centred in the free space (empty and error states). */
function Hint({ children }: { children: string }) {
  return (
    <p className="flex min-h-0 flex-1 items-center justify-center px-1 text-center text-[11px] leading-snug text-muted">
      {children}
    </p>
  )
}

/** First non-empty line after `index` (instrumental gaps are skipped in the preview). */
function nextLine(lines: readonly LyricLine[], index: number): string | null {
  for (let i = index + 1; i < lines.length; i++) {
    const line = lines[i]!.line.trim()
    if (line) return line
  }
  return null
}

function SyncedLines({ state, lines }: { state: PlayerState; lines: LyricLine[] }) {
  const { anchor } = useLynx()
  const position = usePosition(state, anchor)
  // Same active-line logic as the Lyrics tab, on the same shared position anchor.
  const index = activeLine(lines, position)
  const current = index >= 0 ? lines[index]!.line.trim() : ''
  const next = nextLine(lines, index)
  return (
    <div className="flex min-h-0 flex-1 flex-col justify-center gap-[clamp(2px,2vh,6px)]">
      <p
        className={cx(
          'line-clamp-2 text-[clamp(13px,9vh,18px)] leading-snug font-semibold tracking-tight',
          current ? 'text-fg' : 'text-muted/60',
        )}
      >
        {current || '···'}
      </p>
      {next && <p className="truncate text-[clamp(11px,6.5vh,14px)] leading-snug text-muted">{next}</p>}
    </div>
  )
}

function LyricsCompact({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const { pending, failed, lyrics, synced } = useTrackLyrics()
  let body
  if (pending) {
    body = (
      <div className="flex min-h-0 flex-1 animate-pulse flex-col justify-center gap-1.5">
        <div className="h-3.5 w-4/5 rounded bg-surface-2" />
        <div className="h-2.5 w-3/5 rounded bg-surface-2" />
      </div>
    )
  } else if (failed) {
    body = <Hint>{t('pip.lyricsError')}</Hint>
  } else if (synced) {
    body = <SyncedLines state={state} lines={synced} />
  } else {
    body = <Hint>{lyrics?.plain ? t('pip.lyricsPlain') : t('pip.noLyrics')}</Hint>
  }
  return (
    <>
      <TrackLine state={state} />
      {body}
      <Progress state={state} />
    </>
  )
}

/** One upcoming song: cover, title over artist (two lines when there is room), duration. */
function QueueRow({ track, roomy }: { track: Track; roomy: boolean }) {
  const artists = track.artists.join(', ')
  return (
    <li
      className="flex min-w-0 items-center gap-[clamp(6px,3vw,10px)]"
      title={artists ? `${track.title} · ${artists}` : track.title}
    >
      <Artwork
        url={track.artworkUrl}
        className={cx('rounded-md', roomy ? 'size-[clamp(22px,15vh,32px)]' : 'size-[clamp(18px,12vh,24px)]')}
        iconSize={11}
      />
      <span className="flex min-w-0 flex-1 flex-col justify-center leading-tight">
        <span className="truncate text-[clamp(11px,5.5vh,13px)] font-medium">
          {track.title}
          {track.reason && <AiTag />}
        </span>
        {roomy && artists && <span className="truncate text-[clamp(10px,4.5vh,11px)] text-muted">{artists}</span>}
      </span>
      <span className="shrink-0 font-mono text-[10px] text-muted tabular-nums">{fmtTime(track.durationMs)}</span>
    </li>
  )
}

/** Compact queue: the next three songs with their covers, then "+N more". */
function QueueCompact({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const height = useViewportHeight()
  // Title + artist per row only when the window is tall enough for three two-line rows.
  const roomy = height >= 170
  const upcoming = state.upcoming
  const shown = upcoming.slice(0, 3)
  const more = upcoming.length - shown.length
  return (
    <>
      <TrackLine state={state} />
      {shown.length === 0 ? (
        <Hint>{t('pip.upNextEmpty')}</Hint>
      ) : (
        <div className="flex min-h-0 flex-1 flex-col justify-center gap-[clamp(2px,1.5vh,6px)] border-t border-line pt-[clamp(4px,3vh,8px)]">
          <ol className="flex flex-col gap-[clamp(3px,2vh,6px)]" aria-label={t('queue.upNext')}>
            {shown.map((tr) => (
              <QueueRow key={tr.uid} track={tr} roomy={roomy} />
            ))}
          </ol>
          {more > 0 && <div className="text-right text-[10px] leading-tight text-muted">{t('pip.more', { count: more })}</div>}
        </div>
      )}
    </>
  )
}

function RadioCompact({ state }: { state: PlayerState | null }) {
  const { t } = useTranslation()
  const { user, permissions, channelId, run } = useLynx()
  const interactive = useCompactInteractive()
  const [busy, setBusy] = useState(false)
  const ai = state?.ai ?? null
  const active = !!ai?.active
  const job = ai?.job ?? null
  const jobRunning = !!job && AI_STEPS.includes(job.phase)
  const canStop = permissions.canControl || ai?.ownerId === user.id
  const added = (state?.upcoming ?? []).filter((x) => x.reason).length

  let status
  if (job && jobRunning) {
    status = (
      <span className="flex min-w-0 items-center gap-1.5 text-accent-600 dark:text-accent-400">
        <Spinner size={12} className="shrink-0" />
        <span className="truncate">{t(`ai.phase.${job.phase}`)}</span>
      </span>
    )
  } else if (active) {
    status = <span className="block truncate">{t('pip.radioOn', { count: added })}</span>
  } else {
    status = (
      <span className="block truncate">{permissions.canAdd ? t('pip.radioOffHint') : t('pip.radioNoPermission')}</span>
    )
  }

  const act = async (fn: () => Promise<unknown>) => {
    setBusy(true)
    await run(fn)
    setBusy(false)
  }
  let control = null
  if (interactive && channelId) {
    if (active && canStop) {
      control = (
        <Button variant="danger" size="xs" icon={<Square size={11} />} loading={busy} onClick={() => act(() => api.aiStop(channelId))}>
          {t('pip.radioStop')}
        </Button>
      )
    } else if (!active && !jobRunning && permissions.canAdd) {
      control = (
        <Button
          variant="primary"
          size="xs"
          icon={<Radio size={12} />}
          loading={busy}
          onClick={() => act(() => api.aiStart(channelId))}
        >
          {t('pip.radioStart')}
        </Button>
      )
    }
  }

  const pill = active
    ? 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-300'
    : 'bg-surface-2 text-muted'
  return (
    <>
      {state?.current && <TrackLine state={state} />}
      <div className="flex min-h-0 flex-1 flex-col justify-center gap-[clamp(4px,3vh,8px)]">
        <div className="flex min-w-0 items-center gap-2">
          <Radio size={15} className="shrink-0 text-accent-600 dark:text-accent-400" />
          <span className="min-w-0 truncate text-[13px] font-semibold">{t('ai.title')}</span>
          <span className={cx('ml-auto inline-flex shrink-0 items-center gap-1 rounded-full px-1.5 py-px text-[10px] font-medium', pill)}>
            <span className={cx('size-1.5 rounded-full', active ? 'animate-pulse bg-emerald-500' : 'bg-zinc-400')} />
            {active ? t('ai.on') : t('ai.off')}
          </span>
        </div>
        <div className="flex min-w-0 items-center gap-2 text-[11px] text-muted" aria-live="polite">
          <div className="min-w-0 flex-1">{status}</div>
          {control}
        </div>
      </div>
    </>
  )
}

/**
 * Compact view for Discord's picture-in-picture window and grid tile. It follows the tab the user
 * was on in the panel: synced lyrics, the next songs, the radio, or (any other tab) the player with
 * the few controls the user may use. Same live state as the full panel; no scrolling.
 */
export function PipView({ tab = 'player' }: { tab?: TabId }) {
  const { t } = useTranslation()
  const { state, loaded, channelId, aiEnabled } = useLynx()
  const variant = tab === 'lyrics' || tab === 'queue' || (tab === 'radio' && aiEnabled) ? tab : 'player'

  let body
  let centered = true
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
  } else if (variant === 'radio' && channelId) {
    body = <RadioCompact state={state} />
    centered = false
  } else if (!state?.current) {
    body = <NothingPlaying state={state} />
  } else if (variant === 'lyrics') {
    body = <LyricsCompact state={state} />
    centered = false
  } else if (variant === 'queue') {
    body = <QueueCompact state={state} />
    centered = false
  } else {
    body = <NowPlaying state={state} />
  }

  return (
    <main
      aria-label={t('pip.label')}
      data-variant={variant}
      className={cx(
        'fixed inset-0 flex flex-col overflow-hidden bg-bg p-[clamp(6px,5vh,12px)] select-none',
        centered ? 'justify-center gap-[clamp(8px,6vh,16px)]' : 'gap-[clamp(4px,4vh,10px)]',
      )}
    >
      {body}
    </main>
  )
}
