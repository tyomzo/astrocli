import type { ReactNode } from 'react';

import { age } from '../lib/format';
import { useNow } from '../lib/useNow';
import {
  selectLink,
  selectStackStatus,
  selectTransferStatus,
  useTelemetryStore,
} from '../store/telemetry';
import { Card, Field } from '../ui/Card';

/**
 * The stack, as far as M1 knows it — SDD §5.9's stack slot, M1 row; USB-06.
 *
 * The tablet sketch puts these statistics in the **left column**, under `── STACK ──`, not under
 * the image: "the tablet target column carries the stack statistics deliberately: both answer
 * 'what is this session doing', and keeping them together leaves the image surface uninterrupted".
 * The preview itself is on the shared `ImageSurface`, because §5.9 makes `FRAME` and `STACK` two
 * sources of one surface rather than two panels.
 *
 * # It takes two topics to answer USB-06
 *
 * "connected/disconnected, queue depth, current stack frame count, last preview timestamp" — and
 * the queue depth is not the stacking server's. It is the *field* node's upload queue, on
 * `transfer.status` (§5.10.4), while the rest arrives on `stack.status` (§4.3). Joining them here
 * is what makes the panel able to say the one thing the operator actually wants to know on a bad
 * night: frames are still being taken and are waiting, rather than being lost.
 *
 * # The distinctions this panel must not blur
 *
 * Three states look similar and mean entirely different things, and the T11 handoff is explicit
 * that the last of them must never render as "all frames delivered":
 *
 * | State | What it means | How it gets here |
 * |---|---|---|
 * | reachable | the stacking server is answering | `stack.status.connected` |
 * | not answering | it is down, or the tunnel is | `stack.status.connected === false` |
 * | not configured | this node has no stacking server | no `stack.status` at all (`unknown`) |
 *
 * The third is a *deliberate* absence: the field node publishes nothing when
 * `stacking_server.enabled` is false, because §5.8.3 defines an absent topic as "the node has no
 * value for it" — and telling an operator their stacking server is down when they turned it off
 * sends them outside to look at a machine that is behaving.
 *
 * # No knobs, and no disabled knobs
 *
 * §5.9's M1 row: "**No knobs** — the stub does no stacking, so there is nothing to tune". The
 * space they will occupy is reserved under the image surface (`StackSlots`), empty rather than
 * filled with greyed-out controls that promise capability this build does not have.
 */
export function StackPanel(): ReactNode {
  const stack = useTelemetryStore(selectStackStatus);
  const transfer = useTelemetryStore(selectTransferStatus);
  const link = useTelemetryStore(selectLink);
  const now = useNow(5000);

  const dimmed = link.phase === 'live' ? '' : 'opacity-60';

  return (
    <Card title="Stack">
      {stack.state === 'unknown' ? (
        <p className="text-sm text-muted">
          {link.phase === 'live'
            ? 'No stacking server on this node. Frames are stored here only.'
            : 'No stack status on this connection.'}
        </p>
      ) : (
        <dl className={dimmed}>
          <Field
            label="stacking server"
            value={
              <span className={stack.value.connected ? 'text-ok' : 'text-danger'}>
                <span aria-hidden="true">{stack.value.connected ? '●' : '⊘'}</span>{' '}
                {stack.value.connected ? 'reachable' : 'not answering'}
              </span>
            }
          />
          <Field label="frames received" value={stack.value.session_frame_count} />
          <Field
            label="last preview"
            value={
              stack.value.last_preview_ts === null
                ? 'none yet'
                : age(Date.parse(stack.value.last_preview_ts), now)
            }
          />
          {/*
            "Processing" is the stacking server's compute child. It starts on demand, so idle is
            its normal resting state and is deliberately not coloured as a fault. Restarts are
            the number §5.12.3 calls "the failure mode most likely to go unnoticed", and they are
            shown only once there are some: a permanent "restarts 0" teaches the eye to skip the
            row that matters.
          */}
          <Field label="processing" value={workerText(stack.value.worker_state)} />
          {stack.value.restarts > 0 && (
            <Field
              label="processing restarts"
              value={<span className="text-warn">{stack.value.restarts}</span>}
            />
          )}
        </dl>
      )}

      {transfer.state === 'observed' && (
        <dl className={`mt-3 border-t border-edge pt-3 ${dimmed}`}>
          <Field label="waiting to upload" value={queueText(transfer.value.queue_depth)} />
          {transfer.value.oldest_queued_age_s !== null && (
            <Field
              label="oldest waiting"
              value={
                <span className={transfer.value.queue_depth > 0 ? 'text-warn' : undefined}>
                  {waitingText(transfer.value.oldest_queued_age_s)}
                </span>
              }
            />
          )}
        </dl>
      )}
    </Card>
  );
}

/**
 * Operator language, not enum names. `stopped` is honest but reads as a fault; "idle" is the same
 * fact in the words someone deciding whether to worry would use.
 */
function workerText(state: string | null): string {
  switch (state) {
    case null:
      return 'unknown';
    case 'stopped':
      return 'idle';
    case 'starting':
      return 'starting';
    case 'ready':
      return 'ready';
    case 'busy':
      return 'working';
    case 'restarting':
      return 'restarting';
    case 'failed':
      return 'failed';
    default:
      return state;
  }
}

/**
 * The queue, counted in frames.
 *
 * The T11 handoff is explicit that the depth **includes the frame in flight**, so `1` means one
 * frame is on its way rather than one frame stuck — which is why the copy says "waiting" and the
 * empty case says "all delivered" only when the number is genuinely zero.
 */
function queueText(depth: number): string {
  if (depth === 0) return 'nothing waiting';
  return depth === 1 ? '1 frame' : `${depth} frames`;
}

function waitingText(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  return `${hours} h ${minutes % 60} min`;
}
