import { useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import { mountGoto } from '../lib/commands';
import { parseDecDegrees, parseRaHours } from '../lib/coords';
import { selectLink, selectMountStatus, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { FailureNote } from '../ui/FailureNote';

/**
 * M1's target chooser: RA/DEC typed by hand — SDD §5.9's four-slot table, "Target chooser".
 *
 * Phase 2a's catalog picker (PLN-03/04) replaces **this component** and nothing else. That is the
 * point of the slot: it changes how a target is chosen, not what the rest of the UI does with one,
 * so it drops into `<TargetRegion>` beside an unchanged pointing readout and an unchanged goto.
 *
 * # Validation is local and immediate; refusal comes from the node
 *
 * Two different rejections, kept visibly apart. A coordinate that is not a coordinate is caught
 * here, on the keystroke, because the node should never be asked to parse `5:99:00`. A coordinate
 * that is legal but unreachable — below the altitude limit, past the meridian — is the node's
 * judgement and arrives as a `LIMIT_ALTITUDE` envelope, because the limits live in config on the
 * field node and a client that second-guessed them would be a second, drifting copy of the safety
 * rules (§5.4).
 *
 * # Pressing goto changes nothing on screen
 *
 * The button reports that the request was accepted (202) and stops there. The coordinates move
 * when `mount.position` says they moved. See `lib/commands.ts` for why that is enforced rather
 * than merely intended.
 */
export function ManualTargetChooser(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const link = useTelemetryStore(selectLink);
  const status = useTelemetryStore(selectMountStatus);

  const [ra, setRa] = useState('');
  const [dec, setDec] = useState('');
  const [failure, setFailure] = useState<RequestFailure | null>(null);
  const [accepted, setAccepted] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  const parsedRa = parseRaHours(ra);
  const parsedDec = parseDecDegrees(dec);
  // An empty field is not an error until the operator tries to use it — showing "enter a
  // coordinate" under a box nobody has touched is noise.
  const raProblem = ra.trim() === '' ? null : parsedRa.ok ? null : parsedRa.problem;
  const decProblem = dec.trim() === '' ? null : parsedDec.ok ? null : parsedDec.problem;

  const slewing = status.state === 'observed' && status.value.slewing;
  const ready = parsedRa.ok && parsedDec.ok && link.phase === 'live' && !sending && !slewing;

  return (
    <form
      className="flex flex-col gap-3"
      onSubmit={(event) => {
        event.preventDefault();
        if (!parsedRa.ok || !parsedDec.ok) return;
        setFailure(null);
        setAccepted(null);
        setSending(true);
        void mountGoto(token, { raHours: parsedRa.value, decDegrees: parsedDec.value }).then(
          (result) => {
            setSending(false);
            if (result.ok) {
              // The `correlation_id` and `watch_topic` the node returns are for correlating a
              // request with a log line, not for an operator: telling someone to "watch
              // mount.position" names an internal topic and asks them to do something the
              // pointing readout is already doing for them.
              setAccepted('Accepted. The mount is slewing — watch the coordinates above.');
            } else {
              setFailure(result.failure);
            }
          },
        );
      }}
    >
      <CoordinateInput
        id="target-ra"
        label="RA"
        hint="hh:mm:ss — 05:35:17.3"
        value={ra}
        onChange={setRa}
        problem={raProblem}
      />
      <CoordinateInput
        id="target-dec"
        label="DEC"
        hint="±dd°mm'ss&quot; — -05°23'28&quot;"
        value={dec}
        onChange={setDec}
        problem={decProblem}
      />

      <Button type="submit" variant="primary" disabled={!ready} className="w-full">
        {sending ? 'Sending…' : 'GOTO'}
      </Button>

      {!ready && link.phase !== 'live' && (
        <p className="text-sm text-muted">
          Not connected to the mount — a goto can&rsquo;t be confirmed. Reconnecting.
        </p>
      )}
      {slewing && <p className="text-sm text-muted">A slew is in flight; wait for it to settle.</p>}

      {accepted !== null && (
        <p role="status" className="text-sm text-muted">
          {accepted}
        </p>
      )}
      {failure !== null && <FailureNote failure={failure} action="goto" />}
    </form>
  );
}

/**
 * One coordinate field.
 *
 * `inputMode="text"` rather than `numeric`: a numeric keypad on Android has no colon, degree sign
 * or minus, which are most of what a coordinate is made of. The hint shows the printed form so
 * the operator can see what is expected without reading a validation message first.
 */
function CoordinateInput({
  id,
  label,
  hint,
  value,
  onChange,
  problem,
}: {
  id: string;
  label: string;
  hint: string;
  value: string;
  onChange: (value: string) => void;
  problem: string | null;
}): ReactNode {
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={id} className="text-sm text-muted">
        {label} <span className="text-faint">{hint}</span>
      </label>
      <input
        id={id}
        name={id}
        type="text"
        inputMode="text"
        autoComplete="off"
        autoCapitalize="off"
        spellCheck={false}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        aria-invalid={problem !== null}
        aria-describedby={problem === null ? undefined : `${id}-problem`}
        className={`tabular min-h-touch w-full rounded-md border bg-surface px-3 font-mono text-base text-fg placeholder:text-faint ${
          problem === null ? 'border-edge-strong' : 'border-danger'
        }`}
      />
      {problem !== null && (
        <p id={`${id}-problem`} className="text-sm text-danger">
          {problem}
        </p>
      )}
    </div>
  );
}
