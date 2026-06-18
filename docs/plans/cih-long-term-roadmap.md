# CIH Long-Term Roadmap — Code Intelligence Hub

## Vision

Build a Code Intelligence Hub that answers deep technical questions about any enterprise
codebase — regardless of programming language or architectural style — through a layered
system of graph extraction, semantic embedding, and AI agent tools.

The system must:
1. Parse source code into a language-neutral knowledge graph.
2. Detect communities (feature clusters) and business processes automatically.
3. Generate human-readable wiki documentation enriched by LLM.
4. Support semantic and graph-traversal search as tools for AI agents.
5. Scale horizontally to multiple teams with different languages and architectures.

The first production target is the ocb-sp05/platform banking codebase
(Java/Spring/OSGi, 12,889 files, 55 modules, ~1.85M LOC). Every architectural decision
must make it easy — not expensive — to onboard the next team.

**Core principle:** framework-specific extractors emit framework-neutral graph facts.

---

## Current State (Completed as of 2026-06-18)

| Component | Status | Notes |
|---|---|---|
| `cih-engine scan` | ✅ | File discovery, RepoMap, module detection |
| Java parsing (tree-sitter) | ✅ | Methods, classes, fields, interfaces |
| Spring MVC route extraction | ✅ | @GetMapping, @PostMapping, @RequestMapping, etc. |
| DB access extraction | ✅ | JPA, named queries |
| `cih-engine analyze` | ✅ | Graph artifacts (`nodes.jsonl`, `edges.jsonl`) |
| `cih-engine discover` | ✅ | Community detection (Leiden), process tracing (BFS), stereotyping — CLI may display "Louvain" in some help text; the algorithm is Leiden (see `leiden.rs`); terminology should be normalised when next touching discover docs |
| `cih-wiki` render | ✅ | PO/BA/Dev pages per feature and community |
| LLM wiki enrichment | ✅ | Per-community and feature-level summaries (llm-summary mode) |
| Process evidence (P-items) | ✅ | Process nodes wired into LLM evidence packs |
| Feature-level LLM caching | ✅ | FeatureMetaEntry in wiki_meta.json |
| FalkorDB graph storage | ✅ | Nodes/edges persisted; MCP tools running |
| `cih-embed` | ✅ partial | Chunking, text extraction, embedding model/store plumbing exist; AST-strip and production benchmarking remain |
| `cih-search` | ✅ partial | BM25, tokenization, and RRF exist; unified production retrieval API still needs hardening |
| Built-in graph browser | ✅ | Local `/graph` UI and bounded graph APIs exist in `cih-server` |
| Multi-language core vocabulary | ✅ partial | `NodeKind::Function` exists; integration/message node kinds and extractor contracts remain |

---

## Phase 1 — Enterprise Java Genericity
**Scope:** 1–2 sprints | **Detail:** `docs/enterprise-java-plan.md`

### Purpose

The extraction layer currently treats Spring MVC as the only HTTP framework and ignores
XML-configured integration entirely. Apache Fineract (eval repo) exposes all its HTTP APIs
via JAX-RS (`@Path`, `@GET`) — the current route count for Fineract is near zero. ServiceMix
(second eval repo) has all its integration logic in Camel/Blueprint XML — completely
invisible to the current analyzer.

The banking codebase (ocb-sp05) uses OSGi Blueprint XML, Spring XML, and Camel for
wiring services and routing messages. Without this phase, the wiki output for ocb-sp05 has
structural holes: JAX-RS endpoints are missing, Camel routes are absent, and integration
dependencies between services are invisible.

This phase also establishes the genericity pattern that Phase 5 extends to other languages:
**per-framework inner functions, enum-typed source props, typed annotation arrays** — so
adding a third HTTP framework or a second message broker is a bounded change, not a
fork of existing logic.

### Approach

**1a — Framework-Neutral HTTP Extraction** (`crates/cih-lang/src/java/parse.rs`)

Refactor `spring_method_routes` into a per-framework architecture:
- Outer `method_routes` collects from `spring_method_routes_inner` + `jaxrs_method_routes_inner`,
  deduplicates, returns sorted candidates.
