# M0-T06 — Frontend pipeline and PWA shell

**Milestone:** M0 · **Depends on:** M0-T01 (integrates after T05) · **Crates:** frontend/, astroctl-field
**Size:** M · **Status:** done
**Spec:** SDD §5.9 (stack, colour architecture, store discipline, target platform); PRD USB-01/02/08/09/10/12, ARC-02/ARC-14, EXT-06

## Objective

React + TypeScript + Vite scaffold whose production build is embedded in and served by the field
binary, plus an installable PWA shell with offline-cached chrome.

**This task sets the pattern five M1 tasks then follow** — M1-T04 (WS store, mount panel), T08
(camera panel), T09 (live view), T14 (stack panel), T15 (predictive display). Conventions
established here are cheap now and expensive later, which is why the design was settled before
implementation rather than during it.

## Decisions already made — implement these, do not re-litigate

Per SDD §5.9:

- **Tailwind** over a semantic token layer, plus headless primitives (Radix/Ark) only where
  accessible behaviour is genuinely needed. Not a component library — the visual language here is
  unusual enough that a general-purpose one gets fought rather than used, and bundle size is a
  functional requirement (USB-10).
- **Zustand** for state, shaped as an explicit reducer over the WS event stream, with
  selector-based subscription.
- **Android/Chrome is the target.** iOS is untested and not gated. EXT-06 is advisory, so
  Android-only APIs are permitted where they earn their place.
- **True black surfaces**, not dark grey — better on OLED and better for dark adaptation.

## Scope

**Colour tokens first.** Every colour resolves through semantic CSS custom properties
(`--surface`, `--fg`, `--accent`, `--warn`, `--danger`, …) — never a literal in a component. Ship
the `:root[data-mode="night"]` override alongside the default even though USB-02's night mode is
Phase 4: proving the mechanism now is what keeps it cheap, and a token layer with no second theme
is one nobody trusts. **A component containing a hex colour is a review failure.**

**Layout skeleton** per SDD §5.9 — header status bar with a permanently reserved e-stop slot,
single-column phone / multi-panel tablet breakpoints (USB-08). Touch targets: 44 px floor,
**60–70 px for primary controls** because the operator may be gloved, e-stop larger still and
positionally constant across every view (USB-03, USB-12).

**PWA** — manifest (name, icons, theme colour, `display: standalone`), service worker caching the
shell only and **never API responses** (USB-10). Wire `beforeinstallprompt` for a real install
flow. Hold a **Screen Wake Lock** while the app is foregrounded: the display must not sleep while
someone is watching live view, and this is one of the things targeting Android buys.

**Health screen** — the only screen in M0. Fetches field health plus stack health *through the
field node's proxy*, bearer token attached. Token entry in `localStorage` for now, with a `TODO`
naming the onboarding that replaces it.

**Embedding** — `include_dir!` of `frontend/dist` served by astroctl-field at `/` with SPA
fallback. `cargo build` must succeed when `dist/` is absent (a `cfg` fallback page saying "run the
frontend build"), so a Rust-only contributor is never blocked on npm. M0-T05 owns the axum app:
put static serving in its own module and wire it with one line.

**`scripts/build-frontend.sh`** producing `dist/` deterministically.

Out of scope: any panel beyond the health screen, the WS store (M1-T04), and night-mode *styling*
beyond proving the token override works.

## Acceptance criteria

- [x] `npm run build && cargo run -p astroctl-field` serves the app — verified with `curl`.
      **Lighthouse installability is not checked** (needs a browser; see Result notes)
- [ ] **Add-to-home-screen works on Android** (manual check, documenting device and browser
      version). iOS is explicitly not checked — **outstanding, needs a device**
- [ ] Screen Wake Lock holds while foregrounded and releases on background — **outstanding,
      needs a device**
- [ ] Shell loads with the network disabled after a first visit — **outstanding, needs a
      browser**; health screen shows both nodes with a valid token and a clear error state
      without one is implemented and rendered headlessly
- [x] Stack health arrives **through the field node's proxy**. Asserted structurally instead of
      from a network log: the app's only stack path is the relative `/stack/api/system/health`,
      and `lib/api.ts` throws on any absolute URL
- [x] `grep -rE '#[0-9a-fA-F]{3,6}' frontend/src --include=*.tsx` returns nothing
- [x] Setting `data-mode="night"` on the root re-themes the shell without touching a component —
      every colour-bearing rule in the built CSS resolves through `var(--token)`
- [x] `cargo build -p astroctl-field` succeeds with `frontend/dist` deleted

## Result notes

No SDD amendment was needed; §5.9 covered everything this task touched.

Deliberate decisions and interpretations, each argued in the code it affects:

- **Merging the PWA under `/` would have silently undone an M0-T05 property.** Auth sits outside
  routing so that "wrong token" and "no such path" are indistinguishable; an unauthenticated SPA
  fallback answers 200 + HTML for an undeclared `/api/...` path, which gives the difference away.
  `pwa.rs` therefore refuses to serve the shell for `/api`, `/stack` and `/ws` prefixes, re-runs
  `AuthPolicy::authorize` and renders the 401 through the *same function* the middleware uses.
  Undeclared API paths are 401 without a credential and 404 with one.
- **`/ws` is reserved before the WS hub exists** (M1-T03). An upgrade request that fell through to
  the SPA fallback would get `index.html` and a browser waiting forever on a handshake.
- **No headless component library and no Radix/Ark yet.** SDD §5.9 permits headless primitives
  "only where accessible behaviour is genuinely needed"; the health screen has no dialog, slider
  or menu. M1-T04's speed selector is the first real candidate.
- **No poller.** §5.9's store discipline says no REST polling, so the health screen observes on
  mount, on credential change and on `visibilitychange` — not on a timer that M1-T04 would delete.
- **The e-stop is present, unarmed, and says so.** `/api/mount/estop` lands with M1-T03. It is not
  `disabled` — §5.9 requires an unavailable affordance to explain itself, and a disabled button
  fires no event — and it carries the hollow-plus-slash treatment so its state survives night mode.
- **The header clock shows UTC, not LST.** LST needs `astroctl-planning` (M1). A local time
  labelled as sidereal would be worse than neither.
- **Lighthouse, add-to-home-screen, offline start and wake-lock behaviour are unverified.** They
  need a real browser or a real Android device; no Chrome was reachable from this environment.
  They are listed above as outstanding rather than assumed.
- **There is no frontend test runner.** Nothing in this task named one, and M1-T04 — which adds a
  reducer worth testing — is the right place to choose it. `store/nodes.ts` is written as a pure
  `reduce` function precisely so that choice is cheap.

## Notes for whoever picks this up

The store is out of scope, but the *shape* you leave behind is not: M1-T04 adds a WS-fed Zustand
store whose state must never be optimistically mutated by a command (SDD §5.9). Do not introduce a
pattern here — a `useState` that a fetch writes into, say — that M1-T04 then has to undo.
