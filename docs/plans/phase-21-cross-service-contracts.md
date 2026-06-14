# Phase 21 — Cross-Service Contract Extraction

**Source:** `docs/gitnexus-discovery.md` §6
**Depends on:** Phase 18 (registry), Phase 19 (detect_changes)
**Feeds:** Phase 22 (api_impact + shape_check)

---

## Goal

Detect inter-service communication patterns within each Java/Spring repo and link them
across repos in a named group:

1. **HTTP clients** — `@FeignClient`, `RestTemplate`, `WebClient` call sites → emit
   `EXTERNAL_CALL` edges with URL template as a node property.
2. **Kafka events** — `@KafkaListener` (consumer) and `publishEvent`/`KafkaTemplate.send`
   (producer) → emit `LISTENS_TO` / `PUBLISHES_EVENT` edges to a shared `KafkaTopic` node.
3. **Group registry** — `~/.cih/groups.json` tracks which repos belong to the same
   microservice landscape.
4. **`group sync`** — matches providers and consumers across repos; writes a
   `~/.cih/groups/<name>/contracts.jsonl` artifact that Phase 22 tools query.

**Done when:** A Kafka producer in service A and a `@KafkaListener` in service B are linked
via a shared `KafkaTopic` node; `group sync` writes the cross-repo contract file; the new
MCP tool `group_contracts` returns matched provider/consumer pairs.

---

## New types in `cih-core`

### NodeKind variants (add to `cih-core/src/lib.rs`)

```rust
// existing:  File, Folder, Class, Interface, Enum, Record, Annotation,
//            Method, Function, Constructor, Field, Route, Community, Process, Other

KafkaTopic,       // a Kafka topic — shared node between publisher and listener
ExternalEndpoint, // a REST endpoint in another service (URL template)
```

**Label strings:**
- `KafkaTopic` → `"KafkaTopic"`
- `ExternalEndpoint` → `"ExternalEndpoint"`

### EdgeKind variants (add to `cih-core/src/lib.rs`)

```rust
// existing: Contains, Calls, Extends, Implements, HasMethod, HasField,
//           Imports, Accesses, Uses, MethodOverrides, MethodImplements,
//           MemberOf, StepInProcess, HandlesRoute, Other

PublishesEvent,  // method → KafkaTopic  (producer)
ListensTo,       // method → KafkaTopic  (consumer/listener)
ExternalCall,    // method → ExternalEndpoint (HTTP client call)
```

**Cypher labels:**
- `PublishesEvent` → `"PUBLISHES_EVENT"`
- `ListensTo` → `"LISTENS_TO"`
- `ExternalCall` → `"EXTERNAL_CALL"`

### NodeId helpers (add to `cih-core/src/lib.rs`)

```rust
pub fn kafka_topic_id(topic: &str) -> NodeId {
    NodeId::new(format!("KafkaTopic:{topic}"))
}

pub fn external_endpoint_id(method: &str, url_template: &str) -> NodeId {
    NodeId::new(format!("ExternalEndpoint:{method}:{url_template}"))
}
```

---

## New IR types in `cih-core/src/ir.rs`