- JAX-RS: class-level and method-level `@Path`; HTTP verb annotations `@GET`, `@POST`,
  `@PUT`, `@DELETE`, `@PATCH`, `@HEAD`, `@OPTIONS`.
- Replace composite `decorator: "Path+GET"` prop with typed `route_annotations: ["Path", "GET"]` array.
- Add `RouteSource` enum in `cih-core`: `SpringMvc | JaxRs` — serialized snake_case, no magic strings.

**1b — XML Integration Extraction** (`crates/cih-resolve/src/integration_xml.rs` — new)

> **Why `cih-resolve`, not `cih-lang`?**
> `cih-lang` is responsible for parsing a single source file into raw symbol facts (classes,
> methods, annotations). XML wiring files do not define symbols in isolation — they wire
> together Java beans and services that were defined by `cih-lang`. Extracting them requires
> Java symbol facts to already exist so that wiring edges can reference real node IDs. That
> dependency on post-parse symbol state is what makes this a resolve-layer concern, not a
> parse-layer concern. The rule: if an extractor needs to cross-reference symbols from another
> source file or config file, it belongs in `cih-resolve`; if it only reads one file to emit
> raw facts, it belongs in `cih-lang`.

- Glob patterns for Camel, Blueprint, Spring XML, CXF config files; filter by namespace signature.
- Emit `IntegrationRoute`, `MessageDestination`, `ExternalEndpoint` node kinds.
- Emit `ListensTo`, `PublishesEvent`, `ExternalCall`, `IntegrationLink` edge kinds.
  - `direct:`, `seda:`, `vm:` → `IntegrationLink` between routes; no `MessageDestination` node.
  - `jms:`, `activemq:`, `kafka:`, `amqp:`, `rabbitmq:` → `MessageDestination` with `destination_type`.
- Add `IntegrationSource` enum in `cih-core`: `CamelXml | BlueprintXml | SpringXml | CxfXml`.
- Bad XML files log `warn!` and continue — never abort an analyze run.
- Wire into `cih-engine/src/analyze.rs` after Java parse/resolve.

**1c — Evidence Packs**

Add `EvidenceKind::IntegrationRoute` and `EvidenceKind::MessageDestination` to `evidence.rs`.
Wiki LLM calls can cite `[I1]`, `[M1]` items.

**1d — Eval Harness** (`scripts/eval-enterprise-java.sh` — new)

Runs fineract, servicemix, spring-petclinic. spring-petclinic is the Spring-only regression
guard — route count must not decrease.

### Success Criteria
- Fineract route count increases significantly from JAX-RS extraction.
- ServiceMix emits non-zero IntegrationRoute, MessageDestination, IntegrationLink facts.
- spring-petclinic route count does not decrease.
- All unit tests pass; no Spring regression.

---

## Phase 2 — ocb-sp05 Production Quality
**Scope:** 2–4 weeks | **Goal:** First complete wiki run on the banking codebase

### Purpose

ocb-sp05/platform is the primary user codebase. Running on it for the first time will
expose issues that don't show up on small eval repos:

- **442K unresolved call refs** (Spring DI, reflection) — evidence packs for these
  communities have almost no grounded route or call data.
- **4,115 Leiden communities** from a monolith — likely over-fragmented (a 55-module
  codebase should yield ~200–400 meaningful communities, not 4,115).
- **OSGi Blueprint wiring** that connects services outside of direct Java call chains.

This phase produces the first wiki that business stakeholders (PO, BA) at the bank can use.
Issues found here drive pragmatic fixes rather than new architecture. It also validates
Phase 1's extraction on a real production codebase, not just eval fixtures.

### Approach

**2a — Spring/Blueprint DI Resolution** (`crates/cih-resolve/src/di_xml.rs` — new)

- Parse `applicationContext.xml`, Blueprint XML `<bean>` and `<reference>` / `<service>` bindings.
- Resolve field injection: when a field type `T` matches a bean class `C`, add a `CALLS` edge.
- Target: reduce 442K unresolved refs to <200K. Reflection is genuinely unresolvable —
  reduce the fixable DI fraction first.

