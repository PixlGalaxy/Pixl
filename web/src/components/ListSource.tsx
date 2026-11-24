import { BookmarkPlus, ExternalLink, ListMusic, ListOrdered } from 'lucide-react'
import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { openExternal } from '../lib/discord'
import { showInQueue } from '../lib/events'
import { cx, mediaUrl } from '../lib/format'
import { useLynx } from '../lib/lynx'
import type { PlayerState, SourcePlatform, Track, TrackSource } from '../lib/types'
import { FloatingMenu, menuItemCls } from './TrackMenu'
import { Card, Spinner } from './ui'
import './ListSource.css'

/** Brand-ish color per platform: the dot next to the platform name and covers without an image. */
export const PLATFORM_COLOR: Record<SourcePlatform, string> = {
  spotify: '#1db954',
  youtube: '#ff0033',
  youtube_music: '#ff4e45',
  apple_music: '#fa2d48',
  soundcloud: '#ff5500',
  other: '#71717a',
}

/** Platforms whose lists the saved playlists accept. */
const SAVABLE: SourcePlatform[] = ['spotify', 'youtube', 'youtube_music', 'apple_music']

function usePlatformLabel() {
  const { t } = useTranslation()
  return (p: SourcePlatform) => t(`source.platforms.${p}`)
}

function listName(t: (k: string) => string, s: TrackSource) {
  return s.name.trim() || t('source.untitled')
}

/** A list cover: its image, or a tile in the platform color. */
function Cover({ source, className, style }: { source: TrackSource; className?: string; style?: CSSProperties }) {
  const [broken, setBroken] = useState(false)
  const src = mediaUrl(source.imageUrl)
  if (!src || broken) {
    return (
      <span
        aria-hidden
        className={cx('flex shrink-0 items-center justify-center text-white/90', className)}
        style={{ background: PLATFORM_COLOR[source.platform], ...style }}
      >
        <ListMusic className="size-1/3 max-h-8 max-w-8" />
      </span>
    )
  }
  return (
    <img
      src={src}
      alt=""
      loading="lazy"
      draggable={false}
      onError={() => setBroken(true)}
      className={cx('shrink-0 bg-surface-2 object-cover', className)}
      style={style}
    />
  )
}

/** Tiny rounded square after the artist of a queue row: the list the song came from. */
export function SourceDot({ source }: { source: TrackSource | null | undefined }) {
  const { t } = useTranslation()
  if (!source) return null
  const title = t('source.fromList', { name: listName(t, source) })
  return (
    <span title={title} aria-label={title} className="mr-1.5 inline-flex size-3 shrink-0 align-[-1px]">
      <Cover source={source} className="size-3 rounded-[3px]" />
    </span>
  )
}

/** "Spotify · 18/64" with the platform dot. */
function PlatformLine({ source, className }: { source: TrackSource; className?: string }) {
  const { t } = useTranslation()
  const label = usePlatformLabel()
  return (
    <span className={cx('inline-flex min-w-0 items-center gap-1.5 text-[11px] text-muted', className)}>
      <span className="size-1.5 shrink-0 rounded-full" style={{ background: PLATFORM_COLOR[source.platform] }} />
      <span className="truncate">
        {label(source.platform)}
        {source.total > 0 && (
          <>
            {' · '}
            <span className="font-mono tabular-nums">{t('source.position', { index: source.index, total: source.total })}</span>
          </>
        )}
      </span>
    </span>
  )
}

/** Open on the platform / save to my playlists / show in queue. */
function SourceMenu({
  source,
  anchor,
  onClose,
}: {
  source: TrackSource
  anchor: React.RefObject<HTMLElement | null>
  onClose: () => void
}) {
  const { t } = useTranslation()
  const { run, state } = useLynx()
  const label = usePlatformLabel()
  const [saving, setSaving] = useState(false)
  const savable = SAVABLE.includes(source.platform)
  const upcoming = (state?.upcoming ?? []).some((x) => x.source?.url === source.url)
  return (
    <FloatingMenu anchor={anchor} label={t('source.menuLabel', { name: listName(t, source) })} onClose={onClose} width={240}>
      <button
        type="button"
        role="menuitem"
        className={menuItemCls}
        onClick={() => {
          onClose()
          void openExternal(source.url)
        }}
      >
        <ExternalLink size={15} className="shrink-0 text-muted" />
        {source.platform === 'other' ? t('source.openLink') : t('source.openOn', { platform: label(source.platform) })}
      </button>
      <button
        type="button"
        role="menuitem"
        className={menuItemCls}
        disabled={!savable || saving}
        title={savable ? undefined : t('source.cantSave')}
        onClick={async () => {
          setSaving(true)
          await run(() => api.savePlaylist(source.url), t('source.saved'))
          setSaving(false)
          onClose()
        }}
      >
        {saving ? <Spinner size={15} /> : <BookmarkPlus size={15} className="shrink-0 text-muted" />}
        {t('source.save')}
      </button>
      <button
        type="button"
        role="menuitem"
        className={menuItemCls}
        disabled={!upcoming}
        title={upcoming ? undefined : t('source.notUpcoming')}
        onClick={() => {
          onClose()
          showInQueue(source.url)
        }}
      >
        <ListOrdered size={15} className="shrink-0 text-muted" />
        {t('source.showInQueue')}
      </button>
    </FloatingMenu>
  )
}

