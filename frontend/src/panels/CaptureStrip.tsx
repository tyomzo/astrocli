import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import type { CameraSettings, ImageFormat } from '../lib/commands';
import {
  cameraAbort,
  cameraAcknowledgeFault,
  cameraCapture,
  cameraConnect,
  cameraSetSettings,
  cameraSettings,
} from '../lib/commands';
import type { CaptureProgress } from '../lib/link/protocol';
import {
  selectCameraStatus,
  selectCaptureProgress,
  selectLink,
  useTelemetryStore,
} from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { FailureNote } from '../ui/FailureNote';

/**
 * The capture strip — SDD §5.9's bottom row of the `FRAME` destination.
 *
 * ```text
 * │ ISO 1600    30s    RAW        [ CAPTURE ]  │
 * ```
 *
 * §5.9 puts this *under the image*, not in a panel of its own, and the reason is the same one that
 * puts the D-pad on top of the image: settings and framing are one decision, and an operator who
 * has to leave the frame to change the exposure has lost what they were looking at.
 *
 * # Everything on screen comes from the node
 *
 * The settings are read from `/api/camera/settings` and re-read after every change, so what is
 * shown is what the *camera* accepted rather than what was tapped — a body may coerce a value, and
 * the node refuses a token the body does not offer instead of substituting the nearest one. The
 * capture state is rendered from `capture.progress` events alone. Nothing here moves because a
 * button was pressed; see `lib/commands.ts` for why that rule is enforced rather than followed.
 *
 * The one thing this component may put on screen from its own request is the node's refusal of
 * that request — `FailureNote` — because a `507` is the answer to what the operator just asked,
 * not a claim about the camera.
 *
 * # Two components, one behaviour
 *
 * The store reading lives here and the markup lives in [`CaptureStripView`], which takes what it
 * renders as props. That is the split `NudgeBadge` already uses, and it is not decoration: it is
 * what lets `CaptureStrip.test.tsx` assert "the button says Stop while the shutter is open" from
 * a given store state without a DOM, because everything the strip shows is an argument rather
 * than a subscription.
 */
export function CaptureStrip(): ReactNode {
  const link = useTelemetryStore(selectLink);
  const cameraState = useTelemetryStore(selectCameraStatus);
  const progress = useTelemetryStore(selectCaptureProgress);

  return (
    <CaptureStripView
      live={link.phase === 'live'}
      connected={cameraState.state === 'observed' && cameraState.value.connected}
      capture={progress.state === 'observed' ? progress.value : null}
    />
  );
}

/** The strip's markup and its commands, with the observed state as arguments. */
export function CaptureStripView({
  live,
  connected,
  capture,
}: {
  live: boolean;
  connected: boolean;
  /** The latest `capture.progress`, or `null` if the node has not reported one. */
  capture: CaptureProgress | null;
}): ReactNode {
  const token = useTokenStore((state) => state.token);

  const [settings, setSettings] = useState<CameraSettings | null>(null);
  const [failure, setFailure] = useState<RequestFailure | null>(null);
  const [busy, setBusy] = useState(false);
  const [bulbSeconds, setBulbSeconds] = useState('120');
  /**
   * Whether the node told us capture is held after a fault.
   *
   * Learned from the refusal of a capture, which is the only place it surfaces: SDD §4.3 has no
   * topic for the orchestrator's state and §5.8.1 has no route that reports it, so a panel that
   * showed a "faulted" badge before the operator had asked for anything would be inventing one.
   * The cost is one refused tap after a reload, and the acknowledgement appears the moment it can
   * honestly be offered.
   */
  const [heldFault, setHeldFault] = useState<string | null>(null);

  // Read once the camera is answering, and again whenever it reconnects. Not on a timer: the
  // settings only change when something changes them, and this component is one of the things.
  useEffect(() => {
    if (!connected) {
      setSettings(null);
      return;
    }
    let current = true;
    void cameraSettings(token).then((result) => {
      if (current && result.ok) setSettings(result.value);
    });
    return () => {
      current = false;
    };
  }, [connected, token]);

  // `saved` and `preview_ready` are the end of an exposure, not a state it is in.
  const exposing =
    capture !== null && (capture.state === 'exposing' || capture.state === 'downloading');
  const bulbSelected = settings?.shutter === 'bulb';

  async function run(action: () => Promise<{ ok: boolean; failure?: RequestFailure }>) {
    setBusy(true);
    setFailure(null);
    const result = await action();
    if (!result.ok && result.failure !== undefined) {
      setFailure(result.failure);
      // `BUSY` with a fault is the node telling us capture is held. Anything else clears the
      // holding state: the operator has moved on and a stale badge would be a lie.
      const held =
        result.failure.kind === 'api' &&
        result.failure.code === 'BUSY' &&
        result.failure.message.includes('acknowledge');
      setHeldFault(held ? result.failure.message : null);
    } else if (result.ok) {
      setHeldFault(null);
    }
    setBusy(false);
    return result;
  }

  async function applySetting(update: Partial<Record<'iso' | 'shutter' | 'format', string>>) {
    const result = await run(() => cameraSetSettings(token, update as never));
    // Re-read rather than trusting the reply's shape: one call, one source of truth.
    const fresh = await cameraSettings(token);
    if (fresh.ok) setSettings(fresh.value);
    return result;
  }

  if (!live) {
    return (
      <Strip>
        <p className="text-sm text-muted">
          Not connected to the telescope — the camera can&rsquo;t be reached from here.
        </p>
      </Strip>
    );
  }

  if (!connected) {
    return (
      <Strip>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-sm text-muted">The camera is not connected.</p>
          <Button
            variant="primary"
            disabled={busy}
            onClick={() => void run(() => cameraConnect(token))}
          >
            Connect camera
          </Button>
        </div>
        {failure !== null && <FailureNote failure={failure} action="connecting the camera" />}
      </Strip>
    );
  }

  return (
    <Strip>
      <div className="flex flex-wrap items-end gap-x-4 gap-y-3">
        <Selector
          label="ISO"
          value={settings?.iso ?? ''}
          options={settings?.available.isos ?? []}
          disabled={busy || exposing || settings === null}
          onChange={(iso) => void applySetting({ iso })}
        />
        <Selector
          label="Shutter"
          value={settings?.shutter ?? ''}
          options={settings?.available.shutters ?? []}
          disabled={busy || exposing || settings === null}
          onChange={(shutter) => void applySetting({ shutter })}
        />
        <Selector
          label="Format"
          value={settings?.format ?? ''}
          options={(settings?.available.formats ?? []) as ImageFormat[]}
          disabled={busy || exposing || settings === null}
          onChange={(format) => void applySetting({ format })}
        />

        {/*
          The duration only exists when the shutter is on bulb, because that is the only time the
          camera needs one — and showing it always would suggest the number applies to a timed
          exposure, which it does not.
        */}
        {bulbSelected && (
          <label className="flex flex-col gap-1">
            <span className="text-xs tracking-wide text-muted uppercase">Bulb seconds</span>
            <input
              type="number"
              inputMode="numeric"
              min={1}
              value={bulbSeconds}
              disabled={busy || exposing}
              onChange={(event) => setBulbSeconds(event.target.value)}
              className="min-h-touch w-28 rounded-md border border-edge-strong bg-overlay px-3 font-mono text-fg disabled:opacity-40"
            />
          </label>
        )}

        <div className="ml-auto flex items-end gap-2">
          {exposing ? (
            <Button
              variant="primary"
              disabled={busy}
              className="border-danger text-danger"
              onClick={() => void run(() => cameraAbort(token))}
            >
              Stop exposure
            </Button>
          ) : (
            <Button
              variant="primary"
              disabled={busy || (bulbSelected && !isPositive(bulbSeconds))}
              onClick={() =>
                void run(() =>
                  cameraCapture(token, bulbSelected ? Number(bulbSeconds) : undefined),
                )
              }
            >
              Capture
            </Button>
          )}
        </div>
      </div>

      <CaptureState capture={capture} exposing={exposing} />

      {heldFault !== null && (
        <div className="mt-3 rounded-md border border-danger/50 bg-overlay p-3">
          <p className="text-sm text-fg">{heldFault}</p>
          <Button
            className="mt-2"
            disabled={busy}
            onClick={() =>
              void run(async () => {
                const result = await cameraAcknowledgeFault(token);
                if (result.ok) setHeldFault(null);
                return result;
              })
            }
          >
            Acknowledge and continue
          </Button>
        </div>
      )}

      {failure !== null && <FailureNote failure={failure} action="capture" />}
    </Strip>
  );
}

