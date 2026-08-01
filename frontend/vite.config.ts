import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
// `vitest/config` re-exports Vite's `defineConfig` with the `test` key typed. One config file
// rather than two, for the same reason `scripts/check.sh` is one list of gates: two copies drift.
import { defineConfig } from 'vitest/config';

/**
 * Where `npm run dev` forwards `/api`, `/stack` and `/ws`.
 *
 * Defaults to `server.host`/`server.port` of `config/field-node.example.yaml`, so a field node
 * started from the example config is reachable with nothing else configured. (Until M1-T03 this
 * was also the port `npm run mock` listened on; the mock is gone and the real binary is what runs
 * here now.)
 *
 * Overridable because a node with `server.tls` configured serves **https on the same port**, and
 * the example ships that block commented out — so the default is right for the common case and
 * silently wrong for the deployment one, which is the worst combination. A developer pointing at
 * a TLS node sets:
 *
 * ```sh
 * ASTROCTL_DEV_NODE=https://127.0.0.1:8470 npm run dev
 * ```
 */
// This file runs in Node, but the app it configures does not, so `types` in tsconfig.json is
// `["vite/client"]` and there is no `process`. Declaring the one member used here is a smaller
// and more honest change than adding `@types/node` to the whole project for a single env read —
// and it keeps the app's own code unable to reach `process`, which is the reason `vite/client`
// was the only entry in the first place.
declare const process: { env: Record<string, string | undefined> };

const FIELD_NODE = process.env['ASTROCTL_DEV_NODE'] ?? 'http://127.0.0.1:8470';

/**
 * Certificate verification is off for the dev proxy, and only for it.
 *
 * The field node's certificate is issued for its public name (`field.diirc.online`), so reaching
 * it at `127.0.0.1` fails hostname verification no matter how valid the certificate is. This
 * affects the Vite dev server on a developer's machine and nothing that ships — the PWA in
 * production is served *by* the node, over the same TLS the browser validates itself.
 */
const proxyTarget = { target: FIELD_NODE, secure: false, changeOrigin: true };

export default defineConfig({
  plugins: [tailwindcss(), react()],

  // The build identifies itself. Injected at build time so the running page can *say* which
  // bundle it is — the alternative, observed in the field, is an operator staring at a phone
  // with no way to tell a stale service-worker copy from the deploy they were promised.
  define: {
    __ASTROCTL_BUILD__: JSON.stringify(
      `${process.env.ASTROCTL_GIT_SHA ?? 'dev'} · ${new Date()
        .toISOString()
        .slice(0, 16)
        .replace('T', ' ')}Z`,
    ),
  },

  build: {
    // Android/Chrome is the only target (SDD §5.9), so there is no reason to down-level to a
    // baseline no device in this deployment runs. Every byte of transpilation output is a byte
    // USB-10's "opens while the tunnel is still connecting" has to carry.
    target: 'es2022',
    // The bundle is embedded in the binary (ARC-02) and served over a VPN to one operator; a
    // source map would roughly double what `include_dir!` compiles into astroctl-field for a
    // debugging affordance nobody has on a phone in a field.
    sourcemap: false,
    // Everything under /assets is content-hashed, which is what lets the service worker cache it
    // with `immutable` and never revalidate.
    assetsDir: 'assets',
  },

  server: {
    // Same-origin in development too. ADR-07 says the browser only ever talks to the field node,
    // and a dev setup that reaches the stack node directly would let a violation of that pass
    // review by working on the developer's machine.
    proxy: {
      '/api': proxyTarget,
      '/stack': proxyTarget,
      // `ws: true` is what makes the upgrade cross the proxy; without it the socket 404s and the
      // app looks like it cannot reach a node that is answering every REST call it makes.
      '/ws': { ...proxyTarget, ws: true },
    },
  },

  test: {
    // No DOM, and no jsdom dependency. Most of what is tested is deliberately renderer-free — the
    // reducer, the coordinate notation, the reconnect state machine. The components that are
    // tested are rendered with `react-dom/server`, which needs no document: that covers the ones
    // whose output is a requirement in itself (the nudge badge's redundant encoding), and stops
    // short of interaction, which is what the task's device-gated criteria are for.
    environment: 'node',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
