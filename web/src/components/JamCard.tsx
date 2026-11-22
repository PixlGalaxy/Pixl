import { Crown, LogOut, Plus, Share2, UserMinus, Users } from 'lucide-react'
import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { ApiError, api } from '../lib/api'
import { openExternal, shareJamLink } from '../lib/discord'
import { cx } from '../lib/format'
import { useLynx } from '../lib/lynx'
import type { Jam, JamMember, JamPreview, JamSettings } from '../lib/types'
import { Button, Card, ConfirmButton, IconButton, Notice, Segmented, SectionLabel, SettingRow, Spinner, Tag, Toggle } from './ui'

function memberKey(m: JamMember) {
  return `${m.guildId}:${m.channelId}`
}

function MemberRow({ jam, member, onKick }: { jam: Jam; member: JamMember; onKick?: () => void }) {
  const { t } = useTranslation()
  const otherServer = member.guildId !== jam.hostGuildId
  return (
    <li className="flex items-center gap-3 border-t border-line px-4 py-2.5 first:border-t-0">
      <span
        className={cx('size-2 shrink-0 rounded-full', member.listening ? 'bg-emerald-500' : 'bg-zinc-400')}
        title={member.listening ? t('jam.listening') : t('jam.empty')}
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-semibold">{member.name || t('jam.unnamedChannel')}</div>
        {(otherServer || member.guildName) && (
          <div className="truncate text-xs text-muted">{member.guildName}</div>
        )}
      </div>
      {member.isHost && (
        <Tag tone="amber">
          <Crown size={10} />
          {t('jam.host')}
        </Tag>
      )}
      {member.you && <Tag tone="sky">{t('jam.you')}</Tag>}
      {onKick && (
        <IconButton label={t('jam.kick', { name: member.name })} onClick={onKick}>
          <UserMinus size={15} />
        </IconButton>
      )}
    </li>
  )
}

function HostSettings({ jam }: { jam: Jam }) {
  const { t } = useTranslation()
  const { channelId, run } = useLynx()
  const [draft, setDraft] = useState<JamSettings>(jam.settings)
  useEffect(() => setDraft(jam.settings), [jam.settings])
  const can = jam.you.canManage && !!channelId
  const set = (patch: Partial<JamSettings>) => {
    if (!channelId) return
    setDraft((d) => ({ ...d, ...patch }))
    void run(() => api.jamSettings(channelId, patch))
  }
  return (
    <div className="space-y-3">
      <SectionLabel>{t('jam.settingsTitle')}</SectionLabel>
      {!can && <p className="px-1 text-xs text-muted">{t('jam.readOnly', { name: jam.hostName })}</p>}
      <SettingRow label={t('jam.allowChannels')} description={t('jam.allowChannelsHint')}>
        <Toggle label={t('jam.allowChannels')} checked={draft.allowChannels} disabled={!can} onChange={(v) => set({ allowChannels: v })} />
      </SettingRow>
      <SettingRow label={t('jam.allowServers')} description={t('jam.allowServersHint')}>
        <Toggle label={t('jam.allowServers')} checked={draft.allowServers} disabled={!can} onChange={(v) => set({ allowServers: v })} />
      </SettingRow>
      <SettingRow label={t('jam.editableBy')} description={t('jam.editableByHint')}>
        <Segmented
          label={t('jam.editableBy')}
          value={draft.editableBy}
          disabled={!can}
          options={[
            { value: 'everyone', label: t('jam.editEveryone') },
            { value: 'host', label: t('jam.editHost') },
          ]}
          onChange={(v) => set({ editableBy: v })}
        />
      </SettingRow>
      <SettingRow label={t('jam.closed')} description={t('jam.closedHint')}>
        <Toggle label={t('jam.closed')} checked={draft.closed} disabled={!can} onChange={(v) => set({ closed: v })} />
      </SettingRow>
    </div>
  )
}

