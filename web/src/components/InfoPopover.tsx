import { CircleHelp } from 'lucide-react'
import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useTranslation } from 'react-i18next'

const WIDTH = 320
const MARGIN = 8
const GAP = 6

/**
 * A small "?" button that opens a help card: what something does plus a concrete example.
 * `k` is an i18n prefix with `body` and (optional) `example` strings; `label` names the subject.
 *
 * The card is `position: fixed` and rendered in `document.body`, so cards with `overflow-hidden`
 * (and transformed dialogs) don't clip or offset it. It opens below the button, flips above near
 * the bottom of the viewport, and closes on outside click, Escape, scroll and resize.
 */
export function InfoPopover({ k, label, className = '' }: { k: string; label: string; className?: string }) {
  const { t, i18n } = useTranslation()
  const anchor = useRef<HTMLButtonElement>(null)
  const card = useRef<HTMLDivElement>(null)
  const [open, setOpen] = useState(false)
  const [pos, setPos] = useState<{ left: number; top: number; width: number } | null>(null)
  const titleId = useId()
  const example = i18n.exists(`${k}.example`) ? t(`${k}.example`) : null

  useLayoutEffect(() => {
    if (!open || !anchor.current) {
      setPos(null)
      return
    }
    const r = anchor.current.getBoundingClientRect()
    const vw = document.documentElement.clientWidth || window.innerWidth
    const vh = window.innerHeight
    const width = Math.min(WIDTH, vw - 2 * MARGIN)
    const height = card.current?.offsetHeight ?? 160
    const left = Math.max(MARGIN, Math.min(r.left + r.width / 2 - width / 2, vw - width - MARGIN))
    const below = r.bottom + GAP
    const above = r.top - GAP - height
    let top = below
    if (below + height > vh - MARGIN) top = above >= MARGIN ? above : Math.max(MARGIN, vh - MARGIN - height)
    setPos({ left, top, width })
  }, [open])

  useEffect(() => {
    if (!open) return
    const close = () => setOpen(false)
    const inside = (target: EventTarget | null) =>
      target instanceof Node && (card.current?.contains(target) || anchor.current?.contains(target))
    const onDown = (e: PointerEvent) => !inside(e.target) && close()
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // Only close the card, not a dialog it sits in.
      e.stopPropagation()
      close()
      anchor.current?.focus()
    }
    const onScroll = (e: Event) => !inside(e.target) && close()
    document.addEventListener('pointerdown', onDown, true)
    window.addEventListener('keydown', onKey, true)
    window.addEventListener('resize', close)
    window.addEventListener('scroll', onScroll, true)
    return () => {
      document.removeEventListener('pointerdown', onDown, true)
      window.removeEventListener('keydown', onKey, true)
      window.removeEventListener('resize', close)
      window.removeEventListener('scroll', onScroll, true)
    }
  }, [open])

  return (
    <>
      <button
        ref={anchor}
        type="button"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={t('info.about', { name: label })}
        title={t('info.about', { name: label })}
        onClick={(e) => {
          e.stopPropagation()
          setOpen((o) => !o)
        }}
        className={`inline-flex size-5 shrink-0 items-center justify-center rounded-full align-middle text-muted/80 transition-colors hover:text-fg focus-visible:outline-2 focus-visible:outline-accent-500 ${
          open ? 'text-accent-600 dark:text-accent-400' : ''
        } ${className}`}
      >
        <CircleHelp size={14} aria-hidden="true" />
      </button>
      {open &&
        createPortal(
          <div
            ref={card}
            role="dialog"
            aria-labelledby={titleId}
            style={{ left: pos?.left ?? -9999, top: pos?.top ?? -9999, width: pos?.width ?? WIDTH }}
            className="fixed z-[60] max-h-[calc(100vh-16px)] animate-[pop-in_.12s_ease-out] overflow-y-auto rounded-xl border border-line bg-surface p-3 text-left font-normal text-fg shadow-xl"
          >
            <div id={titleId} className="text-sm font-semibold">
              {label}
            </div>
            <p className="mt-1 text-xs leading-relaxed text-muted">{t(`${k}.body`)}</p>
            {example && (
              <div className="mt-2.5 rounded-lg border border-line bg-surface-2 px-2.5 py-2">
                <div className="text-[10px] font-semibold tracking-wide text-muted uppercase">{t('info.example')}</div>
                <p className="mt-0.5 text-xs leading-relaxed">{example}</p>
              </div>
            )}
          </div>,
          document.body,
        )}
    </>
  )
}
