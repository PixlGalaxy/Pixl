import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { useToast } from '../components/Toasts'
import { ApiError, api } from './api'
import type { Connection } from './discord'
import { useLivePlayer, type PositionAnchor } from './live'
import type { GuildSettings, Permissions, PlayerState, PublicConfig, User } from './types'

type LynxContext = {
  config: PublicConfig
  user: User
  guildName: string | null
  channelId: string | null
  aiEnabled: boolean
  permissions: Permissions
  settings: GuildSettings | null
  setSettings: (s: GuildSettings) => void
  state: PlayerState | null
  setState: (fn: (s: PlayerState | null) => PlayerState | null) => void
  anchor: PositionAnchor
  setAnchor: (a: PositionAnchor) => void
  loaded: boolean
  live: boolean
  refresh: () => Promise<void>
  /** Runs an action, toasts a translated error, refreshes when the socket is down. Returns null on failure. */
  run: <T>(fn: () => Promise<T>, success?: string) => Promise<T | null>
  /** Translated message for an API error code. */
  errorText: (error: unknown) => string
}

const Ctx = createContext<LynxContext | null>(null)

export function useLynx(): LynxContext {
  const ctx = useContext(Ctx)
  if (!ctx) throw new Error('useLynx outside LynxProvider')
  return ctx
}

export function LynxProvider({ conn, children }: { conn: Connection; children: ReactNode }) {
  const { t } = useTranslation()
  const toast = useToast()
  const [permissions, setPermissions] = useState<Permissions>(conn.auth.permissions)
  const [settings, setSettings] = useState<GuildSettings | null>(null)

  const live = useLivePlayer(conn.channelId, { onSettings: setSettings, onPermissions: setPermissions })

  useEffect(() => {
    if (!conn.auth.guild) return
    api.settings().then(setSettings, (e) => console.warn('[lynx] settings', e))
  }, [conn.auth.guild])

  const errorText = useCallback(
    (error: unknown) => {
      if (error instanceof ApiError) {
        const key = `errors.${error.code}`
        const text = t(key, { defaultValue: '' })
        if (text) return text
        return error.message || t('errors.generic')
      }
      return t('errors.generic')
    },
    [t],
  )

  const { refresh, socketOpen } = live
  const run = useCallback(
    async <T,>(fn: () => Promise<T>, success?: string): Promise<T | null> => {
      try {
        const result = await fn()
        if (success) toast.push('success', success)
        return result
      } catch (error) {
        console.warn('[lynx] action failed', error)
        toast.push('error', errorText(error))
        return null
      } finally {
        if (!socketOpen) void refresh()
      }
    },
    [toast, errorText, socketOpen, refresh],
  )

  const value = useMemo<LynxContext>(
    () => ({
      config: conn.config,
      user: conn.auth.user,
      guildName: conn.auth.guild?.name ?? null,
      channelId: conn.channelId,
      aiEnabled: conn.config.features?.ai === true,
      permissions,
      settings,
      setSettings,
      state: live.state,
      setState: live.setState,
      anchor: live.anchor,
      setAnchor: live.setAnchor,
      loaded: live.loaded,
      live: live.socketOpen,
      refresh: live.refresh,
      run,
      errorText,
    }),
    [conn, permissions, settings, live, run, errorText],
  )

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>
}
