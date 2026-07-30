import type { ReactNode } from 'react';

import { AlertStrip } from '../panels/AlertStrip';
import { CameraVitals } from '../panels/CameraVitals';
import { CaptureStrip } from '../panels/CaptureStrip';
import { ClockSkewNote } from '../panels/ClockSkewNote';
import { ImageSurface } from '../panels/ImageSurface';
import { LinkBanner } from '../panels/LinkBanner';
import { SessionFrames } from '../panels/SessionFrames';
import { StackPanel } from '../panels/StackPanel';
import { TargetRegion } from '../panels/TargetRegion';
import { TrackingControl } from '../panels/TrackingControl';
import type { Destination } from '../store/ui';
import { useUiStore } from '../store/ui';
import { PanelGrid } from '../ui/PanelGrid';

/**
 * The operating screens — SDD §5.9's two sketches, one component.
 *
 * On a phone exactly one destination is visible and the bottom navigation switches between them.
 * On a tablet all three are on screen at once and the navigation disappears: a narrow
 * target-and-statistics column beside the wide image surface, which is the second sketch.
 *
 * # One tree, two arrangements
 *
 * The breakpoint is expressed entirely in CSS — each region is `hidden` on a phone unless it is
 * the current destination, and unconditionally shown from `md` up. Nothing here measures the
 * viewport. That matters more than it looks: a JavaScript breakpoint would mean the layout is
 * wrong for one frame after every rotation, would need a resize listener on a device where every
 * wake-up is a resize, and would render two different component trees — so a D-pad hold started
 * on a phone-shaped viewport would be torn down by rotating the device mid-nudge.
 *
 * # The image surface belongs to two destinations, not one (M1-T14)
 *
 * §5.9 promotes the accumulating stack from a status readout to a **primary view**: "`FRAME` and
 * `STACK` are two sources sharing one image surface, not two panels". So the surface is not inside
 * the FRAME region — it is shown for *both* FRAME and STACK, and which picture it carries is
 * `store/ui`'s `imageSource`. On a tablet it is simply always there, uninterrupted, which is the
 * arrangement the second sketch draws.
 *
 * What stays destination-specific is what sits *under* it: the ISO/shutter/CAPTURE row belongs to
 * FRAME, and the reserved stacking regions belong to STACK (they live inside `ImageSurface`,
 * because the sketch puts them directly beneath the image). The stack *statistics* are in the left
 * column with the target, deliberately — §5.9: "both answer 'what is this session doing', and
 * keeping them together leaves the image surface uninterrupted".
 */
export function OperatingView(): ReactNode {
  const destination = useUiStore((state) => state.destination);

  return (
    <>
      <LinkBanner />
      {/* Above the alerts: a wrong clock changes how every timestamp below it should be read. */}
      <ClockSkewNote />
      <AlertStrip />

      <PanelGrid layout="sidebar">
        <div className="flex flex-col gap-3">
          <Region on={['target']} current={destination}>
            <TargetRegion />
            <TrackingControl />
          </Region>
          <Region on={['stack']} current={destination}>
            <StackPanel />
            {/*
              The frame list lives with STACK rather than with FRAME, and that follows §5.9's own
              decomposition: FRAME is where an exposure is *taken* and STACK is where the operator
              asks "is this working". A count of stored frames answers the second question, and
              putting it under the image would push the capture controls off a phone.
            */}
            <SessionFrames />
          </Region>
        </div>

        <div className="flex flex-col gap-3">
          {/*
            One surface, two destinations. On a phone it follows the operator between FRAME and
            STACK rather than being duplicated, so the sockets behind it are never torn down by a
            tab change — which is what makes switching to STACK instant rather than a reconnect.
          */}
          <Region on={['frame', 'stack']} current={destination}>
            <ImageSurface />
          </Region>
          <Region on={['frame']} current={destination}>
            {/*
              Under the image, not beside it — §5.9's `ISO 1600  30s  RAW  [CAPTURE]` row. Settings
              and framing are one decision, so the control and the thing it affects stay in one
              field of view, which is the same argument that puts the D-pad on top of the image.
            */}
            <CaptureStrip />
            <CameraVitals />
          </Region>
        </div>
      </PanelGrid>
    </>
  );
}

function Region({
  on,
  current,
  children,
}: {
  /** Destinations this region belongs to. More than one for the shared image surface. */
  on: readonly Destination[];
  current: Destination;
  children: ReactNode;
}): ReactNode {
  // `hidden` is `display: none`, which takes the region out of the accessibility tree as well as
  // off the screen — a screen reader should not read three destinations when one is shown. The
  // components stay mounted, so switching destinations does not restart their subscriptions.
  return (
    <div className={`flex-col gap-3 md:flex ${on.includes(current) ? 'flex' : 'hidden'}`}>
      {children}
    </div>
  );
}
