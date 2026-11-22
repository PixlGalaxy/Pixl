// Stand-in for @discord/embedded-app-sdk, aliased only by `vite --mode mock` (see vite.config.ts).
// It mirrors the small surface the panel uses so every screen works in a normal browser.

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms))
const params = new URLSearchParams(location.search)

// Layout mode as Discord reports it (0 focused, 1 pip, 2 grid). `?pip=1` / `?grid=1` start in that
// mode; in the browser press `p` to toggle picture-in-picture and `g` to toggle the grid tile.
type LayoutListener = (data: { layout_mode: 0 | 1 | 2 | -1 }) => unknown
let layoutMode: 0 | 1 | 2 = params.get('pip') === '1' ? 1 : params.get('grid') === '1' ? 2 : 0
const layoutListeners = new Set<LayoutListener>()
function setLayout(mode: 0 | 1 | 2) {
  layoutMode = mode
  for (const l of layoutListeners) l({ layout_mode: mode })
}
window.addEventListener('keydown', (e) => {
  const el = e.target as HTMLElement | null
  if (e.ctrlKey || e.metaKey || e.altKey || el?.closest('input, textarea, select, [contenteditable]')) return
  if (e.key === 'p') setLayout(layoutMode === 1 ? 0 : 1)
  else if (e.key === 'g') setLayout(layoutMode === 2 ? 0 : 2)
})

export class DiscordSDK {
  readonly clientId: string
  readonly instanceId = params.get('instance_id') ?? 'mock-instance'
  readonly guildId: string | null = params.get('guild_id')
  readonly channelId: string | null = params.get('channel_id')

  constructor(clientId: string) {
    this.clientId = clientId
  }

  async ready() {
    await wait(250)
  }

  async subscribe(event: string, listener: LayoutListener) {
    if (event === 'ACTIVITY_LAYOUT_MODE_UPDATE') {
      layoutListeners.add(listener)
      // Discord dispatches the current mode right after subscribing.
      setTimeout(() => listener({ layout_mode: layoutMode }), 0)
    }
    return this
  }

  async unsubscribe(event: string, listener: LayoutListener) {
    if (event === 'ACTIVITY_LAYOUT_MODE_UPDATE') layoutListeners.delete(listener)
    return this
  }

  commands = {
    setConfig: async (args: { use_interactive_pip: boolean }) => ({ use_interactive_pip: args.use_interactive_pip }),
    authorize: async (_args: { client_id: string; scope: string[] }) => {
      await wait(350)
      if (params.get('fail') === 'authorize') throw { code: 5000, message: 'OAuth2 Error: invalid_request (mock)' }
      return { code: 'mock-code' }
    },
    authenticate: async (args: { access_token: string }) => {
      await wait(150)
      return { access_token: args.access_token, user: { id: '1001', username: 'pixl' }, scopes: ['identify'] }
    },
    openExternalLink: async ({ url }: { url: string }) => {
      window.open(url, '_blank', 'noopener')
      return { opened: true }
    },
  }
}

export function patchUrlMappings() {}
