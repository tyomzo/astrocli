import { useCallback, useEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent, ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import type { SlewAxis, SlewDirection, SlewSpeed } from '../lib/commands';
import { SLEW_SPEEDS } from '../lib/commands';
import { formatDegrees } from '../lib/coords';
import type { MountPosition } from '../lib/link/protocol';
import type { SlewHold } from '../lib/slewHold';
import { beginSlewHold } from '../lib/slewHold';
import type { Slot } from '../store/telemetry';
import { selectMountPosition, useTelemetryStore } from '../store/telemetry';
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
/*
 * The buttons name the **axis and its sign**, not compass directions.
 *
 * N/S/E/W are sky directions, and turning one into motor motion needs the pier side and the
 * hemisphere — a translation the driver performs correctly and the operator cannot see. Near the
 * pole, where this mount parks, the same compass word legitimately reverses which way declination
 * moves: press "N" past dec 90° and the tube goes over the top, so declination starts falling
 * again. The operator watched that happen and reasonably read it as a random direction bug. The
 * axis-and-sign labels cannot lie that way — DEC+ makes the DEC readout climb, at the pole or
 * anywhere else — and the sign matches the number on screen, which is the whole point.
 */
  direction: SlewDirection;
  glyph: string;
  label: string;
  cell: string;
}[] = [
  { axis: 'dec', direction: 'positive', glyph: 'DEC+', label: 'declination up', cell: 'col-start-2 row-start-1' },
  { axis: 'ra', direction: 'negative', glyph: 'RA−', label: 'right ascension down', cell: 'col-start-1 row-start-2' },
  { axis: 'ra', direction: 'positive', glyph: 'RA+', label: 'right ascension up', cell: 'col-start-3 row-start-2' },
  { axis: 'dec', direction: 'negative', glyph: 'DEC−', label: 'declination down', cell: 'col-start-2 row-start-3' },
];

/*
 * Travel from the home pose, shown while the D-pad is up — M3-T07.
 *
 * # Why this number is on screen at all
 *
 * A Synta mount has no soft limits and its axes will turn as long as they are told to. On
 * 2026-08-02 an operator holding these buttons wound the right-ascension axis 215.6° from home
 * without anything on screen changing to say so, and the only recovery was to power the mount
 * off, loosen the clutch and unwind it by hand. The axis counter is the one thing that knows,
 * and until now it was not published. The field node refuses a slew that would wind an axis past
 * `mount.limits.max_travel_from_home_degrees`, but a refusal that arrives with no warning is a
 * button that stops working for no visible reason — so the number goes where the thumb is.
 *
 * # It is a number, deliberately
 *
 * §5.9 forbids colour as the only channel. The strongest answer here is not a second channel but
 * the *first* one being quantitative: an operator can read 173° and decide, where a colour can
 * only say "concerned". Nothing here is colour-coded, so there is no colour to be alone. A
 * threshold marker would need the configured limit, which this payload does not carry — inventing
 * one in the client would draw a line the node does not enforce.
 *
 * # Degrees of axis, not of sky
 *
 * These are mechanical: the right-ascension axis angle is not an hour angle and the declination
 * axis angle is not a declination. The label says "from home" rather than naming a coordinate,
 * and the value is accumulated rotation — never folded, so half a turn reads as more than 180°
 * rather than wrapping back toward zero while the cable keeps winding.
 */
export function TravelReadout({ position }: { position: Slot<MountPosition> }): ReactNode {
  const travel = position.state === 'observed' ? position.value : null;
  return (
    <dl className="flex items-baseline justify-center gap-4 text-xs">
      <div className="flex items-baseline gap-1.5">
        <dt className="text-muted">RA from home</dt>
        <dd className="tabular font-mono text-fg">{formatDegrees(travel?.ra_travel ?? null)}</dd>
      </div>
      <div className="flex items-baseline gap-1.5">
        <dt className="text-muted">DEC from home</dt>
        <dd className="tabular font-mono text-fg">{formatDegrees(travel?.dec_travel ?? null)}</dd>
      </div>
      <span className="sr-only">
        How far each axis has been driven from the mount&apos;s home pose. The node refuses a nudge
        that would wind an axis further than the configured maximum.
      </span>
    </dl>
  );
}

export function NudgeOverlay({ onDismiss }: { onDismiss: () => void }): ReactNode {
  // Level 2 (8x sidereal) as the default: a nudge is a centering motion, and 8x is the fastest
  // rate measured turning cleanly (839 counts/s) with margin below the mount's standing-start
  // limit (E16: 32x turns, 39x jams). The ladder above stays reachable because the stall
  // threshold is a property of this mount, its balance and its load — the operator's ears remain
  // the only rotor sensor in the system.
  const [speed, setSpeed] = useState<SlewSpeed>(2);
  const [failure, setFailure] = useState<RequestFailure | null>(null);
  // Subscribed here rather than passed down from `ImageSurface`: `mount.position` ticks at 1 Hz
  // and the surface renders the frame, so reading it there would re-render the image every second
  // to move two small numbers (the note on the telemetry selectors makes this rule explicit).
  const position = useTelemetryStore(selectMountPosition);

  return (
    <div className="absolute inset-0 flex flex-col justify-between bg-surface/50 p-3 backdrop-blur-[1px]">
      <TravelReadout position={position} />

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
      className={`flex size-control touch-none items-center justify-center rounded-lg border-2 font-mono text-sm font-bold tracking-tight select-none ${className} ${
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
 * needs to know whether the next rung down is a little slower or eight times slower. This is the
 * ladder of rates this mount was *heard to start*: E16 (2026-08-02) measured that it will not
 * begin an unbounded slew above the rotor's standing-start limit — 32× turns, 39× jams, and a
 * driver-side ramp of the running axis is refused by the motor controller — under any speed
 * class. Faster manual motion is coming back as firmware-ramped bounded moves (the goto
 * mechanism, measured cruising at 835×), not as bigger numbers on this ladder.
 */
const SPEED_LABEL: Record<SlewSpeed, string> = {
  1: '1×',
  2: '8×',
  3: '16×',
  4: '24×',
  5: '32×',
};

/**
 * Rungs this mount was heard to stall on (`spikes/skywatcher-heq5/FINDINGS.md`, E11, E16).
 *
 * **Empty since 2026-08-02**: every rung on the ladder is at or below the 32× the mount was heard
 * to start cleanly, so none promises motion it refuses.
 *
 * Kept as a mechanism rather than deleted: the marking is how an operator carries forward a warning
 * the instruments cannot give them. A Synta counter counts *commanded* steps, so a stalled axis
 * reports the motion it did not make, and their ears remain the only rotor sensor in the system.
 * If a rung is ever heard to buzz, it goes back in here.
 */
const SPEED_STALLED: ReadonlySet<SlewSpeed> = new Set<SlewSpeed>([]);

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
          <span aria-hidden="true" className="font-mono text-xs leading-tight">
            {SPEED_LABEL[level]}
            {SPEED_STALLED.has(level) && '!'}
          </span>
        </button>
      ))}
    </div>
  );
}
