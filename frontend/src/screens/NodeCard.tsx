import type { ReactNode } from 'react';

import { age, diskFree, uptime } from '../lib/format';
import type { RequestFailure } from '../lib/api';
import type { NodeId, NodeSlot } from '../store/nodes';
import { selectNode, useNodesStore } from '../store/nodes';
import { Card, Field } from '../ui/Card';

/**
 * One node's health. The only screen content M0 has.
 *
 * `path` is displayed rather than hidden because it is the acceptance criterion made visible: the
 * stack node's row reads `/stack/api/system/health`, i.e. a request to the *field* node, and
 * anyone can confirm from the UI itself that the browser never dialled the stacking server
 * (ADR-07, STK-19).
 */
export function NodeCard({
  node,
  title,
  path,
}: {
  node: NodeId;
  title: string;
  path: string;
}): ReactNode {
  const slot = useNodesStore(selectNode(node));

  return (
    <Card title={title} accessory={<Freshness slot={slot} />}>
      <p className="mb-3 font-mono text-xs break-all text-faint">GET {path}</p>
      <Body slot={slot} />
    </Card>
  );
}

function Body({ slot }: { slot: NodeSlot }): ReactNode {
  const { observation } = slot;

  if (observation.state === 'unknown') {
    return <p className="text-sm text-muted">Not probed yet.</p>;
  }

  if (observation.state === 'unavailable') {
    return <Failure failure={observation.failure} />;
  }

  const { health } = observation;
  return (
    <dl>
      <Field label="status" value={health.status} />
      <Field label="uptime" value={uptime(health.uptime_s)} />
      <Field label="disk free" value={diskFree(health.disk_free_gb)} />
      <Field label="clock synced" value={health.clock_synced ? 'yes' : 'no'} />
      <Field label="version" value={health.versions.astroctl} />
      <Field
        label="schemas"
        value={`api ${health.versions.api} · event ${health.versions.event} · err ${health.versions.error_envelope}`}
      />
      {health.worker !== undefined && (
        <Field
          label="worker"
          value={
            // `null` is "no worker has ever been started", which the stack node distinguishes
            // from "the worker stopped" on purpose (SDD §5.12.3). Rendering it as "stopped"
            // would throw that distinction away at the last step.
            health.worker === null
              ? 'not started'
              : `${health.worker.state} · ${String(health.worker.restarts)} restarts`
          }
        />
      )}
    </dl>
  );
}

/**
 * Failure states, distinguished by what the operator should do next.
 *
 * That is the whole reason `RequestFailure` is a union rather than a message string: "fix your
 * token", "the other machine is down" and "the field node refused" lead to three different walks
 * across a dark field.
 */
function Failure({ failure }: { failure: RequestFailure }): ReactNode {
  switch (failure.kind) {
    case 'unauthorized':
      return (
        <Problem heading="Token rejected">
          {failure.message}. Enter the value of the node&rsquo;s{' '}
          <code className="font-mono">server.auth_token_env</code> variable below.
        </Problem>
      );
    case 'api':
      return (
        <Problem heading={failure.code}>
          {failure.message}
          {failure.retryable ? ' This one is worth retrying.' : ''}
        </Problem>
      );
    case 'transport':
      return <Problem heading="Not reachable">{failure.message}</Problem>;
  }
}

function Problem({ heading, children }: { heading: string; children: ReactNode }): ReactNode {
  return (
    <div className="rounded-md border border-danger/60 p-3">
      <p className="flex items-center gap-2 text-sm font-semibold text-danger">
        {/* Shape before colour, as everywhere else — see ui/StatusBadge.tsx. */}
        <span aria-hidden="true">⊘</span>
        {heading}
      </p>
      <p className="mt-1 text-sm text-muted">{children}</p>
    </div>
  );
}

/** How old the displayed picture is (SDD §8.3), and whether a fresher one is on its way. */
function Freshness({ slot }: { slot: NodeSlot }): ReactNode {
  if (slot.probing) {
    return <span className="text-xs text-faint">probing…</span>;
  }
  if (slot.observation.state === 'unknown') {
    return null;
  }
  return <span className="tabular text-xs text-faint">{age(slot.observation.at, Date.now())}</span>;
}