**2b — Community Detection Tuning**

- Keep the existing `discover --resolution` flag as the manual override; do not add a duplicate
  `--community-resolution` flag.
- Add `architecture_hint: monolith | microservice | event_driven | batch` to `repo-map.json`.
  Auto-detected heuristic: >500 files + multi-Maven-module → `monolith`. Community detection
  reads this to choose default resolution, min-community-size, and max-iteration settings.
- For monoliths, avoid automatic over-fragmentation. Start with conservative/default resolution
  and increase `min_community_size`; only raise resolution when eval output proves communities
  are too coarse.

**2c — Wiki Quality Validation**

- Run full `analyze → discover → wiki --llm --wiki-mode llm-summary` on ocb-sp05.
- LLM citations `[R1]`, `[P1]` must reference real evidence items (verify against
  `--llm-debug-evidence` output).

**2d — Performance Baseline**

- Target: graph-only run under 5 minutes; llm-summary under 30 minutes on ocb-sp05.
- Fix any O(n²) hotspots in community indexing or evidence building.

### Success Criteria
- Complete wiki output for all 55 modules — no missing pages.
- PO and BA pages have substantive LLM content (not empty Overview sections).
- No panics, corrupt graph facts, or silent data loss.

---

## Phase 3 — Retrieval Hardening: Embeddings + BM25 + RRF
**Scope:** 4–8 weeks | **Enables:** production-grade semantic code search

### Purpose

CIH already has the core retrieval building blocks: `cih-embed` for embedding storage/model
plumbing, `cih-search` for BM25/RRF, and `cih-server` query tools. This phase is not about
creating new crates from scratch. It is about making retrieval production-grade for large
enterprise repos: better text preparation, incremental embedding, unified hybrid ranking,
and measurable quality.

Graph traversal finds what is structurally connected to a query node. Semantic search answers
"find me code similar to X" or "where is this concept implemented?" Embedding method bodies
enables similarity search, while BM25 catches exact symbol/file names that embeddings can rank
poorly. RRF combines both into one predictable result list for AI tools and UI search.

Embedding naively would embed ~133K methods from ocb-sp05, most of which are logging,
null-guard boilerplate, or trivial accessors — high cost, low signal. AST-strip reduces this
to ~60-70K meaningful methods, cuts token cost ~60%, and improves embedding quality by
removing noise before the encoder sees the text.

### Approach

**3a — AST-Strip / Noise Reduction** (`crates/cih-embed/src/strip.rs` — new)

tree-sitter–based method body reduction:
- Drop logging calls (`log.*`, `logger.*`, `LOG.*`), null-guard-throw blocks, trivial
  getters/setters, `super()` delegations.
- Strip rules are language-specific config files: `strip_profiles/java.toml`,
  `strip_profiles/typescript.toml` — externalised, not baked into Rust code.
- Estimate: ~133K methods → ~60-70K after strip for ocb-sp05.

**3b — Harden Existing Embedding Pipeline** (`crates/cih-embed`)

- Input: node IDs + stripped method bodies from graph artifacts.
- Model: configurable (default: voyage-code-2 or Gemini text-embedding-004).
- Output: pgvector table keyed by stable `node_id`, with configurable embedding dimension and
  HNSW index where appropriate.
- Incremental: hash stripped body; skip if unchanged. Estimated: ~90 MB vectors + ~400 MB
  HNSW index for ocb-sp05.
- Keep local/test paths deterministic and model-free where possible; integration tests that need
  pgvector or external models must be opt-in.

**3c — Harden Existing BM25 Lexical Index** (`crates/cih-search`)

Use method names, class names, file paths, package/module names, route paths, and DB table names.
Full-text fallback is required for exact symbol names that embedding may rank poorly. Keep the
in-memory BM25 path for local artifacts and optionally add PostgreSQL `tsvector` later if large
repos need server-side indexing.

**3d — Hybrid Retrieval API** (`cih-search` + `cih-embed` + `cih-server`)

Given a query string:
1. pgvector ANN (top-K by cosine similarity)
2. FalkorDB subgraph neighbors of matched nodes
3. BM25 full-text on method/class names
4. Reciprocal Rank Fusion → final ranked list of `(node_id, score, snippet)`.

