# M0-T06 — Frontend pipeline and PWA shell

**Milestone:** M0 · **Depends on:** M0-T01 (integrates after T05) · **Crates:** frontend/, astroctl-field
**Size:** M · **Status:** not started
**Spec:** SDD §5.9 (stack, embedding); PRD USB-08/09/10/12, ARC-02/ARC-14

## Objective

React+TS+Vite scaffold whose production build is embedded in and served by the field binary;
installable PWA shell with offline-cached chrome.

## Scope

- `frontend/`: Vite + React + TypeScript, strict tsconfig; layout skeleton per SDD §5.9 (header status bar with reserved e-stop slot, single-column phone / multi-panel tablet breakpoints, 44 px touch targets baseline)
- PWA: manifest (name, icons, theme, `display: standalone`), service worker caching the shell only — **never** API responses (USB-10)
- Health screen: fetches field + proxied stack health with the bearer token (token entry UI stored in localStorage for now; note: replaced by proper onboarding later)
- Embedding: `include_dir!` of `frontend/dist` served by astroctl-field at `/` with SPA fallback; `cargo build` works when `dist/` is absent (feature/cfg fallback page saying "run frontend build")
- `scripts/build-frontend.sh` producing dist deterministically

## Acceptance criteria

- [ ] `npm run build && cargo run -p astroctl-field` serves the app; Lighthouse recognizes installable PWA
- [ ] Add-to-home-screen works on Android and iOS Safari (manual check documented)
- [ ] Shell loads with network disabled after first visit (SW cache); health screen shows both nodes with valid token, clear error state otherwise
