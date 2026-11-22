import { DiscordSDK } from '@discord/embedded-app-sdk'
import { api, setSession } from './api'
import { errorMessage } from './format'
import type { AuthResult, PublicConfig } from './types'

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

export type Connection = { config: PublicConfig; auth: AuthResult; channelId: string | null; guildId: string | null }

/** SDK handshake + OAuth2 login + Lynx session. Call once (the OAuth code is single-use). */
export async function connect(onStep: (s: LoginStep) => void): Promise<Connection> {
  const config = await step('config', onStep, () => api.publicConfig())
  const client = new DiscordSDK(config.clientId)
  await step('ready', onStep, () => client.ready())

  const { code } = await step('authorize', onStep, () =>
    client.commands.authorize({
      client_id: config.clientId,
      response_type: 'code',
      state: '',
      prompt: 'none',
      scope: ['identify'],
    }),
  )

  const auth = await step('signin', onStep, () => api.auth(code, client.guildId, client.channelId))
  await step('authenticate', onStep, () => client.commands.authenticate({ access_token: auth.accessToken }))
  setSession(auth.session)
  sdk = client
  return { config, auth, channelId: client.channelId, guildId: client.guildId }
}

/** External links must go through the client (no new windows inside the iframe). */
export async function openExternal(url: string): Promise<void> {
  if (!sdk) return
  try {
    await sdk.commands.openExternalLink({ url })
  } catch (error) {
    console.warn('[lynx] openExternalLink failed', error)
  }
}
