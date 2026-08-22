# cih-graph-ui

The React 19 + modular D3/Canvas graph explorer that `cih-server` serves at **`/graph`**.

This is **the** built-in graph browser — not a separate app. `vite build` compiles
this source into `../crates/cih-server/assets/graph/` (`outDir` in `vite.config.ts`,
with `emptyOutDir: true`), and the server embeds those files via `include_str!` in
`crates/cih-server/src/browser.rs`. So `npm run build` here overwrites what the server
ships; the Rust routes in `browser.rs` back the UI's `/api/graph/*` calls.

- **Overview** — a bounded hierarchy (repository → community → file), rendered
  with Canvas 2D. The server aggregates and filters; deterministic seed positions
  paint immediately; a cancellable D3 force Worker refines them incrementally.
- **Expansion** — symbols are added as bounded one-hop layers instead of loading
  the entire codebase into the browser.
- **Search / Impact / Flow / Communities / Clusters / Routes** — lighter analytical
  views rendered with inline SVG.

Color palettes live in one place: `src/colors.ts` (`KIND_COLORS` and
`EDGE_COLORS`). The in-UI **Legend** is collapsed by default.

## Develop

```bash
npm install
npm run dev        # Vite dev server; proxies /api/graph -> http://localhost:8080
npm test           # vitest (unit)
npm run test:e2e   # Playwright (screenshot baselines; --update-snapshots to refresh)
npm run build      # tsc + vite build -> crates/cih-server/assets/graph/
```

`npm run dev` expects a running `cih-server` (default port 8080) for live graph data.
