import type { ReactNode } from 'react';

/**
 * The two reserved regions under the image surface — SDD §5.9's four-slot table, rows 2 and 3.
 *
 * The tablet sketch draws both, in this order, directly beneath the image:
 *
 * ```text
 *   └─────────────────────────────╰─⊕─╯─────┘
 *   ( 2b: stretch ▁▃▅  σ-clip 2.5/3.0 [apply] )
 *   ( re-stacking 34/120 ▓▓▓▓░░░░  ← IPP-16 )
 * ```
 *
 * They are **empty in M1 and present anyway**, which is the whole point of a slot: the four-slot
 * table exists "so later phases fill structure rather than replace it". A region that appears for
 * the first time in 2b would push the capture strip down and move every control the operator has
 * learned the position of, in the dark, mid-session.
 *
 * # Why they are labelled rather than blank
 *
 * A blank rectangle under the image is indistinguishable from a rendering bug, and an operator who
 * reports one costs more than the label. Each says what fills it and when, in the operator's terms
 * — not "TODO", which tells them the build is unfinished rather than that the feature is a phase
 * away.
 *
 * # Why there are no disabled controls here
 *
 * §5.9's M1 row: "**No knobs** — the stub does no stacking, so there is nothing to tune". Greyed-out
 * sliders would be worse than empty space: they promise a capability the build does not have, and
 * an operator who drags one and sees nothing happen has learned that this panel lies. The task's
 * no-knobs rule is about the *promise*, not just the wiring.
 */
export function StackControlsSlot(): ReactNode {
  return (
    <section
      aria-label="Stacking controls"
      className="rounded-md border border-dashed border-edge px-3 py-2"
    >
      <p className="text-sm text-faint">
        No stacking controls yet. This build stores and previews frames; it does not stack them, so
        there is nothing to tune. Method, rejection and stretch arrive with real stacking.
      </p>
    </section>
  );
}

/**
 * The rebuilding indicator — reserved, and **never fires in M1**.
 *
 * §5.9 states the problem it solves, and the reason it has to exist before it can fire: IPP-16
 * re-stacks in the background while capture continues, and ADD §5.4.2 keeps the preview serving
 * the *pre-rebuild* image until the swap. So changing a setting and seeing the picture not change
 * is correct behaviour that looks exactly like a bug — "the panel needs an explicit rebuilding
 * state with progress, or the operator turns the knob again, and again".
 *
 * Designing it in later means discovering that as a support question, which is why the shape is
 * fixed now: a labelled region, a progress bar, and a count of frames re-stacked out of the total,
 * exactly as the sketch draws `re-stacking 34/120 ▓▓▓▓░░░░`.
 *
 * `progress` is the whole API. Passing one renders the real indicator; passing `null` — which is
 * everything M1 can do, because nothing on the wire reports a rebuild — renders the reserved
 * space. Phase 2b fills this from the stack node and changes nothing else.
 */
export interface RebuildProgress {
  /** Frames re-stacked so far. */
  done: number;
  /** Frames in the rebuild. */
  total: number;
}

export function RebuildingSlot({ progress }: { progress: RebuildProgress | null }): ReactNode {
  if (progress === null) {
    // Reserved, not hidden: the space is held so the layout does not move when 2b fills it.
    // `aria-hidden` because there is nothing here to announce — a screen reader reading "reserved"
    // on every pass would be noise, and the region carries no information until it fires.
    return <div aria-hidden="true" className="h-8" />;
  }

  const pct =
    progress.total > 0 ? Math.min(100, Math.round((progress.done / progress.total) * 100)) : 0;

  return (
    <section aria-label="Re-stacking" className="flex items-center gap-3">
      {/*
        `role="status"`, not `alert`: this is progress, not a problem. It is polite by design —
        the operator should notice it when they look, not be interrupted by it.
      */}
      <p role="status" className="text-sm text-fg">
        Re-stacking{' '}
        <span className="tabular font-mono">
          {progress.done}/{progress.total}
        </span>
      </p>
      <div
        role="progressbar"
        aria-valuenow={progress.done}
        aria-valuemin={0}
        aria-valuemax={progress.total}
        className="h-2 flex-1 overflow-hidden rounded-full bg-raised"
      >
        <div className="h-full bg-accent" style={{ width: `${pct}%` }} />
      </div>
    </section>
  );
}
