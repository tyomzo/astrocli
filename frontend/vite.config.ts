import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
// `vitest/config` re-exports Vite's `defineConfig` with the `test` key typed. One config file
// rather than two, for the same reason `scripts/check.sh` is one list of gates: two copies drift.
import { defineConfig } from 'vitest/config';

/**
 * Where `npm run dev` forwards `/api`, `/stack` and `/ws`.
 *
 * `server.port` of `config/field-node.example.yaml` — and deliberately also the port
 * `npm run mock` listens on, so switching between the mock and a real field node is starting or
 * stopping a process rather than editing anything.
 */
const FIELD_NODE = 'http://127.0.0.1:8470';

export default defineConfig({
  plugins: [tailwindcss(), react()],

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
      '/api': FIELD_NODE,
      '/stack': FIELD_NODE,
      // `ws: true` is what makes the upgrade cross the proxy; without it the socket 404s and the
      // app looks like it cannot reach a node that is answering every REST call it makes.
      '/ws': { target: FIELD_NODE, ws: true },
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
