# Long-Term Plan: Wiki Redesign — Entry-Point–Centric Navigation

## Context

The current wiki organises docs around **Java class names** (CartController, OrderController). Stakeholders — especially BAs and POs — think in terms of **what the system exposes**, not what class implements it. This plan adds a new **API Flow layer** between the existing BA/PO pages and the existing Technical Reference pages.

### Three-layer doc model (after this plan)

| Layer | Audience | Content | Pages |
|---|---|---|---|
| PO / BA | Product owners, analysts | Business overview, capabilities, workflows | `po.md`, `ba.md` (existing) |
| **API Flow** *(new)* | Everyone | Entry point → step-by-step flow → DB access → events → links to code | `api/{slug}.md` |
| Technical Reference | Developers | Class structure, method calls, source code | `dev/{class}.md` (existing, unchanged) |

**Technical Reference pages are kept exactly as they are.** The API Flow pages sit above them and link into them — they never duplicate code or show raw source.

This is a multi-phase effort. Each phase is independently releasable and adds clear value.

---

## Phase 1 — Rename & Restructure Navigation  *(DONE)*

**Goal:** Replace the raw "controllers/" section with a named "API Surface" section in the nav and feature index page. Quick UX win, zero data changes needed.

### Changes

**`crates/cih-wiki/src/lib.rs`** (nav building, ~line 713)
- Change `kind: "controller"` nav entry → `kind: "api"` so the Docusaurus sidebar shows "API" not "Controllers"
- Rename the generated dir from `controllers/` → `api/`

**`crates/cih-wiki/src/pages/feature_po.rs`**
- In `render_controller_page`: strip "Controller" suffix from page title (e.g. "CartController" → "Cart API")
- Change the `_category_.json` label from `Controllers` → `API Surface`

**`crates/cih-wiki/src/pages/feature_index.rs`**
- Rename the "Controllers" section heading → "API Surface"

### Verification
Run wiki, open `/docs/order/` — sidebar shows "API Surface" → "Cart API", "Order API" instead of "Controllers" → "CartController".

---

## Phase 2 — Per-Endpoint Flow Pages  *(DONE)*

**Goal:** Each HTTP route gets its own dedicated page showing its full call chain with LLM descriptions, instead of all routes being listed in one flat table per controller.

### New Page Type: `api/{slug}.md`

One page per **route handler** (not per controller class). Written so any position — PO, BA, or Dev — can follow it without reading code.

Examples:
- `pages/order/api/add-item-to-cart.md` — "Add Item to Cart"
- `pages/order/api/place-order.md` — "Place Order"

**Page layout:**
```
# Add Item to Cart

> POST /api/v1/cart/items

{LLM business_impact — plain business language, no jargon}

## Flow

{LLM narrative — reads like a story: "The request arrives at the cart controller,
which asks the cart service to locate the user's cart, then calls the product
service to validate stock, and finally persists the updated cart to the database."}

## Steps

| # | Who handles it | What it does | DB access |
|---|---|---|---|
| 1 | CartController   | Receives request, validates auth | — |
| 2 | CartService      | Locates or creates user's cart | READ cart |
| 3 | ProductService   | Validates product exists & has stock | READ product |
| 4 | CartService      | Adds item, saves cart | WRITE cart_item |

## Events

| Direction | Topic | When |
|---|---|---|
| Publishes | cart.item-added | After item persisted |

## Technical Reference

- [CartController →](/docs/order/dev/cart-controller) handles the HTTP layer
- [CartService →](/docs/order/dev/cart-service) owns cart business logic
- [ProductService →](/docs/order/dev/product-service) validates product data
```

No source code is shown on this page. All code detail lives in the linked Technical Reference pages.

### How to wire it

**Route → process link:** Each process node stores `props["route"]` (the triggering route string). Join `routes_by_controller` entries to their process via this prop.

**Renderer:** New function `render_api_endpoint_page(handler, route, process, flow_summary, graph) → String` in `crates/cih-wiki/src/pages/` (new `api_flow.rs` module or extend `feature_po.rs`).

**Generation loop in `lib.rs`:** After the existing controller page loop (~line 699), add a per-handler loop that:
1. Finds the process node whose `props["route"]` matches this handler's route
2. Calls `render_api_endpoint_page` with the matching `FlowLlmSummary`
3. Writes to `pages/{feature}/api/{slug}.md`

**LLM:** `FlowLlmSummary` already exists per process — no new LLM calls needed for this phase.

### Verification
Open `/docs/order/api/add-item-to-cart` — shows step table with descriptions and links to dev class pages.

---

## Phase 3 — Surface Scheduled Jobs & Event Listeners  *(DONE)*

**Goal:** Detect `@Scheduled` and `@KafkaListener` entry points and render them as dedicated sections alongside HTTP routes in the "API Surface" section.

### What already exists

