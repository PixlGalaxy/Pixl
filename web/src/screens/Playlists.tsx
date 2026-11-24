import { ArrowLeft, ChevronRight, ListMusic, ListPlus, Pin, Plus, RefreshCw, Timer, X } from 'lucide-react'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useToast } from '../components/Toasts'
import { Artwork, Button, Card, Empty, IconButton, Notice, PageSpinner, Spinner, TileArtwork } from '../components/ui'
import { ApiError, api } from '../lib/api'
import { cx, fmtAgo, fmtTime } from '../lib/format'
import { useLynx } from '../lib/lynx'
import type { Platform, PlaylistTracks, SavedPlaylist, SavedPlaylists } from '../lib/types'

const revealCls = 'opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 [@media(hover:none)]:opacity-100'

/** Same spacing as the server's refresh limit; the server is the authority, this only avoids useless requests. */
const REFRESH_COOLDOWN_MS = 5000

const PLATFORM_DOT: Record<Platform, string> = {
  spotify: 'bg-emerald-500',
  youtube: 'bg-red-500',
  youtube_music: 'bg-rose-500',
  apple_music: 'bg-pink-500',
}

/**
 * When the server blocked this user's refreshes (spamming), until when. Kept outside the component
 * so leaving the playlist and coming back still shows the wait.
 */
let blockedUntil = 0

function PlatformLabel({ platform }: { platform: Platform }) {
  const { t } = useTranslation()
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <span className={cx('size-1.5 shrink-0 rounded-full', PLATFORM_DOT[platform] ?? 'bg-zinc-400')} />
      <span className="truncate">{t(`playlists.platforms.${platform}`, { defaultValue: platform })}</span>
    </span>
  )
}

function useNow(active: boolean) {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [active])
  return now
}

function PlaylistRow({
  p,
  onOpen,
  onPin,
  onRemove,
}: {
  p: SavedPlaylist
  onOpen: () => void
  onPin: () => void
  onRemove: () => void
}) {
  const { t } = useTranslation()
  const name = p.name || t('playlists.untitled')
  return (
    <li className="group flex items-center gap-1 border-t border-line pr-2 transition-colors hover:bg-surface-2/60">
      <button
        type="button"
        onClick={onOpen}
        aria-label={t('playlists.open', { name })}
        className="flex min-w-0 flex-1 items-center gap-3 py-2.5 pl-3 text-left focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent-500"
      >
        <Artwork url={p.imageUrl} className="size-11 rounded-lg" iconSize={16} />
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-semibold">{name}</div>
          <div className="mt-0.5 flex min-w-0 text-xs text-muted">
            <PlatformLabel platform={p.platform} />
          </div>
        </div>
        <ChevronRight size={16} className="shrink-0 text-muted" />
      </button>
      <IconButton
        label={p.pinned ? t('playlists.unpin', { name }) : t('playlists.pin', { name })}
        active={p.pinned}
        className={p.pinned ? '' : revealCls}
        onClick={onPin}
      >
        <Pin size={15} className={p.pinned ? 'fill-current' : ''} />
      </IconButton>
      <IconButton label={t('playlists.remove', { name })} className={`${revealCls} hover:!text-red-500`} onClick={onRemove}>
        <X size={16} />
      </IconButton>
    </li>
  )
}

