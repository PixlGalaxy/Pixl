import { Check, Circle, CircleAlert, MonitorSmartphone, RefreshCw } from 'lucide-react'
import type { ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { LynxMark } from '../components/Logo'
import { Button, Spinner } from '../components/ui'
import { LOGIN_STEPS, type LoginStep } from '../lib/discord'

function GateLayout({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-screen items-center justify-center px-4 py-10">
      <div className="w-full max-w-sm animate-[fade-in_.15s_ease-out]">
        <div className="mb-6 flex items-center gap-2.5">
          <LynxMark className="size-8 text-fg" />
          <span className="text-base font-semibold tracking-tight">Lynx</span>
        </div>
        {children}
      </div>
    </div>
  )
}

export function ConnectingScreen({ step, error }: { step: LoginStep | null; error?: string }) {
  const { t } = useTranslation()
  const current = step ? LOGIN_STEPS.indexOf(step) : -1
  return (
    <GateLayout>
      <section className="overflow-hidden rounded-2xl border border-line bg-surface shadow-sm">
        <div className="border-b border-line px-4 py-4">
          <h1 className="font-semibold">{error ? t('login.failedTitle') : t('login.title')}</h1>
          <p className="text-xs text-muted">{error ? t('login.failedHint') : t('login.subtitle')}</p>
        </div>
        <ol className="space-y-3 p-4">
          {LOGIN_STEPS.map((s, i) => {
            const done = i < current
            const active = i === current
            const failed = active && !!error
            return (
              <li key={s} className="flex items-center gap-3 text-sm">
                <span className="flex size-5 shrink-0 items-center justify-center">
                  {done ? (
                    <Check size={16} className="text-emerald-500" />
                  ) : failed ? (
                    <CircleAlert size={16} className="text-red-500" />
                  ) : active ? (
                    <Spinner size={16} className="text-sky-500" />
                  ) : (
                    <Circle size={8} className="fill-current text-line" />
                  )}
                </span>
                <span className={done || active ? 'text-fg' : 'text-muted'}>{t(`login.steps.${s}`)}</span>
              </li>
            )
          })}
        </ol>
        {error && (
          <div className="space-y-3 border-t border-line p-4">
            <div className="rounded-lg border border-rose-200 bg-rose-50 px-4 py-3 text-sm break-words text-rose-700 dark:border-rose-500/30 dark:bg-rose-500/10 dark:text-rose-300">
              <span className="font-medium">{step ? t(`login.steps.${step}`) : t('login.title')}:</span> {error}
            </div>
            <Button variant="primary" size="md" icon={<RefreshCw size={14} />} onClick={() => location.reload()}>
              {t('common.retry')}
            </Button>
          </div>
        )}
      </section>
    </GateLayout>
  )
}

function Message({ title, body, icon }: { title: string; body: string; icon: ReactNode }) {
  return (
    <GateLayout>
      <section className="overflow-hidden rounded-2xl border border-line bg-surface shadow-sm">
        <div className="flex items-center gap-3 px-4 py-4">
          <div className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-sky-500/10 text-sky-500">{icon}</div>
          <div className="min-w-0">
            <h1 className="font-semibold">{title}</h1>
            <p className="text-xs text-muted">{body}</p>
          </div>
        </div>
      </section>
    </GateLayout>
  )
}

export function OutsideDiscordScreen() {
  const { t } = useTranslation()
  return <Message title={t('gate.outsideTitle')} body={t('gate.outsideBody')} icon={<MonitorSmartphone size={18} />} />
}

export function NoGuildScreen() {
  const { t } = useTranslation()
  return <Message title={t('gate.noGuildTitle')} body={t('gate.noGuildBody')} icon={<CircleAlert size={18} />} />
}

export function SessionExpiredScreen() {
  const { t } = useTranslation()
  return (
    <GateLayout>
      <section className="space-y-3 rounded-2xl border border-line bg-surface p-4 shadow-sm">
        <h1 className="font-semibold">{t('gate.expiredTitle')}</h1>
        <p className="text-sm text-muted">{t('gate.expiredBody')}</p>
        <Button variant="primary" size="md" icon={<RefreshCw size={14} />} onClick={() => location.reload()}>
          {t('common.retry')}
        </Button>
      </section>
    </GateLayout>
  )
}
