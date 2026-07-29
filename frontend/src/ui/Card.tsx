import type { ReactNode } from 'react';

/**
 * A titled panel. One of the two layout primitives M1's panels build on (the other is
 * [`PanelGrid`](./PanelGrid.tsx)).
 *
 * The visual separation comes from the border, not from a luminance step: on a true-black surface
 * outdoors, a raised grey reads as "smudge" long before it reads as "panel".
 */
export function Card({
  title,
  accessory,
  children,
}: {
  title: string;
  accessory?: ReactNode;
  children: ReactNode;
}): ReactNode {
  return (
    <section className="rounded-lg border border-edge bg-raised">
      <header className="flex min-h-touch items-center justify-between gap-3 border-b border-edge px-4 py-2">
        <h2 className="text-sm font-semibold tracking-wide text-muted uppercase">{title}</h2>
        {accessory}
      </header>
      <div className="px-4 py-3">{children}</div>
    </section>
  );
}

/** A `label — value` row. `value` is monospaced and tabular so a column of them does not twitch. */
export function Field({ label, value }: { label: string; value: ReactNode }): ReactNode {
  return (
    <div className="flex items-baseline justify-between gap-4 py-1">
      <dt className="text-sm text-muted">{label}</dt>
      <dd className="tabular font-mono text-sm text-fg">{value}</dd>
    </div>
  );
}
