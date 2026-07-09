# Analyze Pipeline Performance Plan

> **Update 2026-07-09 — native macOS re-measure (fineract, 6,374 files, artifacts on
> local APFS).** Per-phase breakdown of a `--no-cache --no-load` run:
> scan 0.38s · parse 1.98s · resolve 1.37s · JAR 0.18s · **integration XML 0.098s** ·
> DI XML 0.23s · **artifact write ~3.3s** · total ~7.9s.
>
> Two plan assumptions were wrong on native hardware (they were Windows-Docker I/O
> artifacts, not CPU): **integration XML is ~1%, not 10%** (Fix 2 target), and the
> **artifact-write phase is ~42%, not 8%** — the real local hot path. Instrumenting
> the write phase: content_version (blake3) 0.62s · write_parsed_files 1.0s ·
> `GraphArtifacts::write` (nodes+edges) 0.70s · write_unresolved_reports (270k refs)
> 0.98s.
>
> **Shipped:** parallel serialization for `GraphArtifacts::write` — the flat
> `Node`/`Edge` records serialize on rayon chunks, measured **695ms → ~150ms (~4.5×)**,
> output byte-identical (fineract + servicemix artifact hashes unchanged). Serialization
> is CPU; the write is I/O, so only the serialize step parallelizes.
>
> **Measured and NOT shipped:** the same treatment on `write_parsed_files` *regressed*
> it (1.0s → 1.2–1.6s) — `ParsedFile` is a large nested struct where full-buffer
> materialization outweighs the parallel win — so the parse/resolve writers stay
> sequential. Wall-time delta is within noise on a contended dev machine; the CPU
> reduction scales on the banking target (~10× the node count).
>
> **Fix statuses:** Fix 1 (WSL2 filesystem) unchanged — user-side. Fix 2
> (integration XML) not pursued: ~1% on native, and the Windows slowness was the I/O
> bridge (Fix 1); `di_xml` already carries the parallel pattern if XML parse ever
> dominates. Fix 3 (Falkor batch) already at `BATCH = 4000`. Fix 4 (resolve
> parallelization) obsolete — targeted the since-deleted legacy emitter, and resolve
> is ~1% locally.

## Baseline (Windows Docker, 12,334 Java files, bind-mount from D:\)

Measured 2026-06-22 via `docker compose run --rm engine analyze /repo --all --no-cache`:

| Phase | Duration | % of total | Output |
|---|---|---|---|
| Scan (filesystem walk) | 2m 47s | 15% | 12,334 Java files, 55 modules |
| Hash (blake3, 12,334 files) | 27s | 2% | — |
| Parse (rayon, tree-sitter) | **8m 35s** | **46%** | 236,272 struct nodes |
| Resolve | 13s | 1% | 217,924 edges |
| Integration XML walk | 1m 53s | 10% | 2,130 route nodes |
| DI XML walk | 1m 32s | 8% | 3 bean nodes |
| Artifact write | ~1m 30s | 8% | 239,228 nodes, 456,891 edges |
| FalkorDB load | 2m | 11% | — |
| **Total** | **~19m** | 100% | |

---

## Root Cause: Windows Docker Filesystem Bridge

