import { ChevronRight, History, ListPlus, Trash2, TrendingUp, X } from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { openExternal } from '../lib/discord'
import { onTasteChanged, openAccount } from '../lib/events'
import { cx, fmtAgo } from '../lib/format'
import { useLynx } from '../lib/lynx'
import type { GenreTrack, Taste } from '../lib/types'
import { Artwork, Button, Card, IconButton, Spinner } from './ui'

/** Genre songs fetched per page ("Show more" loads the next one). */
const PAGE = 20
const MAX_GENRES = 8

type GenreList = { tracks: GenreTrack[]; total: number }

/** One genre's songs: first page on open, more on demand, optimistic removal. */
function useGenreTracks(genre: string | null) {
  const [data, setData] = useState<GenreList | null>(null)
  const [failed, setFailed] = useState(false)
  const [loadingMore, setLoadingMore] = useState(false)
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    setData(null)
    setFailed(false)
    if (!genre) return
    let alive = true
    api.tasteGenre(genre, PAGE, 0).then(
      (d) => alive && setData(d),
      () => alive && setFailed(true),
    )
    return () => {
      alive = false
    }
  }, [genre, attempt])

  const more = async () => {
    if (!genre || !data) return
    setLoadingMore(true)
    try {
      const d = await api.tasteGenre(genre, PAGE, data.tracks.length)
      setData((cur) => {
        if (!cur) return cur
        const seen = new Set(cur.tracks.map((x) => x.key))
        return { tracks: [...cur.tracks, ...d.tracks.filter((x) => !seen.has(x.key))], total: d.total }
      })
    } catch {
      setFailed(true)
    } finally {
      setLoadingMore(false)
    }
  }

  const drop = (key: string) =>
    setData((cur) => cur && { tracks: cur.tracks.filter((x) => x.key !== key), total: Math.max(0, cur.total - 1) })
  const restore = (track: GenreTrack, index: number) =>
    setData((cur) => {
      if (!cur || cur.tracks.some((x) => x.key === track.key)) return cur
      const tracks = [...cur.tracks]
      tracks.splice(Math.min(index, tracks.length), 0, track)
      return { tracks, total: cur.total + 1 }
    })

  return { data, failed, loadingMore, more, drop, restore, retry: () => setAttempt((n) => n + 1) }
}

type GenreState = ReturnType<typeof useGenreTracks>

/** Adds one song to the queue of the followed voice channel (hidden without the add permission). */
function AddToQueue({ url, title }: { url: string; title: string }) {
  const { t } = useTranslation()
  const { channelId, channelPermissions: permissions, run } = useLynx()
  const [busy, setBusy] = useState(false)
  if (!channelId || !permissions.canAdd) return null
  return (
    <IconButton
      label={t('taste.addLabel', { title })}
      title={t('taste.addLabel', { title })}
      disabled={busy}
      className="size-7 hover:text-accent-600 dark:hover:text-accent-400"
      onClick={async () => {
        setBusy(true)
        await run(() => api.add(channelId, url), t('queue.added', { count: 1 }))
        setBusy(false)
      }}
    >
      {busy ? <Spinner size={14} /> : <ListPlus size={15} />}
    </IconButton>
  )
}

function SongRow({ track, onRemove }: { track: GenreTrack; onRemove: () => void }) {
  const { t, i18n } = useTranslation()
  const [asking, setAsking] = useState(false)
  return (
    <li className="border-t border-line first:border-t-0">
      <div className="flex items-center gap-3 py-2">
        <Artwork url={track.artworkUrl} className="size-9 rounded-lg" iconSize={14} />
        <button
          type="button"
          onClick={() => openExternal(track.url)}
          className="min-w-0 flex-1 text-left hover:text-accent-600 dark:hover:text-accent-400"
        >
          <div className="truncate text-sm font-medium">{track.title}</div>
          <div className="truncate text-xs text-muted">{track.artists.join(', ')}</div>
        </button>
        <div className="shrink-0 text-right leading-tight">
          <div className="font-mono text-[11px] text-muted tabular-nums">{t('privacy.plays', { count: track.plays })}</div>
          <div className="text-[11px] text-muted">{fmtAgo(track.lastPlayed, i18n.language)}</div>
        </div>
        <AddToQueue url={track.url} title={track.title} />
        <IconButton
          label={t('taste.removeLabel', { title: track.title })}
          title={t('taste.removeHint')}
          aria-expanded={asking}
          onClick={() => setAsking((a) => !a)}
          className={cx('size-7', asking ? 'bg-red-500/10 text-red-600 dark:text-red-400' : 'hover:text-red-600 dark:hover:text-red-400')}
        >
          <Trash2 size={14} />
        </IconButton>
      </div>
      {asking && (
        <div className="mb-2 flex animate-[pop-in_.15s_ease-out] flex-wrap items-center justify-between gap-2 rounded-lg border border-red-500/30 bg-red-500/5 px-3 py-2">
          <p className="min-w-0 flex-1 text-xs font-medium">{t('taste.removeQuestion')}</p>
          <div className="flex shrink-0 gap-2">
            <Button
              variant="danger"
              size="xs"
              icon={<Trash2 size={12} />}
              onClick={() => {
                setAsking(false)
                onRemove()
              }}
            >
              {t('taste.remove')}
            </Button>
            <Button size="xs" onClick={() => setAsking(false)}>
              {t('common.cancel')}
            </Button>
          </div>
        </div>
      )}
    </li>
  )
}