/**
 * "Playing from" block of the Now playing card: the list cover, name and position. Large beside
 * the track info on wide cards, a small row under the title on narrow ones (container query).
 */
export function NowPlayingSource({ source, variant }: { source: TrackSource | null | undefined; variant: 'large' | 'compact' }) {
  const { t } = useTranslation()
  const anchor = useRef<HTMLButtonElement>(null)
  const [open, setOpen] = useState(false)
  if (!source) return null
  const name = listName(t, source)
  return (
    <>
      {variant === 'large' ? (
        <button
          ref={anchor}
          type="button"
          aria-haspopup="menu"
          aria-expanded={open}
          onClick={() => setOpen((o) => !o)}
          className="group hidden w-32 shrink-0 flex-col items-start gap-1.5 rounded-xl p-1.5 text-left transition-colors hover:bg-surface-2 focus-visible:outline-2 focus-visible:outline-accent-500 @md:flex"
        >
          <span className="text-[10px] font-semibold tracking-wider text-muted uppercase">{t('source.playingFrom')}</span>
          <Cover source={source} className="size-24 rounded-lg border border-line shadow-sm" />
          <span className="line-clamp-2 w-full text-xs leading-snug font-semibold break-words group-hover:text-accent-600 dark:group-hover:text-accent-400">
            {name}
          </span>
          <PlatformLine source={source} className="w-full" />
        </button>
      ) : (
        <button
          ref={anchor}
          type="button"
          aria-haspopup="menu"
          aria-expanded={open}
          onClick={() => setOpen((o) => !o)}
          className="group flex max-w-full items-center gap-2 rounded-lg border border-line bg-surface-2/50 py-1 pr-2.5 pl-1 text-left transition-colors hover:bg-surface-2 focus-visible:outline-2 focus-visible:outline-accent-500 @md:hidden"
        >
          <Cover source={source} className="size-8 rounded-md" />
          <span className="min-w-0">
            <span className="block truncate text-xs font-semibold group-hover:text-accent-600 dark:group-hover:text-accent-400">
              <span className="font-normal text-muted">{t('source.playingFrom')} </span>
              {name}
            </span>
            <PlatformLine source={source} className="flex" />
          </span>
        </button>
      )}
      {open && <SourceMenu source={source} anchor={anchor} onClose={() => setOpen(false)} />}
    </>
  )
}

type Group = { source: TrackSource; first: Track; current: boolean }

/** Distinct lists of the current track and the upcoming ones, in queue order (current first). */
export function queueSources(state: PlayerState | null): Group[] {
  if (!state) return []
  const out = new Map<string, Group>()
  for (const tr of [state.current, ...state.upcoming]) {
    if (!tr?.source || out.has(tr.source.url)) continue
    out.set(tr.source.url, { source: tr.source, first: tr, current: tr === state.current })
  }
  return [...out.values()]
}

const CD = 150
const MAX_SHOWN = 5
const STEP = 34
const SPREAD = 128
const SCALES = [1, 0.92, 0.84, 0.76, 0.68]
const OPACITY = [1, 0.85, 0.7, 0.55, 0.4]
/** Width kept for the text beside the stack when the card is wide. */
const TEXT_MIN = 220
const WIDE = 560

