import type { MountPosition, MountStatus, TrackingRate } from './link/protocol';

/*
 * Dead reckoning between `mount.position` events — SDD §5.9's predictive display, §8.3(6)/(8).
 *
 * # This is a render-time transform, and it must stay one
 *
 * Nothing here writes anything. §5.9's store discipline says the store holds observations and only
 * observations, and that rule extends to *time*: a predicted coordinate written into the store
 * would be indistinguishable, one function call later, from something the mount actually reported.
 * So the store keeps the last confirmed value and the instant it was measured, and this module
 * turns that pair plus the known mount state into what the screen should say *now*, on every
 * frame, from scratch. The prediction has no memory, which is why it cannot drift.
 *
 * # Which way the numbers run, and why it is the opposite of the obvious guess
 *
 * The intuitive answer — "a tracking mount is moving, so its coordinates change" — is exactly
 * backwards, and getting it wrong would make the display lie in the state the operator spends the
 * whole night in.
 *
 * §5.2.3: the mount's axes hold a mechanical **hour angle**, and RA is recovered as
 * `RA = LST − HA`. Local sidereal time advances whether or not the motors do. So:
 *
 *   * **Tracking at the sidereal rate**: the RA axis turns west at exactly the rate LST advances.
 *     `HA` grows as fast as `LST`, the difference is constant, and **RA holds still**. That is
 *     what tracking *is* — the mount is standing still relative to the sky.
 *   * **Idle, or tracking off**: the axis does not turn, `HA` is fixed, and **RA climbs at the
 *     full sidereal rate** — 1.00274 hours of RA per hour of clock. A stopped mount whose
 *     displayed RA advances is not a bug; it is what a stopped drive looks like, and it is the
 *     picture that tells an operator their tracking is off before the stars trail.
 *   * **Lunar and solar**: the Moon and Sun drift *east* against the stars, so both rates are
 *     *slower* than sidereal and the mount falls behind the star field. RA therefore creeps
 *     forward at the difference — 0.356″/s on lunar, 0.041″/s on solar.
 *
 * All three are one subtraction, `sidereal − axis rate`, which is why they are implemented as one
 * and not as three branches. The node's simulator stores its axes the same way and for the same
 * reason (`astroctl-drivers::simulator::mount`, `tracking_holds_right_ascension_and_idling_lets_
 * the_sky_slide_past`); the rate constants below are literally its constants, because a display
 * that dead-reckoned at a different rate than the drive would disagree with the mount on purpose.
 *
 * Declination does not move under any of these — a tracking mount corrects one axis. So DEC is
 * carried forward unchanged, which is a prediction that happens to be the identity.
 *
 * # What is deliberately NOT predicted
 *
 * **alt/az.** They change under both a tracking mount (a star rises and sets) and an idle one, and
 * computing them needs the site latitude and longitude, which the node has and this app does not
 * (§5.2.3 — the node computes alt/az with the same transform its altitude limit uses). Predicting
 * them here would mean a second, worse implementation of the number a slew is refused on. They are
 * shown as last reported, and the panel says so.
 *
 * **Anything at all while slewing.** During a goto the axes move at up to 835× sidereal, so a
 * model built on tracking rates would be wrong by degrees within a second. `mount.position` keeps
 * its 1 Hz cadence through a slew, so the truth channel is still the event stream: the display
 * holds the last confirmed numbers and says the mount is in motion. This is the one state where
 * showing a *frozen* number is more honest than showing a moving one.
 *
 * # The guardrail is the safety property, not a nicety
 *
 * Beyond [`PREDICT_LIMIT_MS`] — five times `mount.position`'s 1 Hz cadence (§4.3) — the model
 * stops and the display says stale. A dead-reckoner with no limit will happily advance RA for an
 * hour after the tunnel died, producing a number that is smooth, plausible, and completely
 * invented. The limit is a constant rather than something derived from the *observed* cadence
 * because a link delivering one position every five seconds is not a link with a slower contract,
 * it is a degraded link, and the operator is entitled to see that rather than have the app quietly
 * lower its standards.
 */

/** `mount.position`'s cadence — §4.3, 1 Hz. Every threshold below is quoted against it. */
export const POSITION_CADENCE_MS = 1_000;

