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
npm run dev        # Vite dev server, proxying /api and /stack to 127.0.0.1:8470
npm run typecheck  # tsc --noEmit; Vite itself never type-checks
npm run icons      # regenerate the PWA icon set after changing the artwork
```

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
has the long version. Commands change no state; only observations do, they carry the instant they
were observed at, and subscription is selector-based. M1-T04 adds the WebSocket feed as another
action source into the same reducer.

## Target platform

**Android/Chrome.** iOS may work and is not tested or gated by anything (PRD §11 change note
1.15.0), which is what buys the Screen Wake Lock, `beforeinstallprompt` and reliance on
service-worker cache persistence that `src/lib/pwa/` uses.

The service worker (`public/sw.js`) caches the shell and **never** touches `/api`, `/stack` or
`/ws` — it does not even call `respondWith` for them. A cached telemetry reading is a UI that lies
about where the mount is.

`dist/` and `node_modules/` are git-ignored at the repo root.
