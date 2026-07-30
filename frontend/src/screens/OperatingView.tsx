import type { ReactNode } from 'react';

import { AlertStrip } from '../panels/AlertStrip';
import { ImageSurface } from '../panels/ImageSurface';
import { LinkBanner } from '../panels/LinkBanner';
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
 */
export function OperatingView(): ReactNode {
  const destination = useUiStore((state) => state.destination);

  return (
    <>
      <LinkBanner />
      <AlertStrip />

      <PanelGrid layout="sidebar">
        <div className="flex flex-col gap-3">
          <Region destination="target" current={destination}>
            <TargetRegion />
            <TrackingControl />
          </Region>
          <Region destination="stack" current={destination}>
            <StackPanel />
          </Region>
        </div>

        <Region destination="frame" current={destination}>
          <ImageSurface />
        </Region>
      </PanelGrid>
    </>
  );
}

function Region({
  destination,
  current,
  children,
}: {
  destination: Destination;
  current: Destination;
  children: ReactNode;
}): ReactNode {
  // `hidden` is `display: none`, which takes the region out of the accessibility tree as well as
  // off the screen — a screen reader should not read three destinations when one is shown. The
  // components stay mounted, so switching destinations does not restart their subscriptions.
  return (
    <div className={`flex-col gap-3 md:flex ${destination === current ? 'flex' : 'hidden'}`}>
      {children}
    </div>
  );
}
