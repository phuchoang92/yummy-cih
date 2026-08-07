# Documenting — generate and maintain per-endpoint / per-symbol pages

Persona: documentation agent (Kiro, Claude, …) writing markdown pages for a
codebase indexed by CIH. The server never calls an LLM and never writes docs;
it serves **evidence** (`doc_pack`) and **freshness** (`doc_status`). You write
the prose through your own approved channel and own the files.

## When to use

- "Document every endpoint of this service."
- "Write/refresh the page for `OrderService`."
- "Which of our generated docs are out of date after this re-index?"

## The rules

1. **Claim only what is in the delivered pack.** Every section carries explicit
   `completeness`; an *incomplete* empty list proves nothing, and a section with
   `available: false` is a serving/config condition, not a fact about the code.
   Do not fill gaps from memory or guesswork.
2. **Never edit outside the prose markers.** Everything else on the page is
   regenerated verbatim from evidence; hand edits there are lost by design.
3. **Never edit the frontmatter.** `cih_evidence_hash` and the two profiles are
   how `doc_status` decides freshness.

## Step by step

### 1. Enumerate what to document

```json
route_map()                       // endpoints
architecture_overview(repo=...)   // modules + anchor symbols for type pages
```

### 2. Fetch one node's evidence pack

```json
doc_pack(name="Route:GET /api/orders", group="shop")
doc_pack(name="OrderService")
doc_pack(name="Method:com.acme.OrderService#save/1", sections=["flow","tests"])
```

Supported kinds: Route, Method, Function, Constructor, Class, Interface.
Arguments: `repo` (registry name; empty = primary), `group` (enables the
cross-repo consumers section on routes), `include_source` (default true),
`sections` (subset of `flow`, `upstream`, `tests`, `source`, `contracts`;
omit for all five — an explicit empty list is rejected).

The response contains bounded evidence sections plus:

- `markdown` — a deterministic page skeleton, ready to write to disk;
- `profile` — exactly which sections were **delivered**;
- `requested_profile` — what you asked for (differs only when the response
  byte cap dropped a section — the drop is named in `warnings`);
- `evidence_hash` — blake3 over the delivered node-local evidence.

### 3. Write the page and add prose

Write `markdown` under your docs directory (e.g. `docs/api/get-orders.md`),
then add prose **only** between the three marker pairs:

```markdown
<!-- cih:prose:overview:start -->
One paragraph: what this endpoint is for, written from the delivered evidence.
<!-- cih:prose:overview:end -->
```

Markers exist for `overview` (after the title), `flow` (inside Execution
flow), and `notes` (end of page).

### 4. Check freshness after any re-index

```json
doc_status(docs_dir="docs")
```

Each page row is `fresh`, `stale`, `missing_node`, `unparseable`, or `error`:

- `fresh` — the delivered evidence is byte-identical under the page's stored
  profile. Unrelated repository changes do **not** stale a page; only
  node-local evidence moves the hash.
- `stale` — regenerate (step 5).
- `missing_node` — the symbol no longer exists; retire or re-point the page.
- `unparseable` — the frontmatter is damaged; regenerate the whole page.
- `error` — a *current* serving failure prevented comparison; fix the backend
  condition and re-run. Never treat `error` as fresh.
- `profile_reduced: true` — a prior byte-cap drop shrank the delivered
  profile. Such a page can legitimately stay fresh forever under its reduced
  profile; to retry the full pack, call `doc_pack` with arguments rebuilt from
  `cih_requested_profile` (step 5.2).

### 5. Regenerate a stale page (prose-preserving)

`doc_pack` always returns a fresh skeleton; it never reads or merges your
page. The client-side algorithm:

1. **Extract** each prose block from the existing page: the text between
   `<!-- cih:prose:<name>:start -->` and `<!-- cih:prose:<name>:end -->` for
   `overview`, `flow`, `notes`. If any marker is missing, duplicated, or out
   of order, **stop — do not overwrite automatically**; report that the page
   needs manual reconciliation.
2. **Re-request with the caller's intent**: translate the page's
   `cih_requested_profile` (not the possibly reduced `cih_profile`) back into
   arguments — `group`, `include_source`, `sections` — and call `doc_pack`.
3. **Splice** each preserved prose block between the matching markers of the
   fresh skeleton.
4. **Review** prose whose claims conflict with the changed evidence (the new
   pack shows what moved), then replace the file atomically, keeping the new
   frontmatter untouched.

#### Before/after example

Before (stale page, prose in markers):

```markdown
---
title: "GET /api/orders"
cih_node: "Route:GET /api/orders"
cih_evidence_hash: 3f1a09b2c4d5e6f7a8b9c0d1e2f30415
cih_generator: doc_pack-v1
cih_profile: {"schema":1,"group":"shop","include_source":true,"sections":["flow","upstream","tests","source","contracts"]}
cih_requested_profile: {"schema":1,"group":"shop","include_source":true,"sections":["flow","upstream","tests","source","contracts"]}
---

# GET /api/orders
<!-- cih:prose:overview:start -->
Lists the caller's orders, newest first.
<!-- cih:prose:overview:end -->
...
```

After regeneration the frontmatter carries the **new** hash and the skeleton
reflects the new evidence, while the marker blocks still contain
"Lists the caller's orders, newest first." — updated by you only if the new
evidence contradicts it.

## Known limits (be honest about them)

- **Source freshness**: the hash covers only the delivered source excerpt
  (at most 120 lines / 8 KiB). An edit beyond the truncation point may not
  stale the page unless it also changes the symbol's line span, its typed
  complexity metrics, or another delivered evidence field.
- **Route test scope is direct-only**: an empty Tests section on a Route page
  does not establish that its handler is untested. For handler-level
  coverage, call `test_coverage` on the first callable in the delivered flow.
- **Contracts need a group**: the Cross-repo consumers section is served only
  for Route nodes with `group=...`; treat `contracts_stale: true` as suspect
  and re-run `cih-engine group sync <group>` before quoting consumers.

## Output shape to return to the user

- Pages written/updated (paths), each with its node and evidence hash.
- Stale/missing/error pages found by `doc_status` and what you did about them.
- Any `warnings` from the packs (dropped sections, malformed identity props).
