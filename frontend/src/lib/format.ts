/** Rendering helpers. SDD §2 keeps local time out of the backbone, so it is produced here. */

/** `3d 04:17:09` — an operator reads uptime to answer "did it restart", so days matter. */
export function uptime(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const days = Math.floor(total / 86_400);
  const hh = pad(Math.floor((total % 86_400) / 3600));
  const mm = pad(Math.floor((total % 3600) / 60));
  const ss = pad(total % 60);
  return days > 0 ? `${days}d ${hh}:${mm}:${ss}` : `${hh}:${mm}:${ss}`;
}

/**
 * `12.4 GB`, or an explicit unknown.
 *
 * `null` is rendered as "unavailable" rather than as 0: the field node deliberately reports null
 * when it cannot interrogate the volume, and showing 0 GB free would trip every alarm an operator
 * has for a condition that is not happening.
 */
export function diskFree(gb: number | null): string {
  return gb === null ? 'unavailable' : `${gb.toFixed(1)} GB`;
}

/** How long ago, in the units a glance needs. Used for observation age (SDD §8.3). */
export function age(atMs: number, nowMs: number): string {
  const seconds = Math.max(0, Math.round((nowMs - atMs) / 1000));
  if (seconds < 60) {
    return `${seconds}s ago`;
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m ago`;
  }
  return `${Math.floor(seconds / 3600)}h ago`;
}

/** `21:14:07 UTC`. */
export function clockUtc(date: Date): string {
  return `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())}`;
}

function pad(value: number): string {
  return value.toString().padStart(2, '0');
}
