# OCB-style OSGi/CXF gaps: interface-route stitching, DI XML filters, per-bundle servlet prefix

## Context

The user's banking platform (OCB-SP05) is a multi-bundle OSGi codebase: each `custom-<x>` bundle has `META-INF/spring/` XML (whiteboard servlet pattern `/rest/<x>/*`, secured `/v1` + non-secured `/ns/v1` `<jaxrs:server>`s, Spring `<bean>` defs, Spring-DM `<osgi:reference>`/`<osgi:service>` wiring), and JAX-RS annotations live on the **interface** in the `-api` bundle while the impl beans (two per bundle!) implement it. Tracing this through yummy-cih found three gaps:

1. **Interface-annotated routes never get the CXF prefix** — `stitch_route_prefixes` (`crates/cih-resolve/src/lang/java/cxf.rs`) matches `Route.handler` (interface FQCN) against the jaxrs:server bean class (impl FQCN) with no heritage fallback → routes keep bare local paths. Entangled: both secured and non-secured impls implement the same interface, so a correct fix must represent **both** `/v1` and `/ns/v1` addresses (route cloning).
2. **DI XML invisible** — `di_xml.rs::is_di_xml_path` accepts only `applicationContext*`/`beans.xml`/`blueprint*`/`OSGI-INF/blueprint/`; every OCB file (`bundle-context-*.xml`, `beans_rest*.xml` under `META-INF/spring/`) is skipped → zero XML DI edges and zero `<osgi:reference>` dependency edges. (Good news: `parse_di_document` already strips namespace prefixes, so `<osgi:reference>` parses once the file passes the filter.)
3. **One global servlet prefix** — `resolve_servlet_prefix` takes the FIRST `osgi_servlet` node repo-wide; with N bundles each declaring `/rest/<name>/*`, N−1 get the wrong base path.

OCB itself lives on the user's Windows laptop (Docker analyze workflow); verification here = unit tests + synthetic OCB-shaped fixture + no-regression on the local servicemix/fineract eval corpora.

**Verified facts the design relies on** (double-checked by exploration + plan agents):
- `post_process` (`lang/java/mod.rs:74`) runs AFTER heritage edges exist; `Implements`/`Extends` are `src=impl → dst=interface` (reason "heritage", `emit.rs:297`); `MethodImplements` src=impl-method → dst=iface-method.
- `stitch_route_prefixes` currently takes `nodes: &mut [Node]` (slice) — cloning routes requires `&mut Vec<Node>`; only caller is `java/mod.rs:83`.
- `osgi_servlet` and `cxf_jaxrs_server` IntegrationRoute nodes carry their source XML rel path in `Node.file`.
- `integration_xml.rs::is_integration_xml` requires `<bean` (or a CXF ns) for the spring kind → a pure `<osgi:reference>/<osgi:service>` file is skipped entirely; the `<service>` branch (line ~403) hardcodes `"source":"blueprint_xml"`; spring `<bean id=x>` + `<osgi:service ref=x>` in one file collide on node id.
- cxf.rs has a rich `#[cfg(test)]` helper toolkit (`integration_route`, `route_node`, `handles_route_edge`, `server_and_bean`, `class_node`, hand-rolled `temp_dir`) — mirror it; helpers hardcode `file:"beans.xml"`, so per-bundle tests need a file-path parameter/helper.
- di_xml/integration_xml tests are integration tests (`crates/cih-resolve/tests/{di_xml,integration_xml}.rs`), inline XML strings + one tempdir e2e pattern.
- bnd.bnd is unparsed anywhere — **explicitly out of scope** (module-dependency extraction is a separate feature).

Branch: `dev`. At implementation start, copy this plan to `/Users/phuc/BigMoves/AI/ocb-osgi-cxf-gaps-plan.md` AND (per repo convention) `docs/plans/ocb-osgi-cxf-stitching.md` in yummy-cih (committed with Step 1).

