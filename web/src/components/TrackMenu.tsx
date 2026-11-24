import { ExternalLink, ListPlus } from 'lucide-react'
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { openExternal } from '../lib/discord'
import { useLynx } from '../lib/lynx'
import { Spinner } from './ui'

/** Where a track link opens, for the "Open on ..." item. */
export function platformOf(url: string): string | null {
  let host: string
  try {
    host = new URL(url).hostname.replace(/^www\./, '')
  } catch {
    return null
  }
  if (host === 'music.youtube.com') return 'YouTube Music'
  if (host === 'youtube.com' || host === 'youtu.be' || host === 'm.youtube.com') return 'YouTube'
  if (host.endsWith('spotify.com')) return 'Spotify'
  if (host === 'music.apple.com') return 'Apple Music'
  if (host.endsWith('soundcloud.com')) return 'SoundCloud'
  return null
}

const MENU_WIDTH = 224

/** Class of one item of a [`FloatingMenu`]. */
export const menuItemCls =
  'flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-sm hover:bg-surface-2 focus-visible:bg-surface-2 focus-visible:outline-none disabled:opacity-50'

/**
 * A small menu below (or above, near the bottom) an anchor element. `position: fixed`, so cards
 * with `overflow-hidden` don't clip it (render it outside transformed elements). Closes on outside click, Escape (focus goes back to the
 * anchor), scroll and resize.
 */
export function FloatingMenu({
  anchor,
  label,
  onClose,
  width = MENU_WIDTH,
  children,
}: {
  anchor: RefObject<HTMLElement | null>
  label: string
  onClose: () => void
  width?: number
  children: ReactNode
}) {
  const menu = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const closeRef = useRef(onClose)
  closeRef.current = onClose

  useLayoutEffect(() => {
    if (!anchor.current) return
    const r = anchor.current.getBoundingClientRect()
    const height = menu.current?.offsetHeight ?? 96
    const left = Math.max(8, Math.min(r.left, window.innerWidth - width - 8))
    const below = r.bottom + 4
    const top = below + height > window.innerHeight - 8 ? Math.max(8, r.top - height - 4) : below
    setPos({ left, top })
  }, [anchor, width])

  useEffect(() => {
    const close = () => closeRef.current()
    const onDown = (e: PointerEvent) => {
      const target = e.target as Node
      if (!menu.current?.contains(target) && !anchor.current?.contains(target)) close()
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        close()
        anchor.current?.focus()
      }
    }
    document.addEventListener('pointerdown', onDown)
    document.addEventListener('keydown', onKey)
    window.addEventListener('resize', close)
    window.addEventListener('scroll', close, true)
    return () => {
      document.removeEventListener('pointerdown', onDown)
      document.removeEventListener('keydown', onKey)
      window.removeEventListener('resize', close)
      window.removeEventListener('scroll', close, true)
    }
  }, [anchor])

  return (
    <div
      ref={menu}
      role="menu"
      aria-label={label}
      style={{ left: pos?.left ?? -9999, top: pos?.top ?? -9999, width }}
      className="fixed z-50 animate-[pop-in_.12s_ease-out] rounded-xl border border-line bg-surface p-1 shadow-xl"
    >
      {children}
    </div>
  )
}

/**
 * A song title that opens a small menu on click: add it to the queue (when allowed) or open it
 * on its platform.
 */
export function TrackMenu({ url, title, children }: { url: string; title: string; children: ReactNode }) {
  const { t } = useTranslation()
  const { channelId, channelPermissions: permissions, run } = useLynx()
  const anchor = useRef<HTMLButtonElement>(null)
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const canAdd = !!channelId && permissions.canAdd
  const platform = platformOf(url)
  const item = menuItemCls

  return (
    <>
      <button
        ref={anchor}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        className="min-w-0 flex-1 text-left hover:text-accent-600 dark:hover:text-accent-400"
      >
        {children}
      </button>
      {open && (
        <FloatingMenu anchor={anchor} label={title} onClose={() => setOpen(false)}>
          <button
            type="button"
            role="menuitem"
            className={item}
            disabled={!canAdd || busy}
            title={canAdd ? undefined : t('trackMenu.noAdd')}
            onClick={async () => {
              if (!channelId) return
              setBusy(true)
              await run(() => api.add(channelId, url), t('queue.added', { count: 1 }))
              setBusy(false)
              setOpen(false)
            }}
          >
            {busy ? <Spinner size={15} /> : <ListPlus size={15} className="shrink-0 text-muted" />}
            {t('trackMenu.addToQueue')}
          </button>
          <button
            type="button"
            role="menuitem"
            className={item}
            onClick={() => {
              setOpen(false)
              openExternal(url)
            }}
          >
            <ExternalLink size={15} className="shrink-0 text-muted" />
            {platform ? t('trackMenu.openOn', { platform }) : t('trackMenu.openLink')}
          </button>
        </FloatingMenu>
      )}
    </>
  )
}
