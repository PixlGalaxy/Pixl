import {
  ListMusic,
  Music,
  Pause,
  Play,
  Repeat,
  Repeat1,
  Shuffle,
  SkipBack,
  SkipForward,
  Square,
  ThumbsDown,
  ThumbsUp,
  Timer,
} from 'lucide-react'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { AddBox } from '../components/AddBox'
import { AiProgress } from '../components/AiProgress'
import { JamCard, JamJoinPrompt } from '../components/JamCard'
import { NowPlayingSource, SourceDot } from '../components/ListSource'
import { AnnoyedFaces, Confetti } from '../components/Reactions'
import { AiChip, AiTag, TrackReasonLine } from '../components/TrackBits'
import { InfoPopover } from '../components/InfoPopover'
import { useToast } from '../components/Toasts'
import { Artwork, Avatar, Button, Card, Chip, ConfirmButton, HelpTitle, IconButton, Notice, Slider, Tag } from '../components/ui'
import { api } from '../lib/api'
import { cx, fmtTime } from '../lib/format'
import { localAnchor, useNow, usePosition } from '../lib/live'
import { useLynx } from '../lib/lynx'
import { holdVerdict, lockedBy, releasedByTimeout } from '../lib/sync'
import type { Feedback, LoopMode, PlayerState, Track } from '../lib/types'

const ORIGIN_LABEL: Record<Track['origin'], string> = {
  youtube: 'YouTube',
  youtube_music: 'YouTube Music',
  spotify: 'Spotify',
  other: 'Web',
}

const NEXT_LOOP: Record<LoopMode, LoopMode> = { off: 'queue', queue: 'track', track: 'off' }

function EmptyChannelBanner({ deadline }: { deadline: number | null }) {
  const { t } = useTranslation()
  const now = useNow(deadline != null)
  const left = deadline != null ? Math.max(0, deadline - now) : null
  return (
    <Notice tone="amber" icon={<Timer size={16} />}>
      <div className="font-medium">{t('player.emptyChannelTitle')}</div>
      <div className="text-xs opacity-90">
        {left != null ? (
          <>
            {t('player.emptyChannelCountdown')}{' '}
            <span className="font-mono font-medium tabular-nums">{fmtTime(left)}</span>
          </>
        ) : (
          t('player.emptyChannelBody')
        )}
      </div>
    </Notice>
  )
}

/**
 * Progress bar. Dragging it takes the channel's seek-bar hold (`seek_hold` on the socket) so the
 * others see it disabled with "<name> is moving the bar"; the release seeks and frees it. The
 * server ends a hold after 5 s: the drag is then cancelled (the bar snaps back to the live
 * position) with a short notice.
 */
function SeekBar({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const { anchor, setAnchor, channelId, permissions, run, refresh, user, sendSocket, onSeekHold } = useLynx()
  const toast = useToast()
  const position = usePosition(state, anchor)
  const [drag, setDrag] = useState<number | null>(null)
  // The drag in progress: whether the server granted the hold, and when.
  const dragging = useRef<{ granted: boolean; grantedAt: number | null } | null>(null)
  const duration = state.current?.durationMs ?? null
  const lockedByOther = lockedBy(state.seekHold, user.id, performance.now())

  const endDrag = useCallback(() => {
    if (!dragging.current) return
    dragging.current = null
    setDrag(null)
    sendSocket({ type: 'seek_release' })
  }, [sendSocket])

  // A drag of the previous track never shows on the next one.
  const uid = state.current?.uid
  useEffect(() => endDrag(), [uid, endDrag])
  // Leaving the screen mid-drag frees the bar for the others.
  useEffect(() => () => endDrag(), [endDrag])

  useEffect(
    () =>
      onSeekHold((msg) => {
        const d = dragging.current
        if (!d) return
        const verdict = holdVerdict(msg, user.id, d.granted)
        if (verdict === 'granted') {
          if (!d.granted) dragging.current = { granted: true, grantedAt: performance.now() }
          return
        }
        if (verdict === 'wait') return
        const timedOut = d.granted && !msg.holder && releasedByTimeout(d.grantedAt, performance.now())
        endDrag()
        if (timedOut) toast.push('info', t('player.seekReleased'))
        else if (msg.denied && !msg.holder) toast.push('info', t('player.seekWait'))
      }),
    [onSeekHold, user.id, endDrag, toast, t],
  )

  if (duration == null) {
    return (
      <div className="flex items-center gap-3">
        <Tag tone="red">{t('player.live')}</Tag>
        <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(position)}</span>
      </div>
    )
  }

  const shown = drag ?? position
  const lockText = lockedByOther ? t('player.seekLocked', { name: lockedByOther.name }) : null
  return (
    <div>
      <div className="flex items-center gap-3">
        <span className="w-11 text-right font-mono text-[11px] text-muted tabular-nums">{fmtTime(shown)}</span>
        <Slider
          value={shown}
          max={Math.max(duration, 1)}
          step={1000}
          label={t('player.seek')}
          title={lockText ?? undefined}
          disabled={!permissions.canControl || !channelId || lockedByOther != null}
          onStart={() => {
            if (dragging.current) return
            dragging.current = { granted: false, grantedAt: null }
            sendSocket({ type: 'seek_hold' })
          }}
          onCancel={endDrag}
          onInput={(ms) => {
            // After the hold ended (5 s limit) the pointer may still move: the bar stays live.
            if (dragging.current) setDrag(ms)
          }}
          onCommit={(ms) => {
            if (!dragging.current) {
              setDrag(null)
              return
            }
            // Released first, so the hold is gone even if the seek arrives before it was granted.
            endDrag()
            if (!channelId) return
            // Bound to this track: if it changes before the server confirms, the new one is not moved.
            setAnchor(localAnchor(ms, state.current?.uid))
            // A refused seek (past the real end, seeking unsupported, someone else holding the bar)
            // puts the bar back where it is.
            void run(() => api.seek(channelId, ms)).then((done) => {
              if (done === null) void refresh()
            })
          }}
        />
        <span className="w-11 font-mono text-[11px] text-muted tabular-nums">{fmtTime(duration)}</span>
      </div>
      {lockText && (
        <p role="status" className="mt-1 text-center text-[11px] text-muted">
          {lockText}
        </p>
      )}
    </div>
  )
}

