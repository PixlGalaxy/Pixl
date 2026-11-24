import { useTranslation } from 'react-i18next'
import { openExternal } from '../lib/discord'

const linkCls =
  'cursor-pointer font-medium text-fg/80 underline-offset-2 transition-colors hover:text-accent-500 hover:underline'

/** "Lynx | v0.1.0 | Developed by PixlGalaxy | Hosted on ItzGalaxy.com", at the bottom of every tab. */
export function AppFooter() {
  const { t } = useTranslation()
  return (
    <footer className="mx-auto w-full max-w-6xl px-3 pt-2 pb-5 text-center text-xs leading-6 text-muted sm:px-4">
      <span className="font-medium text-fg/80">Lynx</span>
      <span className="px-2">|</span>
      <span className="font-mono">v{__APP_VERSION__}</span>
      <span className="px-2">|</span>
      <span className="whitespace-nowrap">
        {t('help.developedBy')}{' '}
        <button type="button" onClick={() => void openExternal('https://github.com/PixlGalaxy')} className={linkCls}>
          PixlGalaxy
        </button>
      </span>
      <span className="px-2">|</span>
      <span className="whitespace-nowrap">
        {t('help.hostedOn')}{' '}
        <button type="button" onClick={() => void openExternal('https://itzgalaxy.com')} className={linkCls}>
          ItzGalaxy.com
        </button>
      </span>
    </footer>
  )
}
