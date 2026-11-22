import { GitPullRequest, Heart, Star } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { openExternal } from '../lib/discord'

const REPO = 'https://github.com/PixlGalaxy/Pixl'

/**
 * Thank-you note from the author, at the bottom of Help (same card as DockerUpdates' SupportCard).
 * Links go through the SDK: the Activity iframe cannot open new windows itself.
 */
export function SupportCard() {
  const { t } = useTranslation()
  return (
    <section className="relative overflow-hidden rounded-2xl border border-line bg-gradient-to-br from-accent-500/10 via-surface to-violet-500/10 shadow-sm">
      <div className="pointer-events-none absolute -top-24 -left-24 size-72 rounded-full bg-accent-500/10 blur-3xl" />
      <div className="relative flex flex-col items-center gap-6 p-6 sm:flex-row sm:items-stretch sm:p-8">
        <div className="flex flex-1 flex-col justify-center text-center sm:text-left">
          <p className="inline-flex items-center justify-center gap-1.5 text-xs font-semibold tracking-wider text-accent-600 uppercase sm:justify-start dark:text-accent-400">
            <Heart size={13} className="fill-current" /> {t('support.eyebrow')}
          </p>
          <h2 className="mt-2 text-xl font-semibold tracking-tight sm:text-2xl">{t('support.title')}</h2>
          <p className="mt-2 max-w-xl text-sm leading-relaxed text-muted">{t('support.body')}</p>
          <div className="mt-5 flex flex-wrap justify-center gap-2 sm:justify-start">
            <button
              type="button"
              onClick={() => openExternal(REPO)}
              className="inline-flex h-9 items-center gap-2 rounded-lg bg-amber-400 px-4 text-sm font-semibold text-zinc-900 shadow-sm shadow-amber-400/30 transition-colors hover:bg-amber-300"
            >
              <Star size={15} className="fill-current" /> {t('support.star')}
            </button>
            <button
              type="button"
              onClick={() => openExternal(`${REPO}/issues`)}
              className="inline-flex h-9 items-center gap-2 rounded-lg border border-line bg-surface px-4 text-sm font-medium transition-colors hover:bg-surface-2"
            >
              <GitPullRequest size={15} /> {t('support.contribute')}
            </button>
          </div>
          <button
            type="button"
            onClick={() => openExternal(REPO)}
            className="mt-3 self-center font-mono text-xs text-muted hover:text-accent-600 sm:self-start dark:hover:text-accent-400"
          >
            github.com/PixlGalaxy/Pixl
          </button>
        </div>

        <img
          src="/GalaxyPC.webp"
          alt={t('support.imageAlt')}
          width={1024}
          height={1024}
          loading="lazy"
          draggable={false}
          // The artwork has empty space at the top: crop it so the characters fill the box.
          className="h-52 w-60 shrink-0 object-cover object-bottom select-none"
        />
      </div>
    </section>
  )
}
