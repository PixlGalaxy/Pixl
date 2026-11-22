import {
  closestCenter,
  DndContext,
  KeyboardSensor,
  PointerSensor,
  TouchSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
} from '@dnd-kit/core'
import { arrayMove, SortableContext, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from '@dnd-kit/sortable'
import { CSS } from '@dnd-kit/utilities'
import { GripVertical, History, ListMusic, Plus, RotateCcw, Trash2, X } from 'lucide-react'
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { AddBox } from '../components/AddBox'
import { AiProgress } from '../components/AiProgress'
import { AiTag, TrackReasonLine } from '../components/TrackBits'
import { Artwork, Card, ConfirmButton, Empty, IconButton, PageSpinner, Spinner } from '../components/ui'
import { api } from '../lib/api'
import { cx, fmtTime } from '../lib/format'
import { useLynx } from '../lib/lynx'
import type { Track } from '../lib/types'

const revealCls = 'opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 [@media(hover:none)]:opacity-100'

function Row({ track, index, sortable, canRemove }: { track: Track; index: number; sortable: boolean; canRemove: boolean }) {
  const { t } = useTranslation()
  const { channelId, run, setState } = useLynx()
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } = useSortable({
    id: track.uid,
    disabled: !sortable,
  })
  const style = { transform: CSS.Translate.toString(transform), transition }
  return (
    <li
      ref={setNodeRef}
      style={style}
      className={cx(
        'group relative flex items-center gap-3 border-t border-line bg-surface px-3 py-2.5 transition-colors hover:bg-surface-2/60',
        isDragging && 'z-10 bg-accent-500/5 outline-2 -outline-offset-4 outline-accent-500/50 outline-dashed [&>*]:opacity-30',
      )}
    >
      {sortable ? (
        <button
          type="button"
          ref={setActivatorNodeRef}
          {...attributes}
          {...listeners}
          aria-label={t('queue.dragHandle', { title: track.title })}
          className="-ml-1 inline-flex h-8 w-5 shrink-0 cursor-grab touch-none items-center justify-center text-muted hover:text-fg active:cursor-grabbing"
        >
          <GripVertical size={14} />
        </button>
      ) : (
        <span className="w-4 shrink-0 text-center font-mono text-[11px] text-muted tabular-nums">{index + 1}</span>
      )}
      <Artwork url={track.artworkUrl} />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-semibold">{track.title}</div>
        <div className="mt-0.5 truncate text-xs text-muted">
          {track.artists.join(', ')}
          {track.reason && <AiTag />}
          <span className="px-1">·</span>
          {track.requestedBy.name}
        </div>
        {track.reason && (
          <div className="mt-0.5 flex">
            <TrackReasonLine reason={track.reason} compact />
          </div>
        )}
      </div>
      <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(track.durationMs)}</span>
      {canRemove && channelId && (
        <IconButton
          label={t('queue.remove', { title: track.title })}
          className={`${revealCls} hover:!text-red-500`}
          onClick={() => {
            setState((s) => (s ? { ...s, upcoming: s.upcoming.filter((x) => x.uid !== track.uid) } : s))
            void run(() => api.remove(channelId, track.uid))
          }}
        >
          <X size={16} />
        </IconButton>
      )}
    </li>
  )
}

/** Upcoming tracks with drag-and-drop reorder (`/api/queue/move`). */
function UpcomingList({ limit }: { limit?: number }) {
  const { t } = useTranslation()
  const { state, user, permissions, channelId, run, setState } = useLynx()
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 150, tolerance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  )
  const upcoming = state?.upcoming ?? []
  const shown = limit ? upcoming.slice(0, limit) : upcoming
  const canMove = permissions.canControl && !!channelId

  const onDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id || !channelId) return
    const from = upcoming.findIndex((x) => x.uid === active.id)
    const to = upcoming.findIndex((x) => x.uid === over.id)
    if (from < 0 || to < 0) return
    setState((s) => (s ? { ...s, upcoming: arrayMove(s.upcoming, from, to) } : s))
    void run(() => api.move(channelId, String(active.id), to))
  }

  if (upcoming.length === 0) return <Empty>{t('queue.emptyUpcoming')}</Empty>
  return (
    <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
      <SortableContext items={shown.map((x) => x.uid)} strategy={verticalListSortingStrategy}>
        <ul className="-mt-px">
          {shown.map((track, i) => (
            <Row
              key={track.uid}
              track={track}
              index={i}
              sortable={canMove}
              canRemove={permissions.canControl || track.requestedBy.id === user.id}
            />
          ))}
        </ul>
      </SortableContext>
      {limit && upcoming.length > limit && (
        <p className="border-t border-line px-4 py-2.5 text-center text-xs text-muted">
          {t('queue.more', { count: upcoming.length - limit })}
        </p>
      )}
    </DndContext>
  )
}

