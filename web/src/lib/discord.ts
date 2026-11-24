import { DiscordSDK } from '@discord/embedded-app-sdk'
import { api, setSession } from './api'
import { errorMessage } from './format'
import { layoutFromSdk, setInteractivePip, setSdkLayout } from './layout'
import type { AuthResult, ChannelRef, PublicConfig } from './types'

/** Discord always loads the Activity with `frame_id` (plus instance_id, guild_id, channel_id). */
export function isInsideDiscord(): boolean {
  return new URLSearchParams(location.search).has('frame_id')
}

export type LoginStep = 'config' | 'ready' | 'authorize' | 'signin' | 'authenticate'
export const LOGIN_STEPS: LoginStep[] = ['config', 'ready', 'authorize', 'signin', 'authenticate']

export class LoginError extends Error {
  readonly step: LoginStep
  constructor(step: LoginStep, message: string) {
    super(message)
    this.step = step
  }
}

let sdk: DiscordSDK | null = null
let clientId: string | null = null

/** Runs one login step and labels its error, so the screen says where it failed. */
async function step<T>(name: LoginStep, onStep: (s: LoginStep) => void, fn: () => Promise<T>): Promise<T> {
  onStep(name)
  try {
    return await fn()
  } catch (error) {
    console.error(`[lynx] login step ${name} failed`, error)
    throw new LoginError(name, errorMessage(error))
  }
}

/**
 * `channel` is the voice channel the panel follows (the user's current one), not the channel the
 * Activity was launched in; `launchChannelId` is that one, kept for display and re-authentication.
 */
export type Connection = {
  config: PublicConfig
  auth: AuthResult
  channel: ChannelRef | null
  launchChannelId: string | null
  guildId: string | null
  /** Jam id when the Activity was opened from a jam link (`custom_id` = `jam-<id>`). */
  jamId: string | null
}

/** Jam links carry `custom_id` = `jam-<id>` (see the backend's `player::jam::CUSTOM_ID_PREFIX`). */
export function jamIdFromCustomId(customId: string | null | undefined): string | null {
  const m = /^jam-([a-z0-9]{10,16})$/.exec((customId ?? '').trim())
  return m?.[1] ?? null
}

async function authorizeCode(client: DiscordSDK, id: string): Promise<string> {
  const { code } = await client.commands.authorize({
    client_id: id,
    response_type: 'code',
    state: '',
    prompt: 'none',
    scope: ['identify'],
  })
  return code
}

/** SDK handshake + OAuth2 login + Lynx session. Call once (the OAuth code is single-use). */
export async function connect(onStep: (s: LoginStep) => void): Promise<Connection> {
  const config = await step('config', onStep, () => api.publicConfig())
  const client = new DiscordSDK(config.clientId)
  // Discord never answers the handshake of a reloaded iframe: fail with a clear message instead of
  // spinning forever (the user has to close and reopen the Activity).
  await step('ready', onStep, () =>
    Promise.race([
      client.ready(),
      new Promise<never>((_, reject) =>
        setTimeout(
          () => reject(new Error('Discord did not answer. Close the Activity and open it again.')),
          10_000,
        ),
      ),
    ]),
  )
  void watchLayout(client)

  const code = await step('authorize', onStep, () => authorizeCode(client, config.clientId))

  const auth = await step('signin', onStep, () => api.auth(code, client.guildId, client.channelId))
  await step('authenticate', onStep, () => client.commands.authenticate({ access_token: auth.accessToken }))
  setSession(auth.session)
  sdk = client
  clientId = config.clientId
  return {
    config,
    auth,
    channel: auth.channel ?? null,
    launchChannelId: client.channelId,
    guildId: client.guildId,
    jamId: jamIdFromCustomId(client.customId ?? new URLSearchParams(location.search).get('custom_id')),
  }
}

/**
 * Discord's share sheet for a jam link: the link launches the Activity with the jam's `custom_id`
 * (wherever the opener is; the panel then offers to join from their voice channel). Resolves false
 * when the user closed it or the client does not support it.
 */
export async function shareJamLink(jamId: string, message: string): Promise<boolean> {
  if (!sdk) return false
  try {
    const res = await sdk.commands.shareLink({ message, custom_id: `jam-${jamId}` })
    return res.success
  } catch (error) {
    console.warn('[lynx] shareLink failed', error)
    return false
  }
}

/**
 * Silent sign-in after the server refused the session (restart with an old build, expiry): a fresh
 * OAuth code with prompt none, a new Lynx session. No UI; false when Discord or the backend refuses.
 */
export async function reauthenticate(): Promise<boolean> {
  if (!sdk || !clientId) return false
  const client = sdk
  const code = await authorizeCode(client, clientId)
  const auth = await api.auth(code, client.guildId, client.channelId)
  setSession(auth.session)
  try {
    await client.commands.authenticate({ access_token: auth.accessToken })
  } catch (error) {
    console.warn('[lynx] authenticate after silent sign-in failed', error)
  }
  console.info('[lynx] session renewed')
  return true
}

/**
 * Follows Discord's layout (focused / picture-in-picture / grid) and asks for interactive PiP so the
 * compact view's buttons receive clicks. Best effort: older clients may reject either call, and the
 * layout hook falls back to the viewport size.
 */
async function watchLayout(client: DiscordSDK): Promise<void> {
  try {
    const res = await client.commands.setConfig({ use_interactive_pip: true })
    setInteractivePip(res?.use_interactive_pip !== false)
  } catch (error) {
    console.warn('[lynx] setConfig(use_interactive_pip) not supported', error)
  }
  try {
    await client.subscribe('ACTIVITY_LAYOUT_MODE_UPDATE', ({ layout_mode }) => setSdkLayout(layoutFromSdk(layout_mode)))
  } catch (error) {
    console.warn('[lynx] ACTIVITY_LAYOUT_MODE_UPDATE subscribe failed', error)
  }
}

/** External links must go through the client (no new windows inside the iframe). */
export async function openExternal(url: string): Promise<void> {
  if (!sdk) {
    // The SDK never connected (gate error screens): a plain new tab is the best we can do.
    window.open(url, '_blank', 'noopener')
    return
  }
  try {
    await sdk.commands.openExternalLink({ url })
  } catch (error) {
    console.warn('[lynx] openExternalLink failed', error)
  }
}
