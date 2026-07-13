# Architecture — parser assumptions & known limitations

CIH builds its graph from tree-sitter parses plus a set of framework/SQL
heuristics. The heuristics are deliberately conservative: when a fact can't be
established statically, CIH prefers to emit nothing (or mark it uncertain) rather
than guess. This page documents the assumptions so that answers built on the
graph — impact, route_map, taint_paths — can carry the right caveats.

For the full pipeline overview see `README.md`. This page is only the "where the
graph can be incomplete" list.

## HTTP routes (Java / Spring, `cih-lang/src/java/parse/framework.rs`)

- **Only the five `@*Mapping` shortcuts are treated as verbs**: `@GetMapping`,
  `@PostMapping`, `@PutMapping`, `@DeleteMapping`, `@PatchMapping`. A method
  annotated only with `@RequestMapping(method = RequestMethod.POST)` produces **no
  Route node**. `@RequestMapping` at the *class* level is still honored as the path
  prefix. (Pinned by `method_level_request_mapping_emits_no_route` in
  `cih-lang/tests/java.rs`.)
- **Path composition** trims and collapses slashes: class prefix `"/owners/"` +
  method `"/{id}"` → `/owners/{id}`; a bare `@GetMapping` under a class prefix
  resolves to the prefix alone. Multiple paths in one annotation
  (`@GetMapping({"/a","/b"})`) emit one Route each.
- **Feign clients**: `@FeignClient` URL/path is read from the annotation literally;
  dynamic URL interpolation (`${...}`, concatenation) is not followed.

## Kotlin routes & contract sites (`cih-lang/src/kotlin/framework.rs`)

A 1:1 port of the Java Spring/Feign/Kafka detector (string normalization is
shared via `cih-lang/src/contracts_common.rs`; tree walking is per-language).
Same verb-shortcut and path-composition rules as Java above, plus:

- **Receiver typing is a light per-class env**: a call like
  `restTemplate.getForObject(...)` only counts as an outbound HTTP contract when
  the receiver's simple name matches a *typed primary-constructor parameter* or
  a *typed property* of the enclosing class (`class C(private val rest:
  RestTemplate)`). No inheritance, no local variables, no `this.`-qualified
  chains — an untyped or externally-provided receiver emits nothing.
- **Literal strings only (Phase A)**: an interpolated URL (`"$base/items/$id"`)
  still emits the HTTP contract site but with no `url_template`; an interpolated
  topic in `kafkaTemplate.send(...)` emits nothing. Neither can participate in
  cross-repo matching until dynamic-URL folding lands.
- **Top-level `fun`s** get contract sites (as `Function:` callables); calls in
  `init {}` blocks and property initializers have no callable context and are
  skipped, mirroring Java.

## Go routes & outbound HTTP (`cih-lang/src/go/framework.rs`)

Go has no annotations, so detection is **import-gated per library**, then
shape-gated:

- **Route registrations**: gin/echo uppercase verbs (`r.GET("/path", h)`) only
  when gin/echo is imported; chi capitalized verbs (`r.Get(...)`) only when chi
  is imported; `Handle`/`HandleFunc` only when net/http or gorilla/mux is
  imported. The first argument must be a string starting with `/`, or a
  Go 1.22 method pattern (`"GET /orders/{id}"`, which splits into its verb).
  Plain `HandleFunc("/path", h)` routes register with method **`ANY`**;
  `match_contracts` lets consumers with concrete verbs match `ANY` providers.
- **Route id convention** (deliberate decision): `Route:go:{METHOD}:{path}` —
  the Express-style prefixed format, not Java/Spring's `Route:{METHOD} {path}`.
  Both formats already coexist; nothing parses route ids (props are the
  contract), and the CXF stitcher's id rewriting is Java-specific.
- **`HandlesRoute` only for plain identifier handlers** naming a same-file
  function. Closures, method values, and cross-file handlers produce a Route
  node with `handler: null` and no edge.
- **Outbound**: `http.Get|Post|Head|PostForm` and
  `http.NewRequest(WithContext)` with a literal method argument; `client.Do`
  is skipped (the URL lives on the request). URLs reuse the dynamic-parts
  model; `fmt.Sprintf` format strings split on `%` directives (`Lit` chunks +
  `Dynamic` per directive).
