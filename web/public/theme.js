// Dark by default; apply a saved light preference before first paint (no inline scripts under Discord's CSP).
;(function () {
  var stored = null
  try {
    stored = localStorage.getItem('lynx:theme')
  } catch (e) {
    // storage unavailable inside some iframes
  }
  document.documentElement.classList.toggle('dark', stored !== 'light')
})()