/** Songs of the open genre (inline under the genre on narrow widths, beside the list on wide ones). */
function GenreSongs({
  genre,
  state,
  onRemove,
  onClose,
}: {
  genre: string
  state: GenreState
  onRemove: (track: GenreTrack) => void
  onClose?: () => void
}) {
  const { t } = useTranslation()
  const { data, failed, loadingMore } = state
  return (
    <div className="space-y-2">
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <h4 className="truncate text-[10px] font-semibold tracking-wide text-muted uppercase">
            {data ? t('taste.genreSongsCount', { genre, count: data.total }) : t('taste.genreSongs', { genre })}
          </h4>
          <p className="text-xs text-muted">{t('taste.removeHint')}</p>
        </div>
        {onClose && (
          <IconButton label={t('taste.close')} onClick={onClose} className="-mt-1 size-7">
            <X size={14} />
          </IconButton>
        )}
      </div>
      {failed && !data ? (
        <div className="flex items-center justify-between gap-3 py-2 text-sm text-muted">
          {t('errors.generic')}
          <Button size="xs" onClick={state.retry}>
            {t('common.retry')}
          </Button>
        </div>
      ) : !data ? (
        <div className="flex justify-center py-4 text-muted">
          <Spinner />
        </div>
      ) : data.tracks.length === 0 ? (
        <p className="py-3 text-sm text-muted">{t('taste.genreEmpty')}</p>
      ) : (
        <>
          <ul>
            {data.tracks.map((tr) => (
              <SongRow key={tr.key} track={tr} onRemove={() => onRemove(tr)} />
            ))}
          </ul>
          {data.tracks.length < data.total && (
            <div className="flex justify-center pt-1">
              <Button size="xs" loading={loadingMore} onClick={() => void state.more()}>
                {t('taste.showMore', { count: data.total - data.tracks.length })}
              </Button>
            </div>
          )}
        </>
      )}
    </div>
  )
}

function MostPlayed({ taste }: { taste: Taste }) {
  const { t } = useTranslation()
  if (taste.topTracks.length === 0) return null
  return (
    <div>
      <h4 className="mb-1 text-[10px] font-semibold tracking-wide text-muted uppercase">{t('privacy.topTracks')}</h4>
      <ol>
        {taste.topTracks.slice(0, 5).map((tr, i) => (
          <li key={tr.url} className="flex items-center gap-3 border-t border-line py-2 first:border-t-0">
            <span className="w-4 text-right font-mono text-[11px] text-muted tabular-nums">{i + 1}</span>
            <Artwork url={tr.artworkUrl} className="size-8 rounded-lg" iconSize={13} />
            <button
              type="button"
              onClick={() => openExternal(tr.url)}
              className="min-w-0 flex-1 text-left hover:text-accent-600 dark:hover:text-accent-400"
            >
              <div className="truncate text-sm font-medium">{tr.title}</div>
              <div className="truncate text-xs text-muted">{tr.artists.join(', ')}</div>
            </button>
            <span className="font-mono text-[11px] text-muted tabular-nums">{t('privacy.plays', { count: tr.plays })}</span>
            <AddToQueue url={tr.url} title={tr.title} />
          </li>
        ))}
      </ol>
    </div>
  )
}

/**
 * "Your taste" at the bottom of the Radio tab: top genres (click one to see and prune its songs)
 * and the most played songs. Needs the listening history on.
 */
