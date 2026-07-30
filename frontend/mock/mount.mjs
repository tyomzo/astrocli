/*
 * DEV-ONLY MOCK — M1-T03 DELETES THIS DIRECTORY.
 *
 * A simulated mount, good enough to drive the PWA's real states. It is NOT astroctl-drivers'
 * SimulatorMount (M1-T02) and must never be confused for it: this one lives in the frontend tree,
 * speaks JSON, and exists only so M1-T04 could be built and demonstrated before the field node's
 * mount routes existed. When M1-T03 lands, `npm run mock` is replaced by running the real binary
 * and `frontend/mock/` is deleted whole.
 *
 * What it does simulate faithfully, because the UI's correctness depends on it:
 *
 *  * **A goto is a ramp, not a jump.** The panel's in-motion states (`slewing`, the nudge badge
 *    going unavailable, tracking suspended and restored) only exist for the seconds a slew takes.
 *    A mock that teleported would have let all of them ship broken.
 *  * **Tracking off means the sky moves.** A stationary mount holds alt/az, so the RA it points at
 *    increases at the sidereal rate. Coordinates that froze when tracking stopped would have made
 *    the tracking control look like it did nothing.
 *  * **The slew TTL expires.** SDD §5.8.1's dead-man's switch is the one behaviour where the
 *    client being right matters more than the server: if the PWA's renewal loop is wrong, the axis
 *    stops mid-hold and an alert appears. That has to be reproducible on a laptop.
 */

/** Site the mock observes from. Only alt/az and the limit check depend on it. */
const SITE = { latitudeDeg: 50.0, longitudeDeg: 8.0 };

/** Sidereal rate in RA-hours per second of UTC. */
const SIDEREAL_HOURS_PER_SECOND = 1.0027379 / 3600;

/** Slew speeds 1..5 as degrees per second — the five dots of the SDD §5.9 sketch. */
const SLEW_RATES_DEG_PER_S = [0.02, 0.1, 0.5, 1.5, 4.0];

/** Angular speed of a goto, degrees per second. */
const GOTO_DEG_PER_S = 6.0;

/** SDD §5.4 / MNT-15's altitude floor, as the mock's config would carry it. */
const MIN_ALTITUDE_DEG = 15.0;

/** M42, so the panel has something real on it the moment it opens. */
const INITIAL = { raHours: 5.588056, decDegrees: -5.391111 };

const DEG = Math.PI / 180;

export function julianDay(msSinceEpoch) {
  return msSinceEpoch / 86_400_000 + 2_440_587.5;
}

/** Local apparent sidereal time, hours in [0, 24). Mean sidereal is plenty for a mock. */
export function localSiderealHours(msSinceEpoch, longitudeDeg = SITE.longitudeDeg) {
  const d = julianDay(msSinceEpoch) - 2_451_545.0;
  const gmst = 18.697_374_558 + 24.065_709_824_419_08 * d;
  return mod(gmst + longitudeDeg / 15, 24);
}

/**
 * Equatorial → horizontal.
 *
 * The real one is M1-T05's, shared with the altitude limit so a display bug and a limit bug
 * cannot disagree (that task's acceptance criterion). This is the textbook transform, accurate to
 * a few arcminutes, which is far beyond what a mock needs.
 */
export function altAz(raHours, decDegrees, msSinceEpoch) {
  const haDeg = mod(localSiderealHours(msSinceEpoch) - raHours, 24) * 15;
  const ha = haDeg * DEG;
  const dec = decDegrees * DEG;
  const lat = SITE.latitudeDeg * DEG;

  const sinAlt = Math.sin(dec) * Math.sin(lat) + Math.cos(dec) * Math.cos(lat) * Math.cos(ha);
  const alt = Math.asin(clamp(sinAlt, -1, 1));

  const cosAz =
    (Math.sin(dec) - Math.sin(alt) * Math.sin(lat)) / (Math.cos(alt) * Math.cos(lat) || 1e-9);
  const az = Math.acos(clamp(cosAz, -1, 1));

  return {
    altDegrees: alt / DEG,
    azDegrees: Math.sin(ha) > 0 ? 360 - az / DEG : az / DEG,
    /** Counterweight west, looking east, while the target is east of the meridian. */
    pierSide: haDeg > 180 ? 'east' : 'west',
  };
}