/**
 * How long a report still counts as a report.
 *
 * One and a half cadences, not one: at exactly one the label would flicker between confirmed and
 * predicted on every ordinary jitter, and a marker that blinks during healthy operation is one the
 * operator learns to ignore before the night it means something. It stays well inside §8.3's 3 s
 * amber, so "predicted" appears before "the link is slow" — an annotation, not a warning.
 */
export const CONFIRMED_MS = 1_500;

/** §8.3(8)'s guardrail: five cadences with nothing, and the model stops. See the module docs. */
export const PREDICT_LIMIT_MS = 5 * POSITION_CADENCE_MS;

/**
 * How often a predicting readout should re-render.
 *
 * Fast enough that an idle mount's RA advances by roughly one unit of the last printed digit per
 * tick (the readout prints tenths of a second of RA, and the sky delivers one per 100 ms), slow
 * enough that it is five renders a second of one small card rather than an animation loop.
 */
export const PREDICT_TICK_MS = 200;

/** Length of the sidereal day, seconds — the definition, matching `simulator::profile`. */
const SIDEREAL_DAY_S = 86_164.0905;

/** Arcseconds of sky rotation per second of clock: 360° ÷ one sidereal day = 15.0410686″/s. */
const SIDEREAL_ARCSEC_PER_S = (360 * 3600) / SIDEREAL_DAY_S;

/**
 * Axis rate per tracking mode, arcseconds per second — `simulator::motion::tracking_rate`.
 *
 * Lunar and solar are the standard rates and are **slower** than sidereal, because the bodies they
 * follow move east against the stars. A table that made them faster would drift a lunar sequence
 * the wrong way, which is invisible until somebody images the Moon.
 */
const AXIS_ARCSEC_PER_S: Record<TrackingRate, number> = {
  sidereal: SIDEREAL_ARCSEC_PER_S,
  lunar: 14.685,
  solar: 15.0,
};

/** The subset of a telemetry slot the reckoning needs: a value and when it was true. */
export interface Observation<T> {
  /** Arrival on this device's clock. */
  at: number;
  /** The node's own timestamp for the measurement (RFC 3339). */
  ts: string;
  value: T;
}

/**
 * How the operator should read the numbers on screen.
 *
 * §5.9 names three — confirmed, predicted, stale — on a *freshness* axis. `moving` is a fourth,
 * and it is not a fourth level of freshness: a position measured 300 ms ago during an 800×
 * sidereal slew is perfectly fresh and already wrong by arcminutes. It is the state where the
 * model deliberately declines to run, so it needs a name of its own rather than borrowing one that
 * would promise extrapolation that is not happening.
 */
export type ReckoningKind = 'confirmed' | 'predicted' | 'moving' | 'stale';

export interface Reckoning {
  kind: ReckoningKind;
  /** Milliseconds since the node measured this position, corrected for clock skew. */
  ageMs: number;
  /**
   * Right ascension to display, hours in [0, 24).
   *
   * Carried forward under the model in both `confirmed` and `predicted` — the label says whether a
   * recent report backs it, not whether arithmetic was applied. Held at the last reported value in
   * `moving` and `stale`, where the model deliberately does not run.
   */
  ra: number;
  /** Declination to display, degrees. The model never moves it — see the module docs. */
  dec: number;
  /** Hours of RA per millisecond the model is applying. Zero while `moving` or `stale`. */
  rateHoursPerMs: number;
}

/**
 * How fast the *displayed* RA moves, in hours of right ascension per millisecond.
 *
 * `sidereal − axis`, per the module docs: zero while tracking sidereal, full sidereal while idle.
 *
 * A mount reporting `tracking: true` with no `tracking_mode` — a node older than this bundle, which
 * §5.8.3 treats as a normal condition — is read as sidereal. That is the conservative choice in the
 * only direction that matters: it predicts **no** motion, so an unknown rate can leave the display
 * a little behind, but it can never invent movement the mount is not making.
 */
export function raRateHoursPerMs(status: MountStatus | null): number {
  const axis =
    status !== null && status.tracking
      ? AXIS_ARCSEC_PER_S[status.tracking_mode ?? 'sidereal']
      : 0;
  // arcsec of RA → seconds of RA (÷15) → hours of RA (÷3600) → per millisecond (÷1000).
  return (SIDEREAL_ARCSEC_PER_S - axis) / 15 / 3600 / 1000;
}

