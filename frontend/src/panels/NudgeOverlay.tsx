import { useCallback, useEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent, ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import type { SlewAxis, SlewDirection, SlewSpeed } from '../lib/commands';
import { SLEW_SPEEDS } from '../lib/commands';
import type { SlewHold } from '../lib/slewHold';
import { beginSlewHold } from '../lib/slewHold';
import { useTokenStore } from '../store/token';
import { FailureNote } from '../ui/FailureNote';

/*
 * The D-pad — SDD §5.9, drawn **over the image surface** rather than beside it.
 *
 * Nudging is framing, so the control and the thing it affects have to be in one field of view. A
 * sibling panel makes the operator look back and forth between their hand and the result, at the
 * one moment when what they are doing is watching the result.
 *
 * It is summoned and never automatic: it does not appear when a slew completes, because the
 * operator has just waited out a slew to see the frame and covering it is precisely the wrong
 * thing to do. See `NudgeBadge`.
 *
 * # Press and hold is a lease, not a command
 *
 * Every direction is a dead-man's switch (§5.8.1): the press takes a 500 ms lease, the hold renews
 * it every 250 ms, and the release stops the axis. Silence — a dropped packet, a swallowed touch
 * event, a crashed tab — expires the lease and the field node stops the axis on its own. The
 * client's job is to make release *happen*, and there are four ways a hold can end that are not a
 * finger lifting: the pointer being cancelled by a scroll, capture being lost, the tab being
 * hidden, and the component unmounting. All four are wired below, because each one that is not is
 * an axis that keeps turning.
 */

const DIRECTIONS: readonly {
  axis: SlewAxis;
  direction: SlewDirection;
  glyph: string;
  label: string;
  cell: string;
}[] = [
  { axis: 'dec', direction: 'positive', glyph: 'N', label: 'north', cell: 'col-start-2 row-start-1' },
  { axis: 'ra', direction: 'negative', glyph: 'W', label: 'west', cell: 'col-start-1 row-start-2' },
  { axis: 'ra', direction: 'positive', glyph: 'E', label: 'east', cell: 'col-start-3 row-start-2' },
  { axis: 'dec', direction: 'negative', glyph: 'S', label: 'south', cell: 'col-start-2 row-start-3' },
];

export function NudgeOverlay({ onDismiss }: { onDismiss: () => void }): ReactNode {
  // Level 2 (8x sidereal), not 3. Level 3 is 64x, and on this HEQ5 the motor stalls there
  // unloaded: it ramps, the rotor loses sync, the axis buzzes and stops while the *counter keeps
  // counting the steps that never happened* — open loop, so nothing in software can see it. The
  // operator heard it. Levels 1 and 2 were measured turning cleanly (106 and 839 counts/s), so
  // the default is the fastest rate proven to move metal, and the ladder above it is left
  // reachable because the threshold is a property of this mount, its balance and its load — the
  // operator finds it by ear, which is what the labels below are for. FINDINGS' E11 was never
  // run; this is it, run by an operator's thumb.
  const [speed, setSpeed] = useState<SlewSpeed>(2);
  const [failure, setFailure] = useState<RequestFailure | null>(null);

  return (
    <div className="absolute inset-0 flex flex-col justify-between bg-surface/50 p-3 backdrop-blur-[1px]">
      <div className="flex flex-1 items-center justify-center">
        <div className="grid grid-cols-3 grid-rows-3 gap-2">
          {DIRECTIONS.map((entry) => (
            <NudgeButton
              key={entry.label}
              axis={entry.axis}
              direction={entry.direction}
              glyph={entry.glyph}
              label={entry.label}
              speed={speed}
              className={entry.cell}
              onRefused={setFailure}
            />
          ))}
        </div>
      </div>

      <div className="flex items-end justify-between gap-3">
        <SpeedSelector speed={speed} onChange={setSpeed} />
        <button
          type="button"
          onClick={onDismiss}
          aria-label="Hide the nudge controls"
          className="flex size-touch items-center justify-center rounded-full border border-edge-strong bg-surface/80 text-xl text-fg"
        >
          <span aria-hidden="true">✕</span>
        </button>
      </div>

      {failure !== null && (
        <div className="absolute inset-x-3 bottom-20">
          <FailureNote failure={failure} action="nudge" />
        </div>
      )}
    </div>
  );
}

/**
 * One direction.
 *
 * `touch-none` is not cosmetic: without it Android treats the drag of a held finger as a scroll,
 * cancels the pointer, and the hold ends without the operator releasing anything. `select-none`
 * and the suppressed context menu remove the other two ways a long press on a phone turns into
 * something that is not a long press.
 */
function NudgeButton({
  axis,
  direction,
  glyph,
  label,
  speed,
  className,
  onRefused,
}: {
  axis: SlewAxis;
  direction: SlewDirection;
  glyph: string;
  label: string;
  speed: SlewSpeed;
  className: string;
  onRefused: (failure: RequestFailure) => void;
}): ReactNode {
  const token = useTokenStore((state) => state.token);
  const hold = useRef<SlewHold | null>(null);
  const [holding, setHolding] = useState(false);

  const release = useCallback(() => {
    hold.current?.release();
    hold.current = null;
    setHolding(false);
  }, []);

  const press = (event: ReactPointerEvent<HTMLButtonElement>): void => {
    // Capture keeps the release on this element even if the finger slides off it — otherwise a
    // small movement during a hold ends with no `pointerup` here at all.
    event.currentTarget.setPointerCapture(event.pointerId);
    if (hold.current !== null) return;
    setHolding(true);
    hold.current = beginSlewHold({
      token,
      axis,
      direction,
      speed,
      onRefused: (failure) => {
        setHolding(false);
        onRefused(failure);
      },
    });
  };

  useEffect(() => {
    // The screen locking or the operator switching apps is the most common way a hold ends
    // outdoors, and neither fires `pointerup`.
    const onHidden = (): void => {
      if (document.visibilityState === 'hidden') release();
    };
    document.addEventListener('visibilitychange', onHidden);
    window.addEventListener('pagehide', release);
    return () => {
      document.removeEventListener('visibilitychange', onHidden);
      window.removeEventListener('pagehide', release);
      // Unmounting mid-hold — the overlay being dismissed, the mount going away — must stop too.
      release();
    };
  }, [release]);

  return (
    <button
      type="button"
      aria-label={`Nudge ${label}`}
      onPointerDown={press}
      onPointerUp={release}
      onPointerCancel={release}
      onLostPointerCapture={release}
      onContextMenu={(event) => event.preventDefault()}
      className={`flex size-control touch-none items-center justify-center rounded-lg border-2 text-xl font-bold select-none ${className} ${
        holding
          ? 'border-accent bg-accent text-on-accent'
          : 'border-edge-strong bg-overlay/90 text-fg'
      }`}
    >
      <span aria-hidden="true">{glyph}</span>
    </button>
  );
}

/**
 * `●●●○○ speed` from the sketch.
 *
 * Five ordinals, not five rates: what a step means in degrees per second belongs to the driver
 * (see `lib/commands.ts`). Rendered as filled and hollow dots so the setting survives night mode,
 * and as one button per level rather than a slider because a slider is the hardest control to hit
 * with a gloved thumb in the dark.
 */
/**
 * What each level actually commands, in multiples of the sidereal rate.
 *
 * Unlabelled dots are a rate control that cannot be reasoned about: an operator who hears a stall
 * needs to know whether the next rung down is a little slower or eight times slower. These are the
 * EQMOD ladder's own values, and only the first two are measured turning this mount (106 and 839
 * counts/s); the rest are reachable and unproven, which is exactly the information the labels
 * carry.
 */
const SPEED_LABEL: Record<SlewSpeed, string> = {
  1: '1×',
  2: '8×',
  3: '64×',
  4: '400×',
  5: '800×',
};

/**
 * Rungs this mount was heard to stall on (`spikes/skywatcher-heq5/FINDINGS.md`, E11).
 *
 * Marked, not removed. The threshold belongs to a mount with a particular balance and a particular
 * load — a counterweighted, payload-carrying rig may well take 64× — so hiding the rungs would
 * bake one evening's bare-mount measurement into the product. What the operator needs is the
 * warning they earned, carried forward so they do not have to remember which dot buzzed: the
 * counter cannot tell them, because a Synta counter counts commanded steps and a stalled axis
 * reports the motion it did not make.
 */
const SPEED_STALLED: ReadonlySet<SlewSpeed> = new Set<SlewSpeed>([3, 4, 5]);

function SpeedSelector({
  speed,
  onChange,
}: {
  speed: SlewSpeed;
  onChange: (speed: SlewSpeed) => void;
}): ReactNode {
  return (
    <div className="flex items-center gap-1 rounded-full bg-surface/80 px-2 py-1 backdrop-blur">
      <span className="sr-only">Slew speed</span>
      {SLEW_SPEEDS.map((level) => (
        <button
          key={level}
          type="button"
          aria-label={`Speed ${SPEED_LABEL[level]} sidereal${SPEED_STALLED.has(level) ? ' — heard to skip on this mount' : ''}`}
          aria-pressed={level === speed}
          onClick={() => onChange(level)}
          className={`flex min-h-touch min-w-touch flex-col items-center justify-center px-1 ${
            level === speed ? 'text-accent' : SPEED_STALLED.has(level) ? 'text-warn/70' : 'text-faint'
          }`}
          title={SPEED_STALLED.has(level) ? 'this mount was heard to skip at this rate' : undefined}
        >
          <span aria-hidden="true" className="text-lg leading-none">
            {level <= speed ? '●' : '○'}
          </span>
          <span aria-hidden="true" className="font-mono text-[0.6rem] leading-tight">
            {SPEED_LABEL[level]}
            {SPEED_STALLED.has(level) && '!'}
          </span>
        </button>
      ))}
    </div>
  );
}
