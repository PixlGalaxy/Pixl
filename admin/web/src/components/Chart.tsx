import { useId, useRef, useState } from 'react'

/**
 * Column sparkline of one counter over time: one series in the accent ink, a recessive baseline,
 * a hover tooltip with the exact value. The card title names the series (no legend for one series).
 */
export function Columns({
  values,
  label,
  tooltip,
  height = 56,
}: {
  values: number[]
  /** Accessible name, e.g. "Slash commands, last hour". */
  label: string
  /** Tooltip text for column `i`. */
  tooltip: (i: number, value: number) => string
  height?: number
}) {
  const [hover, setHover] = useState<number | null>(null)
  const box = useRef<HTMLDivElement>(null)
  const id = useId()
  const max = Math.max(1, ...values)
  const n = Math.max(1, values.length)
  // viewBox units: each column is 10 wide with a 2-unit gap (thin marks, separated fills).
  const W = n * 10
  const H = 100
  const onMove = (e: React.PointerEvent) => {
    const r = box.current?.getBoundingClientRect()
    if (!r || r.width === 0) return
    const i = Math.floor(((e.clientX - r.left) / r.width) * n)
    setHover(Math.min(n - 1, Math.max(0, i)))
  }
  const tip = hover !== null ? tooltip(hover, values[hover] ?? 0) : null
  return (
    <div
      ref={box}
      className="relative"
      style={{ height }}
      onPointerMove={onMove}
      onPointerLeave={() => setHover(null)}
      role="img"
      aria-label={label}
      aria-describedby={tip ? id : undefined}
    >
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="block size-full overflow-visible">
        <line x1={0} y1={H} x2={W} y2={H} className="stroke-grid" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        {values.map((v, i) => {
          const h = v === 0 ? 0 : Math.max(3, (v / max) * (H - 4))
          return (
            <rect
              key={i}
              x={i * 10 + 1}
              y={H - h}
              width={8}
              height={h}
              rx={1.5}
              className={hover === null || hover === i ? 'fill-chart' : 'fill-chart opacity-40'}
            />
          )
        })}
      </svg>
      {tip && hover !== null && (
        <div
          id={id}
          role="tooltip"
          className="pointer-events-none absolute -top-8 z-10 -translate-x-1/2 rounded-md border border-line bg-surface px-2 py-1 text-xs whitespace-nowrap text-fg shadow-md"
          style={{ left: `${((hover + 0.5) / n) * 100}%` }}
        >
          {tip}
        </div>
      )}
    </div>
  )
}