function PlaylistList({ onOpen }: { onOpen: (p: SavedPlaylist) => void }) {
  const { t } = useTranslation()
  const { run, errorText } = useLynx()
  const [data, setData] = useState<SavedPlaylists | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setData(await api.playlists())
      setError(null)
    } catch (e) {
      setError(errorText(e))
    }
  }, [errorText])
  useEffect(() => {
    void load()
  }, [load])

  if (!data) {
    if (error) {
      return (
        <Notice tone="red">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span>{error}</span>
            <Button size="xs" onClick={() => void load()}>
              {t('common.retry')}
            </Button>
          </div>
        </Notice>
      )
    }
    return <PageSpinner />
  }

  const sorted = (list: SavedPlaylist[]) =>
    [...list].sort((a, b) => Number(b.pinned) - Number(a.pinned) || b.usedAt - a.usedAt)

  const pin = async (p: SavedPlaylist) => {
    const pinned = !p.pinned
    setData((d) => d && { ...d, playlists: sorted(d.playlists.map((x) => (x.id === p.id ? { ...x, pinned } : x))) })
    const ok = await run(() => api.pinPlaylist(p.id, pinned))
    if (ok === null) void load()
    else if (data.full && !pinned) void load()
  }
  const remove = async (p: SavedPlaylist) => {
    setData((d) => d && { ...d, playlists: d.playlists.filter((x) => x.id !== p.id) })
    const ok = await run(() => api.removePlaylist(p.id), t('playlists.removed'))
    if (ok === null || data.full) void load()
  }

  const count = data.playlists.length
  return (
    <div className="space-y-5">
      {!data.saving && (
        <Notice tone="amber">{data.max === 0 ? t('playlists.savingDisabled') : t('playlists.savingOff')}</Notice>
      )}
      {data.saving && data.full && <Notice tone="amber">{t('playlists.full')}</Notice>}
      <Card
        title={t('playlists.title')}
        help="info.playlists.list"
        description={data.max > 0 ? t('playlists.hint', { max: data.max }) : undefined}
        icon={<ListMusic size={18} />}
        actions={
          data.max > 0 && count > 0 ? (
            <span className="font-mono text-[11px] text-muted tabular-nums">
              {t('playlists.count', { count, max: data.max })}
            </span>
          ) : undefined
        }
        bodyClassName=""
      >
        {count === 0 ? (
          <Empty>{t('playlists.empty')}</Empty>
        ) : (
          <ul className="-mt-px">
            {data.playlists.map((p) => (
              <PlaylistRow key={p.id} p={p} onOpen={() => onOpen(p)} onPin={() => void pin(p)} onRemove={() => void remove(p)} />
            ))}
          </ul>
        )}
      </Card>
    </div>
  )
}

