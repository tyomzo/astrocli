import type { ButtonHTMLAttributes, ReactNode } from 'react';

/**
 * The touch-target contract in one place — USB-12, SDD §5.9.
 *
 * `incidental` is the 44 px floor. `primary` is the 60–70 px band, and it is not a style choice:
 * the operator may be wearing gloves, which materially reduces pointing precision, and every
 * control that starts or stops something physical belongs in that band. The e-stop is larger
 * still and is not built from this component — see `app/EStopButton.tsx`.
 *
 * Sizes come from `--spacing-touch` / `--spacing-control` in the token layer, so a reviewer can
 * check the requirement by reading the variant name instead of measuring.
 */
type Variant = 'incidental' | 'primary';

const SIZE: Record<Variant, string> = {
  incidental: 'min-h-touch px-4 text-sm',
  primary: 'min-h-control px-6 text-base font-semibold',
};

export function Button({
  variant = 'incidental',
  className = '',
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  children: ReactNode;
}): ReactNode {
  return (
    <button
      type="button"
      className={`inline-flex items-center justify-center rounded-md border border-edge-strong bg-overlay text-fg transition-colors active:bg-accent active:text-on-accent disabled:opacity-40 ${SIZE[variant]} ${className}`}
      {...rest}
    >
      {children}
    </button>
  );
}
