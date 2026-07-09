# cih-graph-ui

Standalone React + Three.js graph explorer (Vite dev server, Playwright e2e).
It talks to a running `cih-server` over the same `/api/graph/*` endpoints.

**Not to be confused with** the built-in graph browser that `cih-server`
serves at `/graph` — that UI is the vanilla-JS bundle in
`crates/cih-server/assets/graph/` (routes in `crates/cih-server/src/browser.rs`)
and ships inside the server binary. This directory is the richer,
separately-run 3D explorer.

```bash
npm install
npm run dev        # Vite dev server (expects cih-server on its default port)
npm run test:e2e   # Playwright
```