Do not create a duplicate `cih-retrieval` crate unless the API outgrows `cih-search`. Prefer a
single public retrieval facade exported from existing crates first.

**3e — MCP Tool: search_code**

Expose or align the existing server query tool as `search_code(query: str, limit: int) → Vec<CodeMatch>`.
`CodeMatch`: node_id, qualified_name, file, line, snippet, score.

### Success Criteria
- `search_code("rate limiting")` returns relevant methods ranked above noise.
- Embedding run on ocb-sp05 completes in under 20 minutes.
- HNSW index fits within ~500 MB total storage.

---

## Phase 4 — AI Agent Layer
**Scope:** 8–12 weeks (starts after Phase 3) | **Enables:** Conversational code Q&A

### Purpose

Individual tools (search, context, impact) are more powerful when composed into a multi-turn
conversation. A developer should be able to ask "What does the order payment flow do?" and
follow up with "Which services would break if I change the settlement timeout?" The agent
layer provides this conversational interface, backed by the graph and embedding capabilities
built in earlier phases.

This is the capability that justifies the entire stack to the business: a developer with no
prior knowledge of the codebase gets accurate, grounded, specific answers without reading
source files.

### Approach

**4a — Agent Tool Suite**

| Tool | Input | Output |
|---|---|---|
| `search_code(query)` | Natural language | Ranked method matches (Phase 3) |
| `get_context(node_id)` | Node ID | Callers, callees, community, process membership, wiki summary |
| `trace_impact(node_id, direction)` | Node ID + up/down | BFS impact list from FalkorDB |
| `trace_call_chain(entry_point, depth)` | Route or process ID | Ordered call chain from entry point |

**4b — Provider-Neutral Agent Runtime**

- Agent loop: an LLM calls tools, reads results, calls again or returns a final answer.
- Define an internal `AgentLlmAdapter` first, reusing the provider strategy already used by
  wiki enrichment where practical: Gemini, OpenAI-compatible, Anthropic/Claude, DeepSeek,
  and `http-json`.
- System prompt: describes the codebase (language, architecture_hint, key module names from repo-map).
- Context injection on first turn: inject feature-level wiki PO/BA summary for the module
  most relevant to the query.
- Claude-specific SDK support can be one adapter, not the architecture boundary.

**4c — Multi-Turn State**

- Track fetched node IDs this session to avoid redundant tool calls.
- After N turns, summarize prior context with a short LLM call to stay within token budget.

**4d — MCP Server Interface**

- Prefer integrating the agent endpoint into `cih-server` first, because it already exposes
  graph/search/impact tools and runtime configuration. Split into a separate `cih-agent` crate
  only if the loop becomes large enough to justify its own crate boundary.
- Compatible with Claude Code CLI and IDE extensions (VS Code, JetBrains), while preserving
  Gemini/OpenAI-compatible provider support for teams that cannot use Claude.

### Success Criteria
- Agent answers "What does `POST /orders` do end-to-end?" with specific method names and files.
- Agent answers "What breaks if I change `OrderService.processPayment`?" with a caller list.
- All responses are grounded: citations trace back to actual graph nodes, not hallucinations.

---

## Phase 5 — Multi-Language Extensibility
**Scope:** After Phase 1–4 proven on Java | **Enables:** Onboarding other IT teams

### Purpose

After the system is proven on ocb-sp05, other IT teams with TypeScript microservices,
Python services, or Go services need to onboard. The architecture review identified the
key insight:

**Layers 2 and 3 are already language-neutral.** FalkorDB graph traversal, pgvector, BM25,
RRF, and the agent tool contract have no knowledge of what language was parsed. The
investment in this phase is entirely in Layer 1 (extraction) — making its boundary explicit
so a new team implements one contract and gets retrieval, wiki, and agent capabilities
for free.

Four specific bottlenecks block multi-language adoption today:
1. Entry-point detection is hard-coded to Spring annotation names.
2. AST-strip rules are Java-specific (mitigated by externalising to profiles in Phase 3).
3. Extractor output contracts are implicit rather than documented as a stable language-neutral API.
4. Community detection resolution is graph-size-aware but not architecture-aware.

