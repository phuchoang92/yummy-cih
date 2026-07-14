# Graph UI — usability polish + light/dark theme

## Context

The `/graph` endpoint of `cih-server` serves a polished React 19 + Three.js "galaxy"
SPA. Its source lives in `graph-ui/src/` and is Vite-built into the server's embedded
assets (`crates/cih-server/assets/graph/{index.html,app.js,styles.css}`, wired via
`include_str!` in `crates/cih-server/src/browser.rs`). The UI is well-made, but a review
surfaced concrete rough edges that hurt usability and code health:

- **No in-UI legend.** In the 3D view, star color encodes *node degree* via a
  7-stop temperature ramp computed server-side (`layout.rs::stellar_color`), while the
  rail dots/chips encode *node kind* — two different color systems, both undocumented in
  the UI. Edge colors are also undecipherable. Users can't read the picture.
- **Color constants are duplicated and drift.** `KIND_COLORS` is redefined in
  `Overview.tsx` and `ClassicViews.tsx` with mismatched keys; `EDGE_COLORS` lives in
  `Scene.tsx`. No single source of truth.
- **Impact/Flow tabs are dead-ends.** Their run buttons are `disabled` until a node is
  selected, but neither tab offers a way to pick one — you must arrive from Overview/Search.
- **No camera controls.** Auto-rotate silently starts after 60s idle with no toggle, and
  there is no reset/fit-view.
- **`KindSelector` modal is the one unthemed screen** — styled with hardcoded inline hex
  instead of the CSS custom-property tokens the rest of the UI uses.
- **Stale docs.** `graph-ui/README.md` claims `/graph` serves a "vanilla-JS bundle" that is
  "separately-run" — false; Vite's `outDir` (`vite.config.ts`) overwrites the server's
  assets. `browser.rs`'s module doc calls it "a small static UI."

**Decisions (confirmed with user):** scope = **usability polish**; **add a light/dark
theme toggle** (the UI is currently dark-only).

**Outcome:** the galaxy becomes self-explanatory (legend + camera controls), Impact/Flow
become self-sufficient, colors come from one module, the theme is user-switchable, and the
docs stop lying.

## Changes

All frontend work is in `graph-ui/src/`. Rebuild regenerates the server's embedded assets.

### 1. Shared color source of truth — `graph-ui/src/colors.ts` (new)
Export `KIND_COLORS`, `EDGE_COLORS`, and `STELLAR_RAMP` (the 7 degree stops mirroring
`layout.rs::stellar_color`, with a comment cross-linking the two so they stay in sync).
Import from `Overview.tsx`, `ClassicViews.tsx`, `Scene.tsx`, and the new Legend — delete
the three inline copies. Pure refactor; no behavior change.

### 2. In-UI legend — new `Legend` component in the galaxy workspace
Collapsible panel, **collapsed by default** (a small "Legend" toggle button in the canvas
HUD). When open it shows three sections sourced from `colors.ts`:
- **Stars = degree** — a gradient bar built from `STELLAR_RAMP` labeled "leaf → hub".
- **Node kinds** — the `KIND_COLORS` swatches (matches rail chips/dots).
- **Relationships** — `EDGE_COLORS` swatches with human labels.

Collapsed-by-default avoids duplicate-text collisions with the filter chips in
`Overview.test.tsx` and keeps the default Playwright screenshot stable-ish.

### 3. Camera / view controls — extend `Scene.tsx` + a HUD control cluster
Reset view, auto-rotate toggle, labels toggle, exposed from `Scene.tsx` to `Overview.tsx`;
respects `prefers-reduced-motion`.

### 4. In-tab node picker for Impact/Flow — `ClassicViews.tsx`
Compact search-select in the Impact and Flow toolbars using existing `api.search`; picking
a hit calls `onSelectedId(id)`. No new endpoint.

### 5. Themed `KindSelector` — move inline styles to `styles.css`.

### 6. Light/dark theme toggle
Tokenize `styles.css` + `:root[data-theme="light"]` palette; header toggle in `App.tsx`
with `localStorage` persistence; no-flash init in `index.html`; **3D galaxy canvas stays
dark in both themes** (additive blending + bloom needs a dark backdrop).

### 7. Doc fixes — `graph-ui/README.md` + `browser.rs` module doc comment.

## Constraints — do not break
- Vite output must keep `id="cih-graph-browser"` and `/graph/assets/app.js` in `index.html`
  (`graph_shell_has_browser_mount_points` in `crates/cih-server/tests/browser.rs`).
- Existing vitest suites stay green (watch duplicate-text matches).
- `stellar_color` stays server-authoritative — the legend only *describes* the ramp.
- Theme default = `prefers-color-scheme`; Playwright pins `colorScheme: "dark"`.

## Verification
1. `cd graph-ui && npm test`
2. `npm run build`
3. `npm run dev` + manual drive (legend, camera controls, Impact node picker, theme flip).
4. `npm run test:e2e -- --update-snapshots`, then `npm run test:e2e` clean.
5. `cargo test -p cih-server`.
6. `detect_changes` before commit.
