# frontend/ — placeholder

The React + TypeScript + Vite PWA lives here. **M0-T01 creates only this placeholder**; the
scaffold itself is [M0-T06](../docs/plan/tasks/M0-scaffolding/M0-T06-frontend-pipeline.md).

What M0-T06 puts here, per SDD §5.9:

- Vite + React + TypeScript with a strict `tsconfig`
- the PWA manifest and a service worker that caches the app shell **only** — never API
  responses (USB-10)
- a production build in `frontend/dist/`, embedded into `astroctl-field` with `include_dir!`
  and served at `/` with SPA fallback. `cargo build` must keep working when `dist/` is
  absent, so the embedding is behind a cfg/feature fallback.

`dist/` and `node_modules/` are git-ignored at the repo root.
