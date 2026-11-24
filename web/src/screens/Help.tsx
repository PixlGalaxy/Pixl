import { BookOpen, ExternalLink, KeyRound, LayoutPanelTop, SquareTerminal } from 'lucide-react'
import type { ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { Button, Card, Tag } from '../components/ui'
import { openExternal } from '../lib/discord'
import { useLynx } from '../lib/lynx'
import type { PublicLinks } from '../lib/types'
import { SupportCard } from '../components/SupportCard'

type Access = 'anyone' | 'control' | 'vote'

const COMMANDS: Array<{ name: string; access: Access; ai?: boolean }> = [
  { name: 'play', access: 'anyone' },
  { name: 'pause', access: 'control' },
  { name: 'resume', access: 'control' },
  { name: 'skip', access: 'vote' },
  { name: 'stop', access: 'control' },
  { name: 'queue', access: 'anyone' },
  { name: 'nowplaying', access: 'anyone' },
  { name: 'remove', access: 'anyone' },
  { name: 'move', access: 'control' },
  { name: 'skipto', access: 'control' },
  { name: 'shuffle', access: 'control' },
  { name: 'loop', access: 'control' },
  { name: 'volume', access: 'control' },
  { name: 'lyrics', access: 'anyone' },
  { name: 'invite', access: 'anyone' },
  { name: 'about', access: 'anyone' },
  { name: 'panel', access: 'anyone' },
  { name: 'ai', access: 'anyone', ai: true },
]

const PANEL_AREAS: Array<{ id: string; ai?: boolean }> = [
  { id: 'player' },
  { id: 'queue' },
  { id: 'playlists' },
  { id: 'jam' },
  { id: 'lyrics' },
  { id: 'radio', ai: true },
  { id: 'settings' },
  { id: 'privacy' },
  { id: 'language' },
]

const LINKS: Array<keyof PublicLinks> = ['landing', 'terms', 'privacy', 'support']

const ACCESS_TONE = { anyone: 'neutral', control: 'amber', vote: 'sky' } as const

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <h3 className="text-sm font-semibold">{title}</h3>
      <div className="text-sm leading-relaxed text-muted">{children}</div>
    </div>
  )
}

export function HelpScreen() {
  const { t } = useTranslation()
  const { aiEnabled, config } = useLynx()
  const links = LINKS.filter((k) => !!config.links[k])

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-xl font-semibold tracking-tight">{t('help.title')}</h2>
        <p className="text-sm text-muted">{t('help.subtitle')}</p>
      </div>

      <Card title={t('help.panelTitle')} description={t('help.panelHint')} icon={<LayoutPanelTop size={18} />}>
        {PANEL_AREAS.filter((a) => !a.ai || aiEnabled).map((a) => (
          <Section key={a.id} title={t(`help.panel.${a.id}.title`)}>
            {t(`help.panel.${a.id}.body`)}
            {a.id === 'player' && <p className="mt-1.5">{t('help.seekNote')}</p>}
            {a.id === 'player' && aiEnabled && <p className="mt-1.5">{t('help.feedbackNote')}</p>}
          </Section>
        ))}
      </Card>

      <Card title={t('help.commandsTitle')} description={t('help.commandsHint')} icon={<SquareTerminal size={18} />} bodyClassName="">
        <ul className="-mt-px">
          {COMMANDS.filter((c) => !c.ai || aiEnabled).map((c) => (
            <li key={c.name} className="space-y-1.5 border-t border-line px-4 py-3">
              <div className="flex flex-wrap items-center gap-2">
                <code className="font-mono text-sm font-medium text-accent-600 dark:text-accent-400">/{c.name}</code>
                <Tag tone={ACCESS_TONE[c.access]}>{t(`help.access.${c.access}`)}</Tag>
              </div>
              <p className="text-sm text-muted">{t(`help.commands.${c.name}.description`)}</p>
              <div className="flex min-w-0 items-start gap-2 text-xs">
                <span className="shrink-0 pt-0.5 text-muted">{t('help.example')}</span>
                <code className="min-w-0 rounded-md border border-line bg-surface-2 px-1.5 py-0.5 font-mono text-[11px] break-words">
                  {t(`help.commands.${c.name}.example`)}
                </code>
              </div>
            </li>
          ))}
        </ul>
        <p className="border-t border-line px-4 py-3 text-xs text-muted">{t('help.accessNote')}</p>
      </Card>

      <Card title={t('help.permissionsTitle')} description={t('help.permissionsHint')} icon={<KeyRound size={18} />}>
        <Section title={t('help.permissions.manager.title')}>{t('help.permissions.manager.body')}</Section>
        <Section title={t('help.permissions.dj.title')}>{t('help.permissions.dj.body')}</Section>
        <Section title={t('help.permissions.modes.title')}>
          <p>{t('help.permissions.modes.everyone')}</p>
          <p className="mt-1.5">{t('help.permissions.modes.dj')}</p>
        </Section>
        <Section title={t('help.permissions.vote.title')}>{t('help.permissions.vote.body')}</Section>
      </Card>

      {links.length > 0 && (
        <Card title={t('help.linksTitle')} icon={<BookOpen size={18} />}>
          <div className="flex flex-wrap gap-2">
            {links.map((k) => (
              <Button key={k} icon={<ExternalLink size={14} />} onClick={() => openExternal(config.links[k]!)}>
                {t(`help.links.${k}`)}
              </Button>
            ))}
          </div>
        </Card>
      )}

      <SupportCard />

    </div>
  )
}