function PlaylistDetail({ initial, onBack }: { initial: SavedPlaylist; onBack: () => void }) {
  const { t, i18n } = useTranslation()
  const { channelId, permissions, run, errorText } = useLynx()
  const toast = useToast()
  const [data, setData] = useState<PlaylistTracks | null>(null)
  const [error, setError] = useState<ApiError | Error | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const [cooldownUntil, setCooldownUntil] = useState(0)
  const [adding, setAdding] = useState<string | null>(null)
  const [, force] = useState(0)
  const alive = useRef(true)
  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  const now = useNow(blockedUntil > Date.now() || cooldownUntil > Date.now())
  const blockedLeft = Math.max(0, Math.ceil((blockedUntil - now) / 1000))
  const coolingDown = cooldownUntil > now

  const load = useCallback(async () => {
    setError(null)
    try {
      const res = await api.playlistTracks(initial.id)
      if (alive.current) setData(res)
    } catch (e) {
      if (alive.current) setError(e instanceof Error ? e : new Error(String(e)))
    }
  }, [initial.id])
  useEffect(() => {
    void load()
  }, [load])

  const refresh = async () => {
    if (refreshing || coolingDown || blockedLeft > 0) return
    setRefreshing(true)
    setCooldownUntil(Date.now() + REFRESH_COOLDOWN_MS)
    try {
      const res = await api.refreshPlaylist(initial.id)
      if (!alive.current) return
      setData(res)
      setError(null)
      toast.push(res.refreshed ? 'success' : 'info', res.refreshed ? t('playlists.refreshed') : t('playlists.refreshTooSoon'))
    } catch (e) {
      if (e instanceof ApiError && e.code === 'refresh_blocked') {
        blockedUntil = Date.now() + (e.retryAfter ?? 300) * 1000
        force((n) => n + 1)
      } else {
        toast.push('error', errorText(e))
      }
    } finally {
      if (alive.current) setRefreshing(false)
    }
  }

  const add = async (key: string, trackUrl?: string) => {
    if (!channelId) return
    setAdding(key)
    const res = await run(() => api.addPlaylist(channelId, initial.id, trackUrl))
    setAdding(null)
    if (res) toast.push('success', t('queue.added', { count: res.added }))
  }

  const p = data?.playlist ?? initial
  const name = p.name || t('playlists.untitled')
  const canAdd = !!channelId && permissions.canAdd
  const tracks = data?.tracks ?? []
  const total = tracks.reduce((sum, x) => sum + (x.durationMs ?? 0), 0)

  return (
    <div className="space-y-5">
      <Button variant="ghost" size="xs" icon={<ArrowLeft size={14} />} onClick={onBack} className="-ml-1">
        {t('playlists.back')}
      </Button>
      <Card
        title={<span className="block truncate">{name}</span>}
        description={
          <span className="flex min-w-0 flex-wrap items-center gap-x-1.5">
            <PlatformLabel platform={p.platform} />
            {data && (
              <>
                <span>·</span>
                <span>{t('playlists.songs', { count: tracks.length, duration: fmtTime(total) })}</span>
                <span>·</span>
                <span>{t('playlists.updatedAgo', { ago: fmtAgo(data.fetchedAt, i18n.language) })}</span>
              </>
            )}
          </span>
        }
        media={<TileArtwork url={p.imageUrl} fallback={<ListMusic size={18} />} />}
        bodyClassName=""
      >
        <div className="flex flex-wrap items-center gap-2 px-4 py-3">
          <Button
            variant="primary"
            size="sm"
            icon={<ListPlus size={14} />}
            loading={adding === 'all'}
            disabled={!canAdd || !data || tracks.length === 0 || adding !== null}
            onClick={() => void add('all')}
          >
            {t('playlists.addAll')}
          </Button>
          <Button
            size="sm"
            icon={<RefreshCw size={14} className={refreshing ? 'animate-spin' : ''} />}
            disabled={refreshing || coolingDown || blockedLeft > 0 || !data}
            onClick={() => void refresh()}
          >
            {t('playlists.refresh')}
          </Button>
        </div>
        {(blockedLeft > 0 || !canAdd) && (
          <div className="space-y-2 px-4 pb-3">
            {blockedLeft > 0 && (
              <Notice tone="amber" icon={<Timer size={15} />}>
                {t('playlists.blocked', { time: fmtTime(blockedLeft * 1000) })}
              </Notice>
            )}
            {!channelId ? (
              <p className="text-xs text-muted">{t('playlists.noChannel')}</p>
            ) : (
              !permissions.canAdd && <p className="text-xs text-muted">{t('playlists.noAdd')}</p>
            )}
          </div>
        )}
        {!data && !error && (
          <div className="flex items-center justify-center gap-2 border-t border-line px-4 py-8 text-sm text-muted">
            <Spinner size={14} /> {t('playlists.loading')}
          </div>
        )}
        {!data && error && (
          <div className="border-t border-line p-4">
            <Notice tone="red">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span>{errorText(error)}</span>
                <Button size="xs" onClick={() => void load()}>
                  {t('common.retry')}
                </Button>
              </div>
            </Notice>
          </div>
        )}
        {data && tracks.length === 0 && <Empty>{t('playlists.noSongs')}</Empty>}
        {data && tracks.length > 0 && (
          <ul>
            {tracks.map((tr, i) => (
              <li
                key={`${i}-${tr.url}`}
                className="group flex items-center gap-3 border-t border-line px-3 py-2 transition-colors hover:bg-surface-2/60"
              >
                <span className="hidden w-5 shrink-0 text-center font-mono text-[11px] text-muted tabular-nums min-[420px]:inline">
                  {i + 1}
                </span>
                <Artwork url={tr.artworkUrl} className="size-9 rounded-lg" iconSize={14} />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-semibold">{tr.title}</div>
                  <div className="mt-0.5 truncate text-xs text-muted">{tr.artists.join(', ')}</div>
                </div>
                <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(tr.durationMs)}</span>
                {canAdd && (
                  <IconButton
                    label={t('playlists.addSong', { title: tr.title })}
                    className={revealCls}
                    disabled={adding !== null}
                    onClick={() => void add(`t${i}`, tr.url)}
                  >
                    {adding === `t${i}` ? <Spinner size={14} /> : <Plus size={16} />}
                  </IconButton>
                )}
              </li>
            ))}
          </ul>
        )}
      </Card>
    </div>
  )
}

/** Saved playlists of the user (any server): open one to see its songs and queue them. */
export function PlaylistsScreen() {
  const [open, setOpen] = useState<SavedPlaylist | null>(null)
  useEffect(() => {
    window.scrollTo({ top: 0 })
  }, [open])
  if (open) return <PlaylistDetail key={open.id} initial={open} onBack={() => setOpen(null)} />
  return <PlaylistList onOpen={setOpen} />
}
