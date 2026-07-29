/*
 * AstroCtl service worker — USB-10, SDD §5.9 ("shell cached, data never cached").
 *
 * # The one rule
 *
 * **The shell is cached. Data is never cached.** A cached telemetry response is a UI that shows
 * the operator where the mount was, indistinguishably from where it is, which is the single thing
 * this display must never do. Everything below follows from that.
 *
 * The enforcement is stronger than "do not put API responses in a cache": for `/api`, `/stack` and
 * `/ws` this worker does not call `respondWith` at all. The request goes to the network as if no
 * service worker were installed, and this file never touches the response — so there is no code
 * path, present or future, in which a mistake here can serve a stale reading.
 *
 * # Why hand-written rather than Workbox
 *
 * Four rules, and bundle size is a functional requirement rather than a nicety (USB-10 promises
 * the shell opens while the tunnel is still connecting). A generated worker would be larger than
 * this file and harder to audit against the rule above, which is the only property that matters.
 *
 * # Update policy
 *
 * `skipWaiting` is deliberately NOT called. A new worker takes over on the next cold start, not
 * mid-session: swapping the JavaScript under a running app while the operator is watching a
 * capture is a self-inflicted version of exactly the failure the live-view panel exists to
 * explain. Content still self-heals, because the shell is served stale-while-revalidate.
 */

const CACHE = 'astroctl-shell-v1';

/**
 * The minimum that must be present for the app to render offline. Hashed assets are not listed —
 * their names are only known at build time — but they are cached on first use below, which is
 * enough: the first visit is by definition online.
 */
const SHELL = [
  '/',
  '/index.html',
  '/manifest.webmanifest',
  '/icons/icon-192.png',
  '/icons/icon-512.png',
  '/icons/icon-maskable-512.png',
];

/** Paths whose responses must never be cached, inspected or replayed. */
function isLiveData(pathname) {
  return (
    pathname === '/api' ||
    pathname.startsWith('/api/') ||
    pathname === '/stack' ||
    pathname.startsWith('/stack/') ||
    pathname === '/ws' ||
    pathname.startsWith('/ws/')
  );
}

/** Everything Vite emits into the bundle, plus the static files listed above. */
function isShellAsset(pathname) {
  return (
    pathname.startsWith('/assets/') ||
    pathname.startsWith('/icons/') ||
    pathname === '/manifest.webmanifest' ||
    pathname === '/favicon.ico'
  );
}

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(names.filter((name) => name !== CACHE).map((name) => caches.delete(name)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('fetch', (event) => {
  const request = event.request;

  // Non-GET is a command. Commands are never replayed from a cache, and a queued retry of one is
  // a motion command arriving at a time nobody asked for it (SDD §5.8.1 has a staleness window
  // for precisely this reason). Left entirely to the network.
  if (request.method !== 'GET') {
    return;
  }

  const url = new URL(request.url);
  if (url.origin !== self.location.origin || isLiveData(url.pathname)) {
    return;
  }

  // A navigation is the app shell whatever the path says — the server does the same thing (SPA
  // fallback), and answering from cache is what makes USB-10's "opens instantly" true.
  if (request.mode === 'navigate') {
    event.respondWith(shell(request));
    return;
  }

  if (isShellAsset(url.pathname)) {
    event.respondWith(staleWhileRevalidate(request));
  }
});

async function shell(request) {
  const cache = await caches.open(CACHE);
  const cached = await cache.match('/index.html');
  if (cached) {
    // Refresh in the background so the next launch has the new shell, without making this one
    // wait for a tunnel that may still be connecting.
    void revalidate(cache, new Request('/index.html'));
    return cached;
  }
  try {
    return await fetch(request);
  } catch {
    return new Response('AstroCtl is offline and its shell has not been cached yet.', {
      status: 503,
      headers: { 'Content-Type': 'text/plain; charset=utf-8' },
    });
  }
}

async function staleWhileRevalidate(request) {
  const cache = await caches.open(CACHE);
  const cached = await cache.match(request);
  const network = revalidate(cache, request);
  return cached ?? (await network) ?? Response.error();
}

async function revalidate(cache, request) {
  try {
    const response = await fetch(request);
    // Only complete, same-origin, successful responses. An opaque or partial response in the
    // cache is a blank screen on the next offline start.
    if (response.ok && response.type === 'basic') {
      await cache.put(request, response.clone());
    }
    return response;
  } catch {
    return undefined;
  }
}