/** Queue tab: the lists in the queue as stacked CDs, the current song's list in front. */
export function QueueSourcesCard() {
  const { t } = useTranslation()
  const { state } = useLynx()
  const label = usePlatformLabel()
  const groups = queueSources(state)
  const wrap = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(0)
  const anchor = useRef<HTMLElement | null>(null)
  const [menu, setMenu] = useState<string | null>(null)

  useLayoutEffect(() => {
    const el = wrap.current
    if (!el) return
    setWidth(el.clientWidth)
    const ro = new ResizeObserver(() => setWidth(el.clientWidth))
    ro.observe(el)
    return () => ro.disconnect()
  }, [groups.length > 0])

  // The menu's list left the queue.
  const menuGroup = groups.find((g) => g.source.url === menu) ?? null
  useEffect(() => {
    if (menu && !menuGroup) setMenu(null)
  }, [menu, menuGroup])

  if (groups.length === 0) return null

  const shown = groups.slice(0, MAX_SHOWN)
  const extra = groups.length - shown.length
  const front = shown[0]!
  const behind = groups.slice(1)
  const wide = width >= WIDE
  const n = shown.length
  const room = Math.max(0, (wide ? width - TEXT_MIN - 24 : width) - CD - (extra > 0 ? 40 : 0))
  const spread = n > 1 ? Math.max(STEP, Math.min(SPREAD, room / (n - 1))) : 0
  const slot = new Map(shown.map((g, i) => [g.source.url, i]))
  // Stable DOM order (by URL): a list changing slots keeps its element, so it slides there.
  const ordered = [...shown].sort((a, b) => (a.source.url < b.source.url ? -1 : 1))
  const stackStyle = {
    '--w': `${CD + STEP * (n - 1)}px`,
    '--wh': `${CD + spread * (n - 1)}px`,
  } as CSSProperties

  return (
    <Card
      title={t('source.cardTitle')}
      description={t('source.cardHint', { count: groups.length })}
      icon={<ListMusic size={18} />}
      help="info.queue.sources"
    >
      <div ref={wrap} className={cx('flex gap-x-6 gap-y-4', wide ? 'flex-row items-center' : 'flex-col')}>
        <div className="flex shrink-0 items-center gap-2">
          <div className="cd-stack" style={stackStyle}>
            {ordered.map((g) => {
              const i = slot.get(g.source.url)!
              const isFront = i === 0
              const style = {
                '--x': `${i * STEP}px`,
                '--xh': `${i * spread}px`,
                '--s': SCALES[i],
                '--o': OPACITY[i],
                zIndex: MAX_SHOWN - i,
              } as CSSProperties
              return (
                <button
                  key={g.source.url}
                  type="button"
                  className={cx(
                    'cd focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-500',
                    isFront && g.current && 'cd-front',
                  )}
                  style={style}
                  aria-haspopup="menu"
                  aria-expanded={menu === g.source.url}
                  aria-label={listName(t, g.source)}
                  title={listName(t, g.source)}
                  onClick={(e) => {
                    anchor.current = e.currentTarget
                    setMenu((m) => (m === g.source.url ? null : g.source.url))
                  }}
                >
                  <Cover source={g.source} className="size-full" />
                </button>
              )
            })}
          </div>
          {extra > 0 && (
            <span className="rounded-full bg-surface-2 px-2 py-0.5 font-mono text-[11px] text-muted tabular-nums">
              {t('source.more', { count: extra })}
            </span>
          )}
        </div>
        <div className="min-w-0 flex-1 space-y-1.5">
          <span
            className={cx(
              'inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium',
              front.current ? 'bg-accent-500/10 text-accent-700 dark:text-accent-300' : 'bg-surface-2 text-muted',
            )}
          >
            {front.current && <span className="size-1.5 animate-pulse rounded-full bg-accent-500" />}
            {front.current ? t('source.nowPlaying') : t('source.upNext')}
            {front.source.total > 0 && (
              <span className="font-mono text-[11px] tabular-nums opacity-80">
                {t('source.position', { index: front.source.index, total: front.source.total })}
              </span>
            )}
          </span>
          <div className="line-clamp-2 text-base font-semibold break-words">{listName(t, front.source)}</div>
          <div className="flex items-center gap-1.5 text-xs text-muted">
            <span className="size-1.5 shrink-0 rounded-full" style={{ background: PLATFORM_COLOR[front.source.platform] }} />
            <span className="truncate">
              {t('source.addedBy', { platform: label(front.source.platform), name: front.first.requestedBy.name })}
            </span>
          </div>
          {behind.length > 0 && (
            <div className="line-clamp-2 text-xs text-muted">
              {t('source.behind', { names: behind.map((g) => listName(t, g.source)).join(', ') })}
            </div>
          )}
        </div>
      </div>
      {menuGroup && (
        <SourceMenu key={menuGroup.source.url} source={menuGroup.source} anchor={anchor} onClose={() => setMenu(null)} />
      )}
    </Card>
  )
}
