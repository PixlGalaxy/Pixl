// Tiny window events between parts of the panel that do not share state.

const TASTE_CHANGED = 'lynx:taste-changed'
const OPEN_ACCOUNT = 'lynx:open-account'

/** Your history, prefs or votes changed (the taste card reloads). */
export function notifyTasteChanged() {
  window.dispatchEvent(new Event(TASTE_CHANGED))
}
export function onTasteChanged(fn: () => void): () => void {
  window.addEventListener(TASTE_CHANGED, fn)
  return () => window.removeEventListener(TASTE_CHANGED, fn)
}

/** Ask the shell to open the account view. */
export function openAccount() {
  window.dispatchEvent(new Event(OPEN_ACCOUNT))
}
export function onOpenAccount(fn: () => void): () => void {
  window.addEventListener(OPEN_ACCOUNT, fn)
  return () => window.removeEventListener(OPEN_ACCOUNT, fn)
}

const SHOW_IN_QUEUE = 'lynx:show-in-queue'
let pendingShow: string | null = null

/** Open the Queue tab and scroll to the first upcoming song of a list (its source URL). */
export function showInQueue(listUrl: string) {
  pendingShow = listUrl
  window.dispatchEvent(new Event(SHOW_IN_QUEUE))
}
export function onShowInQueue(fn: () => void): () => void {
  window.addEventListener(SHOW_IN_QUEUE, fn)
  return () => window.removeEventListener(SHOW_IN_QUEUE, fn)
}
/** The list waiting to be shown (cleared on read). */
export function takeShowInQueue(): string | null {
  const url = pendingShow
  pendingShow = null
  return url
}
