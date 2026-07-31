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
 * The five glyphs are chosen to differ in *fill and outline*, not just shape, so they survive
 * being 12 px tall on a phone held at arm's length.
 *
 * `degraded` is M1-T15's addition and is deliberately not folded into `starting`, even though both
 * are amber. They read identically to a colour-blind operator and mean opposite things: `starting`
 * is a link on its way up, `degraded` is one that is up and not keeping pace (§8.3(8)). Answering
 * "should I wait or should I distrust the screen" is the whole job of this badge.
 */
export type Status = 'ok' | 'degraded' | 'starting' | 'down' | 'unknown';

const GLYPH: Record<Status, string> = {
  ok: '●', // ● filled
  degraded: '◉', // ◉ filled, ringed — present but not keeping up
  starting: '◐', // ◐ half
  down: '⊘', // ⊘ hollow, slashed
  unknown: '○', // ○ hollow
};

const TONE: Record<Status, string> = {
  ok: 'text-ok',
  degraded: 'text-warn',
  starting: 'text-warn',
  down: 'text-danger',
  unknown: 'text-faint',
};

const WORDING: Record<Status, string> = {
  ok: 'ok',
  degraded: 'running slow',
  starting: 'starting',
  down: 'not reachable',
  unknown: 'not probed yet',
};

export function StatusBadge({
  label,
  status,
  detail,
  trailing,
}: {
  label: string;
  status: Status;
  /**
   * What to announce instead of the generic wording.
   *
   * The link badge knows *why* it is amber — a slow round trip and a picture that is behind are
   * different problems — and "running slow" alone would throw that away at the one moment it is
   * worth having.
   */
  detail?: string;
  /** Rendered after the label. The link badge's numerals (§8.3(8)) live here. */
  trailing?: ReactNode;
}): ReactNode {
  const wording = detail ?? WORDING[status];

  return (
    <span
      className="inline-flex items-center gap-1 text-sm"
      // The wording, not the glyph, is what a screen reader announces — and it is also the tooltip
      // for anyone who cannot tell ● from ◐.
      title={`${label}: ${wording}`}
    >
      <span aria-hidden="true" className={TONE[status]}>
        {GLYPH[status]}
      </span>
      <span className="text-muted">{label}</span>
      <span className="sr-only">{wording}</span>
      {trailing}
    </span>
  );
}