export function TasteCard() {
  const { t } = useTranslation()
  const { run } = useLynx()
  const [historyOn, setHistoryOn] = useState<boolean | null>(null)
  const [taste, setTaste] = useState<Taste | null>(null)
  const [failed, setFailed] = useState(false)
  const [selected, setSelected] = useState<string | null>(null)
  const [reloadKey, setReloadKey] = useState(0)
  const genre = useGenreTracks(selected)

  useEffect(() => onTasteChanged(() => setReloadKey((n) => n + 1)), [])
  useEffect(() => {
    let alive = true
    api.prefs().then(
      (p) => alive && setHistoryOn(p.historyEnabled),
      () => alive && setHistoryOn(true),
    )
    return () => {
      alive = false
    }
  }, [reloadKey])

  const loadTaste = useCallback(async () => {
    try {
      setTaste(await api.taste())
      setFailed(false)
    } catch {
      setFailed(true)
    }
  }, [])
  useEffect(() => {
    if (!historyOn) return
    setSelected(null)
    void loadTaste()
  }, [historyOn, reloadKey, loadTaste])

  const remove = async (track: GenreTrack) => {
    const index = genre.data?.tracks.findIndex((x) => x.key === track.key) ?? 0
    genre.drop(track.key)
    const ok = await run(() => api.forgetTrack(track.key), t('taste.removed', { title: track.title }))
    if (ok === null) {
      genre.restore(track, index)
      return
    }
    // Bars and most played come straight from the history: refresh them in place.
    void loadTaste()
  }

  const toggle = (g: string) => setSelected((cur) => (cur === g ? null : g))
  const genres = (taste?.topGenres ?? []).slice(0, MAX_GENRES)
  const max = Math.max(1e-9, ...genres.map((g) => g.weight))

  let body
  if (historyOn === null || (historyOn && !taste && !failed)) {
    body = (
      <div className="flex justify-center py-2 text-muted">
        <Spinner />
      </div>
    )
  } else if (!historyOn) {
    body = (
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="min-w-0 flex-1 text-sm text-muted">{t('taste.historyOff')}</p>
        <Button size="xs" icon={<History size={12} />} onClick={openAccount}>
          {t('taste.openPrivacy')}
        </Button>
      </div>
    )
  } else if (failed || !taste) {
    body = (
      <div className="flex items-center justify-between gap-3 text-sm text-muted">
        {t('errors.generic')}
        <Button size="xs" onClick={() => void loadTaste()}>
          {t('common.retry')}
        </Button>
      </div>
    )
  } else if (taste.topGenres.length === 0 && taste.topTracks.length === 0) {
    body = <p className="text-sm text-muted">{t('privacy.tasteEmpty')}</p>
  } else {
    body = (
      <div className="@container">
        <div className="grid gap-5 @2xl:grid-cols-[minmax(0,2fr)_minmax(0,3fr)] @2xl:gap-6">
          <div className="space-y-1">
            <h4 className="mb-1 text-[10px] font-semibold tracking-wide text-muted uppercase">{t('privacy.topGenres')}</h4>
            <p className="pb-1 text-xs text-muted">{t('taste.genresHint')}</p>
            {genres.length === 0 && <p className="text-sm text-muted">{t('taste.noGenres')}</p>}
            {genres.map((g) => {
              const open = selected === g.genre
              return (
                <div key={g.genre}>
                  <button
                    type="button"
                    aria-expanded={open}
                    onClick={() => toggle(g.genre)}
                    className={cx(
                      'group flex w-full items-center gap-3 rounded-lg px-2 py-1.5 text-left transition-colors',
                      open ? 'bg-accent-500/10' : 'hover:bg-surface-2',
                    )}
                  >
                    <span
                      className={cx(
                        'w-24 shrink-0 truncate text-sm',
                        open ? 'font-medium text-accent-600 dark:text-accent-400' : '',
                      )}
                    >
                      {g.genre}
                    </span>
                    <span className="h-1.5 flex-1 overflow-hidden rounded-full bg-line">
                      <span
                        className="block h-full rounded-full bg-accent-500 transition-[width] duration-700 ease-out"
                        style={{ width: `${(g.weight / max) * 100}%` }}
                      />
                    </span>
                    <ChevronRight
                      size={14}
                      className={cx(
                        'shrink-0 transition-transform',
                        open
                          ? 'rotate-90 text-accent-600 @2xl:rotate-0 dark:text-accent-400'
                          : 'text-muted group-hover:text-fg',
                      )}
                    />
                  </button>
                  {open && (
                    <div className="mt-1 mb-2 rounded-xl border border-line bg-surface-2/40 px-3 py-3 @2xl:hidden">
                      <GenreSongs genre={g.genre} state={genre} onRemove={remove} />
                    </div>
                  )}
                </div>
              )
            })}
          </div>
          {selected ? (
            <div className="hidden @2xl:block">
              <div className="rounded-xl border border-line bg-surface-2/40 px-3 py-3">
                <GenreSongs genre={selected} state={genre} onRemove={remove} onClose={() => setSelected(null)} />
              </div>
            </div>
          ) : (
            <MostPlayed taste={taste} />
          )}
          {selected && (
            <div className="@2xl:hidden">
              <MostPlayed taste={taste} />
            </div>
          )}
        </div>
      </div>
    )
  }

  return (
    <Card title={t('privacy.taste')} description={t('privacy.tasteHint')} icon={<TrendingUp size={18} />}>
      {body}
    </Card>
  )
}
