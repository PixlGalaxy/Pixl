import type { CSSProperties } from 'react'

/**
 * One-shot reactions behind the Like / Dislike buttons in Now playing: confetti bursting up for a
 * Like, a few annoyed faces floating up for a Dislike. Rendered behind the button (it is opaque),
 * so the particles only show once they leave its edges. `burst` changes on every new vote, which
 * remounts the particles and restarts the animation. Nothing renders under reduced motion.
 */

const COLORS = ['#f5b70a', '#4ade80', '#38bdf8', '#f472b6', '#a78bfa', '#fb923c']

type Vars = CSSProperties & Record<`--${string}`, string>

function reducedMotion(): boolean {
  try {
    return window.matchMedia('(prefers-reduced-motion: reduce)').matches
  } catch {
    return false
  }
}

export function Confetti({ burst }: { burst: number }) {
  if (burst === 0 || reducedMotion()) return null
  return (
    <span key={burst} aria-hidden className="pointer-events-none absolute inset-0 z-0">
      {Array.from({ length: 18 }, (_, i) => {
        const angle = ((-90 + (i - 8.5) * 9) * Math.PI) / 180
        const dist = 55 + ((i * 37) % 45)
        const round = i % 3 === 0
        const style: Vars = {
          width: round ? 5 : 7,
          height: round ? 5 : 3,
          borderRadius: round ? 999 : 1,
          background: COLORS[i % COLORS.length],
          '--dx': `${Math.round(Math.cos(angle) * dist * 0.9)}px`,
          '--dy': `${Math.round(Math.sin(angle) * dist - 10)}px`,
          '--r': `${(i * 47) % 360}deg`,
          animationDelay: `${(i % 4) * 0.02}s`,
        }
        return <span key={i} className="lynx-particle lynx-confetti" style={style} />
      })}
    </span>
  )
}

function AnnoyedFace() {
  return (
    <svg viewBox="0 0 36 36" width="100%" height="100%">
      <circle cx="18" cy="18" r="16" fill="#ffcc4d" stroke="#e0a412" strokeWidth="1.2" />
      <path d="M9 13.5 L15 15.5" stroke="#664500" strokeWidth="2.4" strokeLinecap="round" />
      <path d="M27 13.5 L21 15.5" stroke="#664500" strokeWidth="2.4" strokeLinecap="round" />
      <ellipse cx="12.5" cy="18.5" rx="2" ry="1.4" fill="#664500" />
      <ellipse cx="23.5" cy="18.5" rx="2" ry="1.4" fill="#664500" />
      <path d="M12 26.5 Q18 23.5 24 26.5" stroke="#664500" strokeWidth="2.4" strokeLinecap="round" fill="none" />
    </svg>
  )
}

export function AnnoyedFaces({ burst }: { burst: number }) {
  if (burst === 0 || reducedMotion()) return null
  return (
    <span key={burst} aria-hidden className="pointer-events-none absolute inset-0 z-0">
      {Array.from({ length: 6 }, (_, k) => {
        const size = 18 + (k % 3) * 5
        const style: Vars = {
          width: size,
          height: size,
          '--dx': `${Math.round((k - 2.5) * 22 + ((k * 17) % 11) - 5)}px`,
          '--dy': `${-(70 + ((k * 23) % 40))}px`,
          '--r': `${k % 2 ? 12 : -12}deg`,
          animationDelay: `${k * 0.07}s`,
        }
        return (
          <span key={k} className="lynx-particle lynx-annoyed" style={style}>
            <AnnoyedFace />
          </span>
        )
      })}
    </span>
  )
}
