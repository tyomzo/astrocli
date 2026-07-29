/**
 * Service-worker registration — USB-10.
 *
 * The worker itself is `public/sw.js`: hand-written, not generated. Workbox would have written a
 * better one, but it arrives as a build-time dependency whose output is several KB of runtime on
 * a page whose load time is a functional requirement, to solve a caching problem that is four
 * rules wide here. What it caches, and what it must never cache, is documented in that file.
 *
 * Not registered in development: Vite serves unbundled modules over HMR, and a worker caching
 * them turns every edit into "why is my change not showing".
 */
export function registerServiceWorker(): void {
  if (!import.meta.env.PROD || !('serviceWorker' in navigator)) {
    return;
  }
  // After `load`, so precaching the shell competes with nothing the operator is waiting for.
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch((error: unknown) => {
      // A failed registration costs offline start-up, nothing else. The app must still run.
      console.warn('service worker registration failed; the shell will not be cached', error);
    });
  });
}
