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
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { AddBox } from '../components/AddBox'
import { AiProgress } from '../components/AiProgress'
import { JamCard, JamJoinPrompt } from '../components/JamCard'
import { NowPlayingSource, SourceDot } from '../components/ListSource'
import { AiChip, AiTag, TrackReasonLine } from '../components/TrackBits'
import { InfoPopover } from '../components/InfoPopover'
import { Artwork, Avatar, Button, Card, Chip, ConfirmButton, HelpTitle, IconButton, Notice, Slider, Tag } from '../components/ui'
import { api } from '../lib/api'
import { cx, fmtTime } from '../lib/format'
import { localAnchor, useNow, usePosition } from '../lib/live'
import { useLynx } from '../lib/lynx'
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

function SeekBar({ state }: { state: PlayerState }) {
  const { t } = useTranslation()
  const { anchor, setAnchor, channelId, permissions, run, refresh } = useLynx()
  const position = usePosition(state, anchor)
  const [drag, setDrag] = useState<number | null>(null)
  const duration = state.current?.durationMs ?? null
  // A drag of the previous track never shows on the next one.
  const uid = state.current?.uid
  useEffect(() => setDrag(null), [uid])

  if (duration == null) {
    return (
      <div className="flex items-center gap-3">
        <Tag tone="red">{t('player.live')}</Tag>
        <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(position)}</span>
      </div>
    )
  }

  const shown = drag ?? position
  return (
    <div className="flex items-center gap-3">
      <span className="w-11 text-right font-mono text-[11px] text-muted tabular-nums">{fmtTime(shown)}</span>
      <Slider
        value={shown}
        max={Math.max(duration, 1)}
        step={1000}
        label={t('player.seek')}
        disabled={!permissions.canControl || !channelId}
        onInput={setDrag}
        onCommit={(ms) => {
          setDrag(null)
          if (!channelId) return
          // Bound to this track: if it changes before the server confirms, the new one is not moved.
          setAnchor(localAnchor(ms, state.current?.uid))
          // A refused seek (past the real end, seeking unsupported) puts the bar back where it is.
          void run(() => api.seek(channelId, ms)).then((done) => {
            if (done === null) void refresh()
          })
        }}
      />
      <span className="w-11 font-mono text-[11px] text-muted tabular-nums">{fmtTime(duration)}</span>
    </div>
  )
}

function FeedbackButtons({ track }: { track: Track }) {
  const { t } = useTranslation()
  const { channelId, run, setState } = useLynx()
  const set = (value: Feedback | null) => {
    if (!channelId) return
    setState((s) => (s?.current?.uid === track.uid ? { ...s, current: { ...s.current, feedback: value } } : s))
    void run(() => api.feedback(channelId, track.uid, value))
  }
  const liked = track.feedback === 'like'
  const disliked = track.feedback === 'dislike'
  return (
    <div className="flex items-center gap-2">
      <Button
        size="xs"
        aria-pressed={liked}
        icon={<ThumbsUp size={14} />}
        onClick={() => set(liked ? null : 'like')}
        className={liked ? '!border-emerald-500/40 !bg-emerald-500/10 !text-emerald-700 dark:!text-emerald-300' : ''}
      >
        {liked ? t('player.liked') : t('player.like')}
      </Button>
      <Button
        size="xs"
        aria-pressed={disliked}
        icon={<ThumbsDown size={14} />}
        onClick={() => set(disliked ? null : 'dislike')}
        className={disliked ? '!border-red-500/30 !bg-red-500/10 !text-red-600 dark:!text-red-400' : ''}
      >
        {disliked ? t('player.disliked') : t('player.dislike')}
      </Button>
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
