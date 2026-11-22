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
