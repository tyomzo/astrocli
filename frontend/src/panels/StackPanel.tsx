import type { ReactNode } from 'react';

import { age } from '../lib/format';
import { useNow } from '../lib/useNow';
import { selectLink, selectStackStatus, useTelemetryStore } from '../store/telemetry';
import { Card, Field } from '../ui/Card';

/**
 * The stack, as far as M1 knows it — SDD §5.9's stack slot, M1 row.
 *
 * The table in §5.9 is explicit about what belongs here now: preview, queue depth, frame count,
 * last-preview age, and **no knobs**, because the M1 stub worker does no stacking and there is
 * nothing to tune. Phase 2b adds method, rejection thresholds and stretch; Phase 2c the
 * post-processing chain. So this is a readout, and the space the knobs will occupy is left empty
 * rather than filled with disabled controls that promise capability the build does not have.
 *
 * # The rebuilding indicator is a reserved slot that never fires in M1
 *
 * IPP-16 rebuilds the accumulator in the background while capture continues, and the preview keeps
 * serving the *pre-rebuild* stack until the swap. Adjusting sigma and seeing the image not change
 * is therefore correct behaviour that looks exactly like a bug — so the panel needs an explicit
 * "re-stacking 34/120" state or the operator turns the knob again, and again. Reserving it here,
 * unfired, is what makes Phase 2b a fill rather than a redesign.
 *
 * The preview image itself is M1-T14's; it lands on the shared image surface (`ImageSurface`),
 * not in this card, because §5.9 makes `FRAME` and `STACK` two sources of one surface.
 */
export function StackPanel(): ReactNode {
  const stack = useTelemetryStore(selectStackStatus);
  const link = useTelemetryStore(selectLink);
  const now = useNow(5000);

  return (
    <Card title="Stack">
      {stack.state === 'unknown' ? (
        <p className="text-sm text-muted">
          {link.phase === 'live'
            ? 'The field node has not reported the stack node yet.'
            : 'No stack status on this connection.'}
        </p>
      ) : (
        <dl className={link.phase === 'live' ? '' : 'opacity-60'}>
          <Field
            label="stack node"
            value={
              <span className={stack.value.connected ? 'text-ok' : 'text-danger'}>
                <span aria-hidden="true">{stack.value.connected ? '●' : '⊘'}</span>{' '}
                {stack.value.connected ? 'reachable' : 'unreachable'}
              </span>
            }
          />
          <Field label="frames" value={stack.value.session_frame_count} />
          <Field
            label="worker"
            value={stack.value.worker_state ?? 'unknown'}
          />
          <Field label="restarts" value={stack.value.restarts} />
          <Field
            label="last preview"
            value={
              stack.value.last_preview_ts === null
                ? 'none yet'
                : age(Date.parse(stack.value.last_preview_ts), now)
            }
          />
        </dl>
      )}

      <p className="mt-3 text-sm text-faint">
        No stacking controls in M1: the stub worker produces a preview and no statistics, so there
        is nothing to tune. Method, rejection and stretch arrive in Phase 2b.
      </p>
    </Card>
  );
}