- Historical note: tree-sitter-go is pinned to the 0.23 line — 0.25 ships
  language ABI 15, which the tree-sitter 0.24 runtime rejects, making every
  Go parse panic at parser construction.

## TypeScript / Python outbound HTTP (`typescript/parse.rs`, `python/parse.rs`)

The TypeScript provider also handles **JavaScript** (`.js`, `.jsx`, `.mjs`,
`.cjs`): JS is a syntactic superset-compatible subset of the TypeScript grammar,
so `.js`/`.mjs`/`.cjs` parse cleanly and `.jsx` gets the same error-tolerant JSX
handling as `.tsx` (no TSX grammar switch). Functions, classes, Express routes,
and outbound `fetch`/HTTP calls are extracted identically to `.ts`. Nodes from
JS files carry `language = "typescript"` (one provider for the whole family).

### Backend route frameworks (`RouteSource`)

Beyond NestJS decorators and Express, the parser emits `Route` nodes for:

- **Fastify** — `fastify.get|post|…(path, …)` and `fastify.route({ method, url })`
  (method may be an array). Import-gated on `fastify`.
- **Koa** (`@koa/router`) — `router.get|post|…(path, …)`, import-gated so it does
  not collide with Express's `router` receiver.
- **hapi** — `server.route({ method, path })` (`@hapi/hapi`).
- **Next.js** (file-based, keyed off the file path): `pages/api/**` default-export
  handlers → one `ALL`-method route (path `/api/…`, `[id]` → `:id`); App Router
  `app/**/route.ts` → one route per exported `GET/POST/…` (path from the dir).
- **Remix** — `app/routes/**` modules exporting `loader` (→ GET) / `action`
  (→ POST); path derived from the flat-route filename (`$id` → `:id`).

Receiver-name disambiguation (`app`/`router`) is import-gated so **Express output
is unchanged** when neither Fastify nor Koa is imported. GraphQL resolvers
(`@Query`/`@Mutation`/`@Subscription`, type-graphql / NestJS) and tRPC procedures
(`.query`/`.mutation`, `@trpc/server`-gated) emit `ContractKind::Custom("graphql"|"trpc")`
sites — visible/queryable, not auto-resolved to edges.

Deliberately tight recognizers — false positives poison cross-repo matching:

- **TypeScript**: bare `fetch|$fetch|ofetch(url[, {method}])` (default GET;
  `method` from a literal options object), import-gated `got`/`ky`,
  `axios.get|post|…(url, …)` and `axios(url, {method})`. Instance & member
  clients are now in scope: `axios.create({ baseURL })` instances (a literal
  `baseURL` folds into the call path), Angular `HttpClient` / Nest `HttpService`
  (`(this.)http|httpClient|httpService.<verb>`, import-gated on
  `@angular/common/http` / `@nestjs/axios`), and import-gated `superagent` /
  `undici.request`. Node's core `http.get` stays out (no client import).
- **Python**: module-receiver `requests.*` / `httpx.*` verb calls plus
  `requests.request("POST", url)`. Session/client instances (`session.get`)
  are out of scope v1.
- **URLs reuse the dynamic-parts model** (below): template-string / f-string
  interpolations become `Dynamic` parts and fold to `{*}`; constant references
  currently never resolve for TS/Py (no constant index) and also degrade to
  `{*}`.
- **`in_callable` fallback**: calls inside tracked functions attribute to the
  function; module-level calls (and TS arrow functions, untracked v1) fall back
  to the **file id**. This degrades the *first leg* of cross-repo tracing
  (entry resolution), not just display granularity — pinned by test.

### JS/TS DB & ORM access (`DbTable` / `DbQuery`)

The parser emits `DbTable`/`DbQuery` nodes + `ExecutesQuery` (caller→query) and
`Reads/WritesTable` (query→table) edges directly — the same node ids
(`DbTable:<UPPER>`, `DbQuery:<file>:<line>:<col>`) and edge kinds Java JPA/SQL
uses, so `taint_paths(category="sql")` and DB tooling work uniformly. Detectors:

- **Prisma** — `prisma.<model>.<op>()` (op = `findMany`/`create`/…); receiver
  `prisma` (or `db` when `@prisma/client` is imported). Model name → table.
- **Mongoose / Sequelize** — a pre-pass records model vars
  (`const User = mongoose.model('User', …)`, `sequelize.define('t', …)`) → table;
  `User.find()/create()/…` then reads/writes it.