The repo is bind-mounted from Windows NTFS (`D:\projects\`) into Docker via the
Hyper-V/WSL2 boundary. Every `read()` syscall crosses that boundary. This explains
why parse takes 8.5 min for work that takes ~45s on native Linux (same rayon code,
same CPU count). The I/O latency per file is ~40ms instead of ~1ms.

**Optimization priority: Fix I/O first. CPU parallelism is secondary.**

---

## Fix 1 — Move repo into WSL2 native filesystem (no code change, ~10x speedup)

**Impact: parse ~8m 35s → ~45s. Scan ~2m 47s → ~15s. XML walks ~3m → ~20s.**

Copy the Java repo into the WSL2 native filesystem before mounting:

```bash
# In a WSL2 terminal
cp -r /mnt/d/projects/your-java-repo ~/projects/your-java-repo
```

Then run Docker with a WSL2 path, not a Windows path:
```bash
docker run --rm \
  -v ~/projects/your-java-repo:/repo \
  -v /mnt/d/projects/yummy-cih/.cih:/repo/.cih \
  phuchoang29/yummy-cih:latest \
  cih-engine analyze /repo --all
```

Or update `docker-compose.yml` so the `repo` volume bind-mounts from the WSL2 path.
Expected total analyze time: ~3-4 minutes instead of ~19 minutes.

> **Note:** This is the single highest-value change. Do this first and re-measure
> before implementing any code optimization.

---

## Fix 2 — Parallelize Integration XML and DI XML walks (code change, ~3m savings)

### Problem

Integration XML (1m 53s → 2,130 results) and DI XML (1m 32s → **3 results**) both
walk the entire repo searching for XML config files. The DI XML taking 92 seconds to
find 3 beans means it is scanning thousands of XML files sequentially. Both use the
`ignore` crate's `WalkBuilder` in sequential mode.

### Change

`crates/cih-resolve/src/integration_xml.rs` and `crates/cih-resolve/src/di_xml.rs`:

1. Add `rayon.workspace = true` to `crates/cih-resolve/Cargo.toml`.
2. Collect the XML file list first (sequential walk is fine — it's cheap).
3. Parse/analyze the files in parallel:

```rust
// Pattern — same as cih-parse
let xml_files: Vec<PathBuf> = collect_xml_files(repo_root);  // fast sequential walk

let results: Vec<XmlResult> = xml_files
    .par_iter()
    .filter_map(|path| parse_xml_file(path).ok())
    .collect();
```

Both parsers are stateless (no shared index needed) so this is safe without any wrapper.

### Files to modify
- `crates/cih-resolve/Cargo.toml` — add rayon
- `crates/cih-resolve/src/integration_xml.rs` — parallelize parse loop
- `crates/cih-resolve/src/di_xml.rs` — parallelize parse loop

---

## Fix 3 — FalkorDB load: batch size tuning (config/code change, ~30–60s savings)

### Problem

The FalkorDB load takes 2 minutes for 239k nodes / 456k edges via UNWIND-batch MERGE
over Redis TCP. The per-batch overhead depends on the batch size set in the bulk loader.

### Change

In `crates/cih-falkor/src/bulk.rs` (or wherever `BATCH_SIZE` / `EDGE_BATCH_SIZE` is defined):

- Try batch sizes of 500, 1000, 2000 and measure. Optimal is usually 1000–2000 for
  FalkorDB on local Docker.
- If the Redis client doesn't set `tcp_nodelay`, enable it to reduce ACK latency on
  small writes.
- Consider pipelining: send multiple UNWIND commands before waiting for ACKs.

---

## Fix 4 — cih-resolve parallelization (low priority, saves ~10s)

**This is 1% of total time. Do after Fixes 1–3.**

### Parallel per-file edge collection (Pass 1)

`emit_receiver_bound_calls` in `crates/cih-resolve/src/emit.rs` iterates all 12,334 files
sequentially. `ResolveIndex` is fully read-only after `build()`, so per-file resolution is
safe to parallelize.

**Step 1:** Make resolution pure. Change `resolve_receiver_expr_type` from `&mut self` to
`&self`, returning the optional external FQCN as a second return value instead of inserting
into `self.unresolved_external_fqcns` inline.

**Step 2:** Add a local result collector:
```rust
struct PerFileResult {
    edges: Vec<Edge>,
    handled: Vec<(usize, usize)>,
    ext_fqcns: Vec<String>,
    skipped: u64,
}
```

**Step 3:** Replace the sequential loop with `par_iter` + sequential merge:
```rust
fn emit_receiver_bound_calls(&mut self) {
    let results: Vec<PerFileResult> = self.parsed
        .par_iter()
        .enumerate()
        .map(|(file_idx, pf)| { /* pure resolution */ })
        .collect();

    for r in results {            // merge
        self.edges.extend(r.edges);
        for key in r.handled { self.handled.insert(key); }
        self.unresolved_external_fqcns.extend(r.ext_fqcns);
        self.skipped += r.skipped;
    }
}
```

### rayon::join for independent passes (Passes 4, 5, 6)

Passes 4 (`emit_import_edges`), 5 (`emit_heritage_edges`), 6 (`emit_mro_edges`) do not
read `self.handled`. Extract each to a standalone pure function and run concurrently:

```rust
let ((import_edges, heritage_out), mro_edges) = rayon::join(
    || rayon::join(
        || collect_import_edges(&self.index, self.parsed),
        || collect_heritage_edges(&self.index, self.parsed),
    ),
    || collect_mro_edges(&self.index),
);
```

### Parallel dedup in `finish()`

Replace sequential `BTreeMap` dedup with rayon sort:
```rust
self.edges.par_sort_unstable_by(|a, b| { /* src, dst, kind */ });
self.edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst
    && a.kind.cypher_label() == b.kind.cypher_label());
```

### Files to modify
- `crates/cih-resolve/Cargo.toml` — add `rayon.workspace = true`
- `crates/cih-resolve/src/emit.rs` — all changes above

---

## Implementation Order

| Priority | Fix | Effort | Expected saving |
|---|---|---|---|
| 1 | WSL2 native filesystem | 5 min (config) | ~15 min |
| 2 | Parallel XML walks | 1–2 hours | ~3 min |
| 3 | FalkorDB batch tuning | 30 min | ~30–60s |
| 4 | cih-resolve par_iter | 3–4 hours | ~10s |

---

## Verification

After each fix, re-run and compare:
```bash
docker compose run --rm engine analyze /repo --all --no-cache
```

Correctness invariant: `nodes=239228 edges=456891` must stay identical.
Timing: record each phase duration from the `tracing` log lines.
