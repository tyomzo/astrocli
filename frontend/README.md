# frontend/ — the operator PWA

React + TypeScript + Vite. The production build lands in `frontend/dist/` and is compiled into
`astroctl-field` with `include_dir!`, so deployment stays a single service (ARC-02). Scaffolded by
[M0-T06](../docs/plan/tasks/M0-scaffolding/M0-T06-frontend-pipeline.md); the design is
[SDD §5.9](../docs/design/ASTROCTL-SDD-001.md).

## Build

```sh
../scripts/build-frontend.sh          # npm ci, clean, build — what CI and a release use
../scripts/build-frontend.sh --no-install --check   # iterate, and verify determinism
cargo build -p astroctl-field         # picks the bundle up
```

`cargo build` works with `dist/` absent: `build.rs` sets `cfg(pwa_embedded)` only when the bundle
exists, and without it the binary serves a page telling you to run the script. Nobody on the Rust
side needs Node installed.

```sh
npm run dev        # Vite dev server, proxying /api, /stack and /ws to 127.0.0.1:8470
npm test           # vitest, run once; part of scripts/check.sh's frontend gate
npm run test:watch # vitest in watch mode
npm run typecheck  # tsc --noEmit; Vite itself never type-checks
npm run icons      # regenerate the PWA icon set after changing the artwork
```

## Running it without a field node

`mock/` is a dev-only Node process that impersonates the field node's Phase 1 surface — the
ws-ticket exchange, `/ws` with snapshot-on-connect, and the mount routes — closely enough to drive
every state the mount panel can be in, including a goto ramp and an expiring slew lease.

```sh
npm run mock       # terminal 1: 127.0.0.1:8470, the port vite already proxies to
npm run dev        # terminal 2: http://localhost:5173/
```

**[`mock/README.md`](mock/README.md) is the written contract** — every route, every frame, and the
two frame shapes SDD §5.8.3 leaves undefined. M1-T03 implements that and deletes `mock/`.

## Tests

`vitest`, `environment: 'node'`, no jsdom. What is tested is what would fail silently: the store's
reducer (especially that a resnapshot *replaces* rather than merges), the reconnect state machine
(a fresh ticket per attempt, backoff, buffered pre-snapshot events, a socket that goes silent
without closing), coordinate notation, and the two rules this codebase enforces mechanically —
that `lib/commands.ts` cannot import the store, and that the nudge badge encodes its state in the
glyph and not only in the colour.

Components that read stores are not rendered in tests; that is what the device-gated acceptance
criteria are for.

## The four conventions this scaffold exists to set

M1 adds five panels on top of this. These are the parts that are cheap now and expensive later.

**1. Colour lives in exactly one file.** `src/styles/tokens.css` holds every literal; everything
else names a semantic token through a Tailwind utility (`bg-surface`, `text-danger`). This is
checkable, so check it:

```sh
grep -rE '#[0-9a-fA-F]{3,6}' src --include=*.tsx    # must print nothing
```

Night mode (USB-02, Phase 4) is then the `:root[data-mode="night"]` block in that same file —
setting `data-mode="night"` on `<html>` re-themes the app with no component involved. The override
ships now, unfinished, because a token layer with one theme is one nobody trusts.

**2. Colour is never the only channel.** Night mode collapses every hue toward red, so `--ok` and
`--danger` become two shades of the same colour, and ~8% of men have a red-green deficiency in any
lighting. State is carried by shape or glyph first — filled/hollow/slashed — with colour second.
See `src/ui/StatusBadge.tsx`.

**3. Touch targets come from the token layer.** `min-h-touch` is USB-12's 44 px floor for
incidental controls; `min-h-control` is the 60–70 px band every control that starts or stops
something physical belongs in, because the operator may be gloved. The e-stop is larger still and
sits in a fixed header slot on every screen (USB-03).

**4. The store is a reducer over observations, and nothing else writes it.** `src/store/nodes.ts`
has the long version and `src/store/telemetry.ts` is the socket-fed one M1-T04 added. Commands
change no state; only observations do, they carry the instant they were observed at, and
subscription is selector-based.

That rule is enforced, not just documented: `src/lib/commands.ts` imports nothing from `store/`,
and `commands.test.ts` fails the build if it ever does. A command layer that cannot see the store
cannot write to it, and the telemetry reducer has no action a command could dispatch either.

## Target platform

**Android/Chrome.** iOS may work and is not tested or gated by anything (PRD §11 change note
1.15.0), which is what buys the Screen Wake Lock, `beforeinstallprompt` and reliance on
service-worker cache persistence that `src/lib/pwa/` uses.

The service worker (`public/sw.js`) caches the shell and **never** touches `/api`, `/stack` or
`/ws` — it does not even call `respondWith` for them. A cached telemetry reading is a UI that lies
about where the mount is.

`dist/` and `node_modules/` are git-ignored at the repo root.
