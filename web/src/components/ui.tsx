import { LoaderCircle, Music } from 'lucide-react'
import { useState, type ButtonHTMLAttributes, type ComponentProps, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { cx, mediaUrl } from '../lib/format'

type Variant = 'primary' | 'secondary' | 'warning' | 'ghost' | 'danger'
type Size = 'xs' | 'sm' | 'md'

const VARIANTS: Record<Variant, string> = {
  primary: 'bg-accent-600 text-white shadow-sm shadow-accent-600/25 hover:bg-accent-500 focus-visible:outline-accent-500',
  secondary: 'border border-line bg-surface text-fg shadow-xs hover:bg-surface-2 focus-visible:outline-accent-500',
  warning: 'bg-amber-500 text-white shadow-sm shadow-amber-500/25 hover:bg-amber-400 focus-visible:outline-amber-500',
  ghost: 'text-muted hover:bg-surface-2 hover:text-fg focus-visible:outline-accent-500',
  danger:
    'border border-red-500/30 bg-red-500/10 text-red-600 hover:bg-red-500/20 dark:text-red-400 focus-visible:outline-red-500',
}
const SIZES: Record<Size, string> = {
  xs: 'h-7 gap-1.5 px-2.5 text-xs',
  sm: 'h-8 gap-1.5 px-3 text-sm',
  md: 'h-9 gap-2 px-4 text-sm',
}

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant
  size?: Size
  icon?: ReactNode
  loading?: boolean
}

export function Button({
  variant = 'secondary',
  size = 'sm',
  icon,
  loading,
  children,
  className = '',
  disabled,
  ...props
}: ButtonProps) {
  return (
    <button
      type="button"
      disabled={disabled || loading}
      className={`inline-flex shrink-0 items-center justify-center rounded-lg font-medium whitespace-nowrap transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 disabled:pointer-events-none disabled:opacity-50 ${VARIANTS[variant]} ${SIZES[size]} ${className}`}
      {...props}
    >
      {loading ? <LoaderCircle size={14} className="animate-spin" /> : icon}
      {children}
    </button>
  )
}

export function IconButton({
  label,
  className = '',
  children,
  active,
  ...props
}: ComponentProps<'button'> & { label: string; active?: boolean }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      aria-pressed={active}
      className={cx(
        'inline-flex size-8 shrink-0 items-center justify-center rounded-lg transition-colors focus-visible:outline-2 focus-visible:outline-accent-500 disabled:pointer-events-none disabled:opacity-40',
        active ? 'bg-accent-500/10 text-accent-600 dark:text-accent-400' : 'text-muted hover:bg-surface-2 hover:text-fg',
        className,
      )}
      {...props}
    >
      {children}
    </button>
  )
}

