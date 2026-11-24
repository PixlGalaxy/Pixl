import mark from '../assets/lynx-mark.svg'

/** Lynx mark drawn with the current text color (CSS mask over the bundled SVG). */
export function LynxMark({ className = 'size-7' }: { className?: string }) {
  return (
    <span
      aria-hidden
      className={`inline-block shrink-0 bg-current ${className}`}
      style={{
        maskImage: `url(${mark})`,
        WebkitMaskImage: `url(${mark})`,
        maskSize: 'contain',
        WebkitMaskSize: 'contain',
        maskRepeat: 'no-repeat',
        WebkitMaskRepeat: 'no-repeat',
        maskPosition: 'center',
        WebkitMaskPosition: 'center',
      }}
    />
  )
}