### Approach

**5a — Language Extractor Contract**

Document the extraction output schema as a public spec. Rules any extractor must follow:

- HTTP routes → `NodeKind::Route` with props `httpMethod`, `path`, `source` (enum value).
- Message destinations → `NodeKind::MessageDestination` with `destination_type` + `component`.
- Internal integration links → `EdgeKind::IntegrationLink` (never `Uses`).
- Entry points → node prop `"entry_point": true, "entry_point_kind": "http|event|scheduled|export"`.
- Callables without classes (Go `func`, Python `def`) → existing `NodeKind::Function`.

**5b — Pluggable Entry-Point Registry**

Replace hard-coded Spring annotation scan in `discover` with per-language config files:

```toml
# entry_points/java.toml
annotations = ["@RestController", "@Controller", "@Path", "@KafkaListener", "@Scheduled", "@RabbitListener"]

# entry_points/typescript.toml
patterns = ["@Get", "@Post", "@MessagePattern", "export default function"]

# entry_points/python.toml
patterns = ["@app.route", "@router.get", "@task", "@app.task"]
```

Process tracing (BFS) reads the registry; zero Java-specific checks remain in the core.

**5c — TypeScript/Node.js Extractor** (`crates/cih-lang/src/typescript/`)

- Classes, functions, interfaces.
- HTTP routes: Express `app.get('/path', handler)`, NestJS `@Get` / `@Post` decorators.
- DI: NestJS `@Injectable`; Kafka: `@MessagePattern`.
- Entry points: exported module functions, decorated controllers.

**5d — Python Extractor** (`crates/cih-lang/src/python/`)

- Classes, functions (module-level and method-level), `@property` accessors.
- HTTP routes: Flask `@app.route`, FastAPI `@router.get`.
- Entry points: Celery `@task`, Django views, FastAPI route handlers.

**5e — Architecture Hint in Community Detection**

`architecture_hint` auto-detected from repo-map or user-supplied:

| Hint | Heuristic | Community defaults |
|---|---|---|
| `monolith` | >500 files + multi-module build | conservative/default resolution + larger `min_community_size` |
| `microservice` | thin internal graph, few files per service | lower/coarser resolution only when eval shows over-splitting |
| `event_driven` | many message destinations, few HTTP routes | moderate resolution; prioritize message destinations as entry points |
| `batch` | scheduled entry points dominate | moderate resolution; prioritize scheduled entry points |

Prevents the 4,115-community over-fragmentation seen on ocb-sp05 from repeating on every
onboarded monolith.

### Success Criteria
- TypeScript microservice produces a complete wiki with HTTP routes and entry points.
- Python Django app produces a wiki with routes and business processes.
- Adding a new language extractor touches zero Java-specific code.
- Community detection resolution auto-adjusts correctly between monolith and microservice repos.

---

## Extension Points Summary

| Concern | Phase 1 state | Phase 5 state |
|---|---|---|
| HTTP routes | Spring MVC + JAX-RS inner functions | Per-language extractor module |
| Integration links | Camel/Spring XML in `integration_xml.rs` | Per-format extractor module |
| Entry-point detection | Hard-coded Spring annotation list | `entry_points/<lang>.toml` registry |
| Noise removal (AST-strip) | Java rules → `strip_profiles/java.toml` | `strip_profiles/<lang>.toml` per language |
| Community resolution | Graph-size heuristic | + `architecture_hint` from repo-map |
| Evidence items | R/P/T/S/B/I/M item kinds | Extensible `EvidenceKind` enum |
| NodeKind vocabulary | `Function` already exists; integration/message kinds missing | + `IntegrationRoute`, `MessageDestination`, optional `Module` if needed |

---

## Phase Dependency

```
Phase 1: Enterprise Java Genericity
  └── Phase 2: ocb-sp05 Production Quality
        └── Phase 3: Retrieval Hardening
              └── Phase 4: Provider-Neutral AI Agent

Phase 1 also unblocks Phase 5 (Multi-Language):
  the framework-neutral contract pattern established in Phase 1
  scales to language-level in Phase 5.
```

