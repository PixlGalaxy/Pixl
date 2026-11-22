import { useSyncExternalStore } from 'react'

/**
 * How Discord is showing the Activity. `focused` is the normal panel; `pip` is the small floating
 * window Discord shows while the user is elsewhere in the client; `grid` is a tile next to other
 * participants' streams. Both `pip` and `grid` get the compact view.
 */
export type LayoutMode = 'focused' | 'pip' | 'grid'

/** `ACTIVITY_LAYOUT_MODE_UPDATE.layout_mode`: -1 unhandled, 0 focused, 1 pip, 2 grid. */
export function layoutFromSdk(value: number): LayoutMode | null {
  if (value === 1) return 'pip'
  if (value === 2) return 'grid'
  if (value === 0) return 'focused'
  return null
}

/** Below this the full shell can't fit, whatever the SDK says (also covers clients without the event). */
const COMPACT_MAX_HEIGHT = 260
const COMPACT_MAX_WIDTH = 360

let sdkMode: LayoutMode | null = null
const listeners = new Set<() => void>()

function emit() {
  for (const l of listeners) l()
}

/** Called by the SDK layer when Discord reports a layout change. */
export function setSdkLayout(mode: LayoutMode | null) {
  if (mode === sdkMode) return
  sdkMode = mode
  emit()
}

function viewportIsCompact(): boolean {
  return window.innerHeight < COMPACT_MAX_HEIGHT || window.innerWidth < COMPACT_MAX_WIDTH
}

function snapshot(): LayoutMode {
  if (sdkMode === 'pip' || sdkMode === 'grid') return sdkMode
  return viewportIsCompact() ? 'pip' : 'focused'
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  window.addEventListener('resize', listener)
  return () => {
    listeners.delete(listener)
    window.removeEventListener('resize', listener)
  }
}

/** Current layout: the SDK event when available, else a viewport-size fallback. */
export function useLayoutMode(): LayoutMode {
  return useSyncExternalStore(subscribe, snapshot, () => 'focused')
}

/** True for the compact layouts (PiP and grid tile). */
export function useCompactLayout(): boolean {
  return useLayoutMode() !== 'focused'
}
