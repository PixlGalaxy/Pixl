import { Headphones } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { Card } from './ui'

/** The user is not in a voice channel: the panel shows no player at all until they join one. */
export function NoVoiceCard() {
  const { t } = useTranslation()
  return (
    <Card title={t('noVoice.title')} description={t('noVoice.body')} icon={<Headphones size={18} />} bodyClassName="">
      <p className="px-4 py-4 text-sm text-muted">{t('noVoice.hint')}</p>
    </Card>
  )
}
