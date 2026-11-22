import { Link2, Plus, Search, X } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { api } from '../lib/api'
import { fmtTime } from '../lib/format'
import { useLynx } from '../lib/lynx'
import type { SearchResult } from '../lib/types'
import { useToast } from './Toasts'
import { Artwork, Button, IconButton, Spinner, inputCls } from './ui'

const isUrl = (s: string) => /^https?:\/\/\S+$/i.test(s.trim())

/** Add box: a URL is added directly, text searches `/api/search` and lists results to add. */
export function AddBox() {
  const { t } = useTranslation()
  const { channelId, run, errorText } = useLynx()
  const toast = useToast()
  const [query, setQuery] = useState('')
  const [results, setResults] = useState<SearchResult[] | null>(null)
  const [searching, setSearching] = useState(false)
  const [searchError, setSearchError] = useState<string | null>(null)
  const [adding, setAdding] = useState<string | null>(null)
  const seq = useRef(0)

  const trimmed = query.trim()
  const url = isUrl(trimmed)

  useEffect(() => {
    if (url || trimmed.length < 2) {
      setResults(null)
      setSearchError(null)
      setSearching(false)
      return
    }
    const id = ++seq.current
    setSearching(true)
    const timer = setTimeout(async () => {
      try {
        const { results } = await api.search(trimmed)
        if (id === seq.current) {
          setResults(results)
          setSearchError(null)
        }
      } catch (error) {
        if (id === seq.current) setSearchError(errorText(error))
      } finally {
        if (id === seq.current) setSearching(false)
      }
    }, 350)
    return () => clearTimeout(timer)
  }, [trimmed, url, errorText])

  const add = async (q: string, key: string) => {
    if (!channelId) return
    setAdding(key)
    const res = await run(() => api.add(channelId, q))
    setAdding(null)
    if (res) {
      toast.push('success', t('queue.added', { count: res.added }))
      setQuery('')
      setResults(null)
    }
  }

  return (
    <div className="space-y-2">
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault()
          if (!trimmed) return
          void add(trimmed, 'form')
        }}
      >
        <div className="relative min-w-0 flex-1">
          {url ? (
            <Link2 size={15} className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-muted" />
          ) : (
            <Search size={15} className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-muted" />
          )}
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('queue.addPlaceholder')}
            aria-label={t('queue.addPlaceholder')}
            className={`${inputCls} pr-8 pl-9`}
            enterKeyHint={url ? 'go' : 'search'}
          />
          {query && (
            <button
              type="button"
              aria-label={t('common.clear')}
              onClick={() => setQuery('')}
              className="absolute top-1/2 right-2.5 -translate-y-1/2 text-muted hover:text-fg"
            >
              <X size={14} />
            </button>
          )}
        </div>
        <Button type="submit" variant="primary" size="md" icon={<Plus size={14} />} loading={adding === 'form'} disabled={!trimmed}>
          <span className="hidden min-[420px]:inline">{t('queue.add')}</span>
        </Button>
      </form>

      {searching && !results && (
        <div className="flex items-center gap-2 px-1 py-2 text-xs text-muted">
          <Spinner size={13} /> {t('queue.searching')}
        </div>
      )}
      {searchError && <p className="px-1 text-xs text-red-600 dark:text-red-400">{searchError}</p>}
      {results && (
        <div className="overflow-hidden rounded-xl border border-line">
          {results.length === 0 ? (
            <p className="px-3 py-4 text-center text-sm text-muted">{t('queue.noResults')}</p>
          ) : (
            <ul className="max-h-80 overflow-y-auto">
              {results.map((r, i) => (
                <li
                  key={r.url}
                  className={`flex items-center gap-3 px-3 py-2 transition-colors hover:bg-surface-2/60 ${i > 0 ? 'border-t border-line' : ''}`}
                >
                  <Artwork url={r.artworkUrl} className="size-9 rounded-lg" iconSize={14} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-semibold">{r.title}</div>
                    <div className="mt-0.5 truncate text-xs text-muted">{r.artists.join(', ')}</div>
                  </div>
                  <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(r.durationMs)}</span>
                  <IconButton label={t('queue.addThis', { title: r.title })} disabled={adding !== null} onClick={() => add(r.url, r.url)}>
                    {adding === r.url ? <Spinner size={14} /> : <Plus size={16} />}
                  </IconButton>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  )
}