/** "Jam" section below Now playing: start one, or the members, host settings and leave / end. */
export function JamCard() {
  const { t } = useTranslation()
  const { state, channelId, channelName, permissions, run } = useLynx()
  const [busy, setBusy] = useState(false)
  if (!state || !channelId) return null
  const jam = state.jam ?? null

  const share = async (id: string) => {
    const ok = await shareJamLink(id, t('jam.shareMessage', { channel: channelName ?? '' }))
    if (!ok) console.info('[lynx] share sheet closed or not available')
  }

  if (!jam) {
    return (
      <Card
        title={t('jam.title')}
        description={t('jam.startBody')}
        icon={<Users size={18} />}
        actions={
          <Button
            size="xs"
            variant="primary"
            icon={<Share2 size={14} />}
            loading={busy}
            disabled={!permissions.canAdd && !permissions.canControl}
            onClick={async () => {
              setBusy(true)
              const r = await run(() => api.jamCreate(channelId))
              setBusy(false)
              if (r) await share(r.jamId)
            }}
          >
            {t('jam.invite')}
          </Button>
        }
        bodyClassName=""
      >
        <p className="px-4 py-3 text-xs text-muted">{t('jam.startHint')}</p>
      </Card>
    )
  }

  const you = jam.you
  const title = you.isHost ? t('jam.hosting') : t('jam.withHost', { name: jam.hostName || t('jam.unnamedChannel') })
  return (
    <Card
      title={title}
      description={t('jam.channels', { count: jam.members.length, max: jam.maxMembers })}
      icon={<Users size={18} />}
      actions={
        <Button size="xs" icon={<Share2 size={14} />} disabled={jam.settings.closed} onClick={() => share(jam.id)}>
          {t('jam.share')}
        </Button>
      }
      bodyClassName="space-y-4 py-4"
    >
      {you.member && !you.canEdit && (
        <div className="px-4">
          <Notice tone="amber">{t('jam.hostOnlyNotice', { name: jam.hostName })}</Notice>
        </div>
      )}
      <ul className="mx-4 overflow-hidden rounded-xl border border-line">
        {jam.members.map((m) => (
          <MemberRow
            key={memberKey(m)}
            jam={jam}
            member={m}
            onKick={
              you.canManage && !m.isHost
                ? () => void run(() => api.jamKick(channelId, m.guildId, m.channelId), t('jam.kicked', { name: m.name }))
                : undefined
            }
          />
        ))}
      </ul>
      <div className="px-4">
        <HostSettings jam={jam} />
      </div>
      <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-4 pt-3">
        <span className="font-mono text-[11px] break-all text-muted">{jam.shareLink}</span>
        {you.isHost
          ? you.canManage && (
              <ConfirmButton
                label={t('jam.end')}
                question={t('jam.endQuestion')}
                icon={<LogOut size={12} />}
                onConfirm={() => run(() => api.jamEnd(channelId))}
              />
            )
          : you.member && (
              <ConfirmButton
                label={t('jam.leave')}
                question={t('jam.leaveQuestion')}
                icon={<LogOut size={12} />}
                onConfirm={() => run(() => api.jamLeave(channelId))}
              />
            )}
      </div>
    </Card>
  )
}

/**
 * The Activity was opened from a jam link: offer to join it from the user's voice channel. Hidden
 * once the channel is in that jam or the user dismissed it.
 */
export function JamJoinPrompt() {
  const { t } = useTranslation()
  const { pendingJamId, clearPendingJam, channelId, channelName, state, errorText, refresh } = useLynx()
  const [preview, setPreview] = useState<JamPreview | null>(null)
  const [failure, setFailure] = useState<{ text: string; invite: string | null } | null>(null)
  const [joining, setJoining] = useState(false)
  const inIt = !!pendingJamId && state?.jam?.id === pendingJamId

  useEffect(() => {
    if (!pendingJamId || inIt) return
    let cancelled = false
    setFailure(null)
    api.jamPreview(pendingJamId).then(
      (p) => !cancelled && setPreview(p),
      (e) => !cancelled && setFailure({ text: errorText(e), invite: null }),
    )
    return () => {
      cancelled = true
    }
  }, [pendingJamId, inIt, channelId, errorText])

  useEffect(() => {
    if (inIt) clearPendingJam()
  }, [inIt, clearPendingJam])

  if (!pendingJamId || inIt || !channelId) return null
  const jam = preview?.jam
  const host = jam?.hostName || t('jam.unnamedChannel')
  const code = preview?.join.code ?? null

  const join = async () => {
    setJoining(true)
    setFailure(null)
    try {
      await api.jamJoin(channelId, pendingJamId)
      clearPendingJam()
      void refresh()
    } catch (error) {
      setFailure({ text: errorText(error), invite: error instanceof ApiError ? error.invite : null })
    } finally {
      setJoining(false)
    }
  }

  return (
    <section className="overflow-hidden rounded-2xl border border-accent-500/30 bg-surface shadow-sm">
      <div className="flex items-start gap-3 px-4 py-4">
        <span className="inline-flex size-9 shrink-0 items-center justify-center rounded-xl bg-accent-500/10 text-accent-600 dark:text-accent-400">
          <Users size={18} />
        </span>
        <div className="min-w-0 flex-1 space-y-1">
          {jam ? (
            <>
              <h2 className="font-semibold">{t('jam.joinTitle', { name: host })}</h2>
              <p className="text-xs text-muted">
                {t('jam.joinBody', { channel: channelName ?? '' })}{' '}
                {jam.hostGuildName && t('jam.joinFrom', { guild: jam.hostGuildName, count: jam.members.length })}
              </p>
            </>
          ) : failure ? (
            <h2 className="font-semibold">{t('jam.joinUnavailable')}</h2>
          ) : (
            <Spinner size={16} className="text-muted" />
          )}
        </div>
      </div>
      {(failure || (code && code !== 'jam_already_in')) && (
        <div className="px-4 pb-3">
          <Notice tone="amber">
            <div>{failure ? failure.text : t(`errors.${code}`, { defaultValue: t('errors.generic') })}</div>
            {failure?.invite && (
              <Button size="xs" className="mt-2" icon={<Plus size={14} />} onClick={() => openExternal(failure.invite!)}>
                {t('jam.addBot')}
              </Button>
            )}
          </Notice>
        </div>
      )}
      <div className="flex flex-wrap justify-end gap-2 border-t border-line px-4 py-3">
        <Button size="xs" variant="ghost" onClick={clearPendingJam}>
          {t('jam.notNow')}
        </Button>
        <Button size="xs" variant="primary" loading={joining} disabled={!preview?.join.allowed} onClick={join}>
          {t('jam.join')}
        </Button>
      </div>
    </section>
  )
}
