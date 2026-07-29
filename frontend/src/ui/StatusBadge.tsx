import type { ReactNode } from 'react';

/**
 * A connection/health indicator for the header strip (USB-04).
 *
 * **The glyph carries the state; colour is the second channel, never the only one.** SDD §5.9
 * spells out why for the nudge badge and the reasoning is general: night mode collapses every hue
 * toward red, so `--ok` and `--danger` become two shades of the same colour, and ~8% of men have
 * a red-green deficiency in any lighting. A badge that is only coloured is unreadable in exactly
 * the conditions this app exists for.
 *
 * The four glyphs are chosen to differ in *fill and outline*, not just shape, so they survive
 * being 12 px tall on a phone held at arm's length.
 */
export type Status = 'ok' | 'starting' | 'down' | 'unknown';

const GLYPH: Record<Status, string> = {
  ok: '●', // ● filled
  starting: '◐', // ◐ half
  down: '⊘', // ⊘ hollow, slashed
  unknown: '○', // ○ hollow
};

const TONE: Record<Status, string> = {
  ok: 'text-ok',
  starting: 'text-warn',
  down: 'text-danger',
  unknown: 'text-faint',
};

const WORDING: Record<Status, string> = {
  ok: 'ok',
  starting: 'starting',
  down: 'not reachable',
  unknown: 'not probed yet',
};

export function StatusBadge({ label, status }: { label: string; status: Status }): ReactNode {
  return (
    <span
      className="inline-flex items-center gap-1 text-sm"
      // The wording, not the glyph, is what a screen reader announces — and it is also the tooltip
      // for anyone who cannot tell ● from ◐.
      title={`${label}: ${WORDING[status]}`}
    >
      <span aria-hidden="true" className={TONE[status]}>
        {GLYPH[status]}
      </span>
      <span className="text-muted">{label}</span>
      <span className="sr-only">{WORDING[status]}</span>
    </span>
  );
}