- **Knex** — the receiver chain (`knex('t').where(…).select()`) is unwound to the
  root `knex('t')` call for the table.
- **TypeORM / sequelize-typescript** — `@Entity('t')` / `@Table('t')` class
  decorators → `DbTable` (arg overrides class name).

Op classification (`db_op_kind`) gates which method names count as data access;
the table must still resolve to a model/prisma/knex receiver, so plain
`array.find(…)` never emits (pinned by test).

### JS/TS component stereotypes + DI

- **Stereotypes** (`stereotype` prop on `Class`/`Function` nodes, feeding
  `feature_map`/`communities`/`architecture_hint`): NestJS
  (`nestjs_controller`/`nestjs_injectable`), Angular (`angular_component`/
  `_directive`/`_pipe`/`_module`/`_injectable` — `@Injectable` disambiguated from
  Nest by an `@angular/core` import), `graphql_resolver`, and React
  (`react_component`/`react_hook`). React function components are matched by
  PascalCase name + a `react` import (the TypeScript grammar can't confirm JSX);
  class components by `extends …Component`. Both `function X()` declarations and
  the dominant `const X = () => …` / function-expression arrow-const forms are
  emitted as `Function` nodes (the latter only when they name a component/hook),
  and calls inside them attribute to the component rather than the file.
- **DI**: a provider class's `constructor(private x: Dep)` param types are emitted
  as `TypeRef` reference sites from the class, which the resolver turns into
  `Uses` edges — the JS analog of Spring constructor injection.

### JS/TS messaging / realtime

Emits `EventPublish`/`EventListen` `ContractSite`s (topic + `messaging_framework`);
the resolver folds these into `KafkaTopic` nodes + `PublishesEvent`/`ListensTo`
edges — the same path Java Kafka/Spring events use, so single- and cross-service
event flows are visible. All detectors are **import-gated** (the method names
`emit`/`on`/`send`/`add` are too common otherwise):

- **socket.io** — `socket.emit('e')` → publish, `socket.on('e')` → listen.
- **kafkajs** — `producer.send({ topic })` → publish, `consumer.subscribe({ topic })` → listen.
- **Bull/BullMQ** — a pre-pass records `new Queue('n')` vars; `queue.add(…)` → publish to `n`.
- **amqplib** — `channel.sendToQueue/publish` → publish, `channel.consume` → listen.
- **NestJS** — `@MessagePattern`/`@EventPattern`/`@SubscribeMessage` method decorators → listen.

For cross-repo grouping these JS frameworks map to the topic-keyed
`ContractMatchKind::KafkaTopic` bucket (matched by topic string).

**Limitation — dynamic topics.** Only a **literal** topic is captured (the first
string arg, or the `{ topic }` config value). Topics built at runtime — a channel
key variable (`io.emit(channelKey)`), a template (`` `chat:${id}` ``), or a
parameter (`socket.on(addKey)`) — are skipped, since there is no concrete topic to
key a node/match on. This mostly affects **socket.io** apps, which lean on dynamic
per-channel rooms; kafkajs/NestJS/Bull typically use literal or config-declared
topics and are captured well. (Observed on a real Discord clone: only the literal
`connect`/`disconnect` lifecycle events resolved; the domain events used
parameter-supplied keys.)

## Dynamic-URL folding (`ContractSite.url_parts`, `cih-resolve/src/contracts.rs`)

Outbound URLs/topics that are not plain literals are captured as structured
parts — `Lit` / `ConstRef` / `Dynamic` — and folded at resolve time through the
cross-file constant index (Java `static final String`; Kotlin
companion-object/`object` `val`s with literal initializers):

- **Unresolved parts degrade to `{*}`, never to wrong matches.** A `ConstRef`
  the index can't resolve, and every `Dynamic` part (method call, `${expr}`
  interpolation, arithmetic), wildcard their *entire* path segment — the fold
  of `BASE + "/" + id` is `/api/orders/{*}`, never `v{*}`-style partial
  segments. Fully-literal URLs are untouched (`url_template`, no parts).