/**
 * The mount.
 *
 * `publish(topic, data)` goes straight to the WS hub and the snapshot builder; `alert(severity,
 * code, message)` is the §4.3 `alert` topic. Both are injected so this file has no idea a socket
 * exists.
 */
export function createMount({ publish, alert }) {
  const state = {
    connected: true,
    tracking: 'sidereal',
    raHours: INITIAL.raHours,
    decDegrees: INITIAL.decDegrees,
    /** Set while a goto is running. */
    goto: null,
    /** Per-axis manual slew lease — SDD §5.8.1's dead-man's switch. */
    leases: new Map(),
    lastTickMs: Date.now(),
  };

  let lastStatusJson = '';

  function lifecycleState() {
    if (!state.connected) return 'disconnected';
    if (state.goto !== null || state.leases.size > 0) return 'slewing';
    return 'idle';
  }

  function status() {
    const lifecycle = lifecycleState();
    return {
      state: lifecycle,
      // The mount is not tracking while a goto is in flight; the rate resumes when it settles.
      tracking: state.connected && state.tracking !== 'off' && state.goto === null,
      slewing: lifecycle === 'slewing',
      parked: lifecycle === 'parked',
    };
  }

  function position(nowMs = Date.now()) {
    const horizontal = altAz(state.raHours, state.decDegrees, nowMs);
    return {
      ra: round(state.raHours, 6),
      dec: round(state.decDegrees, 6),
      alt: round(horizontal.altDegrees, 3),
      az: round(horizontal.azDegrees, 3),
      pier_side: horizontal.pierSide,
    };
  }

  /** Publish `mount.status` only when it actually changed — §4.3 says "on change". */
  function publishStatusIfChanged() {
    const next = status();
    const json = JSON.stringify(next);
    if (json !== lastStatusJson) {
      lastStatusJson = json;
      publish('mount.status', next);
    }
  }

  function advance(nowMs) {
    const dt = Math.max(0, (nowMs - state.lastTickMs) / 1000);
    state.lastTickMs = nowMs;
    if (!state.connected) return;

    if (state.goto !== null) {
      const run = state.goto;
      const t = clamp((nowMs - run.startedMs) / run.durationMs, 0, 1);
      // Smoothstep, so the position stream shows a mount accelerating and settling rather than
      // sliding at a constant rate. The settle is what the operator watches for.
      const eased = t * t * (3 - 2 * t);
      state.raHours = mod(run.fromRa + shortestRaDelta(run.fromRa, run.toRa) * eased, 24);
      state.decDegrees = run.fromDec + (run.toDec - run.fromDec) * eased;
      if (t >= 1) {
        state.goto = null;
      }
      return;
    }

    for (const [axis, lease] of state.leases) {
      if (nowMs > lease.expiresAtMs) {
        state.leases.delete(axis);
        alert(
          'warning',
          'SLEW_TTL_EXPIRED',
          `manual slew on the ${axis} axis stopped: no renewal within ${lease.ttlMs} ms`,
        );
        continue;
      }
      const degrees = SLEW_RATES_DEG_PER_S[lease.speed - 1] * dt * lease.sign;
      if (axis === 'ra') {
        state.raHours = mod(state.raHours + degrees / 15, 24);
      } else {
        state.decDegrees = clamp(state.decDegrees + degrees, -90, 90);
      }
    }

    // A stationary mount holds alt/az, so the sky slides past it: the RA it points at climbs at
    // the sidereal rate. Only true when nothing else is driving the axes.
    if (state.tracking === 'off' && state.leases.size === 0) {
      state.raHours = mod(state.raHours + SIDEREAL_HOURS_PER_SECOND * dt, 24);
    }
  }

  return {
    status,
    position,
    isConnected: () => state.connected,

    /**
     * Called at 1 Hz by the server; the only place `mount.status` changes are noticed.
     *
     * It deliberately does **not** publish `mount.position`: the server owns that, because the
     * server is what knows whether alt/az are being reported (the M1-T03 shape) or nulled. Having
     * both publish it is how the first run of this mock emitted every position twice.
     */
    tick(nowMs = Date.now()) {
      advance(nowMs);
      publishStatusIfChanged();
    },

    /** Faster than the event cadence so a TTL expiry lands within its own deadline. */
    subTick(nowMs = Date.now()) {
      if (state.leases.size > 0 || state.goto !== null) {
        advance(nowMs);
        publishStatusIfChanged();
      }
    },

    connect() {
      state.connected = true;
      state.lastTickMs = Date.now();
      publishStatusIfChanged();
      return status();
    },

    disconnect() {
      state.connected = false;
      state.goto = null;
      state.leases.clear();
      publishStatusIfChanged();
      return status();
    },

    setTracking(mode) {
      state.tracking = mode;
      publishStatusIfChanged();
      return status();
    },

    /** `{ok: true, ...}` or `{ok: false, status, code, message}` — the server maps the latter. */
    startGoto(raHours, decDegrees) {
      if (!state.connected) {
        return { ok: false, status: 409, code: 'NOT_CONNECTED', message: 'the mount is not connected' };
      }
      if (state.goto !== null) {
        return { ok: false, status: 409, code: 'BUSY', message: 'a goto is already in flight' };
      }
      const target = altAz(raHours, decDegrees, Date.now());
      if (target.altDegrees < MIN_ALTITUDE_DEG) {
        return {
          ok: false,
          status: 403,
          code: 'LIMIT_ALTITUDE',
          message: `target altitude ${target.altDegrees.toFixed(1)}° is below the ${MIN_ALTITUDE_DEG}° limit`,
        };
      }

      state.leases.clear();
      const separation = Math.hypot(
        shortestRaDelta(state.raHours, raHours) * 15 * Math.cos(state.decDegrees * DEG),
        decDegrees - state.decDegrees,
      );
      state.goto = {
        startedMs: Date.now(),
        durationMs: Math.max(2000, (separation / GOTO_DEG_PER_S) * 1000),
        fromRa: state.raHours,
        fromDec: state.decDegrees,
        toRa: raHours,
        toDec: decDegrees,
      };
      publishStatusIfChanged();
      return { ok: true, correlationId: randomId(), watchTopic: 'mount.position' };
    },

    /** One dead-man lease. A repeat with identical parameters extends it (SDD §5.8.1). */
    slew(axis, direction, speed, ttlMs) {
      if (!state.connected) {
        return { ok: false, status: 409, code: 'NOT_CONNECTED', message: 'the mount is not connected' };
      }
      if (state.goto !== null) {
        return { ok: false, status: 409, code: 'BUSY', message: 'a goto is in flight' };
      }
      state.leases.set(axis, {
        sign: direction === 'positive' ? 1 : -1,
        speed,
        ttlMs,
        expiresAtMs: Date.now() + ttlMs,
      });
      publishStatusIfChanged();
      return { ok: true, axis, expires_in_ms: ttlMs };
    },

    stopSlew(axis) {
      if (axis === undefined) {
        state.leases.clear();
      } else {
        state.leases.delete(axis);
      }
      publishStatusIfChanged();
      return status();
    },
  };
}

export const SLEW_SPEEDS = SLEW_RATES_DEG_PER_S.length;

function shortestRaDelta(fromHours, toHours) {
  const delta = mod(toHours - fromHours, 24);
  return delta > 12 ? delta - 24 : delta;
}

function mod(value, modulus) {
  return ((value % modulus) + modulus) % modulus;
}

function clamp(value, low, high) {
  return Math.min(high, Math.max(low, value));
}

function round(value, digits) {
  const scale = 10 ** digits;
  return Math.round(value * scale) / scale;
}

function randomId() {
  return [...crypto.getRandomValues(new Uint8Array(8))]
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}
