import { create } from 'zustand';

import type { RequestFailure } from '../lib/api';

/*
 * Node health, as an explicit reducer over observations — SDD §5.9 "Store discipline".
 *
 * # The shape M1-T04 inherits
 *
 * M0 has no WebSocket, so there is nothing to reduce yet except two HTTP probes. The *shape* is
 * the deliverable anyway, because M1-T04 adds the WS-fed store and the one thing it must not have
 * to do first is undo a pattern established here. Three rules, all of them load-bearing:
 *
 * 1. **Nothing outside `dispatch` writes state.** There is no `setHealth`, no `set` exported, no
 *    component-level `useState` mirroring a fetch. A new event source in M1 is a new action
 *    variant and a new arm of `reduce` — not a new writer.
 * 2. **Commands do not write state; observations do.** SDD §5.9: a command is a REST call that
 *    optimistically does nothing, and the UI moves only when the corresponding event arrives. On
 *    a link where a command may not have landed, optimistic UI actively lies about where the
 *    mount is. `reduce` has no action a *command* can dispatch, and that is deliberate.
 * 3. **Every observation carries the wall-clock instant it was observed at.** M1-T15's
 *    confirmed/predicted/stale triad and §8.3's telemetry-age indicator are both derived from
 *    that field; a store that overwrites values without timestamping them cannot tell the
 *    operator how old their picture is, and by then the fix is a migration.
 *
 * Subscription is selector-based ([`useNodes`] takes a selector) because §5.9 needs 1 Hz
 * `mount.position` updates not to re-render panels that never read a position.
 *
 * There is deliberately **no poller**. §5.9 forbids REST polling as the store's feed; the health
 * screen takes one-shot observations on mount, on token change and when the tab becomes visible.
 * M1-T04 is what makes this live, by feeding the same reducer from the socket.
 */

/** The two nodes of ADD §5.5. `stack` is reached through the field node's proxy, never directly. */
export type NodeId = 'field' | 'stack';

export const NODE_IDS: readonly NodeId[] = ['field', 'stack'];

/** `GET /api/system/health` — SDD §5.8.1 (field) and §5.11.1 (stack). */
export interface NodeHealth {
  v: number;
  status: 'starting' | 'ok';
  /** `null` when the volume cannot be interrogated — never a fabricated 0. */
  disk_free_gb: number | null;
  clock_synced: boolean;
  uptime_s: number;
  versions: {
    astroctl: string;
    api: number;
    event: number;
    error_envelope: number;
  };
  /** Stack node only, and `null` until the worker supervisor exists (M1-T13). */
  worker?: { state: string; restarts: number } | null;
}

/**
 * The last thing actually learned about one node.
 *
 * `unknown` and `unavailable` are separate states because they are different facts: nobody has
 * asked yet, versus the node was asked and did not answer. Collapsing them is how a UI ends up
 * showing a red badge for a node it has simply not got round to probing.
 */
export type NodeObservation =
  | { state: 'unknown' }
  | { state: 'observed'; at: number; health: NodeHealth }
  | { state: 'unavailable'; at: number; failure: RequestFailure };

/**
 * An observation plus whether a fresher one is on its way.
 *
 * The two are separate fields, and this is the part M1 depends on: a request in flight must not
 * erase what is already known. §8.3 has the header showing telemetry *age*, which requires the
 * previous picture to still be there while the next one is being fetched — and a store that
 * blanks itself on every refresh cannot render "confirmed / predicted / stale" (M1-T15) at all,
 * because two of those three states are about values that are no longer fresh.
 */
export interface NodeSlot {
  observation: NodeObservation;
  probing: boolean;
}

export type NodeAction =
  | { type: 'health/probing'; node: NodeId; at: number }
  | { type: 'health/observed'; node: NodeId; at: number; health: NodeHealth }
  | { type: 'health/unavailable'; node: NodeId; at: number; failure: RequestFailure }
  /** The operator changed the credential, so everything known was learned under the old one. */
  | { type: 'session/reset' };

export interface NodesState {
  nodes: Record<NodeId, NodeSlot>;
}

const IDLE: NodeSlot = { observation: { state: 'unknown' }, probing: false };

const EMPTY: NodesState = { nodes: { field: IDLE, stack: IDLE } };

export function reduce(state: NodesState, action: NodeAction): NodesState {
  switch (action.type) {
    case 'health/probing':
      return withNode(state, action.node, (slot) => ({ ...slot, probing: true }));
    case 'health/observed':
      return withNode(state, action.node, () => ({
        observation: { state: 'observed', at: action.at, health: action.health },
        probing: false,
      }));
    case 'health/unavailable':
      return withNode(state, action.node, () => ({
        observation: { state: 'unavailable', at: action.at, failure: action.failure },
        probing: false,
      }));
    case 'session/reset':
      return EMPTY;
  }
}

function withNode(
  state: NodesState,
  node: NodeId,
  update: (slot: NodeSlot) => NodeSlot,
): NodesState {
  return { ...state, nodes: { ...state.nodes, [node]: update(state.nodes[node]) } };
}

interface NodesStore extends NodesState {
  dispatch: (action: NodeAction) => void;
}

export const useNodesStore = create<NodesStore>()((set) => ({
  ...EMPTY,
  dispatch: (action) => set((state) => reduce(state, action)),
}));

/** Read one node without subscribing to the other — see the selector note above. */
export const selectNode =
  (node: NodeId) =>
  (state: NodesState): NodeSlot =>
    state.nodes[node];

export const dispatch = (action: NodeAction): void => useNodesStore.getState().dispatch(action);
