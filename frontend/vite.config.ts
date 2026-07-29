import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

/** Where `npm run dev` forwards `/api` and `/stack` — `server.port` of config/field-node.example.yaml. */
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
    },
  },
});
