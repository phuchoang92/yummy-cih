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
| [how-current-code-builds-graph.md](how-current-code-builds-graph.md) | Deep-dive walkthrough: scan → parse → resolve → load pipeline with Mermaid diagrams |
| [glossary.md](glossary.md) | Term definitions: graph model, parse IR, Spring terms, edge labels, resolver concepts |
| [architecture-improvements.md](architecture-improvements.md) | Post-Phase-6 architecture review (completed); explains structural cleanup decisions |
| [gitnexus-discovery.md](gitnexus-discovery.md) | Research notes from the GitNexus reference system; feature ideas and license constraints |

---

## For Ops — deployment

| File | What it covers |
|---|---|
| [../docs-viewer/DEPLOY.md](../docs-viewer/DEPLOY.md) | Build and push the docs-viewer Docker image; run single-repo and multi-repo modes |

---

## Historical — implementation plans (completed phases)

All phases are complete. These files capture the original design decisions.

| File | Phase |
|---|---|
| [plans/phase-3.md](plans/phase-3.md) · [plans/phase-3-impl-spec.md](plans/phase-3-impl-spec.md) | Phase 3 — Scan, scope, parse, load |
| [plans/phase-4.md](plans/phase-4.md) | Phase 4 — Scope resolution + MRO |
| [plans/phase-5.md](plans/phase-5.md) | Phase 5 — Communities + processes |
| [plans/phase-6.md](plans/phase-6.md) | Phase 6 — BM25 + embeddings + hybrid search |
| [plans/phase-7.md](plans/phase-7.md) | Phase 7 — Spring annotations |
| [plans/phase-9.md](plans/phase-9.md) | Phase 9 — Incremental re-index + cache |
| [plans/phase-10a-plan.md](plans/phase-10a-plan.md) · [plans/phase-10a-llm-wiki-plan.md](plans/phase-10a-llm-wiki-plan.md) | Phase 10a — Role-based wiki generation |
| [plans/phase-10c-llm-adapter-plan.md](plans/phase-10c-llm-adapter-plan.md) · [plans/phase-10c-gitnexus-style-wiki-plan.md](plans/phase-10c-gitnexus-style-wiki-plan.md) | Phase 10c — LLM adapter layer |
| [plans/phase-db-access-plan.md](plans/phase-db-access-plan.md) | Phase 10b — Table-level DB access |
| [plans/phase-16-plan.md](plans/phase-16-plan.md) | Phase 16 — Test intelligence |
| [plans/phase-17-plan.md](plans/phase-17-plan.md) | Phase 17 — Visualization output |
| [plans/phase-21-cross-service-contracts.md](plans/phase-21-cross-service-contracts.md) | Phase 21 — Cross-service contract extraction |
