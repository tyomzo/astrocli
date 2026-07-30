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
 *
 * # The image source is separate from the destination, and has to be
 *
 * §5.9: "`FRAME` and `STACK` are two sources sharing one image surface, not two panels", with the
 * tablet sketch putting a `[ FRAME │ STACK ]` toggle above the surface. On a phone the bottom
 * navigation *is* that toggle — choosing a destination chooses a source — but on a tablet all
 * three regions are on screen at once and `destination` no longer says which picture to show. So
 * the surface reads `imageSource`, the phone's `show()` moves both, and the tablet's toggle moves
 * only the source. One variable could not express the tablet.
 *
 * # Switching to STACK when a sequence starts, once
 *
 * §5.9: "the app switches to `STACK` when a capture sequence starts, and the operator can switch
 * back at will". Both halves are the requirement. [`captureStarted`] is therefore edge-triggered
 * by the panel that watches `capture.progress`, and — this is the part that makes "at will" true —
 * a manual choice afterwards sticks: `show()` and `showImage()` set `imagePinned`, and a pinned
 * surface is never moved again until the *next* sequence begins. Without the pin, an operator who
 * switched back to FRAME to check focus would be yanked to STACK on every frame of a 60-frame
 * sequence, which is a fight rather than a feature.
 */

export type Destination = 'target' | 'frame' | 'stack';

/** Which of §5.9's two sources the shared image surface is showing. */
export type ImageSource = 'frame' | 'stack';

interface UiStore {
  destination: Destination;
  /** Which source the one image surface is showing (§5.9). */
  imageSource: ImageSource;
  /** Whether the operator has chosen the source since the last sequence started. */
  imagePinned: boolean;
  systemOpen: boolean;
  /** Selecting a destination also leaves the system detour — the operator asked for the session. */
  show: (destination: Destination) => void;
  /** The tablet's `[ FRAME │ STACK ]` toggle: move the picture, not the page. */
  showImage: (source: ImageSource) => void;
  /**
   * A capture is running — switch the surface to `STACK` unless the operator has already said
   * otherwise. Safe to call on every frame of a sequence: it is idempotent, and once the operator
   * has pinned a source it does nothing at all until [`releaseImagePin`].
   */
  captureStarted: () => void;
  openSystem: () => void;
  closeSystem: () => void;
}

export const useUiStore = create<UiStore>()((set) => ({
  destination: 'target',
  imageSource: 'frame',
  imagePinned: false,
  systemOpen: false,
  show: (destination) =>
    set(
      destination === 'target'
        ? { destination, systemOpen: false }
        : // On a phone the bottom navigation is the source toggle, so choosing FRAME or STACK is
          // a deliberate choice of picture and pins it. TARGET is not a source and leaves the
          // surface where it was.
          { destination, systemOpen: false, imageSource: destination, imagePinned: true },
    ),
  showImage: (source) => set({ imageSource: source, imagePinned: true }),
  captureStarted: () =>
    set((state) =>
      state.imagePinned
        ? // The operator has chosen since the last sequence; leave them where they are.
          state
        : { ...state, imageSource: 'stack' },
    ),
  openSystem: () => set({ systemOpen: true }),
  closeSystem: () => set({ systemOpen: false }),
}));

/**
 * Release the pin so the next sequence may move the surface again.
 *
 * Called when a sequence *ends*, not when one starts: the pin's job is to survive the sequence the
 * operator overrode, and clearing it at the start would make the override last exactly one frame.
 */
export function releaseImagePin(): void {
  useUiStore.setState({ imagePinned: false });
}
