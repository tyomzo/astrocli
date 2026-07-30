import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import type { SessionView } from '../lib/commands';
import { sessionCurrent } from '../lib/commands';
import { selectFramesSavedCount, selectLink, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Card, Field } from '../ui/Card';
import { FailureNote } from '../ui/FailureNote';

/**
 * Tonight's frames — SDD §5.8.1's `/api/session/current`, rendered.
 *
 * # Why this is a request and not a slot in the event store
 *
 * §5.9's store discipline says the telemetry store is "fed exclusively by WS events plus the
 * connect snapshot — no REST polling", and it is right about telemetry: a value that says what the
 * observatory is *doing* must never be a stale poll. A frame list is not that. It is a directory
 * listing with no cadence, it survives restarts of both the node and this app, and §5.8.3
 * deliberately keeps `frame.saved` out of the connect snapshot because replaying occurrences on
 * every reconnect shows the operator things they have already dealt with.
 *
 * So the list is read from the node, and re-read when the node says it changed — on a new
 * `frame.saved` and on the link coming back. There is no timer anywhere in this file. What the
 * operator sees is always a listing the node produced, never one this app assembled from the
 * captures it happened to issue, which is the property §5.9 is actually protecting.
 *
 * # A frame with no metadata is still a frame
 *
 * `quality: null` means the sidecar write was interrupted (SDD §5.5 note 3): the frame is durable,
 * its exposure parameters are not recorded. REL-05 says it is still a frame, so it is listed and
 * labelled rather than hidden — hiding it would tell an operator counting frames that an exposure
 * they watched happen did not.
 */
export function SessionFrames(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const link = useTelemetryStore(selectLink);
  const savedCount = useTelemetryStore(selectFramesSavedCount);

  const [session, setSession] = useState<SessionView | null>(null);
  const [failure, setFailure] = useState<RequestFailure | null>(null);

  const live = link.phase === 'live';
  useEffect(() => {
    if (!live) return;
    let current = true;
    void sessionCurrent(token).then((result) => {
      if (!current) return;
      if (result.ok) {
        setSession(result.value);
        setFailure(null);
      } else {
        setFailure(result.failure);
      }
    });
    return () => {
      current = false;
    };
    // `savedCount` is the revalidation trigger: it changes exactly when the node reports a frame
    // durable. `live` covers the reconnect, where frames may have landed while this app was away.
  }, [token, live, savedCount]);

  return (
    <Card
      title="Frames"
      accessory={
        session === null ? null : (
          <span className="font-mono text-sm text-muted">{session.frame_count}</span>
        )
      }
    >
      {session === null ? (
        <p className="text-sm text-muted">
          {live ? 'Reading tonight&rsquo;s frames…' : 'No frame list on this connection.'}
        </p>
      ) : (
        <>
          <dl className={live ? '' : 'opacity-60'}>
            <Field label="session" value={session.session_id} />
            <Field label="equipment" value={session.equipment.telescope} />
          </dl>

          {session.frames.length === 0 ? (
            <p className="mt-3 text-sm text-faint">
              No frames yet. The first exposure of the night appears here as soon as it is stored.
            </p>
          ) : (
            <ul className="mt-3 flex flex-col gap-1">
              {/*
                Newest first: the frame an operator wants to check is the one that just landed, and
                a list that grows downward puts it off the bottom of a phone by frame twenty.
              */}
              {[...session.frames].reverse().map((frame) => (
                <li
                  key={frame.frame_id}
                  className="flex items-baseline justify-between gap-3 border-b border-edge py-1 last:border-b-0"
                >
                  <span className="font-mono text-sm text-fg">{frame.frame_id}</span>
                  <span className="font-mono text-sm text-muted">
                    {frame.quality === null ? (
                      <span className="text-warn" title="stored before its metadata was written">
                        no exposure record
                      </span>
                    ) : (
                      `${frame.quality.settings.shutter}s · ISO ${frame.quality.settings.iso}`
                    )}
                  </span>
                  <span className="font-mono text-sm text-faint">{megabytes(frame.size_bytes)}</span>
                </li>
              ))}
            </ul>
          )}

          {session.frames_reserved > session.frame_count && (
            <p className="mt-3 text-sm text-faint">
              {/*
                A gap in the numbering is normal and costs nothing: an id reaches disk before it is
                granted, so an exposure that never completed leaves its number unused rather than
                letting a later frame reuse it (SDD §5.5 note 2, REL-04). Saying so beats an
                operator finding light_00007 missing and assuming a frame was lost.
              */}
              {session.frames_reserved - session.frame_count} numbered slot
              {session.frames_reserved - session.frame_count === 1 ? '' : 's'} went unused — an
              exposure that did not complete never reuses its number.
            </p>
          )}
        </>
      )}

      {failure !== null && <FailureNote failure={failure} action="reading the frame list" />}
    </Card>
  );
}

/** Frames are tens of megabytes; bytes are unreadable and gigabytes lose the distinction. */
function megabytes(bytes: number): string {
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}
