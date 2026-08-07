# Contributing to yummy-cih

New here? Read [docs/DEVELOPERS.md](docs/DEVELOPERS.md) first — it explains the
pipeline and which crate owns what. This file is the **house style** and the
**checks every change must pass**.

## Code structure standard

The workspace is 16 crates; keep every one legible the same way.

- **`lib.rs` is a map, not a dump.** It opens with a `//!` overview (what the crate
  does and where it sits in the `parse → resolve → load → discover → wiki`
  pipeline), then declares its modules and re-exports. Implementation lives in the
  modules, not in `lib.rs`.
- **One module per concern, named for the concept.** A file called `routes.rs`,
  `db.rs`, `load.rs`, `query.rs`, `serialize.rs` should contain exactly that. When
  a file grows past a few hundred lines, split it along the `// ──` section
  comments into concern modules. Each module opens with a one-line `//!`.
- **Every public item gets a one-line doc** saying what it's for. Spell out
  abbreviations at least once (FQCN = fully-qualified class name, MRO = method
  resolution order, …); add unfamiliar terms to [docs/glossary.md](docs/glossary.md).
- **Talk to ports, not adapters.** Depend on the `GraphStore` trait
  (`cih-graph-store`), not `cih-falkor` directly.
- **Prefer clarity over cleverness.** A newcomer should be able to follow the flow.

## Build, test, lint (the gates)

All three must be green before a change lands:

```bash
cargo test --workspace                                  # hermetic — no DB needed
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
cargo fmt --all --check                                 # fmt-normalized
```

Local services, only when you actually exercise them: FalkorDB on **6380**
(Homebrew redis squats 6379), Postgres on **5433**; set
`FALKOR_URL=redis://127.0.0.1:6380`.

## Two rules specific to this codebase

1. **Bump `PARSE_CACHE_SCHEMA` when parser output changes.** Any change to a
   parser/extractor that alters `ParsedUnit` output must bump
   `cih_lang::PARSE_CACHE_SCHEMA` (`crates/cih-lang/src/lib.rs`) and update the
   paired `GOLDEN` in `crates/cih-engine/tests/parse_schema_guard.rs` (the test
   prints the new hash). Without it, the per-file parse cache silently serves stale
   output after an upgrade.

2. **Behavior-preserving changes must prove it.** For refactors (moving code, splits)
   the graph must not change: re-index a fixture with `--no-cache` and confirm the
   `.cih/artifacts/*/nodes.jsonl` + `edges.jsonl` hashes are **byte-identical** to
   before. `parse_schema_guard` staying green (no bump needed) is the fast signal;
   the byte-identical re-index is the proof.

## Docs & git

- Design docs go in `docs/plans/`; finished/historical docs go in `docs/archive/`.
  Keep `docs/README.md` (the index) current when you add or retire a doc.
- Don't commit on the default branch — branch first. Keep the workspace green at
  every commit. See `CLAUDE.md` for agent-specific conventions.