function FeedbackButtons({ track }: { track: Track }) {
  const { t } = useTranslation()
  const { channelId, run, setState } = useLynx()
  // A new burst only when a vote is cast (not when it is taken back, nor for state from elsewhere).
  const [likeBurst, setLikeBurst] = useState(0)
  const [dislikeBurst, setDislikeBurst] = useState(0)
  const set = (value: Feedback | null) => {
    if (!channelId) return
    if (value === 'like') setLikeBurst((n) => n + 1)
    if (value === 'dislike') setDislikeBurst((n) => n + 1)
    setState((s) => (s?.current?.uid === track.uid ? { ...s, current: { ...s.current, feedback: value } } : s))
    void run(() => api.feedback(channelId, track.uid, value))
  }
  const liked = track.feedback === 'like'
  const disliked = track.feedback === 'dislike'
  // Opaque fills (color-mix over the surface) so the particles behind never show through.
  return (
    <div className="flex items-center gap-2">
      <span className="relative inline-flex">
        <Confetti burst={liked ? likeBurst : 0} />
        <Button
          size="xs"
          aria-pressed={liked}
          icon={
            <ThumbsUp
              key={likeBurst}
              size={14}
              fill={liked ? 'currentColor' : 'none'}
              className={liked && likeBurst > 0 ? 'animate-[lynx-thumb-pop_.5s_ease-out] motion-reduce:animate-none' : ''}
            />
          }
          onClick={() => set(liked ? null : 'like')}
          className={cx(
            'relative z-10',
            liked &&
              '!border-emerald-500/60 !bg-[color-mix(in_srgb,var(--color-emerald-500)_14%,var(--surface))] !text-emerald-700 dark:!text-emerald-300',
          )}
        >
          {liked ? t('player.liked') : t('player.like')}
        </Button>
      </span>
      <span className="relative inline-flex">
        <AnnoyedFaces burst={disliked ? dislikeBurst : 0} />
        <Button
          size="xs"
          aria-pressed={disliked}
          icon={
            <ThumbsDown
              key={dislikeBurst}
              size={14}
              fill={disliked ? 'currentColor' : 'none'}
              className={disliked && dislikeBurst > 0 ? 'animate-[lynx-thumb-shake_.5s_ease-out] motion-reduce:animate-none' : ''}
            />
          }
          onClick={() => set(disliked ? null : 'dislike')}
          className={cx(
            'relative z-10',
            disliked &&
              '!border-red-500/50 !bg-[color-mix(in_srgb,var(--color-red-500)_12%,var(--surface))] !text-red-600 dark:!text-red-400',
          )}
        >
          {disliked ? t('player.disliked') : t('player.dislike')}
        </Button>
      </span>
      <InfoPopover k="info.player.feedback" label={`${t('player.like')} / ${t('player.dislike')}`} />
    </div>
  )
}

function SkipVotes({ votes }: { votes: { current: number; required: number } }) {
  const { t } = useTranslation()
  const pct = votes.required > 0 ? Math.min(100, (votes.current / votes.required) * 100) : 0
  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between text-xs">
        <span className="font-medium">
          <HelpTitle title={t('player.skipVotes')} help="info.player.skipVotes" />
        </span>
        <span className="font-mono text-[11px] text-muted tabular-nums">
          {votes.current} / {votes.required}
        </span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full bg-line">
        <div className="h-full rounded-full bg-accent-500 transition-[width] duration-700 ease-out" style={{ width: `${pct}%` }} />
      </div>
    </div>
  )
}

