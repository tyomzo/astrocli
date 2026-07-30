import { useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import { mountEstop } from '../lib/commands';
import { selectAlerts, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { FailureNote } from '../ui/FailureNote';

/**
 * The e-stop — USB-03, USB-12, MNT-08, SDD §5.9, §5.8.2.
 *
 * Three properties are fixed from M0 and must not be renegotiated by a later panel:
 *
 *  * **Top-right of the header, on every screen.** SDD §5.9 lists it among the things that are
 *    fixed rather than slotted. Constant position is what lets it be hit without looking, which is
 *    the entire requirement (USB-03) — a button that moves is a button you have to find.
 *  * **Larger than any other control** (`size-estop`, above the 60–70 px band the D-pad and
 *    capture use). It is also diagonally distant from the nudge badge in the image surface's
 *    bottom-right corner: the frequent control should be easy to reach and the irreversible one
 *    should take a deliberate stretch.
 *  * **No confirmation step.** REL-01 says it must work regardless of application state, and a
 *    dialog is application state. The cost of a stop nobody wanted is a re-slew; the cost of a
 *    confirmation nobody found in the dark is the camera hitting the tripod.
 *
 * # Pressed, and then what
 *
 * The tap sends the request and nothing else. It does **not** draw the mount as stopped, because
 * this app never renders a mutation optimistically (SDD §5.9: "commands are REST calls that
 * optimistically do nothing; UI state changes only when the corresponding event arrives"). On a
 * link where the request may not have landed, a button that showed "stopped" because it had been
 * pressed would be lying about a telescope that is still moving — and this is the one control
 * where that lie is unaffordable.
 *
 * So there are two independent things on screen and they come from two different places:
 *
 *  * **Sent / not sent** is local. It is this component's own request and this component may say
 *    what became of it — a `FailureNote` if the node refused or nothing answered.
 *  * **Stopped** comes from the telescope, as an `EMERGENCY_STOP` alert on the event stream that
 *    arrived after the tap. Until one does, the button says the request went out and no more.
 *
 * The gap between them is not a defect to paper over: it is the operator being told, honestly,
 * that the app has asked and has not yet heard back.
 */
export function EStopButton(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const alerts = useTelemetryStore(selectAlerts);
  const [request, setRequest] = useState<RequestState>({ phase: 'ready' });

  const confirmedAt = lastStopConfirmation(alerts);
  const confirmed =
    request.phase === 'sent' && confirmedAt !== null && confirmedAt >= request.at;

  async function press(): Promise<void> {
    // The instant of the tap, not of the reply: the confirmation test below is "did the telescope
    // report a stop *after* I asked", and using the reply time would miss an alert that overtook
    // a slow response.
    const at = Date.now();
    setRequest({ phase: 'sending' });
    const result = await mountEstop(token);
    setRequest(result.ok ? { phase: 'sent', at } : { phase: 'failed', failure: result.failure });
  }

  return (
    <div className="relative">
      <button
        type="button"
        aria-label="Emergency stop"
        onClick={() => {
          void press();
        }}
        className="flex size-estop flex-col items-center justify-center rounded-lg border-4 border-danger text-danger transition-colors active:bg-danger active:text-on-danger"
      >
        <span className="text-lg leading-none font-black tracking-tight">STOP</span>
      </button>

      {request.phase !== 'ready' && (
        <div className="absolute top-full right-0 z-10 mt-2 w-64">
          {request.phase === 'failed' ? (
            <>
              <FailureNote failure={request.failure} action="Stop" />
              <p role="status" className="mt-2 rounded-md border border-edge bg-overlay p-3 text-sm text-fg">
                {/* The instruction of last resort. If the stop did not reach the telescope, the
                    app has nothing left to offer and saying so is more use than a retry button
                    that may not work either. */}
                The telescope may still be moving. Cut power to the mount.
              </p>
            </>
          ) : (
            <p
              role="status"
              aria-live="assertive"
              className="rounded-md border border-edge bg-overlay p-3 text-sm text-fg"
            >
              {confirmed
                ? 'The telescope has stopped. Tracking is off — start it again when you are ready.'
                : 'Stop sent. Waiting for the telescope to report that it has halted.'}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

/** What became of the last tap. */
type RequestState =
  | { phase: 'ready' }
  | { phase: 'sending' }
  /** The node accepted the request at `at`. This says nothing about the mount. */
  | { phase: 'sent'; at: number }
  | { phase: 'failed'; failure: RequestFailure };

/**
 * When the telescope last reported that it had halted, or `null`.
 *
 * The code is matched, not the prose: `EMERGENCY_STOP` is what the safety layer publishes when a
 * stop lands (it is not one of SDD §4.2's request-failure codes, because an emergency stop is not
 * a request that failed). Matching on the message text would break the first time somebody
 * improved the wording.
 */
function lastStopConfirmation(alerts: ReturnType<typeof selectAlerts>): number | null {
  const stop = alerts.find((alert) => alert.value.code === 'EMERGENCY_STOP');
  return stop === undefined ? null : stop.at;
}
