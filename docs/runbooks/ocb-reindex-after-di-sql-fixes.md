# OCB re-index after the DI / SQL / trace fixes

Applies to the change window that shipped: qualifier-aware Spring DI dispatch,
widened SQL execution-site detection, trace filtering/paging with the
`isAccessor` prop, the grep fast path, the `routes` registry fix, line ranges on
graph reads, SQL-searchable DbQuery nodes, and the `reaches` tool
described below. Parse cache schema is now **27** and the persisted search
sidecar format is now **2**; both changes force stale derived data to be rebuilt
without a graph-schema migration.

## One re-index covers everything

The parser changes (`@Qualifier` capture, SQL constants/sites, `isAccessor`)
share the single schema bump, so the laptop workflow from
[ocb-reindex-after-cxf-fixes.md](ocb-reindex-after-cxf-fixes.md) runs **once**:

1. Rebuild the Docker image from `dev` at or after this change window.
2. If OCB's audit/DAO wrappers are known by name, declare them in the repo's
   `cih.toml` before analyzing (the config feeds the parse-cache namespace, so a
   later config edit re-parses correctly but costs a re-parse — set it up front):

   ```toml
   [analyze]
   sql_apis = ["AuditQueue.enqueue"]   # Receiver.method, repeatable
   ```

   Even without config, SQL constants flowing into unlisted wrappers are picked
   up heuristically (DbQuery props carry `"heuristic": true`).
3. Run `analyze --all --no-cache` per the container runbook, then wiki/embedding
   regeneration as usual.

## What to expect after the re-index

- **The change-password trace resolves through the qualifier**: the
  `CustomUserImpl.modifyUserPassword → CustomUserImpl.modifyUserPassword`
  self-loop is gone; the edge goes to the XML-wired impl with reason
  `di-qualifier` (conf 0.95). Where no wiring/qualifier exists the call lands on
  the interface method (`receiver-bound`) instead of a guessed impl; sole-impl
  guesses are demoted to `di-single-impl` (0.75).
- **`trace_flow` output changes shape**: default page of 100 hops with honest
  `completeness.complete=false` + `next_offset` when truncated; pass
  `business_only: true` to hide constructors/accessors, `max_nodes`/`offset` to
  page. A `db_effects` section lists every table read/write of traced methods —
  the audit chain shows `INSERT` → `AUDIT_LOG` directly.
- **`reaches` answers the AUDIT_LOG question directly**:
  `reaches(from="confirmChangePassword", to="AUDIT_LOG", access="write")`
  returns shortest evidence paths whose final edge is `WRITES_TABLE`, with
  per-edge confidence; bare table names fall back to the `DbTable:` id. A
  budget-limited search reports `status: "inconclusive"`, never a false
  unreachable result.
- **`status` shows real route counts** right after analyze (no discover needed).
  Entries indexed by older builds carry `routes_current: false`; text status
  labels their route count `stale — re-run analyze` instead of treating a
  historical zero as a codebase fact.
- **`search_code("AUDIT_LOG")` surfaces the physical SQL** (DbQuery nodes are
  now BM25-indexed: table names, operation, constant name, SQL preview).
- **`context`/`impact` symbol ranges are real** (line 0/0 came from reads that
  dropped the persisted `startLine`/`endLine`; no re-index needed for this one).
- **Single-file `grep_files` is O(1)**: a metacharacter-free glob stats the file
  instead of walking the 500k-file volume; literal glob prefixes prune the walk
  to their subtree. Exact file symlinks are accepted only when their canonical
  target stays in the repository; directory symlink prefixes are never walked.
  Default grep concurrency is now 2
  (`CIH_GREP_MAX_CONCURRENT_REQUESTS`).
- **Reads tolerate FalkorDB dataset loads**: after a container restart, reads
  wait out `BusyLoadingError` up to `CIH_FALKOR_READ_LOAD_WAIT_SECS` (20s)
  instead of failing immediately with "graph store unavailable".
