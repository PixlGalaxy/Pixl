// Stand-in for @discord/embedded-app-sdk, aliased only by `vite --mode mock` (see vite.config.ts).
// It mirrors the small surface the panel uses so every screen works in a normal browser.

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms))
const params = new URLSearchParams(location.search)

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

  commands = {
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
