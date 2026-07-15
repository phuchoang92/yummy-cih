# yummy-cih documentation

Start with **[DEVELOPERS.md](DEVELOPERS.md)** if you're new — it explains how the
system works and which crate owns what. The root [README](../README.md) is the
*user* guide (installing, indexing, the MCP tools).

## What's here

| Doc | For |
|---|---|
| **[DEVELOPERS.md](DEVELOPERS.md)** | Contributor onboarding: the pipeline, crate-ownership map, reading order. **Start here.** |
| **[ARCHITECTURE.md](ARCHITECTURE.md)** | Authoritative: parser assumptions, graph model, resolution reach + known limits. |
| **[glossary.md](glossary.md)** | Definitions of CIH terms (node/edge kinds, FQCN, contracts, …). |
| **[SECURITY.md](SECURITY.md)** | Security posture: auth on non-loopback bind, LLM-egress-free server. |
| **[DOCKER-QUICKSTART.md](DOCKER-QUICKSTART.md)** | Running the stack via Docker Compose. |
| **[llm-providers.md](llm-providers.md)** | Configuring LLM providers for the wiki/NL features. |
| **[jar-decompile-feature.md](jar-decompile-feature.md)** | The JVM decompile/JAR API-extraction feature. |
| **[ROADMAP.md](ROADMAP.md)** | Where the project is heading. |
| **[high-architecture.mmd](high-architecture.mmd)** | Deployment-level architecture diagram (app + engine + stores). |

## Subdirectories

| Dir | For |
|---|---|
| **[agent-workflows/](agent-workflows/)** | Persona playbooks (exploring, impact-analysis, debugging, security, …) — when-to-use + tool sequences. |
| **[plans/](plans/)** | Active design/implementation plans (e.g. the native bulk loader). |
| **[runbooks/](runbooks/)** | Operational runbooks (re-index flows, container ops). |
| **[ci/](ci/)** | CI-related notes. |
| **[archive/](archive/)** | Finished/historical planning & discovery docs — kept for provenance, not current. |

## Contributing

See **[CONTRIBUTING.md](../CONTRIBUTING.md)** for the code-structure standard and the
build/test/lint gates.