export function Toggle({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  label: string
  disabled?: boolean
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-500 disabled:opacity-50 ${checked ? 'bg-accent-600' : 'bg-line'}`}
    >
      <span
        className={`size-4 rounded-full bg-white shadow-sm transition-transform ${checked ? 'translate-x-[18px]' : 'translate-x-0.5'}`}
      />
    </button>
  )
}

/** The `size-9 rounded-xl` accent tile that marks a card header. */
export function IconTile({ children }: { children: ReactNode }) {
  return (
    <div className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-accent-500/10 text-accent-500">{children}</div>
  )
}

/** Artwork in place of a card's icon tile (same size and corners); the tile with `fallback` when it has none. */
export function TileArtwork({ url, fallback }: { url: string | null | undefined; fallback: ReactNode }) {
  const [broken, setBroken] = useState<string | null>(null)
  const src = mediaUrl(url)
  if (!src || broken === src) return <IconTile>{fallback}</IconTile>
  return (
    <img
      src={src}
      alt=""
      onError={() => setBroken(src)}
      className="size-9 shrink-0 rounded-xl border border-line bg-surface-2 object-cover"
    />
  )
}

export function Card({
  title,
  description,
  icon,
  media,
  actions,
  children,
  bodyClassName = 'space-y-5 p-4',
}: {
  title: ReactNode
  description?: ReactNode
  icon?: ReactNode
  /** Replaces the icon tile (e.g. `TileArtwork`). */
  media?: ReactNode
  actions?: ReactNode
  children: ReactNode
  bodyClassName?: string
}) {
  return (
    <section className="overflow-hidden rounded-2xl border border-line bg-surface shadow-sm">
      <div className="flex flex-wrap items-center gap-3 border-b border-line px-4 py-4">
        {media ?? (icon && <IconTile>{icon}</IconTile>)}
        <div className="min-w-0 flex-1">
          <h2 className="font-semibold">{title}</h2>
          {description && <p className="text-xs text-muted">{description}</p>}
        </div>
        {actions && <div className="flex items-center gap-2">{actions}</div>}
      </div>
      <div className={bodyClassName}>{children}</div>
    </section>
  )
}

export function SettingRow({
  label,
  description,
  children,
}: {
  label: ReactNode
  description?: ReactNode
  children: ReactNode
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div className="min-w-0">
        <div className="text-sm font-medium">{label}</div>
        {description && <div className="text-xs text-muted">{description}</div>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  )
}

export const inputCls =
  'h-9 w-full rounded-lg border border-line bg-surface px-3 text-sm shadow-xs placeholder:text-muted/70 focus:border-accent-500 focus:ring-2 focus:ring-accent-500/20 focus:outline-none disabled:cursor-not-allowed disabled:opacity-40'
export const labelCls = 'mb-1 block text-xs font-medium text-muted'

/** Destructive action that turns into "question + Confirm + Cancel" (no window.confirm in the iframe). */
export function ConfirmButton({
  label,
  question,
  confirmLabel,
  onConfirm,
  icon,
  disabled,
  size = 'xs',
}: {
  label: string
  question: string
  confirmLabel?: string
  onConfirm: () => Promise<unknown> | void
  icon?: ReactNode
  disabled?: boolean
  size?: Size
}) {
  const { t } = useTranslation()
  const [asking, setAsking] = useState(false)
  const [busy, setBusy] = useState(false)
  if (!asking) {
    return (
      <Button variant="danger" size={size} icon={icon} disabled={disabled} onClick={() => setAsking(true)}>
        {label}
      </Button>
    )
  }
  return (
    <span className="inline-flex flex-wrap items-center justify-end gap-2 text-xs">
      <span className="text-muted">{question}</span>
      <Button
        variant="danger"
        size="xs"
        loading={busy}
        onClick={async () => {
          setBusy(true)
          try {
            await onConfirm()
          } finally {
            setBusy(false)
            setAsking(false)
          }
        }}
      >
        {confirmLabel ?? t('common.confirm')}
      </Button>
      <Button size="xs" disabled={busy} onClick={() => setAsking(false)}>
        {t('common.cancel')}
      </Button>
    </span>
  )
}

export function Spinner({ size, className = '' }: { size?: number; className?: string }) {
  return <LoaderCircle size={size} className={`animate-spin ${className}`} />
}

export function PageSpinner() {
  return (
    <div className="flex justify-center py-16 text-muted">
      <LoaderCircle className="animate-spin" />
    </div>
  )
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="px-4 py-8 text-center text-sm text-muted">{children}</p>
}

export function Chip({ children, className = '', title }: { children: ReactNode; className?: string; title?: string }) {
  return (
    <span
      title={title}
      className={`inline-flex items-center gap-1 rounded-md border border-line bg-surface-2 px-1.5 py-0.5 font-mono text-[11px] text-muted ${className}`}
    >
      {children}
    </span>
  )
}

type Tone = 'sky' | 'emerald' | 'amber' | 'red' | 'neutral'
const TAG_TONES: Record<Tone, string> = {
  sky: 'border-accent-500/30 bg-accent-500/10 text-accent-600 dark:text-accent-400',
  emerald: 'border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300',
  amber: 'border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300',
  red: 'border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-400',
  neutral: 'border-line bg-surface-2 text-muted',
}

export function Tag({ tone = 'neutral', children }: { tone?: Tone; children: ReactNode }) {
  return (
    <span
      className={`inline-flex items-center gap-1 rounded-md border px-1.5 py-px text-[10px] font-semibold tracking-wide uppercase ${TAG_TONES[tone]}`}
    >
      {children}
    </span>
  )
}

/** Tinted notice box. */
export function Notice({ tone, icon, children }: { tone: Exclude<Tone, 'neutral'>; icon?: ReactNode; children: ReactNode }) {
  const tones: Record<typeof tone, string> = {
    sky: 'border-accent-500/30 bg-accent-500/10 text-accent-700 dark:text-accent-300',
    emerald: 'border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300',
    amber: 'border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300',
    red: 'border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300',
  }
  return (
    <div className={`flex items-start gap-2.5 rounded-xl border px-3.5 py-3 text-sm ${tones[tone]}`}>
      {icon && <span className="mt-0.5 shrink-0">{icon}</span>}
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  )
}

/** Native range with the accent fill track. */
export function Slider({
  value,
  min = 0,
  max,
  step = 1,
  label,
  disabled,
  onInput,
  onCommit,
  className = '',
}: {
  value: number
  min?: number
  max: number
  step?: number
  label: string
  disabled?: boolean
  onInput: (v: number) => void
  onCommit?: (v: number) => void
  className?: string
}) {
  const pct = max > min ? ((value - min) / (max - min)) * 100 : 0
  const commit = (e: { currentTarget: HTMLInputElement }) => onCommit?.(Number(e.currentTarget.value))
  return (
    <input
      type="range"
      min={min}
      max={max}
      step={step}
      value={value}
      disabled={disabled}
      aria-label={label}
      title={label}
      onChange={(e) => onInput(Number(e.target.value))}
      onPointerUp={commit}
      onKeyUp={commit}
      onTouchEnd={commit}
      className={`lynx-range h-1.5 min-w-0 flex-1 cursor-pointer appearance-none rounded-full disabled:cursor-not-allowed disabled:opacity-50 ${className}`}
      style={{ background: `linear-gradient(to right, var(--accent-600) ${pct}%, var(--line) ${pct}%)` }}
    />
  )
}

/** Segmented control for small enums (loop mode, control mode). */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
  disabled,
}: {
  value: T
  options: Array<{ value: T; label: string }>
  onChange: (v: T) => void
  label: string
  disabled?: boolean
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-lg border border-line bg-surface-2 p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          disabled={disabled}
          onClick={() => onChange(o.value)}
          className={cx(
            'h-7 rounded-md px-2.5 text-xs font-medium transition-colors disabled:opacity-50',
            value === o.value ? 'bg-surface text-fg shadow-xs' : 'text-muted hover:text-fg',
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}

export function Artwork({ url, className = 'size-10 rounded-xl', iconSize = 16 }: { url: string | null; className?: string; iconSize?: number }) {
  const [broken, setBroken] = useState(false)
  const src = mediaUrl(url)
  if (!src || broken) {
    return (
      <div className={`flex shrink-0 items-center justify-center border border-line bg-surface-2 text-muted ${className}`}>
        <Music size={iconSize} />
      </div>
    )
  }
  return (
    <img
      src={src}
      alt=""
      loading="lazy"
      onError={() => setBroken(true)}
      className={`shrink-0 border border-line bg-surface-2 object-cover ${className}`}
    />
  )
}

export function Avatar({ url, name, className = 'size-6' }: { url: string | null; name: string; className?: string }) {
  const src = mediaUrl(url)
  if (!src) {
    return (
      <span
        className={`inline-flex shrink-0 items-center justify-center rounded-full bg-surface-2 text-[10px] font-semibold text-muted ${className}`}
      >
        {name.slice(0, 1).toUpperCase()}
      </span>
    )
  }
  return <img src={src} alt="" className={`shrink-0 rounded-full object-cover ${className}`} />
}

/** Uppercase section label used above lists. */
export function SectionLabel({ children, right }: { children: ReactNode; right?: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 px-1">
      <h3 className="text-[10px] font-semibold tracking-wide text-muted uppercase">{children}</h3>
      {right}
    </div>
  )
}
