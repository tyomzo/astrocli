import { useEffect, useState } from 'react';

/**
 * A ticking wall clock, for rendering ages.
 *
 * This is **not** a poller and does not violate SDD §5.9's "no REST polling": it asks nothing of
 * the node. It exists because "12 s ago" has to become "13 s ago" without an event, and telemetry
 * age is most important exactly when no events are arriving (§8.3).
 */
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);

  return now;
}
