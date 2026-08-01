import { useEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent, ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import type { CameraSettings } from '../lib/commands';
import { cameraSettings, liveViewStart, liveViewStop } from '../lib/commands';
import { useLiveView, useStackPreview } from '../lib/link/useLiveView';
import type { SurfacePhase } from '../lib/liveview';
import { exposureSeconds, formatDuration, surfacePhase } from '../lib/liveview';
import { nudgeAvailability } from '../lib/nudge';
import { useNow } from '../lib/useNow';
import {
  selectCameraStatus,
  selectCaptureProgress,
  selectLink,
  selectMountStatus,
  useTelemetryStore,
} from '../store/telemetry';
import { useTokenStore } from '../store/token';
import type { ImageSource } from '../store/ui';
import { useUiStore } from '../store/ui';
import { Button } from '../ui/Button';
import { FailureNote } from '../ui/FailureNote';
import { NudgeBadge } from './NudgeBadge';
import { NudgeOverlay } from './NudgeOverlay';
import { RebuildingSlot, StackControlsSlot } from './StackSlots';

/**
 * The persistent centre of the app — SDD §5.9, filled in by M1-T09.
 *
 * §5.9 calls `FRAME` and `STACK` "two sources sharing one image surface", not two panels: live
 * view answers "is this framed and focused", the last frame answers "did that exposure work", the
 * stack answers "is this session working", and they are the same rectangle at different times.
 * **This component is that rectangle**, and as of M1-T14 it serves all three.
 *
 * # Two sockets, one surface
 *
 * `FRAME` is the field node's `/ws/liveview`; `STACK` is the stacking server's `/ws/preview`
 * through the proxy (ADR-07 — still this origin). Both are open whenever the link is, because the
 * node holds its last preview in a slot and hands it over at connect time: a socket opened only
 * when the operator switches to `STACK` would show an empty rectangle for one whole sub while the
 * image it wanted sat on the other end of a connection it had not made.
 *
 * The toggle above the surface is the tablet sketch's `[ FRAME │ STACK ]`. On a phone the bottom
 * navigation does the same job, which is why the source lives in `store/ui` rather than here —
 * two controls, one piece of state.
 *
 * # What sits under it
 *
 * The sketch puts two reserved regions directly beneath the image, and they are `StackSlots`:
 * Phase 2b's knobs (IPP-07) and the rebuilding indicator (IPP-16). Both are empty in M1 and
 * present anyway, so 2b fills structure rather than moving every control the operator has learned
 * the position of.
 *
 * # The pause that has to explain itself
 *
 * The camera has one sensor, so **every** exposure blanks live view for its whole duration. SDD
 * §5.7 is explicit that this must not read as a fault — "no reconnect attempt, no stream-fault
 * alert, no client teardown" — because a panel that cried wolf on every frame of a normal night
 * would teach the operator to ignore the one warning that matters.
 *
 * The decision lives in `lib/liveview.ts::surfacePhase` and is tested there. What this component
 * owes it is the inputs: `capture.progress` from the control socket, the last frame's arrival time
 * from the image socket, and the configured exposure so the wait can be counted *down* rather than
 * up. Nothing here re-derives the rule.
 *
 * # Why the exposure length is fetched rather than read off the event
 *
 * `capture.progress` carries `elapsed_s` and no total (SDD §4.3), so the event stream alone can
 * produce a stopwatch but not a countdown. The shutter setting is where the length actually lives,
 * so the panel reads it once when the camera connects, exactly as the capture strip does. A bulb
 * frame has no length to read and the panel counts up instead, which is the honest answer: a
 * countdown that reached zero while the shutter was still open would look like a node that had
 * lost track of its own exposure.
 */
export function ImageSurface(): ReactNode {
  const link = useTelemetryStore(selectLink);
  const mount = useTelemetryStore(selectMountStatus);
  const camera = useTelemetryStore(selectCameraStatus);
  const progress = useTelemetryStore(selectCaptureProgress);
  const token = useTokenStore((state) => state.token);

  const availability = nudgeAvailability(link, mount);
  const linkLive = link.phase === 'live';
  const cameraConnected = camera.state === 'observed' && camera.value.connected;

  const source = useUiStore((state) => state.imageSource);
  const showImage = useUiStore((state) => state.showImage);
  const captureStarted = useUiStore((state) => state.captureStarted);

  // Both sockets follow the link, not the toggle — see the module docs on why.
  const images = useLiveView(token, linkLive);
  const stack = useStackPreview(token, linkLive);

  const [streaming, setStreaming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<RequestFailure | null>(null);
  const [settings, setSettings] = useState<CameraSettings | null>(null);

  const [summoned, setSummoned] = useState(false);
  const [explanation, setExplanation] = useState<string | null>(null);

  // A ticking clock, because a countdown that only moved when an event arrived would sit still for
  // a second at a time. 250 ms reads as smooth and costs far less than a frame rate.
  const now = useNow(250);

  const available = availability.available;
  useEffect(() => {
    if (!available) setSummoned(false);
  }, [available]);

  // The camera going away takes live view with it, server-side; the button must agree rather than
  // offering to stop something that is already stopped.
  useEffect(() => {
    if (!cameraConnected) setStreaming(false);
  }, [cameraConnected]);

  // §5.9: "the app switches to `STACK` when a capture sequence starts, and the operator can
  // switch back at will". The store owns "at will" — this only reports that a capture is running,
  // and a pinned surface ignores it.
  const capturing =
    progress.state === 'observed' &&
    (progress.value.state === 'exposing' || progress.value.state === 'downloading');
  useEffect(() => {
    if (capturing) captureStarted();
  }, [capturing, captureStarted]);

  useEffect(() => {
    if (!cameraConnected) {
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
  }, [cameraConnected, token]);

  // The live-view phase machine is about the *camera's* stream — its pauses, its stalls, its
  // countdown. None of that describes the stacking server, whose preview arrives when a frame
  // finishes its trip and is not "stalled" in between. Applying it to STACK would put "live view
  // has stalled" over a picture that is exactly as fresh as it should be.
  const framePhase = surfacePhase({
    linkLive,
    streaming,
    lastFrameAt: images.live?.at ?? null,
    capture: progress.state === 'observed' ? progress.value : null,
    captureAt: progress.state === 'observed' ? progress.at : null,
    exposureSeconds: exposureSeconds(settings?.shutter),
    now,
  });

  // Within FRAME, whichever of the two field-node images spoke most recently: a capture's preview
  // is what the surface shows the moment it is pushed, and live view takes it back on the next
  // frame. STACK has one image and no such contest.
  const preview = images.preview;
  const live = images.live;
  const framePicture =
    preview !== null && (live === null || preview.at >= live.at)
      ? { image: preview, source: 'preview' as const }
      : live !== null
        ? { image: live, source: 'live' as const }
        : null;
  const stackPicture =
    stack.preview === null ? null : { image: stack.preview, source: 'stack' as const };
  const showing = source === 'stack' ? stackPicture : framePicture;

  async function toggle(next: boolean) {
    setBusy(true);
    setFailure(null);
    const result = await (next ? liveViewStart(token) : liveViewStop(token));
    if (result.ok) {
      setStreaming(next);
    } else {
      setFailure(result.failure);
    }
    setBusy(false);
  }

  const stackConnected = stack.connected;

  return (
    <section className="flex flex-col gap-2">
      <SourceToggle source={source} onSelect={showImage} />

      {/*
        The stage suppresses the browser's own long-press behaviours for everything inside it —
        the live image, the summoned D-pad, the badge. The D-pad's buttons carry their own
        guards, but they float over an <img>, and Android's long-press on an *image* opens the
        save-image sheet regardless of what the element on top prevents: a near-miss lands on
        the picture, the sheet steals the pointer, the hold dies (the dead-man doing its job
        against a hijacked gesture), and dismissing the sheet reads as an outside tap that
        closes the overlay. Found by an operator's thumb, not by any test — a desktop pointer
        never long-presses.
      */}
      <div
        className="relative touch-none overflow-hidden rounded-lg border border-edge bg-raised select-none [-webkit-touch-callout:none]"
        onContextMenu={(event) => event.preventDefault()}
      >
        <Zoomable>
          {showing === null ? (
            <Placeholder
              source={source}
              phase={framePhase}
              stackConnected={stackConnected}
              linkLive={linkLive}
            />
          ) : (
            <img
              src={showing.image.url}
              alt={altFor(showing.source, showing.image.frameId)}
              className="h-full w-full object-contain"
              draggable={false}
            />
          )}
        </Zoomable>

        {/*
          Over the image rather than beside it, like the D-pad and for the same reason: the state
          of the picture and the picture are one thing to look at. Omitted while the surface is
          empty, where the placeholder is already saying it.
        */}
        {showing !== null && (
          <StatusOverlay
            phase={showing.source === 'stack' ? null : framePhase}
            source={showing.source}
            frameId={showing.image.frameId}
          />
        )}

        {/*
          The nudge affordance belongs to framing, not to the accumulated stack: pointing while
          looking at STACK would mean nudging by a picture that is minutes of integration old.
          §5.9 puts the badge on the live image, and this is that distinction made literal.
        */}
        {source === 'frame' &&
          (summoned ? (
            <NudgeOverlay onDismiss={() => setSummoned(false)} />
          ) : (
            <NudgeBadge
              availability={availability}
              onSummon={() => {
                setExplanation(null);
                setSummoned(true);
              }}
              onExplain={() => setExplanation(availability.available ? null : availability.reason)}
            />
          ))}
      </div>

      {/*
        §5.9's tablet sketch puts both of these directly under the image, in this order. They are
        reserved and empty in M1 — see `StackSlots` — and shown only on STACK, where the controls
        they are reserving for will live.
      */}
      {source === 'stack' && (
        <>
          <StackControlsSlot />
          {/*
            `null` is everything M1 can pass: nothing on the wire reports a rebuild, because
            nothing rebuilds. Phase 2b fills this from the stack node and changes nothing else.
          */}
          <RebuildingSlot progress={null} />
        </>
      )}

      {source === 'frame' && (
        <div className="flex flex-wrap items-center gap-2">
          <Button
            disabled={busy || !linkLive || !cameraConnected}
            onClick={() => void toggle(!streaming)}
          >
            {streaming ? 'Stop live view' : 'Start live view'}
          </Button>
          {linkLive && !cameraConnected && (
            <span className="text-sm text-muted">Connect the camera to use live view.</span>
          )}
        </div>
      )}

      {failure !== null && (
        <FailureNote
          failure={failure}
          action={streaming ? 'stopping live view' : 'starting live view'}
        />
      )}

      {explanation !== null && (
        <p role="status" className="rounded-md border border-edge bg-overlay p-3 text-sm text-fg">
          {explanation}
        </p>
      )}
    </section>
  );
}

/**
 * The tablet sketch's `[ FRAME │ STACK ]`, above the surface.
 *
 * A radio group rather than two buttons: the two are mutually exclusive views of one surface, and
 * `aria-checked` is what says so to a screen reader. On a phone the bottom navigation drives the
 * same store, so this and it can never disagree.
 */
function SourceToggle({
  source,
  onSelect,
}: {
  source: ImageSource;
  onSelect: (source: ImageSource) => void;
}): ReactNode {
  const options: readonly { id: ImageSource; label: string }[] = [
    { id: 'frame', label: 'Frame' },
    { id: 'stack', label: 'Stack' },
  ];

  return (
    <div
      role="radiogroup"
      aria-label="Image source"
      className="flex self-center rounded-md border border-edge bg-raised p-0.5"
    >
      {options.map((option) => {
        const current = source === option.id;
        return (
          <button
            key={option.id}
            type="button"
            role="radio"
            aria-checked={current}
            onClick={() => onSelect(option.id)}
            className={`min-h-touch rounded px-5 py-1.5 text-sm tracking-wide uppercase ${
              current ? 'bg-overlay text-accent' : 'text-muted'
            }`}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}

/** The alt text for whichever source produced the picture on screen. */
function altFor(source: 'live' | 'preview' | 'stack', frameId?: string): string {
  switch (source) {
    case 'stack':
      return `Stacking server preview${frameId === undefined ? '' : ` of frame ${frameId}`}`;
    case 'preview':
      return `Preview of frame ${frameId ?? ''}`.trim();
    default:
      return 'Live view from the camera';
  }
}

/**
 * What the surface says when there is no image on it yet.
 *
 * The two sources are empty for entirely different reasons, so they must not share a sentence.
 * FRAME's emptiness is the live-view phase machine's business (§5.7 — a pause during an exposure
 * is normal and must not read as a fault). STACK's is a trip through two nodes: a frame has to be
 * captured, uploaded, ingested and previewed before anything can arrive here, and "waiting for
 * the first frame" over a working link would look like a stall when it is a sequence that has not
 * started.
 */
function Placeholder({
  source,
  phase,
  stackConnected,
  linkLive,
}: {
  source: ImageSource;
  phase: SurfacePhase;
  stackConnected: boolean;
  linkLive: boolean;
}): ReactNode {
  const message =
    source === 'stack'
      ? stackMessage(stackConnected, linkLive)
      : 'message' in phase
        ? phase.message
        : 'Waiting for the first frame…';

  return (
    <div className="flex h-full w-full items-center justify-center p-6 text-center">
      <p role="status" className="text-sm text-faint">
        {message}
      </p>
    </div>
  );
}

function stackMessage(connected: boolean, linkLive: boolean): string {
  if (!linkLive) return 'Not connected to the field node.';
  // The socket being down and the stacking server being down are the same symptom here: no
  // picture. The panel beside this one carries the diagnosis, and saying it twice in different
  // words is how two parts of a screen come to disagree.
  if (!connected) return 'Waiting for the stacking server’s preview link…';
  return 'No stacked preview yet. The first one arrives after a frame reaches the stacking server.';
}

/**
 * The caption over the image: what is being shown, and why it is or is not moving.
 *
 * Rendered even while streaming normally, because "which of the two sources am I looking at" is a
 * question the operator would otherwise answer by guessing — a live view of a star field and a
 * preview of the same star field look identical.
 */
function StatusOverlay({
  phase,
  source,
  frameId,
}: {
  /** `null` on the stack source: the live-view phase machine describes the camera, not the stack. */
  phase: SurfacePhase | null;
  source: 'live' | 'preview' | 'stack';
  frameId?: string | undefined;
}): ReactNode {
  const label =
    source === 'stack'
      ? `Stacked preview${frameId === undefined ? '' : ` · ${frameId}`}`
      : source === 'preview'
        ? `Last frame${frameId === undefined ? '' : ` · ${frameId}`}`
        : 'Live view';

  if (phase === null) {
    return (
      <div className="pointer-events-none absolute inset-x-0 top-0 flex flex-col gap-1 bg-gradient-to-b from-surface/80 to-transparent p-3">
        <span className="font-mono text-xs tracking-wide text-muted uppercase">{label}</span>
      </div>
    );
  }

  return (
    <div className="pointer-events-none absolute inset-x-0 top-0 flex flex-col gap-1 bg-gradient-to-b from-surface/80 to-transparent p-3">
      <span className="font-mono text-xs tracking-wide text-muted uppercase">{label}</span>
      {phase.kind === 'capturing' && (
        <p role="status" className="text-sm text-fg">
          {phase.message}{' '}
          <span className="tabular font-mono">
            {phase.remainingS === null
              ? `${formatDuration(phase.elapsedS)} so far`
              : `${formatDuration(phase.remainingS)} left`}
          </span>
        </p>
      )}
      {phase.kind === 'stalled' && (
        <p role="status" className="text-sm text-warn">
          {phase.message}
        </p>
      )}
      {phase.kind === 'starting' && (
        <p role="status" className="text-sm text-muted">
          {phase.message}
        </p>
      )}
    </div>
  );
}

/** Zoom limits. Below 1 the image would float inside its own frame; above 8 it is pixels. */
const MIN_SCALE = 1;
const MAX_SCALE = 8;

/**
 * Pinch to zoom, drag to pan, double-tap to reset.
 *
 * Pointer events rather than a gesture library: this is the whole of the interaction, a library
 * would be bytes USB-10 has to move before the shell renders, and Android/Chrome is the only
 * target (§5.9) so there is no cross-browser pointer-event story to buy.
 *
 * `touch-action: none` is load-bearing. The app sets `touch-action: manipulation` globally so
 * Chrome does not add its 300 ms double-tap delay to every control; that setting also leaves the
 * browser owning pinch, which would scroll the page instead of zooming the image. This is the one
 * surface that wants the raw pointers.
 *
 * `.image-surface` is the night-mode filter hook — a stretched star field is the most
 * greyscale-white thing in the app and destroys 20–30 minutes of dark adaptation in one glance
 * (§5.9). It stays on the content, so the filter follows the image rather than the furniture.
 */
function Zoomable({ children }: { children: ReactNode }): ReactNode {
  const [scale, setScale] = useState(1);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  // Live pointer positions, outside React state: these change on every `pointermove` and a render
  // per move would drop frames on a phone.
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const pinch = useRef<{ distance: number; scale: number } | null>(null);
  const pan = useRef<{ x: number; y: number; offset: { x: number; y: number } } | null>(null);
  const lastTap = useRef(0);

  function down(event: ReactPointerEvent<HTMLDivElement>) {
    event.currentTarget.setPointerCapture(event.pointerId);
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });

    if (pointers.current.size === 2) {
      pinch.current = { distance: spread(pointers.current), scale };
      pan.current = null;
    } else if (pointers.current.size === 1) {
      pan.current = { x: event.clientX, y: event.clientY, offset };
      const tappedAt = Date.now();
      if (tappedAt - lastTap.current < 300) {
        // Double-tap resets. The gesture an operator reaches for without being told, and the only
        // way back from a zoom that went somewhere unhelpful in the dark.
        setScale(1);
        setOffset({ x: 0, y: 0 });
        pan.current = null;
      }
      lastTap.current = tappedAt;
    }
  }

  function move(event: ReactPointerEvent<HTMLDivElement>) {
    if (!pointers.current.has(event.pointerId)) return;
    pointers.current.set(event.pointerId, { x: event.clientX, y: event.clientY });

    if (pointers.current.size >= 2 && pinch.current !== null) {
      const distance = spread(pointers.current);
      if (pinch.current.distance > 0) {
        setScale(clamp((pinch.current.scale * distance) / pinch.current.distance, MIN_SCALE, MAX_SCALE));
      }
      return;
    }

    if (pan.current !== null && scale > 1) {
      // Divided by the scale so a drag moves the image by the distance the finger moved rather
      // than by that distance magnified — which at 8× feels like the image is fleeing.
      setOffset({
        x: pan.current.offset.x + (event.clientX - pan.current.x) / scale,
        y: pan.current.offset.y + (event.clientY - pan.current.y) / scale,
      });
    }
  }

  function up(event: ReactPointerEvent<HTMLDivElement>) {
    pointers.current.delete(event.pointerId);
    if (pointers.current.size < 2) pinch.current = null;
    if (pointers.current.size === 0) {
      pan.current = null;
      // Back to centre when the zoom is released near 1, so the image cannot be left
      // imperceptibly off-centre with no obvious way back.
      if (scale <= MIN_SCALE + 0.01) setOffset({ x: 0, y: 0 });
    }
  }

  return (
    <div
      className="image-surface aspect-[4/3] w-full touch-none overflow-hidden"
      onPointerDown={down}
      onPointerMove={move}
      onPointerUp={up}
      onPointerCancel={up}
    >
      <div
        className="h-full w-full origin-center"
        style={{
          transform: `scale(${scale}) translate(${offset.x}px, ${offset.y}px)`,
          // No transition: a transition on a property that changes every `pointermove` renders
          // the gesture as lag rather than as smoothing.
          willChange: 'transform',
        }}
      >
        {children}
      </div>
    </div>
  );
}

function spread(pointers: Map<number, { x: number; y: number }>): number {
  const [a, b] = Array.from(pointers.values());
  if (a === undefined || b === undefined) return 0;
  return Math.hypot(a.x - b.x, a.y - b.y);
}

function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}
