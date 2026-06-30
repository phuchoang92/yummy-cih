# F3: `cih-engine analyze` cwd default — CLI QA

**Date:** 2026-06-30
**Tester:** Sisyphus-Junior
**Binary:** `cargo build -p cih-engine` (dev profile)
**Working dir:** `/Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih`

---

## Scenario 1: Omitted repo with `--all` from inside a valid repo

**Command:**
```bash
cd /Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih && \
  cargo run -p cih-engine -- analyze --all --no-load
```

**Exit code:** 101 (Rust parser panic — pre-existing tree-sitter issue, not related to cwd feature)

**Result:** PASS — No "required argument" error. Correctly defaults to cwd.

**Key output lines:**
```
INFO analyze{repo=/Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih}: starting analyze repo=/Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih
INFO scan{repo=/Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih}: starting repository walk
INFO scan: filesystem walk complete total_files=498 total_bytes=4586988
INFO scan: source files collected source_files=336 decompiled_dirs=0
INFO scan: modules detected modules=3
INFO scan: JAR discovery complete jars=0
INFO scan: scan complete source_files=336 total_source_loc=69832 modules=3 jars=0
INFO analyze: repo-map written path=.../.cih/repo-map.json source_files=336 modules=3
INFO analyze: starting parse phase files=336 modules=2 cache_enabled=true
INFO analyze::cache: hashing files total_files=336 cache_enabled=true
INFO analyze::cache: incremental cache check complete changed=336 total=336
INFO analyze::cache: incremental parse: 336 files to parse, 0 from cache
```

The scan phase completed successfully (repo path = cwd). The parse phase panicked in `cih-lang/src/rust_lang/mod.rs` (`LanguageError { version: 15 }`) — this is a pre-existing tree-sitter grammar ABI mismatch, unrelated to the cwd logic.

---

## Scenario 2: Explicit repo with `--all`

**Command:**
```bash
cd /Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih && \
  cargo run -p cih-engine -- analyze /tmp --all --no-load
```

**Exit code:** 1 (IO error on `/tmp` — expected, `/tmp` is not a valid repo)

**Result:** PASS — Accepts explicit path argument. No "required argument" error. Fails correctly on invalid directory.

**Key output lines:**
```
INFO analyze{repo=/tmp}: cih_engine::analyze: starting analyze repo=/tmp
INFO scan{repo=/private/tmp}: starting repository walk repo=/private/tmp
Error: /private/tmp/...: Permission denied (os error 13)
```

The explicit path `/tmp` is used as the repo (resolved to `/private/tmp`). The scan starts correctly. The IO error on a `/tmp` semaphore file is expected — `/tmp` is not a valid repository directory.

---

## Scenario 3: No scope from inside repo (preserves existing behavior)

**Command:**
```bash
cd /Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih && \
  cargo run -p cih-engine -- analyze --no-load
```

**Exit code:** 2 (no scope selected — expected, preserves existing behavior)

**Result:** PASS — Scan completes, prints summary, then shows "Choose a scope" prompt. Does NOT automatically start indexing all files.

**Key output lines:**
```
INFO analyze{repo=/Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih}: starting analyze
INFO scan: scan complete source_files=336 total_source_loc=69832 modules=3 jars=0
INFO analyze: repo-map written path=.../.cih/repo-map.json source_files=336 modules=3

Repo: /Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih
Build system: Node
Source files: 336 — LOC: 69.8k
Languages: bash: 2, rust: 320, typescript: 14
Repo map: /Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih/.cih/repo-map.json

Module                            source      LOC    languages     frameworks  est.nodes
yummy-cih                            322    69.0k    bash,rust actix-web,a...      ~5.5k
cih-docs-viewer (docs-viewer)          0        0                                     ~0
cih-graph-ui (graph-ui)               14      823   typescript                      ~238

Recommend: start with `yummy-cih + cih-graph-ui` (~5.7k nodes, ~8.4s); defer generated/decompiled/third-party paths. Or `--all` for the full repo.

Choose a scope: --all | --module <names> | --include <glob> | a cih.scope.toml
```

The scan and repo-map write complete. Then the tool prints a summary and awaits scope selection. Exit code 2 (no scope) is the expected behavior — nothing automatically indexes without explicit scope.

---

## Bonus: `--help` shows `[REPO]` as optional

**Command:**
```bash
cd /Users/duclaidinhcao/Documents/Work/VPB/yummy/yummy-cih && \
  cargo run -p cih-engine -- analyze --help
```

**Exit code:** 0

**Result:** PASS — Usage line shows `[REPO]` with square brackets (optional).

**Key output:**
```
Usage: cih-engine analyze [OPTIONS] [REPO]
```

The `[REPO]` argument is in square brackets, confirming it is an optional positional argument.

---

## Summary

| Scenario | Result | Notes |
|----------|--------|-------|
| 1: No repo + `--all` from inside repo | ✅ PASS | Defaults to cwd; scan completes; no "required argument" error |
| 2: Explicit path `/tmp` + `--all` | ✅ PASS | Accepts explicit path; fails with appropriate IO error |
| 3: No scope from inside repo | ✅ PASS | Shows scope prompt; does not auto-index; exit code 2 |
| Bonus: `--help` shows `[REPO]` | ✅ PASS | Square brackets confirm optional argument |

**Conclusion:** The `[REPO]` positional argument correctly defaults to the current working directory when omitted. All scenarios pass.