/**
 * What the exposure is doing, from `capture.progress` and nothing else.
 *
 * Rendered even between exposures, because a strip that shows the last frame's id is how an
 * operator confirms the previous capture landed without leaving the frame view.
 */
function CaptureState({
  capture,
  exposing,
}: {
  capture: CaptureProgress | null;
  exposing: boolean;
}): ReactNode {
  if (capture === null) {
    return (
      <p className="mt-2 text-sm text-faint">No exposure taken on this connection yet.</p>
    );
  }

  const elapsed = Math.floor(capture.elapsed_s);
  return (
    <p role="status" className="mt-2 font-mono text-sm text-fg">
      <span aria-hidden="true" className={exposing ? 'text-accent' : 'text-ok'}>
        {exposing ? '◐' : '●'}
      </span>{' '}
      {capture.frame_id} — {describe(capture.state)}
      {exposing && ` ${elapsed}s`}
    </p>
  );
}

/** The four `capture.progress` states, in words an operator reads rather than state names. */
function describe(state: string): string {
  switch (state) {
    case 'exposing':
      return 'shutter open';
    case 'downloading':
      return 'reading off the camera';
    case 'saved':
      return 'stored';
    case 'preview_ready':
      return 'preview ready';
    default:
      return state;
  }
}

function isPositive(value: string): boolean {
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds > 0;
}

/** A labelled `<select>` sized to the touch-target floor (USB-12). */
function Selector({
  label,
  value,
  options,
  disabled,
  onChange,
}: {
  label: string;
  value: string;
  options: readonly string[];
  disabled: boolean;
  onChange: (value: string) => void;
}): ReactNode {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-xs tracking-wide text-muted uppercase">{label}</span>
      <select
        value={value}
        disabled={disabled || options.length === 0}
        onChange={(event) => onChange(event.target.value)}
        className="min-h-touch rounded-md border border-edge-strong bg-overlay px-3 font-mono text-fg disabled:opacity-40"
      >
        {/*
          An empty option only while the values are still being read. Rendering the current value
          as an option the list does not contain would show a setting the camera does not have.
        */}
        {options.length === 0 && <option value="">…</option>}
        {options.map((option) => (
          <option key={option} value={option}>
            {option}
          </option>
        ))}
      </select>
    </label>
  );
}

/** The strip's frame: full width under the image, not a card beside it. */
function Strip({ children }: { children: ReactNode }): ReactNode {
  return (
    <section
      aria-label="Capture"
      className="rounded-lg border border-edge bg-raised px-4 py-3"
    >
      {children}
    </section>
  );
}