/**
 * How stale the picture is, in milliseconds — §8.3(8)'s telemetry age.
 *
 * Measured from the node's `ts` and **corrected by the measured clock skew** (`lib/clock.ts`,
 * §5.8.1), because the node's timestamp is on the node's clock: a phone forty seconds fast would
 * otherwise report every event as forty seconds stale, which is the amber threshold thirteen times
 * over, on a link that is working perfectly.
 *
 * Two more things fold in, and both are floors rather than corrections:
 *
 *   * The age is never less than the time since the frame *arrived* on this device. That number
 *     needs no clock agreement at all, so it is the one thing here that cannot be argued with —
 *     and it is what stops a not-yet-measured skew (zero until the first `pong`, five seconds
 *     after connect) from making a picture look fresher than it demonstrably is.
 *   * On a healthy link the node-stamped age is the *larger* of the two, because it includes
 *     transit. That is the number §8.3 asks for: a position that arrived 100 ms ago but was
 *     measured a second before that is a second old, and on a two-second tunnel the difference is
 *     most of the staleness budget.
 *
 * An unparseable `ts` falls back to arrival age alone. A node that stamps something this client
 * cannot read is a reason to stop trusting the stamp, not a reason to stop showing an age.
 */
export function telemetryAgeMs(
  observation: { at: number; ts: string },
  nowMs: number = Date.now(),
  skewMs = 0,
): number {
  const sinceArrival = nowMs - observation.at;
  const stamped = Date.parse(observation.ts);
  if (Number.isNaN(stamped)) return Math.max(0, sinceArrival);
  return Math.max(0, sinceArrival, nowMs + skewMs - stamped);
}

/**
 * What to put on screen right now, given the last confirmed position and the mount's state.
 *
 * Pure, and called on every render tick. The extrapolation runs from the moment the event landed
 * rather than switching on at the [`CONFIRMED_MS`] boundary — otherwise the number would sit still
 * for a second and a half and then jump by a second and a half of RA, which reads as a glitch and
 * is *less* accurate than the smooth version in between. The boundary moves the **label**, not the
 * arithmetic: `confirmed` says a report backs this within the cadence, `predicted` says nothing
 * has arrived for longer than that and the app is carrying the last one forward.
 */
export function reckon(input: {
  position: Observation<MountPosition>;
  status: MountStatus | null;
  nowMs: number;
  skewMs?: number;
}): Reckoning {
  const { position, status, nowMs, skewMs = 0 } = input;
  const ageMs = telemetryAgeMs(position, nowMs, skewMs);
  const confirmed = { ra: position.value.ra, dec: position.value.dec };

  if (ageMs > PREDICT_LIMIT_MS) {
    return { kind: 'stale', ageMs, ...confirmed, rateHoursPerMs: 0 };
  }

  // A slew outruns any model built on tracking rates within a second (see the module docs), so the
  // numbers are held. `slewing` and `state` are checked together because either alone has been
  // seen unset on a mount mid-goto, and holding a number that did not need holding costs nothing.
  if (status !== null && (status.slewing || status.state === 'slewing')) {
    return { kind: 'moving', ageMs, ...confirmed, rateHoursPerMs: 0 };
  }

  const rateHoursPerMs = raRateHoursPerMs(status);
  const ra = wrapHours(confirmed.ra + rateHoursPerMs * ageMs);
  return {
    kind: ageMs > CONFIRMED_MS ? 'predicted' : 'confirmed',
    ageMs,
    ra,
    dec: confirmed.dec,
    rateHoursPerMs,
  };
}

/**
 * RA is a circle: an idle mount left overnight would otherwise print hour 27.
 *
 * The in-range early return is not an optimisation. `((h % 24) + 24) % 24` is not the identity in
 * floating point — it moves 5.0028 to 5.002800000000001 — and the value that matters most here is
 * the one at age zero, which is the number the mount just reported. Snapping onto a report has to
 * land on *exactly* that report, or every event introduces a hair of drift the app invented.
 */
function wrapHours(hours: number): number {
  if (hours >= 0 && hours < 24) return hours;
  return ((hours % 24) + 24) % 24;
}
