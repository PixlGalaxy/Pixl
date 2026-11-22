// First paint before React loads (no inline scripts under Discord's CSP): theme mode and accent from
// the local cache of the user's prefs (src/lib/appearance.ts). Dark by default.
;(function () {
  var theme = null
  var accent = null
  try {
    theme = localStorage.getItem('lynx:theme')
    accent = localStorage.getItem('lynx:accent')
  } catch (e) {
    // storage unavailable inside some iframes
  }
  var dark = theme !== 'light'
  if (theme === 'system' && window.matchMedia) dark = window.matchMedia('(prefers-color-scheme: dark)').matches
  document.documentElement.classList.toggle('dark', dark)
  if (accent && accent !== 'sky' && /^[a-z]+$/.test(accent)) document.documentElement.setAttribute('data-accent', accent)
})()