- **Matching stays normalized-string equality.** `{*}` pairs only with provider
  path variables (`{id}`/`:id` normalize to `{*}` in
  `normalize_contract_path`); segment-wise true wildcard matching is an
  explicit non-goal. Endpoints that fold to `/` or all-`{*}` are dropped as
  uninformative; folded endpoints carry `dynamic: true` and a confidence
  discount (0.65 vs 0.75).
- **Topics must fold to a full literal** — topic matching is exact-string, so a
  partially-dynamic topic emits nothing.
- **Kotlin constant scope is companion/`object` only.** Top-level `const val`
  has no declaring class, so bare-name references to it don't resolve (they
  degrade to `{*}`). Kafka `send` reads its topic from positional arg 0 only —
  a literal payload is never mistaken for the topic.

## CXF / OSGi route stitching (`cih-resolve/src/lang/java/cxf.rs`)

- **Base paths come from XML, per bundle.** Each JAX-RS route gets
  `servlet_prefix + <jaxrs:server address> + method_path`. The servlet prefix is
  resolved per server file: the OSGi whiteboard pattern
  (`osgi.http.whiteboard.servlet.pattern`) whose declaring XML shares the most
  leading directory components wins; a lone repo-wide pattern applies everywhere;
  multiple unrelated patterns are never guessed across bundles
  (`servlet_prefix_source: "none"`; `cxf_base_path` in `cih.toml` overrides all).
- **Interface-annotated endpoints stitch via heritage.** When the `jaxrs:server`
  bean is an impl class but the route handler is the annotated interface (JAX-RS
  annotation inheritance, common on OSGi platforms with `-api` bundles), the
  target matches any interface the bean class transitively implements. Exact
  impl-class matches always take priority.
- **One route per server address.** A handler matched by several servers with
  distinct addresses (secured `/v1` + non-secured `/ns/v1` impls of one
  interface) yields one Route node per resulting path — the first rewrites in
  place, further addresses are cloned with duplicated incoming edges. Route
  counts on such platforms intentionally reflect every real URL.
- **Spring-DM OSGi wiring is DI input.** `META-INF/spring/*.xml` files (any
  name) are DI-XML candidates; `<osgi:reference interface=…>` produces the same
  interface→implementor `CALLS` edges as Blueprint `<reference>` (reason string
  `di-xml-blueprint-reference` is kept for compatibility and covers both).

## Script-language URL constants (`cih-lang` ts/py parse, `java/constant_resolver.rs`)

- **What resolves**: `${IDENT}` / f-string `{IDENT}` interpolations become
  `ConstRef`s only for `SCREAMING_SNAKE` identifiers — params, locals, and
  property chains stay `Dynamic` (`{*}`), so they can never mis-resolve.
- **Constant sources**: module-level `const X = 'lit'` (TS) / `X = "lit"` (Py)
  only, plus env-override defaults: `x ?? 'lit'`, `x || 'lit'`, `x or "lit"`,
  `os.environ.get(k, "lit")`, `os.getenv(k, "lit")` emit the literal default
  with `env_default` provenance — endpoints folded from one carry
  `base_source: "env_default"` (the runtime value may differ; the folded path
  reflects the code default).
- **Cross-file resolution order** (script-language sites only; Java/Kotlin
  scoping is unchanged and structurally ungated): same-module owner →
  import-scoped (the site's file must import the constant's module) →
  repo-wide unique name (exactly one candidate; 2+ → `{*}`, never a guess).

## Same-repo HTTP wrappers (TypeScript, `cih-lang` ts parse + `cih-resolve/src/contracts.rs`)

- **Detection** (parse time): a module-scope function/arrow whose FIRST param
  is a plain identifier and whose body calls fetch/axios with a URL of shape
  `<Lit/ConstRef prefix…><param>` — the param must be the FINAL piece; one
  level of `const url = …` same-body indirection is followed; closures,
  destructured params, mid-URL params, and ambiguous locals all bail.
- **Call sites**: calls to plain identifiers with a URL-ish first arg (leading
  `/`) become PROVISIONAL sites; at resolve they join the wrapper index
  (same module → import-scoped → repo-unique name; ambiguity or no match →
  the site silently vanishes — no fabricated endpoints).
- **Two-context fold**: the wrapper's prefix constants resolve in the
  wrapper's own module; the caller's suffix in the caller's context. Endpoints
  carry `via_wrapper: "<module>#<name>"` (plus `base_source`/`dynamic` as
  usual).