function totalDuration(tracks: Track[]) {
  return tracks.reduce((sum, x) => sum + (x.durationMs ?? 0), 0)
}

function UpcomingCard({ limit }: { limit?: number }) {
  const { t } = useTranslation()
  const { state, permissions, channelId, run, setState } = useLynx()
  const upcoming = state?.upcoming ?? []
  return (
    <Card
      title={t('queue.upNext')}
      description={
        upcoming.length > 0
          ? t('queue.summary', { count: upcoming.length, duration: fmtTime(totalDuration(upcoming)) })
          : undefined
      }
      icon={<ListMusic size={18} />}
      actions={
        permissions.canControl && channelId && upcoming.length > 0 ? (
          <ConfirmButton
            label={t('queue.clear')}
            question={t('queue.clearQuestion')}
            icon={<Trash2 size={12} />}
            onConfirm={async () => {
              setState((s) => (s ? { ...s, upcoming: [] } : s))
              await run(() => api.clear(channelId))
            }}
          />
        ) : undefined
      }
      bodyClassName=""
    >
      <UpcomingList limit={limit} />
    </Card>
  )
}

function HistoryCard() {
  const { t } = useTranslation()
  const { state, permissions, channelId, run } = useLynx()
  const [adding, setAdding] = useState<string | null>(null)
  const history = state?.history ?? []
  return (
    <Card title={t('queue.history')} description={t('queue.historyHint')} icon={<History size={18} />} bodyClassName="">
      {history.length === 0 ? (
        <Empty>{t('queue.emptyHistory')}</Empty>
      ) : (
        <ul className="-mt-px">
          {history.map((track) => (
            <li key={track.uid} className="group flex items-center gap-3 border-t border-line px-3 py-2.5 transition-colors hover:bg-surface-2/60">
              <Artwork url={track.artworkUrl} className="size-9 rounded-lg opacity-80" iconSize={14} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm font-medium">{track.title}</div>
                <div className="mt-0.5 truncate text-xs text-muted">
                  {track.artists.join(', ')}
                  {track.reason && <AiTag />}
                </div>
              </div>
              <span className="font-mono text-[11px] text-muted tabular-nums">{fmtTime(track.durationMs)}</span>
              {permissions.canAdd && channelId && (
                <IconButton
                  label={t('queue.addAgain', { title: track.title })}
                  className={revealCls}
                  disabled={adding !== null}
                  onClick={async () => {
                    setAdding(track.uid)
                    await run(() => api.add(channelId, track.url), t('queue.added', { count: 1 }))
                    setAdding(null)
                  }}
                >
                  {adding === track.uid ? <Spinner size={14} /> : <RotateCcw size={15} />}
                </IconButton>
              )}
            </li>
          ))}
        </ul>
      )}
    </Card>
  )
}

export function QueueScreen() {
  const { t } = useTranslation()
  const { loaded, permissions, state } = useLynx()
  if (!loaded) return <PageSpinner />
  return (
    <div className="space-y-5">
      {permissions.canAdd && (
        <Card title={t('queue.addTitle')} description={t('queue.addHint')} icon={<Plus size={18} />}>
          <AddBox />
        </Card>
      )}
      {state?.ai?.job && <AiProgress />}
      <UpcomingCard />
      <HistoryCard />
    </div>
  )
}

/** Right column on wide screens: radio progress + the next tracks. */
export function UpNextPanel() {
  const { state, loaded } = useLynx()
  if (!loaded) return null
  return (
    <div className="space-y-5">
      {state?.ai?.job && <AiProgress />}
      <UpcomingCard limit={12} />
    </div>
  )
}
