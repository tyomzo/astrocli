import { FIELD_HEALTH, STACK_HEALTH, getJson } from './api';
import type { NodeHealth, NodeId } from '../store/nodes';
import { NODE_IDS, dispatch } from '../store/nodes';

/**
 * Take one health observation per node and reduce it into the store.
 *
 * This is the only writer of node state in the app, and it writes exclusively by dispatching —
 * see the discipline note in `store/nodes.ts`. M1-T04 adds a second source (the socket) that
 * dispatches into the same reducer.
 */

const PATHS: Record<NodeId, string> = {
  field: FIELD_HEALTH,
  stack: STACK_HEALTH,
};

export async function probe(node: NodeId, token: string | null): Promise<void> {
  dispatch({ type: 'health/probing', node, at: Date.now() });
  const result = await getJson<NodeHealth>(PATHS[node], token);
  dispatch(
    result.ok
      ? { type: 'health/observed', node, at: Date.now(), health: result.value }
      : { type: 'health/unavailable', node, at: Date.now(), failure: result.failure },
  );
}

/**
 * Probe both nodes concurrently.
 *
 * Concurrently rather than in sequence because the stack probe crosses two hops and may sit on
 * the proxy's 30 s upstream timeout; running it after the field probe would make a healthy field
 * node look slow for a reason that has nothing to do with it.
 */
export async function probeAll(token: string | null): Promise<void> {
  await Promise.all(NODE_IDS.map((node) => probe(node, token)));
}
