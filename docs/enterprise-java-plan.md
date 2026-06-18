# Plan: Enterprise Java Genericity — JAX-RS Routes + XML Integration + Eval Harness

## Context

CIH currently only extracts Spring MVC routes (`@GetMapping`, `@PostMapping`, etc.). Apache Fineract
(6,386 Java files, 172 JAX-RS files) uses `jakarta.ws.rs` annotations exclusively — `@Path` on the
class and `@GET`/`@POST` etc. on methods. ServiceMix has 26 Blueprint XML files with Camel routes
that define messaging topology but no Java annotations to capture it. The existing eval repos at
`/Users/phuc/BigMoves/AI/cih-eval-repos/{fineract,servicemix}` are the test fixtures.

Three things need to happen:
1. **JAX-RS route extraction** — extend the Java parser to handle the JAX-RS annotation pattern
2. **Blueprint/Camel XML extraction** — new extractor for enterprise messaging topology
3. **Eval harness** — reproducible script to measure before/after on both repos

---

## Part 1 — JAX-RS Route Extraction

### How JAX-RS differs from Spring

| Concern | Spring | JAX-RS |
|---|---|---|
| Class-level prefix | `@RequestMapping("/v1/cart")` | `@Path("/v1/charges")` |
| HTTP verb + path | `@GetMapping("/sub")` (one annotation) | `@GET` + `@Path("/sub")` (two separate annotations) |
| Verb annotation type | `annotation` with arg | `marker_annotation` (no args) |

### Files to change: `crates/cih-lang/src/java/parse.rs`

**Step A — extend `spring_class_prefix` (line 1298) to also read `@Path`:**

```rust
fn spring_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node).into_iter().find_map(|ann| {
        match annotation_name(ann, src).as_deref() {
            Some("RequestMapping") | Some("Path") => first_route_value(ann, src),
            _ => None,
        }
    })
}
```

`TypeContext.spring_prefix` already flows to `emit_spring_routes_for_method` — no struct change needed.

**Step B — add `jaxrs_http_method()` (new function, alongside `spring_http_method` ~line 1364):**

```rust
fn jaxrs_http_method(annotation: &str) -> Option<&'static str> {
    match annotation {
        "GET" => Some("GET"), "POST" => Some("POST"),
        "PUT" => Some("PUT"), "DELETE" => Some("DELETE"),
        "PATCH" => Some("PATCH"), "HEAD" => Some("HEAD"),
        _ => None,
    }
}
```

**Step C — extend `spring_method_routes` (line 1305) to handle JAX-RS pattern:**

JAX-RS uses two separate annotations on the same method: `@GET` (verb, marker, no path) +
optional `@Path("/sub")` (sub-path). Collect all method annotations first, then:

```rust
fn spring_method_routes(node: TsNode<'_>, src: &str) -> Vec<SpringRoute> {
    let anns: Vec<TsNode<'_>> = annotations(node);

    // JAX-RS path: @GET (or @POST etc.) + optional sibling @Path
    if let Some((http_method, verb_ann)) = anns.iter().find_map(|&ann| {
        jaxrs_http_method(annotation_name(ann, src)?.as_str()).map(|m| (m, ann))
    }) {
        let path = anns.iter().find_map(|&ann| {
            if annotation_name(ann, src).as_deref() == Some("Path") {
                first_route_value(ann, src)
            } else { None }
        }).unwrap_or_default();
        return vec![SpringRoute {
            annotation: http_method.to_ascii_lowercase() + "jaxrs",
            http_method, path, range: range_of(verb_ann),
        }];
    }

    // Original Spring handling (unchanged below)
    ...
}
```

Early-return for JAX-RS avoids double-emitting on mixed codebases. The rest of the function
is unchanged.

**Step D — add unit tests** in `crates/cih-parse/src/lib.rs`:
- `jaxrs_class_path_used_as_prefix` — class with `@Path("/v1/loans")` + method `@GET` + `@Path("/{id}")` → `Route:GET /v1/loans/{id}`
- `jaxrs_bare_get_no_subpath` — class `@Path("/v1/charges")` + method `@GET` (no method `@Path`) → `Route:GET /v1/charges`
- `jaxrs_post_with_subpath` — `@POST` + `@Path("/batch")` → `Route:POST /v1/charges/batch`

---

## Part 2 — Blueprint/Camel XML Extraction

### Architecture: new module in `cih-resolve`

Follow the exact pattern of `crates/cih-resolve/src/db_access.rs`:
- **Input**: `repo_root: &Path` (scan XML files directly, no ParsedFile needed)
- **Output**: `(Vec<Node>, Vec<Edge>)`
- **Deduplication**: `HashSet<NodeId>` for seen nodes

**New file**: `crates/cih-resolve/src/blueprint_xml.rs`

