import { useTranslation } from 'react-i18next'
import { openExternal } from '../lib/discord'

const STATUS_URL = 'https://onlinestatus.itzgalaxy.com'
const STATUS_HOST = 'onlinestatus.itzgalaxy.com'

/** "Check the status on onlinestatus.itzgalaxy.com", the domain opening the status page. */
export function StatusLink({ className = '' }: { className?: string }) {
  const { t } = useTranslation()
  return (
    <p className={`text-xs text-muted ${className}`}>
      {t('health.statusBefore')}{' '}
      <button
        type="button"
        onClick={() => void openExternal(STATUS_URL)}
        className="font-medium text-accent-600 hover:underline dark:text-accent-400"
      >
        {STATUS_HOST}
      </button>
    </p>
  )
}
