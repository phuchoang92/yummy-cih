# CIH Documentation Index

All documentation files in this project, grouped by audience.

---

## For Users — getting started and running CIH

| File | What it covers |
|---|---|
| [../README.md](../README.md) | Main entry point: Docker Compose quickstart, MCP tools table, workspace layout, troubleshooting |
| [DOCKER-QUICKSTART.md](DOCKER-QUICKSTART.md) | Standalone Docker quickstart — step-by-step for users who don't have this repo cloned |
| [usage.md](usage.md) | Local Rust development: `cargo run` commands for scan/analyze/discover/embed/wiki |
| [llm-providers.md](llm-providers.md) | LLM enrichment guide: DeepSeek, Gemini, Anthropic, OpenAI, Ollama; env vars; flags |

---

## For AI Agents — persona workflow guides

Five skill files used as system-prompt grounding for the yummy frontend agent personas.
Each covers when to use the workflow, step-by-step tool calls, and output shape.

| File | Persona | What it covers |
|---|---|---|
| [agent-workflows/README.md](agent-workflows/README.md) | All | Index and quick-reference for all five workflows |
| [agent-workflows/exploring.md](agent-workflows/exploring.md) | Any | Orient to an unfamiliar codebase |
| [agent-workflows/impact-analysis.md](agent-workflows/impact-analysis.md) | Developer, Tech Lead | Blast-radius analysis before a change |
| [agent-workflows/debugging.md](agent-workflows/debugging.md) | Developer | Call-chain tracing for bugs |
| [agent-workflows/product-owner.md](agent-workflows/product-owner.md) | PO, BA | API surface, modules, business processes |
| [agent-workflows/tester.md](agent-workflows/tester.md) | Tester, QA | Regression scope for a diff |

---

## For Contributors — internals and architecture

| File | What it covers |
|---|---|
| [../ROADMAP.md](../ROADMAP.md) | 26-phase build log; each phase's decisions, verified status, and test counts |
| [cih-plan.md](cih-plan.md) | Top-level architecture & build plan (supersedes codegraph-rust-plan.md) |
| [codegraph-rust-plan.md](codegraph-rust-plan.md) | Original Rust engine internals — `cih-*` crate designs |
| [high-architecture.mmd](high-architecture.mmd) | High-level system diagram (Mermaid flowchart) |
| [how-current-code-builds-graph.md](how-current-code-builds-graph.md) | Deep-dive walkthrough: scan → parse → resolve → load pipeline with Mermaid diagrams |
| [glossary.md](glossary.md) | Term definitions: graph model, parse IR, Spring terms, edge labels, resolver concepts |
| [architecture-improvements.md](architecture-improvements.md) | Post-Phase-6 architecture review (completed); explains structural cleanup decisions |
| [gitnexus-discovery.md](gitnexus-discovery.md) | Research notes from the GitNexus reference system; feature ideas and license constraints |

---

## For Contributors — active plans (upcoming / in-flight)

| File | What it covers |
|---|---|
| [plans/cih-long-term-roadmap.md](plans/cih-long-term-roadmap.md) | Long-term product vision; current state table updated as phases land |
| [plans/discover-semantic-enrichment.md](plans/discover-semantic-enrichment.md) | Semantic enrichment in the discover phase |
| [plans/discover-load-preservation.md](plans/discover-load-preservation.md) | Preserving load metadata across incremental discover runs |
| [plans/cih-resolve-split.md](plans/cih-resolve-split.md) | Splitting cih-resolve into finer-grained passes |
| [plans/pluggable-language-support.md](plans/pluggable-language-support.md) | Language-provider plugin design |
| [plans/resolve-language-agnostic.md](plans/resolve-language-agnostic.md) | Making the resolver fully language-agnostic |
| [plans/smarter-method-body-plan.md](plans/smarter-method-body-plan.md) | Wiki: original-line cap + god-function fallback for method body display |

---

## For Ops — deployment

| File | What it covers |
|---|---|
| [../docs-viewer/DEPLOY.md](../docs-viewer/DEPLOY.md) | Build and push the docs-viewer Docker image; run single-repo and multi-repo modes |

---

## Archive — completed phase implementation plans

All phases below are ✅ complete. These files capture the original design decisions; the canonical record is `ROADMAP.md`.

| File | Phase |
|---|---|
| [archive/phase-3.md](archive/phase-3.md) · [archive/phase-3-impl-spec.md](archive/phase-3-impl-spec.md) | Phase 3 — Scan, scope, parse, load |
| [archive/phase-4.md](archive/phase-4.md) | Phase 4 — Scope resolution + MRO |
| [archive/phase-5.md](archive/phase-5.md) | Phase 5 — Communities + processes |
| [archive/phase-6.md](archive/phase-6.md) | Phase 6 — BM25 + embeddings + hybrid search |
| [archive/phase-7.md](archive/phase-7.md) | Phase 7 — Spring annotations |
| [archive/phase-9.md](archive/phase-9.md) | Phase 9 — Incremental re-index + cache |
| [archive/phase-10a-plan.md](archive/phase-10a-plan.md) · [archive/phase-10a-llm-wiki-plan.md](archive/phase-10a-llm-wiki-plan.md) | Phase 10a — Role-based wiki generation |
| [archive/phase-10c-llm-adapter-plan.md](archive/phase-10c-llm-adapter-plan.md) · [archive/phase-10c-gitnexus-style-wiki-plan.md](archive/phase-10c-gitnexus-style-wiki-plan.md) | Phase 10c — LLM adapter layer |
| [archive/phase-db-access-plan.md](archive/phase-db-access-plan.md) | Phase 10b — Table-level DB access |
| [archive/phase-bfs-memory-plan.md](archive/phase-bfs-memory-plan.md) | BFS memory optimisation |
| [archive/phase-16-plan.md](archive/phase-16-plan.md) | Phase 16 — Test intelligence |
| [archive/phase-17-plan.md](archive/phase-17-plan.md) | Phase 17 — Visualization output |
| [archive/phase-21-cross-service-contracts.md](archive/phase-21-cross-service-contracts.md) | Phase 21 — Cross-service contract extraction |
