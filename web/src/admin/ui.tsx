import { LoaderCircle } from 'lucide-react'
import type { ButtonHTMLAttributes, ReactNode } from 'react'
import type { Status } from './types'

type Variant = 'primary' | 'secondary' | 'ghost' | 'danger'

const VARIANTS: Record<Variant, string> = {
  primary: 'bg-accent-600 text-white shadow-sm shadow-accent-600/25 hover:bg-accent-500 focus-visible:outline-accent-500',
  secondary: 'border border-line bg-surface text-fg shadow-xs hover:bg-surface-2 focus-visible:outline-accent-500',
  ghost: 'text-muted hover:bg-surface-2 hover:text-fg focus-visible:outline-accent-500',
  danger:
    'border border-red-500/30 bg-red-500/10 text-red-600 hover:bg-red-500/20 dark:text-red-400 focus-visible:outline-red-500',
}

export function Button({
  variant = 'secondary',
  icon,
  loading,
  children,
  className = '',
  disabled,
  type = 'button',
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; icon?: ReactNode; loading?: boolean }) {
  return (
    <button
      type={type}
      disabled={disabled || loading}
      className={`inline-flex h-8 shrink-0 items-center justify-center gap-1.5 rounded-lg px-3 text-sm font-medium whitespace-nowrap transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 disabled:pointer-events-none disabled:opacity-50 ${VARIANTS[variant]} ${className}`}
      {...props}
    >
      {loading ? <LoaderCircle size={14} className="animate-spin" /> : icon}
      {children}
    </button>
  )
}

export function Card({ title, actions, children }: { title?: ReactNode; actions?: ReactNode; children: ReactNode }) {
  return (
    <section className="rounded-2xl border border-line bg-surface p-4 shadow-xs sm:p-5">
      {(title || actions) && (
        <div className="mb-3 flex items-center gap-2">
          {title && <h2 className="text-sm font-semibold tracking-tight">{title}</h2>}
          {actions && <div className="ml-auto flex items-center gap-2">{actions}</div>}
        </div>
      )}
      {children}
    </section>
  )
}

const TONES: Record<Status | 'neutral' | 'accent', string> = {
  ok: 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-300',
  degraded: 'bg-amber-500/15 text-amber-700 dark:text-amber-300',
  down: 'bg-red-500/15 text-red-700 dark:text-red-300',
  neutral: 'bg-surface-2 text-muted',
  accent: 'bg-orange-500/15 text-orange-700 dark:text-orange-300',
}

export function Pill({ tone, children }: { tone: keyof typeof TONES; children: ReactNode }) {
  return (
    <span className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium whitespace-nowrap ${TONES[tone]}`}>
      {children}
    </span>
  )
}

export function Toggle({
  checked,
  onChange,
  label,
  hint,
  disabled,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  label: string
  hint?: string
  disabled?: boolean
}) {
  return (
    <label className={`flex items-start gap-3 ${disabled ? 'opacity-60' : 'cursor-pointer'}`}>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        disabled={disabled}
        onClick={() => onChange(!checked)}
        className={`relative mt-0.5 inline-flex h-5 w-9 shrink-0 rounded-full transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-500 ${checked ? 'bg-accent-600' : 'bg-line'}`}
      >
        <span
          className={`absolute top-0.5 size-4 rounded-full bg-white shadow transition-transform ${checked ? 'translate-x-4.5' : 'translate-x-0.5'}`}
        />
      </button>
      <span className="min-w-0">
        <span className="block text-sm font-medium">{label}</span>
        {hint && <span className="mt-0.5 block text-xs leading-5 text-muted">{hint}</span>}
      </span>
    </label>
  )
}

export const inputCls =
  'h-9 w-full rounded-lg border border-line bg-surface px-3 text-sm text-fg shadow-xs outline-none transition-colors placeholder:text-muted focus:border-accent-500 focus:ring-2 focus:ring-accent-500/20 disabled:opacity-60'

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-xs font-medium text-muted">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-xs text-muted">{hint}</span>}
    </label>
  )
}

export function Spinner() {
  return <LoaderCircle size={18} className="animate-spin text-muted" />
}

/** Inline message under a form or at the top of a page. */
export function Banner({ tone, children }: { tone: 'error' | 'success' | 'warning'; children: ReactNode }) {
  const cls =
    tone === 'error'
      ? 'border-red-500/30 bg-red-500/10 text-red-700 dark:text-red-300'
      : tone === 'success'
        ? 'border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300'
        : 'border-orange-500/30 bg-orange-500/10 text-orange-800 dark:text-orange-200'
  return (
    <div role={tone === 'error' ? 'alert' : 'status'} className={`rounded-xl border px-3 py-2 text-sm ${cls}`}>
      {children}
    </div>
  )
}