Add `contract_sites: Vec<ContractSite>` to `ParsedFile`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSite {
    pub kind: ContractKind,
    /// URL template for HTTP calls (e.g. "/api/orders/{id}" or full URL).
    #[serde(default)]
    pub url_template: Option<String>,
    /// Kafka/Spring topic name or event class simple name.
    #[serde(default)]
    pub topic: Option<String>,
    /// HTTP method for HTTP calls ("GET", "POST", etc.).
    #[serde(default)]
    pub http_method: Option<String>,
    /// Graph id of the enclosing callable that makes/listens to this call.
    pub in_callable: NodeId,
    pub range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractKind {
    /// HTTP call via RestTemplate / WebClient (non-Feign).
    HttpCall,
    /// @FeignClient interface declaration (the whole interface = one ExternalEndpoint cluster).
    FeignClient,
    /// KafkaTemplate.send() / ApplicationEventPublisher.publishEvent().
    EventPublish,
    /// @KafkaListener / @EventListener method.
    EventListen,
}
```

Add `#[serde(default)] pub contract_sites: Vec<ContractSite>` to `ParsedFile` for
backward-compatible deserialization of old cached artifacts.

---

## Parse-time detection (`cih-parse/src/java.rs`)

Add a new pass at the end of `parse_file()` that scans the AST for contract patterns and
populates `result.contract_sites`.

### HTTP client detection

**`@FeignClient` interface:**
```java
@FeignClient(name = "order-service", url = "${order-service.url}")
public interface OrderClient {
    @GetMapping("/api/orders/{id}")
    Order getOrder(@PathVariable Long id);
}
```
Detection:
1. Any `interface` with `@FeignClient` annotation → extract `name`/`url` attribute.
2. For each method on the interface, extract its Spring mapping annotation
   (`@GetMapping`, `@PostMapping`, etc.) to get the URL path and HTTP method.
3. Emit one `ContractSite { kind: FeignClient, url_template, http_method, in_callable: <method_id> }`
   per method.

**`RestTemplate` call sites:**
```java
restTemplate.getForObject("http://order-service/api/orders/{id}", Order.class, id);
restTemplate.postForEntity("/api/payments", request, PaymentResponse.class);
```
Detection:
1. Any `method_invocation` whose receiver is typed `RestTemplate` (field or local binding).
2. Map method name to HTTP method: `getForObject/getForEntity` → GET, `postForObject/postForEntity` → POST, etc.
3. First string argument = URL template. Normalize: strip `http://hostname` prefix if present.
4. Emit `ContractSite { kind: HttpCall, url_template, http_method, in_callable }`.

**`WebClient` call sites:**
```java
webClient.get().uri("/api/orders/{id}").retrieve()...
```
Detection:
1. Chained call starting with `webClient.get()` / `.post()` / `.put()` / `.delete()`.
2. `.uri("...")` argument = URL template.
3. Emit `ContractSite { kind: HttpCall, url_template, http_method, in_callable }`.

Detection difficulty: medium. WebClient chains are hard to parse from a simple
tree-sitter walk; start with pattern-matching on the `.get().uri(...)` chain.
Fall back to capturing only the `.uri(...)` string argument and inferring HTTP method
from the preceding `.get()`/`.post()` call name.

### Kafka / Spring event detection

**`@KafkaListener` method:**
```java
@KafkaListener(topics = "order-created", groupId = "payment-service")
public void handleOrderCreated(OrderEvent event) { ... }
```
Detection:
1. Any method with `@KafkaListener` annotation.
2. Extract `topics` attribute (single string or array).
3. Emit `ContractSite { kind: EventListen, topic: "<topic>", in_callable: <method_id> }`
   for each topic.

**`KafkaTemplate.send()` call:**
```java
kafkaTemplate.send("order-created", event);
```
Detection:
1. Any `method_invocation` named `send` on a receiver typed `KafkaTemplate`.
2. First string argument = topic name.
3. Emit `ContractSite { kind: EventPublish, topic: "<topic>", in_callable }`.

**`ApplicationEventPublisher.publishEvent()`:**
```java
eventPublisher.publishEvent(new OrderCreatedEvent(order));
```
Detection:
1. Any `method_invocation` named `publishEvent` on a receiver typed `ApplicationEventPublisher`.
2. First argument constructor type = event class simple name (used as topic).
3. Emit `ContractSite { kind: EventPublish, topic: "<EventClassName>", in_callable }`.

**`@EventListener` method:**
```java
@EventListener
public void handleOrderCreated(OrderCreatedEvent event) { ... }
```
Detection:
1. Any method with `@EventListener` annotation.
2. First parameter type simple name = event class name (used as topic).
3. Emit `ContractSite { kind: EventListen, topic: "<EventClassName>", in_callable }`.

---

## Resolve-time edge emission (`cih-resolve/src/lib.rs`)

Add a new function called from `resolve_edges()`:

```rust
pub fn resolve_contract_edges(parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>)
```

For each file, for each `ContractSite`:

**Kafka / Spring events:**
- `EventPublish` → create/merge `KafkaTopic` node with `id = kafka_topic_id(topic)`;
  emit `Edge { kind: PublishesEvent, src: in_callable, dst: topic_node_id }`.
- `EventListen` → same topic node; emit `Edge { kind: ListensTo, src: in_callable, dst: topic_node_id }`.

**HTTP clients:**
- `HttpCall` / `FeignClient` → create/merge `ExternalEndpoint` node with
  `id = external_endpoint_id(http_method, normalized_url_template)`;
  emit `Edge { kind: ExternalCall, src: in_callable, dst: endpoint_node_id }`.
  Store `url_template` and `http_method` in node `props`.

Nodes deduplication: `HashMap<NodeId, Node>` keyed by id — same topic from multiple
callers results in one shared node. Return `(nodes, edges)` added to the existing
`ResolveOutput`.

---

## Group registry (`cih-core/src/group.rs`) — new file

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupEntry {
    pub name: String,
    /// Registry names of member repos (must exist in Registry).
    pub repos: Vec<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroupRegistry {
    pub groups: Vec<GroupEntry>,
}

impl GroupRegistry {
    pub fn load() -> Self         // reads ~/.cih/groups.json; empty if missing
    pub fn save(&self) -> anyhow::Result<()>
    pub fn find(&self, name: &str) -> Option<&GroupEntry>
    pub fn find_mut(&mut self, name: &str) -> Option<&mut GroupEntry>
    pub fn upsert(&mut self, entry: GroupEntry)
    pub fn remove(&mut self, name: &str) -> bool
}

fn groups_path() -> Option<PathBuf>  // ~/.cih/groups.json
```

Add `pub mod group;` and re-exports to `cih-core/src/lib.rs`.

---

## Contract matching (`cih-engine/src/group.rs`) — new file

The `group sync` step:

```rust
pub struct ContractMatch {
    pub kind: ContractMatchKind,
    pub provider_repo: String,
    pub provider_id: String,     // Route NodeId or KafkaTopic NodeId
    pub consumer_repo: String,
    pub consumer_id: String,     // ExternalEndpoint NodeId or KafkaTopic NodeId
    pub match_key: String,       // URL path pattern or topic name
}

pub enum ContractMatchKind {
    HttpRoute,
    KafkaTopic,
    SpringEvent,
}
```

Algorithm:

**Step 1 — collect provider routes per repo:**
Query each repo's FalkorDB graph for `Route` nodes (kind = "Route") and their `path` property.

```cypher
MATCH (n:Symbol) WHERE n.kind = 'Route' RETURN n.id, n.path, n.httpMethod
```

Build a map: `normalized_path → Vec<(repo_name, route_node_id)>`.

**Step 2 — collect consumer HTTP calls per repo:**
Query each repo's FalkorDB graph for `ExternalEndpoint` nodes.

```cypher
MATCH (n:Symbol) WHERE n.kind = 'ExternalEndpoint' RETURN n.id, n.path, n.httpMethod
```

**Step 3 — match HTTP contracts:**
Normalize both sides: strip leading `/`, replace `{variable}` with `{*}`.
`GET /api/orders/{id}` → `GET /api/orders/{*}`.
Match consumer endpoint → provider route by (method, normalized_path).

**Step 4 — collect Kafka/Spring event contracts:**
Query each repo for `KafkaTopic` nodes with `PUBLISHES_EVENT` / `LISTENS_TO` edges.

```cypher
MATCH (m:Symbol)-[r:PUBLISHES_EVENT|LISTENS_TO]->(t:Symbol)
WHERE t.kind = 'KafkaTopic'
RETURN t.id, type(r), m.id
```

Match: same `KafkaTopic.id` across two repos → publisher in one, listener in the other.

**Step 5 — write output artifact:**
Write `~/.cih/groups/<name>/contracts.jsonl` — one JSON object per `ContractMatch`.
Also write `~/.cih/groups/<name>/sync_at` (RFC-3339 timestamp).

---

## CLI subcommands (`cih-engine/src/main.rs`)

Add a `Group` top-level command:

```
cih-engine group create <name>        # create empty group
cih-engine group add <name> <repo>    # add a registry repo to the group
cih-engine group remove <name> <repo> # remove a repo from the group
cih-engine group list                 # list groups and their member repos
cih-engine group sync <name>          # run contract matching, write contracts.jsonl
  --falkor-url <url>                  # defaults to $FALKOR_URL
```

Implementation in a new `cih-engine/src/group_cmd.rs`:

```rust
pub fn run_group_create(name: &str) -> Result<()>
pub fn run_group_add(name: &str, repo: &str) -> Result<()>
pub fn run_group_remove(name: &str, repo: &str) -> Result<()>
pub fn run_group_list() -> Result<()>
pub fn run_group_sync(name: &str, falkor_url: &str) -> Result<()>
```

`run_group_sync` is async (needs tokio for FalkorDB queries); the others are synchronous
registry I/O.

---

## MCP server changes (`cih-server/src/main.rs`)

Add one new tool:

### `group_contracts({ group })`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
struct GroupContractsArgs {
    /// Group name from ~/.cih/groups.json.
    group: String,
    /// Filter by contract kind: "http", "kafka", "spring-event", or "all" (default).
    #[serde(default)]
    kind: Option<String>,
}
```

Implementation: reads `~/.cih/groups/<group>/contracts.jsonl`, deserializes all
`ContractMatch` records, optionally filters by kind, returns JSON array.

No graph query needed — the artifact is authoritative after `group sync`.

---

## FalkorDB schema additions (`cih-falkor/src/lib.rs`)

Update `ensure_schema()` to also create indexes for the new node kinds:

```rust
let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.kind)").await;  // already useful for filter queries
```

Update `resources.rs` `schema_json()` to include the three new edge kinds and two new node kinds.

---

## Files changed summary

| File | Change |
|------|--------|
| `crates/cih-core/src/lib.rs` | Add `KafkaTopic`, `ExternalEndpoint` to `NodeKind`; `PublishesEvent`, `ListensTo`, `ExternalCall` to `EdgeKind`; add id helpers; re-export group types |
| `crates/cih-core/src/ir.rs` | Add `ContractSite`, `ContractKind` structs; add `contract_sites` field to `ParsedFile` |
| `crates/cih-core/src/group.rs` | New file — `GroupEntry`, `GroupRegistry` |
| `crates/cih-parse/src/java.rs` | Add `detect_contracts()` pass; populate `ParsedFile::contract_sites` |
| `crates/cih-resolve/src/lib.rs` | Add `resolve_contract_edges()`; merge result into `ResolveOutput` |
| `crates/cih-engine/src/main.rs` | Add `Group { cmd: GroupCommand }` top-level subcommand |
| `crates/cih-engine/src/group_cmd.rs` | New file — CLI handlers |
| `crates/cih-engine/src/group.rs` | New file — contract matching logic |
| `crates/cih-server/src/main.rs` | Add `group_contracts` MCP tool |
| `crates/cih-server/src/resources.rs` | Add new node/edge kinds to `schema_json()` |

---

## Implementation order

1. **`cih-core`** — NodeKind/EdgeKind additions + IR types (no logic, just data).  
   Tests: `NodeKind::from_label("KafkaTopic")` round-trips; `ContractSite` serde round-trip.

2. **`cih-core/group.rs`** — GroupRegistry load/save/upsert.  
   Tests: empty-file returns empty, upsert-replaces-not-appends (same pattern as registry tests).

3. **`cih-parse/java.rs`** — `detect_contracts()` pass.  
   Tests: one unit test per pattern (Feign interface, RestTemplate call, KafkaListener,
   KafkaTemplate.send, publishEvent, @EventListener).

4. **`cih-resolve/lib.rs`** — `resolve_contract_edges()`.  
   Tests: a two-file slice with a producer and listener on the same topic emits matching
   `PublishesEvent` + `ListensTo` edges to the same `KafkaTopic` node id.

5. **`cih-engine/group_cmd.rs` + `main.rs`** — CLI subcommands + contract matching.  
   Tests: `group create/add/remove/list` round-trips group registry; matching test with two
   synthetic artifact dirs.

6. **`cih-server/main.rs`** — `group_contracts` tool.  
   Tests: read a temp contracts.jsonl and return correct JSON.

---

## Edge cases and constraints

- **Partial detection** — RestTemplate URL is sometimes dynamically constructed
  (`"/api/" + id`); emit a `ContractSite` with `url_template = null` (field present, value
  absent) so downstream tools can flag unresolved contracts without crashing.
- **Multi-topic `@KafkaListener`** — `topics = {"order-created", "order-updated"}` emits
  two `ContractSite` entries, one per topic.
- **Same repo in multiple groups** — allowed; each `group sync` writes to its own group dir.
- **Cached artifacts** — old `ParsedFile` artifacts (before `contract_sites` field) deserialize
  with an empty `Vec` via `#[serde(default)]`. Re-analyze triggers fresh detection.
- **FalkorDB CYPHER parameter for `IN` lists** — same inline-array pattern used in Phase 19
  (`nodes_in_files`); safe because node ids come from our own graph, not user input.
- **URL template normalization** — only normalize the path component; ignore host/port since
  those differ per environment. Replace `{variable}` / `:variable` with `{*}`. Case-insensitive
  HTTP method comparison.

---

## Verification checklist

- [ ] `cargo build --release -p cih-core -p cih-parse -p cih-resolve -p cih-engine -p cih-server` green
- [ ] `cargo clippy -- -D warnings` green across all crates
- [ ] All existing tests still pass (no regressions)
- [ ] `cih-engine analyze <banking-repo>` after the change shows `KafkaTopic` and `ExternalEndpoint`
      nodes in FalkorDB (`MATCH (n:Symbol) WHERE n.kind IN ['KafkaTopic','ExternalEndpoint'] RETURN n`)
- [ ] `cih-engine group create banking` → `~/.cih/groups.json` created
- [ ] `cih-engine group add banking payment-service` + `cih-engine group add banking order-service`
      → both repos listed
- [ ] `cih-engine group sync banking` → `~/.cih/groups/banking/contracts.jsonl` written with
      at least one matched Kafka or HTTP contract
- [ ] MCP tool `group_contracts({ group: "banking" })` returns the matched contracts