---

## Key Files Per Phase

### Phase 1 (next — detail in `docs/enterprise-java-plan.md`)

| File | Change |
|---|---|
| `crates/cih-core/src/lib.rs` | `RouteSource`, `IntegrationSource` enums; `IntegrationRoute`, `MessageDestination` NodeKinds; `IntegrationLink` EdgeKind |
| `crates/cih-lang/src/java/parse.rs` | Framework-neutral route extraction + JAX-RS inner function |
| `crates/cih-resolve/src/integration_xml.rs` | **New** — XML integration extractor |
| `crates/cih-engine/src/analyze.rs` | Wire XML extraction |
| `crates/cih-engine/src/llm/evidence.rs` | I1/M1 evidence items |
| `scripts/eval-enterprise-java.sh` | **New** — eval harness (fineract / servicemix / spring-petclinic) |

### Phase 2

| File | Change |
|---|---|
| `crates/cih-resolve/src/di_xml.rs` | **New** — Spring/Blueprint DI resolution |
| `crates/cih-community/src/lib.rs` | Architecture-aware defaults for resolution/min-community-size/max-iterations |
| `crates/cih-engine/src/main.rs` / `discover.rs` | Keep `--resolution`; add architecture-hint-aware defaults |
| `crates/cih-engine/src/scan/` | `architecture_hint` field in repo-map |

### Phase 3

| File | Change |
|---|---|
| `crates/cih-embed/src/strip.rs` | **New** — AST-strip/noise-reduction before embedding |
| `crates/cih-embed/` | Harden existing embedding pipeline, incremental hashes, pgvector benchmarking |
| `strip_profiles/java.toml` | Java noise-removal rules |
| `crates/cih-search/` | Harden existing BM25/RRF ranking and public retrieval facade |
| `crates/cih-server/src/` | Align existing query/search endpoint with `search_code` contract |

### Phase 4

| File | Change |
|---|---|
| `crates/cih-server/src/` | Provider-neutral agent MCP endpoint reusing existing graph/search tools |
| `crates/cih-engine/src/llm/` or new shared crate | Reusable provider adapters for Gemini/OpenAI-compatible/Anthropic/DeepSeek/http-json |
| `crates/cih-agent/` | Optional later split only if the agent loop outgrows `cih-server` |

### Phase 5

| File | Change |
|---|---|
| `crates/cih-lang/src/typescript/` | **New** — TypeScript extractor |
| `crates/cih-lang/src/python/` | **New** — Python extractor |
| `entry_points/java.toml` | Per-language entry-point registry (extracted from hard-code) |
| `strip_profiles/typescript.toml` | TypeScript noise rules |
| `strip_profiles/python.toml` | Python noise rules |

---

## Verification Per Phase

```bash
# Phase 1
cargo test --workspace
scripts/eval-enterprise-java.sh

# Phase 2
cih-engine analyze /path/to/ocb-sp05
cih-engine discover /path/to/ocb-sp05
cih-engine wiki /path/to/ocb-sp05 --llm --wiki-mode llm-summary
# Check: wiki has PO/BA content for all 55 modules; LLM citations are grounded

# Phase 3
cih-engine embed /path/to/ocb-sp05
# Via MCP: search_code("rate limiting") — inspect result quality

# Phase 4
# Placeholder — exact command/env shape to be decided before implementation.
# Current expectation: cih-server --agent-mode or a sub-command added to cih-engine,
# reusing the existing provider config env vars (GEMINI_API_KEY, OPENAI_API_KEY, etc.)
# and exposing the agent as an MCP endpoint alongside existing graph/search endpoints.
cih-server  # agent endpoint enabled via flag or sub-command (TBD)
# Ask: "What does POST /orders do?" — verify specific, grounded answer

# Phase 5
scripts/eval-enterprise-java.sh            # Java must still pass
cih-engine analyze /path/to/nestjs-service
cih-engine wiki /path/to/nestjs-service
# Check: wiki has HTTP routes from TypeScript decorators
```
