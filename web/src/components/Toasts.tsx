import { CircleAlert, CircleCheck, Info, X } from 'lucide-react'
import { createContext, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'

type ToneName = 'success' | 'error' | 'info'
type Toast = { id: number; tone: ToneName; message: string }

const TONES = {
  success: { icon: CircleCheck, cls: 'text-emerald-500' },
  error: { icon: CircleAlert, cls: 'text-red-500' },
  info: { icon: Info, cls: 'text-sky-500' },
} as const

type ToastApi = { push: (tone: ToneName, message: string) => void }
const ToastContext = createContext<ToastApi>({ push: () => {} })

export function useToast() {
  return useContext(ToastContext)
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const { t } = useTranslation()
  const [toasts, setToasts] = useState<Toast[]>([])
  const nextId = useRef(1)

  const dismiss = useCallback((id: number) => setToasts((list) => list.filter((x) => x.id !== id)), [])
  const push = useCallback(
    (tone: ToneName, message: string) => {
      const id = nextId.current++
      setToasts((list) => [...list.filter((x) => x.message !== message), { id, tone, message }].slice(-4))
      setTimeout(() => dismiss(id), tone === 'error' ? 7000 : 4000)
    },
    [dismiss],
  )
  const value = useMemo(() => ({ push }), [push])

  return (
    <ToastContext.Provider value={value}>
      {children}
      <div className="pointer-events-none fixed right-3 bottom-3 left-3 z-[60] flex flex-col items-end gap-2 sm:left-auto sm:w-[380px]">
        {toasts.map(({ id, tone, message }) => {
          const { icon: Icon, cls } = TONES[tone]
          return (
            <div
              key={id}
              role="status"
              className="pointer-events-auto flex w-full animate-[toast-in_.2s_ease-out] items-start gap-3 rounded-xl border border-line bg-surface p-3 shadow-lg shadow-black/10"
            >
              <Icon size={18} className={`mt-0.5 shrink-0 ${cls}`} />
              <p className="flex-1 text-sm break-words">{message}</p>
              <button type="button" aria-label={t('common.dismiss')} onClick={() => dismiss(id)} className="text-muted hover:text-fg">
                <X size={16} />
              </button>
            </div>
          )
        })}
      </div>
    </ToastContext.Provider>
  )
}
