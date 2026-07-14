# cih-graph-ui

The React 19 + Three.js graph explorer that `cih-server` serves at **`/graph`**.

This is **the** built-in graph browser — not a separate app. `vite build` compiles
this source into `../crates/cih-server/assets/graph/` (`outDir` in `vite.config.ts`,
with `emptyOutDir: true`), and the server embeds those files via `include_str!` in
`crates/cih-server/src/browser.rs`. So `npm run build` here overwrites what the server
ships; the Rust routes in `browser.rs` back the UI's `/api/graph/*` calls.

- **Overview** — a WebGL "galaxy": nodes are a Three.js point cloud positioned by the
  server-side layout (`crates/cih-server/src/layout.rs`); star color encodes node
  degree, rail chips/legend encode node kind.
- **Search / Impact / Flow / Communities / Clusters / Routes** — lighter analytical
  views rendered with inline SVG.

Color palettes live in one place: `src/colors.ts` (`KIND_COLORS`, `EDGE_COLORS`,
`STELLAR_RAMP` — the last mirrors `stellar_color` in `layout.rs`). The in-UI **Legend**
(collapsed by default, top-right of the overview) documents all three.

## Develop

```bash
npm install
npm run dev        # Vite dev server; proxies /api/graph -> http://localhost:8080
npm test           # vitest (unit)
npm run test:e2e   # Playwright (screenshot baselines; --update-snapshots to refresh)
npm run build      # tsc + vite build -> crates/cih-server/assets/graph/
```

`npm run dev` expects a running `cih-server` (default port 8080) for live graph data.