function Controls({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const { channelId, permissions, settings, run, setState } = useLynx()
  const can = permissions.canControl
  const voteSkip = !can && !!settings?.voteSkip.enabled
  const playing = state.status === 'playing'
  const loading = state.status === 'loading'

  if (!channelId) return null
  const loopLabel = t(`player.loop.${state.loop}`)
  return (
    <div className="flex items-center justify-center gap-2 sm:gap-3">
      <IconButton label={t('player.shuffle')} disabled={!can || state.upcoming.length < 2} onClick={() => run(() => api.shuffle(channelId))}>
        <Shuffle size={16} />
      </IconButton>
      <IconButton label={t('player.previous')} disabled={!can || state.history.length === 0} className="size-10" onClick={() => run(() => api.previous(channelId))}>
        <SkipBack size={18} />
      </IconButton>
      <button
        type="button"
        aria-label={playing ? t('player.pause') : t('player.play')}
        title={playing ? t('player.pause') : t('player.play')}
        disabled={!can || loading || !state.current}
        onClick={() => {
          const next = playing ? 'paused' : 'playing'
          setState((s) => (s ? { ...s, status: next, pausedReason: next === 'paused' ? 'user' : null } : s))
          void run(() => (playing ? api.pause(channelId) : api.resume(channelId)))
        }}
        className="inline-flex size-12 items-center justify-center rounded-full bg-accent-600 text-white shadow-sm shadow-accent-600/25 transition-colors hover:bg-accent-500 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-500 disabled:pointer-events-none disabled:opacity-50"
      >
        {playing ? <Pause size={20} fill="currentColor" /> : <Play size={20} fill="currentColor" className="translate-x-px" />}
      </button>
      <IconButton
        label={voteSkip ? t('player.voteSkip') : t('player.next')}
        disabled={!can && !voteSkip}
        className="size-10"
        onClick={() => run(() => api.skip(channelId, state.current?.uid), voteSkip ? t('player.voteSent') : undefined)}
      >
        <SkipForward size={18} />
      </IconButton>
      <IconButton
        label={`${t('player.loopLabel')}: ${loopLabel}`}
        active={state.loop !== 'off'}
        disabled={!can}
        onClick={() => {
          const mode = NEXT_LOOP[state.loop]
          setState((s) => (s ? { ...s, loop: mode } : s))
          void run(() => api.loop(channelId, mode))
        }}
      >
        {state.loop === 'track' ? <Repeat1 size={16} /> : <Repeat size={16} />}
      </IconButton>
    </div>
  )
}

function UpNextPreview({ state, onOpenQueue }: { state: PlayerState; onOpenQueue: () => void }) {
  const { t } = useTranslation()
  const next = state.upcoming.slice(0, 3)
  return (
    <section className="overflow-hidden rounded-2xl border border-line bg-surface shadow-sm lg:hidden">
      <div className="flex items-center justify-between gap-2 px-4 py-3">
        <h2 className="text-sm font-semibold">{t('queue.upNext')}</h2>
        <Button size="xs" variant="ghost" icon={<ListMusic size={14} />} onClick={onOpenQueue}>
          {t('player.openQueue', { count: state.upcoming.length })}
        </Button>
      </div>
      {next.length === 0 ? (
        <p className="border-t border-line px-4 py-4 text-sm text-muted">{t('queue.emptyUpcoming')}</p>
      ) : (
        <ul>
          {next.map((tr) => (
            <li key={tr.uid} className="flex items-center gap-3 border-t border-line px-4 py-2.5">
              <Artwork url={tr.artworkUrl} className="size-9 rounded-lg" iconSize={14} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm font-semibold">{tr.title}</div>
                <div className="mt-0.5 truncate text-xs text-muted">
                  <SourceDot source={tr.source} />
                  {tr.artists.join(', ')}
                  {tr.reason && <AiTag />}
                </div>
              </div>
              <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(tr.durationMs)}</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

function NothingPlaying() {
  const { t } = useTranslation()
  const { permissions, state } = useLynx()
  return (
    <div className="space-y-5">
      <JamJoinPrompt />
      <Card title={t('player.nothingTitle')} description={t('player.nothingBody')} icon={<Music size={18} />}>
        {permissions.canAdd ? <AddBox /> : <p className="text-sm text-muted">{t('player.nothingNoAdd')}</p>}
      </Card>
      {state?.ai?.job && <AiProgress />}
      {state?.jam && <JamCard />}
    </div>
  )
}

export function PlayerScreen({ onOpenQueue }: { onOpenQueue: () => void }) {
  const { t } = useTranslation()
  const { state, loaded, aiEnabled, channelPermissions: permissions, channelId, run } = useLynx()

  if (!loaded) {
    return (
      <div className="space-y-5">
        <div className="animate-pulse rounded-2xl border border-line bg-surface p-4">
          <div className="mx-auto aspect-square w-full max-w-64 rounded-xl bg-surface-2" />
          <div className="mx-auto mt-4 h-4 w-48 rounded bg-surface-2" />
          <div className="mx-auto mt-2 h-3 w-32 rounded bg-surface-2" />
        </div>
      </div>
    )
  }
  if (!state || !state.current) return <NothingPlaying />

  const track = state.current
  const status = state.status
  return (
    <div className="space-y-5">
      <JamJoinPrompt />
      {state.pausedReason === 'empty_channel' && <EmptyChannelBanner deadline={state.emptyChannelDeadline} />}

      <section className="overflow-hidden rounded-2xl border border-line bg-surface shadow-sm">
        <div className="flex flex-col gap-5 p-4 sm:flex-row sm:items-start sm:p-5">
          <Artwork
            url={track.artworkUrl}
            className="mx-auto aspect-square w-full max-w-64 rounded-xl sm:mx-0 sm:size-48 sm:max-w-none"
            iconSize={40}
          />
          <div className="@container flex min-w-0 flex-1 items-start gap-4">
          <div className="min-w-0 flex-1 space-y-3">
            <div className="flex flex-wrap items-center gap-2">
              <span
                className={cx(
                  'inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium',
                  status === 'playing' && 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-300',
                  status === 'paused' && 'bg-amber-500/10 text-amber-700 dark:text-amber-300',
                  (status === 'loading' || status === 'idle') && 'bg-surface-2 text-muted',
                )}
              >
                <span
                  className={cx(
                    'size-1.5 rounded-full',
                    status === 'playing' && 'animate-pulse bg-emerald-500',
                    status === 'paused' && 'bg-amber-500',
                    (status === 'loading' || status === 'idle') && 'bg-zinc-400',
                  )}
                />
                {t(`player.status.${status}`)}
              </span>
              <Chip>{ORIGIN_LABEL[track.origin]}</Chip>
              {track.reason && (
                <span className="inline-flex items-center gap-0.5">
                  <AiChip />
                  <InfoPopover k="info.player.ai" label={t('reason.selectedByAi')} />
                </span>
              )}
              {!state.bot.isMain && <Chip title={t('player.nodeBot')}>{state.bot.name}</Chip>}
            </div>
            <div className="min-w-0">
              <h2 className="text-xl font-semibold tracking-tight break-words">{track.title}</h2>
              <p className="mt-0.5 text-sm text-muted">{track.artists.join(', ') || t('player.unknownArtist')}</p>
            </div>
            <NowPlayingSource source={track.source} variant="compact" />
            <div className="flex items-center gap-2 text-xs text-muted">
              <Avatar url={track.requestedBy.avatarUrl} name={track.requestedBy.name} className="size-5" />
              <span className="truncate">{t(track.reason ? 'player.requestedByAi' : 'player.requestedBy', { name: track.requestedBy.name })}</span>
            </div>
            {track.genres.length > 0 && (
              <div className="flex flex-wrap gap-1.5" aria-label={t('player.genres')}>
                {track.genres.map((g) => (
                  <Chip key={g}>{g}</Chip>
                ))}
              </div>
            )}
            {track.reason && <TrackReasonLine reason={track.reason} />}
          </div>
          <NowPlayingSource source={track.source} variant="large" />
          </div>
        </div>

        <div className="space-y-4 border-t border-line p-4 sm:px-5">
          <SeekBar state={state} />
          <Controls state={state} />
        </div>

        {(aiEnabled || state.skipVotes || permissions.canControl) && (
          <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-4 py-3 sm:px-5">
            {aiEnabled ? <FeedbackButtons track={track} /> : <span />}
            {permissions.canControl && channelId && (
              <div className="flex items-center gap-1">
                <ConfirmButton
                  label={t('player.stop')}
                  question={t('player.stopQuestion')}
                  icon={<Square size={12} />}
                  onConfirm={() => run(() => api.stop(channelId))}
                />
                <InfoPopover k="info.player.stop" label={t('player.stop')} />
              </div>
            )}
          </div>
        )}
        {state.skipVotes && (
          <div className="border-t border-line px-4 py-3 sm:px-5">
            <SkipVotes votes={state.skipVotes} />
          </div>
        )}
      </section>

      <JamCard />
      {state.ai?.job && <div className="lg:hidden"><AiProgress /></div>}
      <UpNextPreview state={state} onOpenQueue={onOpenQueue} />
    </div>
  )
}