**File discovery** — scan for these paths inside `repo_root`:
```
**/OSGI-INF/blueprint/**/*.xml
**/META-INF/spring/**/*.xml
**/META-INF/camel/**/*.xml
```
Filter: only files containing `camelContext` namespace or `<blueprint` root element (skip unrelated XML).

**Parsing** — use `quick_xml` (already a workspace dep, already used in `cih-engine/src/scan/build_files.rs`). Stack-based event loop, detect:

| XML element | Emit |
|---|---|
| `<route id="...">` | `Node { kind: Route, id: "Route:xml:{file}:{id}", ... }` |
| `<from uri="activemq:queue:X">` | `Node { kind: KafkaTopic, id: "Topic:X" }` + `ListensTo` edge from route |
| `<to uri="activemq:queue:X">` | `Node { kind: KafkaTopic }` + `PublishesEvent` edge from route |
| `<to uri="http://...">` | `Node { kind: ExternalEndpoint }` + `ExternalCall` edge |
| `<from uri="timer://...">` | skip (infrastructure, not business topology) |
| `<from uri="direct:X">` | `ExternalCall` edge between two routes |

Extract the component from URIs: `"activemq:queue:LOG.ME"` → component=`activemq`, name=`LOG.ME`.
Kafka-like components (`activemq`, `kafka`, `jms`) → `KafkaTopic`. HTTP components → `ExternalEndpoint`.

**Wire into `crates/cih-engine/src/analyze.rs`** — after line 312 (db nodes), add:
```rust
let (xml_nodes, xml_edges) = cih_resolve::emit_blueprint_xml(&repo)?;
all_nodes.extend(xml_nodes);
edges.extend(xml_edges);
```

**Unit tests** in `crates/cih-resolve/src/blueprint_xml.rs` (inline `#[cfg(test)]`):
- Write Blueprint XML to temp dir via `temp_repo()` + `write_file()` pattern from cih-parse tests
- Assert Route node emitted, ListensTo/PublishesEvent edges present
- Assert `activemq:queue:LOG.ME` → `Topic:LOG.ME` KafkaTopic node

---

## Part 3 — Eval Harness

### Script: `scripts/eval-enterprise-java.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail
REPOS=(fineract servicemix)
EVAL_BASE="target/cih-eval"
ENGINE="./target/debug/cih-engine"

for repo in "${REPOS[@]}"; do
  REPO_ROOT="/Users/phuc/BigMoves/AI/cih-eval-repos/$repo"
  OUT="$EVAL_BASE/$repo"
  mkdir -p "$OUT"

  echo "=== $repo: analyze ==="
  $ENGINE analyze "$REPO_ROOT" --all --no-load 2>&1 | tee "$OUT/analyze.log"

  echo "=== $repo: discover ==="
  $ENGINE discover "$REPO_ROOT" --no-load --json 2>&1 | tee "$OUT/discover.json"

  echo "=== $repo: wiki ==="
  $ENGINE wiki "$REPO_ROOT" --out "$OUT/wiki" 2>&1 | tee "$OUT/wiki.log"
done
```

Output layout: `target/cih-eval/{repo}/{analyze.log,discover.json,wiki.log,wiki/}`.
`target/` is already gitignored.

### Metrics to capture from logs

From `analyze.log` human summary: Java files, modules, nodes, edges, routes, unresolved refs.
From `discover.json`: communities, processes.
From `wiki.log`: pages, routes.

---

## Acceptance Criteria

- Fineract: route count increases significantly (currently ~0 JAX-RS routes → expect 100+)
- ServiceMix: non-zero XML-derived nodes/edges (Topic nodes, PublishesEvent/ListensTo edges)
- All existing workspace tests pass: `cargo test --workspace`
- New unit tests for JAX-RS and XML extraction all pass
- `target/cih-eval/` not committed (already covered by gitignore)

---

## Files Changed

| File | Change |
|---|---|
| `crates/cih-lang/src/java/parse.rs` | `spring_class_prefix` reads `@Path`; new `jaxrs_http_method()`; `spring_method_routes` handles JAX-RS pattern |
| `crates/cih-resolve/src/blueprint_xml.rs` | **New** — Blueprint/Camel XML extractor |
| `crates/cih-resolve/src/lib.rs` | `pub mod blueprint_xml; pub use blueprint_xml::emit_blueprint_xml;` |
| `crates/cih-engine/src/analyze.rs` | Wire `emit_blueprint_xml` into node assembly |
| `crates/cih-parse/src/lib.rs` | JAX-RS route unit tests |
| `scripts/eval-enterprise-java.sh` | **New** — eval harness |

## Out of Scope

- Gradle module naming (Fineract uses Gradle; no Maven changes needed)
- CXF WSDL/SOAP extraction (deferred — adds complexity, low ROI for current goal)
- JAX-RS `@Consumes`/`@Produces` as graph nodes
