import { create } from 'zustand';

/*
 * Which screen the operator is looking at.
 *
 * Client-owned state, like `store/token.ts` and unlike `store/telemetry.ts` — and the distinction
 * is worth restating so it is not cited as precedent for writing telemetry from a component.
 * There is no event that could confirm which tab is open, because the node does not know and
 * should not: the operator's attention is not part of the observatory's state.
 *
 * The three destinations are SDD §5.9's, and they are session concerns rather than subsystems:
 * **Target** (what to point at), **Frame** (acquire), **Stack** (the result). There is no "mount
 * panel" destination, deliberately — manual mount control is a brief fine-adjustment step after a
 * slew settles, and a permanent panel for it would occupy screen space for the 95% of a session
 * when nobody touches it. On a tablet all three are visible at once and the navigation disappears.
 *
 * `system` is not a fourth destination. It is a detour — health, credential, capabilities — that
 * the operator reaches by tapping the header status strip, which is where they were already
 * looking when they wanted it. Making it a peer of the three would put a diagnostic screen in a
 * navigation bar that is otherwise entirely about the session.
 */

export type Destination = 'target' | 'frame' | 'stack';

interface UiStore {
  destination: Destination;
  systemOpen: boolean;
  /** Selecting a destination also leaves the system detour — the operator asked for the session. */
  show: (destination: Destination) => void;
  openSystem: () => void;
  closeSystem: () => void;
}

export const useUiStore = create<UiStore>()((set) => ({
  destination: 'target',
  systemOpen: false,
  show: (destination) => set({ destination, systemOpen: false }),
  openSystem: () => set({ systemOpen: true }),
  closeSystem: () => set({ systemOpen: false }),
}));
