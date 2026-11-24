import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { useToast } from '../components/Toasts'
import { ApiError, api } from './api'
import type { Connection } from './discord'
import { useLivePlayer, type PositionAnchor } from './live'
import type { GuildSettings, NoticesInfo, Permissions, PlayerState, PublicConfig, User } from './types'

/** Notices are also polled (the socket may be down; a reopened panel catches up on focus). */
const NOTICES_POLL_MS = 5 * 60_000

type LynxContext = {
  config: PublicConfig
  user: User
  guildName: string | null
  /** Voice channel the user is in right now (the panel always follows it); null when not in voice. */
  channelId: string | null
  channelName: string | null
  aiEnabled: boolean
  /**
   * Permissions for the shared queue: the channel's own ones, minus editing when the channel is in
   * a jam that only the host channel edits.
   */
  permissions: Permissions
  /** The channel's own permissions (volume and stop stay per channel in a jam). */
  channelPermissions: Permissions
  /** Jam opened from a link (join prompt), until the user dismisses it or joins. */
  pendingJamId: string | null
  clearPendingJam: () => void
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
  /** Operator notices (scheduled downtime...) and maintenance mode. */
  notices: NoticesInfo
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
  const [pendingJamId, setPendingJamId] = useState<string | null>(conn.jamId)
  const clearPendingJam = useCallback(() => setPendingJamId(null), [])

  const [notices, setNotices] = useState<NoticesInfo>(() => ({
    notices: conn.config.notices ?? [],
    maintenance: conn.config.maintenance === true,
  }))

  const live = useLivePlayer(conn.channel, {
    onSettings: setSettings,
    onPermissions: setPermissions,
    onNotices: setNotices,
  })

  useEffect(() => {
    const load = () => api.notices().then(setNotices, (e) => console.warn('[lynx] notices', e))
    const id = setInterval(() => document.visibilityState === 'visible' && void load(), NOTICES_POLL_MS)
    const onVisible = () => document.visibilityState === 'visible' && void load()
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      clearInterval(id)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [])

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

  const jamLocked = live.state?.jam?.you.member === true && !live.state.jam.you.canEdit
  const shared = useMemo<Permissions>(
    () => (jamLocked ? { ...permissions, canControl: false, canAdd: false } : permissions),
    [jamLocked, permissions],
  )

  const value = useMemo<LynxContext>(
    () => ({
      config: conn.config,
      user: conn.auth.user,
      guildName: conn.auth.guild?.name ?? null,
      channelId: live.channel?.id ?? null,
      channelName: live.channel?.name ?? null,
      aiEnabled: conn.config.features?.ai === true,
      permissions: shared,
      channelPermissions: permissions,
      pendingJamId,
      clearPendingJam,
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
      notices,
    }),
    [conn, shared, permissions, pendingJamId, clearPendingJam, settings, live, run, errorText, notices],
  )

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>
}
