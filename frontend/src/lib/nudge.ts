import type { LinkPhase, Slot } from '../store/telemetry';
import type { MountStatus } from './link/protocol';

/**
 * Whether the nudge D-pad can be summoned, and if not, what to tell the operator — SDD §5.9.
 *
 * The badge in the image surface's bottom-right corner shows this *before* it is tapped, because
 * "the mount is disconnected" discovered by pressing a control and getting nothing is the
 * frustrating case §5.9 names. Tapping an unavailable badge must explain itself, so every
 * unavailable branch here carries the sentence that will be shown — the reason is data, not a
 * boolean the UI has to re-derive.
 *
 * # What counts as unavailable at M1
 *
 * §5.9's four-slot table stages this: M1 is "connected / disconnected", Phase 2a adds "goto in
 * flight", Phase 2b adds "axis at a limit". Goto-in-flight is included here anyway, and the
 * reason is that it is free: `mount.status.slewing` exists from M1-T03, so the check is one
 * comparison, and leaving it out would ship the exact frustration the badge exists to prevent for
 * a phase. Axis-at-a-limit genuinely waits — nothing in the M1 event schema reports it.
 *
 * The link itself is the fourth branch and is not in the table because the table is about the
 * mount. A live socket is a precondition for every other answer: with the link down, "connected"
 * is a claim about a mount nobody has heard from.
 */
export type NudgeAvailability = { available: true } | { available: false; reason: string };

export function nudgeAvailability(
  link: LinkPhase,
  mount: Slot<MountStatus>,
): NudgeAvailability {
  if (link.phase !== 'live') {
    return {
      available: false,
      reason:
        'The event link to the field node is down, so nothing here can be commanded and nothing ' +
        'on screen is current. The app is reconnecting on its own.',
    };
  }

  if (mount.state === 'unknown') {
    return {
      available: false,
      reason:
        'The mount has not reported its state yet. This clears within a second of the link ' +
        'coming up; if it does not, the field node has no mount driver configured.',
    };
  }

  switch (mount.value.state) {
    case 'disconnected':
      return {
        available: false,
        reason:
          'The mount is not connected. Connect it from the pointing panel — startup never ' +
          'connects hardware on its own, so that no telescope moves because a machine rebooted.',
      };
    case 'fault':
      return {
        available: false,
        reason:
          'The mount reported a fault and refuses motion until it is acknowledged. The alert ' +
          'that raised it is in the system panel.',
      };
    case 'parked':
      return {
        available: false,
        reason: 'The mount is parked. Motion is refused until it is unparked.',
      };
    case 'slewing':
      return {
        available: false,
        reason:
          'A goto is in flight. Nudging is for the fine adjustment after it settles — wait for ' +
          'the slew to finish, or stop it first.',
      };
    case 'idle':
      return { available: true };
  }
}