## Step 1 — Gap 2: DI XML filters + Spring-DM OSGi (independent; land first)

Files: `crates/cih-resolve/src/di_xml.rs`, `crates/cih-resolve/src/integration_xml.rs`, `crates/cih-resolve/tests/{di_xml,integration_xml}.rs`.

- `is_di_xml_path` (di_xml.rs:53): add `if rel.contains("META-INF/spring/") && lower.ends_with(".xml") { return true; }` — the content gate (`is_di_xml`, applied after read) remains the real filter.
- `is_di_xml` (di_xml.rs:45): also accept `content.contains("http://www.springframework.org/schema/osgi")`.
- `is_integration_xml` (integration_xml.rs:18): add a check for the same Spring-DM namespace returning `Some("spring")` — without it, a `bundle-context-rest-osgi.xml` containing no `<bean>` is skipped and the `<service>` branch is dead code for the OCB shape.
- `<service>` branch (integration_xml.rs:~389–407): use `source_label` instead of hardcoded `"blueprint_xml"`; for `spring_xml` files key the node id as `service:{refer}` to avoid colliding with a same-file `<bean id={refer}>` (blueprint keeps its existing `bean:{id}` namespacing — don't churn blueprint artifacts). Do NOT add `<reference>` capture here — di_xml owns dependency edges.
- Keep reason string `"di-xml-blueprint-reference"` for Spring-DM references (no `ReferenceDef.source` plumbing); widen the doc comment to "Blueprint or Spring-DM `<reference>`".

Tests (mirror existing styles):
- `detects_di_xml_paths` extended: accepts `custom-remittance/.../META-INF/spring/bundle-context-rest.xml`; still rejects `pom.xml`.
- `spring_dm_reference_parses` — inline `<osgi:reference interface=.../>` → one `ReferenceDef`.
- `osgi_reference_in_meta_inf_spring_emits_calls_edge` — tempdir e2e mirroring `field_injection_emits_calls_edge`: Spring-DM file + ParsedFile with `Interface Api` / `Class ApiImpl` + Implements site → `Calls` edge `Interface:Api → Class:ApiImpl`, confidence 0.7.
- `spring_dm_service_uses_spring_source_label` — service node `source == "spring_xml"`, bean + service ids distinct.
- `spring_dm_only_file_is_detected` — no `<bean>`, no cxf ns → still emits the service node.

**Commit A:** `fix(resolve): recognize META-INF/spring DI XML and Spring-DM osgi wiring`

## Step 2 — Gap 3: per-server servlet prefix (before Gap 1 — the clone loop consumes it)

File: `crates/cih-resolve/src/lang/java/cxf.rs`.

Replace the single global `(servlet_prefix, servlet_source)` with:

```rust
struct ServletPrefixResolver {
    config: Option<String>,                              // normalized cxf_base_path override
    osgi: Vec<(String /*file*/, String /*pattern*/)>,    // from osgi_servlet nodes, sorted
    fs_fallback: OnceCell<Option<(String, &'static str)>>, // web.xml → spring-boot, lazy global
}
```

`prefix_for(repo_root, server_file)` chain per `cxf_jaxrs_server` target:
1. config override → `("…","config")` (global, unchanged).
2. Longest-common-leading-directory-components score between `dir(server_file)` and each osgi entry's dir; pick max score **> 0**; ties → higher score, then shortest file path, then lexicographic (deterministic) → `("…","osgi_whiteboard")`.
3. Score 0 everywhere AND exactly one `osgi_servlet` repo-wide → use it (preserves today's single-bundle behavior and today's tests, whose synthetic nodes all carry `file:"beans.xml"`).
4. Multiple exist, none share a directory → skip the osgi layer (the deliberate bug fix vs. today's first-node-wins).
5. Lazy global fs fallback: web.xml `CXFServlet` scan → spring-boot `cxf.path`.
6. `None` → `("", "none")` as today.

Wiring: add `server_file: String` to `Target` (from `n.file`); build resolver once after the `targets.is_empty()` bail; memoize per server_file; per-target prefix feeds `new_path`, the `servlet_prefix_source` prop, and the `cxf-jaxrs-prefix` link's `prefix` prop. Keep `resolve_servlet_prefix(...)` as a thin compat wrapper so the existing `servlet_prefix_*`/`web_xml_*`/`spring_boot_*` tests pass unchanged.

Tests (new helper for node file paths, since existing helpers hardcode `file:"beans.xml"`): `per_bundle_servlet_prefix_selected_by_directory` (two bundles → `/rest/a/v1/x` and `/rest/b/v1/y`), `single_osgi_servlet_applies_across_directories` (regression guard), `multiple_unrelated_osgi_servlets_do_not_cross_apply` (→ `servlet_prefix_source == "none"`), `config_override_beats_per_bundle_pattern`, `servlet_prefix_tie_breaks_deterministically`.

**Commit B:** `fix(resolve): resolve CXF servlet prefix per jaxrs:server bundle`

## Step 3 — Gap 1a: interface-fallback matching (single match, in-place rewrite)

File: `crates/cih-resolve/src/lang/java/cxf.rs` (heritage edges are already in `edges` at post_process time).

- Build alongside the existing FQCN maps: `id_to_fqcn` (Class/Interface/Enum/Record → qualified_name), `kind_by_fqcn`, and adjacency `supers: HashMap<&str, Vec<&str>>` from every `Implements`/`Extends` edge whose endpoints resolve via `id_to_fqcn` (match on `kind`, not reason).
- `supertype_closure(fqcn)` — BFS with visited-set + depth cap (64); keep only FQCNs whose kind is `Interface`. Memoize per distinct target FQCN; store as `Target.interfaces: HashSet<String>`.
- Matching: `handler_class = handler.split('#').next()`. Exact matches (today's rule) keep absolute priority; interface-fallback (`t.interfaces.contains(handler_class)`) applies only when no exact target matches. This step still rewrites in place with the first match.

Tests (new helpers `interface_node(fqcn)`, `implements_edge(...)`, `extends_edge(...)`): `stitch_interface_handler_via_impl_class`, `stitch_interface_fallback_transitive_extends` (Impl implements A, A extends B, handler on B), `stitch_exact_impl_match_beats_interface_fallback`, `stitch_interface_handler_without_heritage_is_noop`.

**Commit C:** `feat(resolve): stitch interface-annotated JAX-RS routes via heritage fallback`

## Step 4 — Gap 1b: dual-server route cloning

Files: `cxf.rs` + `lang/java/mod.rs` (signature: `stitch_route_prefixes` takes `&mut Vec<Node>` instead of `&mut [Node]`).

- Rewrite loop: compute the route's ordered match list (exact-only, else fallback-only), preserving `targets` order.
  - First match with a changed path → in-place rewrite exactly as today (`id_remap` entry, provenance link).
  - Each **additional** match → clone the node with fresh id `Route:{method} {new_path}` (skip when the path is already seen for this route or the id already exists among Route nodes), update path/local_path/servlet_prefix_source (handler kept — the same interface method genuinely serves both), emit the `cxf-jaxrs-prefix` link from the additional server, record `(old_id, clone_id)`.
- Edge handling, order matters: FIRST additively duplicate every edge whose `dst` is a cloned route's old id (HANDLES_ROUTE etc.) with `dst = clone_id`; THEN run the existing 1:1 `id_remap` repoint. `nodes.extend(clones)` at the end; clone order follows route order → deterministic artifacts.

Tests: `stitch_dual_servers_clone_route_per_address` (the OCB remittance shape: `/v1`→SecuredImpl, `/ns/v1`→NonSecuredImpl, both implementing the annotated interface → exactly two Route nodes with two HANDLES_ROUTE edges), `stitch_dual_servers_same_resulting_path_dedups`, `stitch_clone_skipped_when_id_already_exists`, and the composed end-to-end `dual_server_bundle_full_ocb_shape` (whiteboard `/rest/remittance/*` + both servers in one bundle dir → `/rest/remittance/v1/…` AND `/rest/remittance/ns/v1/…`, both `servlet_prefix_source == "osgi_whiteboard"`) — this is the synthetic OCB fixture; no external repo needed.

**Commit D:** `feat(resolve): clone JAX-RS routes for secured + non-secured jaxrs servers`

## Step 5 — docs

`docs/ARCHITECTURE.md`: interface-fallback semantics, per-bundle prefix chain, route cloning, widened `di-xml-blueprint-reference` meaning. Fold into Commit D or a small `docs:` commit.

## Verification

Local:
```bash
cargo clippy --workspace --all-targets && cargo test --workspace
cargo test -p cih-resolve cxf
cargo test -p cih-resolve --test di_xml --test integration_xml
# Regression eval (servicemix + fineract present locally; petclinic WARN-skips):
EVAL_REPOS_DIR=/Users/phuc/BigMoves/AI/cih-eval-repos scripts/eval-enterprise-java.sh
# Before/after artifact diff on servicemix + fineract: expect ADDITIVE-only deltas
# (new di-xml/IntegrationRoute facts); NO Route path may change on these repos —
# if one does, stop and inspect. Do not expect byte-identical hashes.
```

Windows/OCB side (user, after merging): rebuild Docker image, `cih-engine analyze <ocb> --all --no-cache`, then check: `route_map` shows `/rest/<bundle>/v1/…` and `/rest/<bundle>/ns/v1/…` per bundle with `servlet_prefix_source: osgi_whiteboard`; `di-xml-*` edges > 0 in edges.jsonl; `trace_flow` from a remittance route reaches the impl method. Regenerate wiki/embeddings (route ids changed).

## Risks / behavior changes (what each means in practice)

- **Route ids change** where stitching newly applies. A route's graph identity IS its path (`Route:GET /beneficiaries` → `Route:GET /rest/remittance/v1/beneficiaries`), so wiki pages, embeddings, saved queries, and the loaded graph reference ids that stop existing. *Remedy (one-time, per repo):* full `analyze --no-cache`, graph reload, wiki + embedding regeneration on OCB. Nothing fails silently — stale ids just return not-found.
- **Route counts roughly double** on dual-server bundles. Intended: each operation genuinely serves two URLs (secured `/v1` + non-secured `/ns/v1` behind two impl beans). Dashboards/evals/stakeholder numbers tracking route counts will jump; the old number undercounted the real API surface. Side benefit: taint/api_impact now see the previously invisible `/ns/v1` entry points.
- **Prefix semantics change on ambiguity**: today, multiple whiteboard patterns → first-node-wins applied repo-wide (confidently wrong for N−1 bundles). After: pattern↔server matched by shared bundle directory; when nothing matches, NO osgi prefix (falls through to web.xml/spring-boot/none) — a visible gap (`servlet_prefix_source:"none"`) instead of a silent lie. Escape hatch: `cxf_base_path` in cih.toml.
- **`di-xml-blueprint-reference` becomes a slight misnomer**: Spring-DM `<osgi:reference>` edges carry the same reason string as Blueprint ones. Kept deliberately — renaming breaks existing filters/artifacts for zero functional gain. Cosmetic only.
- **Eval artifact hashes change**: this change adds facts, so byte-identity on servicemix/fineract can't be the bar. New bar: no counts decrease, NO existing Route path changes on those repos, diff reviewed additive-only. A changed route path there = unexpected regression → stop.
- Out of scope: bnd.bnd parsing (bundle→bundle module deps) and `<osgi:reference>` browsability nodes in integration_xml (dependency EDGES still come from di_xml; only standalone viz nodes are skipped).
