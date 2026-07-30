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
  const [speed, setSpeed] = useState<SlewSpeed>(3);
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
          aria-label={`Speed ${level} of ${SLEW_SPEEDS.length}`}
          aria-pressed={level === speed}
          onClick={() => onChange(level)}
          className={`flex size-touch items-center justify-center text-lg ${
            level <= speed ? 'text-accent' : 'text-faint'
          }`}
        >
          <span aria-hidden="true">{level <= speed ? '●' : '○'}</span>
        </button>
      ))}
    </div>
  );
}
