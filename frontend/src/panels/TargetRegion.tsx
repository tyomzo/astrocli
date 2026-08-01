import type { ReactNode } from 'react';
import { MountConnect } from './MountConnect';

import { Card } from '../ui/Card';
import { ManualTargetChooser } from './ManualTargetChooser';
import { PointingReadout } from './PointingReadout';

/**
 * The target region — SDD §5.9's slot with a stable contract.
 *
 * **This is a slot, not a form.** §5.9 is explicit that Phase 2a's catalog picker (PLN-03/04)
 * drops into the same place without restructuring anything around it, because choosing a target
 * from a catalog changes *how a target is chosen* and nothing about what the rest of the UI does
 * with one. So the region owns two things — a chooser and the pointing readout — and the chooser
 * is a parameter.
 *
 * The alternative, and the reason this is written down: building M1's manual entry as *the* target
 * screen and letting the layout grow around its shape. The catalog picker would then arrive as a
 * rewrite of the screen rather than a swap of one child, and every panel that had quietly started
 * reading the entry box's state would come with it.
 *
 * `chooser` therefore has exactly one obligation: when the operator has settled on a target, issue
 * the goto. It owns its own validation and its own refusal rendering. Nothing outside it reads
 * what is typed into it.
 */
export function TargetRegion({ chooser }: { chooser?: ReactNode }): ReactNode {
  return (
    <div className="flex flex-col gap-3">
      <Card title="Target" accessory={<MountConnect />}>{chooser ?? <ManualTargetChooser />}</Card>
      <PointingReadout />
    </div>
  );
}