- `EntrypointKind::Scheduled` and `EntrypointKind::EventListener` are already detected in `crates/cih-community/src/entry_points.rs` via annotation matching
- `publishes` / `listens` maps already exist on `WikiGraph` for Kafka topics
- These entry points are not surfaced as pages — only HTTP routes become API pages today

### Changes

**`crates/cih-wiki/src/graph.rs`** — Add to `WikiGraph`:
```rust
pub scheduled_methods: Vec<Node>,   // methods with @Scheduled annotation
pub listener_methods: Vec<Node>,    // methods with @KafkaListener etc.
```
Populate in `build()` / `build_package_grouped()` by checking node props for entrypoint kind.

**New pages:**
- `pages/{feature}/api/scheduled/{slug}.md` — one page per scheduled method
- `pages/{feature}/api/events/{topic-slug}.md` — one page per Kafka topic consumer

**Feature index** — add "Scheduled Jobs" and "Event Flows" sections to `render_feature_index`.

**LLM enrichment** — extend `enrich_one_flow` in `crates/cih-engine/src/wiki_cmd.rs` to also process scheduled/listener method roots (currently only processes HTTP-rooted flows).

### Verification
Open `/docs/order/api/scheduled/` — shows scheduled job pages with call chains.

---

## Phase 4 — Evidence Citations → Real Links  *(~2 days, medium effort)*

**Goal:** Replace `[C1-S2]` citation tags in LLM-generated text with clickable Markdown links to the relevant dev class page.

### How evidence IDs map to pages

Each `EvidenceItem` with kind `Snippet` (prefix `S`) contains the source file path. That path maps to a class node, which maps to a dev page slug.

Example: `[C1-S2]` → `VnpayProvider.java:41-50` → `/docs/payment/dev/vnpay-provider`

### Changes

**`crates/cih-engine/src/wiki_cmd.rs`** — When building evidence packs, produce a JSON sidecar mapping evidence IDs → dev page slugs:
```json
{ "C1-R1": "/docs/payment/api/create-payment", "C1-S2": "/docs/payment/dev/vnpay-provider" }
```
Save as `evidence-map.json` alongside each community's evidence output.

**`crates/cih-wiki/src/lib.rs`** — After LLM enrichment, load `evidence-map.json` per community. Run a post-processing pass over all generated LLM text (PO/BA summaries, flow narratives) replacing `[C1-S2]` with `[C1-S2](/docs/payment/dev/vnpay-provider)`.

**File → slug mapping:** `EvidenceItem.text` for `Snippet` kind starts with the file path. File path → class node lookup already exists in `graph.nodes_by_id`. Class node → dev page slug is computed during generation — capture it into the evidence map at that point.

### Verification
Open `/docs/payment/ba` — citation `[C1-S2]` renders as a clickable link to `/docs/payment/dev/vnpay-provider`.

---

## Phase 5 — Cross-Feature Sequence Diagrams  *(~5 days, high effort)*

**Goal:** For flows that span multiple features (e.g. order placement calling payment, inventory, notification), show a full Mermaid sequence diagram.

### What already exists

- `inter_community_calls` in `WikiGraph` tracks cross-community calls with counts
- `process_steps` already includes calls that cross community boundaries
- Mermaid diagram generation exists in `crates/cih-wiki/src/mermaid.rs`

### Changes

**`crates/cih-wiki/src/mermaid.rs`** — Add `render_sequence_diagram(process, graph) → String` emitting a Mermaid `sequenceDiagram` block grouping steps by community/feature.

**Phase 2 API flow page renderer** — Add the sequence diagram above the step table when the flow crosses more than one feature (check `inter_community_calls` count).

**LLM:** No new calls — reuse existing `FlowLlmSummary.narrative`.

---

## Delivery Order

| Phase | Effort | Value | Depends on |
|---|---|---|---|
| 1 — Rename nav | ~1 day | High — immediate UX win | — |
| 2 — Per-endpoint flow pages | ~3 days | Very high — core feature | Phase 1 |
| 4 — Evidence citations as links | ~2 days | High — trust & traceability | Phase 2 |
| 3 — Scheduled/event entry points | ~3 days | Medium — completeness | Phase 2 |
| 5 — Cross-feature sequence diagrams | ~5 days | High — advanced view | Phases 2+3 |

---

## Critical Files

| File | Changed in |
|---|---|
| `crates/cih-wiki/src/lib.rs` | P1, P2, P3, P4 |
| `crates/cih-wiki/src/pages/feature_po.rs` | P1, P2, P5 |
| `crates/cih-wiki/src/pages/feature_index.rs` | P1, P3 |
| `crates/cih-wiki/src/pages/feature_ba.rs` | P2 |
| `crates/cih-wiki/src/graph.rs` | P3 |
| `crates/cih-wiki/src/mermaid.rs` | P5 |
| `crates/cih-engine/src/wiki_cmd.rs` | P3, P4 |
| `crates/cih-engine/src/llm/evidence.rs` | P4 |
| `docs-viewer/scripts/gen-index.js` | P1 |