- **Python analog**: module-scope `def`s (decorated included) whose first
  param is a plain identifier and whose body calls `requests.*`/`httpx.*`
  (incl. `requests.request("POST", url)` literal-verb form), with one level of
  `url = …` assignment indirection. Python wrappers hard-code their verb —
  recorded as `fixed_method`, overriding the call site's placeholder at join.
  Python imports are recorded as DOTTED modules (`from a.b import x` → `a.b`;
  relative imports normalize against the file's package), which also powers
  cross-file constant resolution via imports.
- **Module-attribute callees**: `import services.api_client as api;
  api.api_get(...)`, the full dotted receiver
  (`services.api_client.api_get(...)`), a plain import's last segment
  (python), and TS namespace imports (`import * as api from './apiClient'`)
  all join. Parse-side emission is gated on a known import binding in the
  same file (arbitrary `obj.method(url)` calls never emit); dotted callees
  resolve import-scoped ONLY — the receiver pins the module, no unique-name
  fallback, miss → drop.
- **v1 limits**: barrel re-exports (bare-name callees rescued only when
  repo-unique), TS default imports and tsconfig path aliases, `new URL()`
  construction, axios.create / requests.Session instances, options objects
  not at arg 1, python `from x import y as z` name aliases, function-local
  imports appearing after the call site, and method-param pass-through
  wrappers (`def call(method, path)`) are out of scope.

## SQL / DB access (`cih-parse/src/sql.rs`, `cih-resolve/src/db_access.rs`)

- **Table extraction is textual** over the SQL string: it handles SELECT/INSERT/
  UPDATE/DELETE/MERGE, JOINs, comma-joins, sub-queries (including nested), UNION,
  schema-qualified names, and Oracle hint/line comments. `DUAL` is ignored.
  `INSERT ... SELECT` records the target as a write and the source as a read.
- **DB-constant resolution is same-file / same-class only.** A SQL string assembled
  from constants defined in another class is not resolved to its tables.
- **Dynamic SQL is not table-resolved.** When a query is built at runtime from
  non-literal parts, the DbQuery node is marked `dynamic = true` and **no table
  edges** are emitted. Taint analysis still treats such dynamic execution as a
  potential `sql` sink — absence of a table edge is not absence of risk.

## Call graph (`cih-resolve`)

- Calls are resolved by receiver type + import/scope binding. **Reflection,
  runtime dynamic dispatch through framework proxies, and calls through
  string-named beans can be missed.** Interface calls resolve to declared
  implementors found in the indexed scope; implementors outside the indexed
  modules are not linked.

## Parse cache (`cih-engine/src/file_cache.rs`)

- **Layout**: `.cih/parse-cache/v<N>/<blake3-16-of-file-bytes>.json`, one cached
  `ParsedUnit` per source-file content hash, scoped by
  `cih_lang::PARSE_CACHE_SCHEMA`.
- **Invalidation** = file-bytes hash × schema version. A schema bump makes every
  older entry invisible and prunes it on the next analyze (legacy pre-versioning
  flat files included); the analyze no-op gate's config fingerprint also folds
  the schema in, so a bump forces one full re-resolve per repo.
- **Bump rule**: any change to parser/extractor output requires a
  `PARSE_CACHE_SCHEMA` bump — enforced by the `parse_schema_guard` golden test,
  which fails until the schema and its paired corpus hash are updated together.

## Performance notes

- **Symbol-ID interning: measured, not needed (2026-07).** NodeId-keyed maps in
  the taint pipeline use `FxHashMap` (rustc-hash). A full 4-phase `taint` run on
  Fineract (~46k nodes) completes in ~0.77s wall time, so interning NodeIds to
  `u32` symbols in the PDG/dataflow hot path was evaluated and rejected — the
  string-keyed maps are not a bottleneck at real-repo scale. Re-evaluate only if
  a profile of a much larger repo shows hashing in the hot path.

## Implications for agents

- A clean `taint_paths` result (or an empty `route_map` prefix) means "nothing
  found under these heuristics," **not** a proof of absence. Security and
  completeness summaries should say so.
- If a codebase relies heavily on `@RequestMapping(method=...)`, reflection, or
  cross-class dynamic SQL, expect the graph to under-report those specific edges.
  Custom sinks/sanitizers can be added via `cih.taint.toml` (see
  `docs/agent-workflows/security.md`).
