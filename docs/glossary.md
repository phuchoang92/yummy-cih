# CIH Glossary

This file explains project terms used in the CIH docs and Rust crates. It is written for quick lookup while reading `phase-3.md`, `phase-4.md`, and the code.

## Project

| Term | Definition |
| --- | --- |
| CIH | Code Intelligence Hub. This project indexes source code into a graph so tools can answer questions like "what calls this method?" or "what context surrounds this class?" |
| Engine | The local indexing CLI, implemented by `cih-engine`. It scans repos, selects scope, parses Java, resolves references, writes graph artifacts, and can load them into FalkorDB. |
| MCP | Model Context Protocol. The `cih-server` crate exposes graph queries as MCP tools for agents and clients. |
| Phase | A planned implementation stage. For example, Phase 3 builds scan/parse/load, while Phase 4 resolves references into a call and heritage graph. |
| GitNexus | The source/reference system this project is porting ideas from. Docs often say "port" when a GitNexus algorithm is being rewritten in Rust. |
| Port | Reimplement an existing GitNexus concept in this Rust codebase. |
| Workspace | The Rust workspace in this repo, made of crates such as `cih-core`, `cih-engine`, `cih-parse`, and `cih-resolve`. |

## Main Workflow

| Term | Definition |
| --- | --- |
| Scan | Fast, parse-free repository discovery. It counts Java files, detects modules, reads build files, gathers Spring signals, and writes `.cih/repo-map.json`. |
| Scope | The subset of files/modules selected for parsing and indexing. CIH avoids parsing everything by default. |
| Analyze | The engine command that scans, resolves scope, parses selected files, resolves references, writes graph artifacts, and optionally loads FalkorDB. |
| Resolve | Turn unresolved references like `service.findOwner()` into graph edges like `CALLS` from the caller method to the target method. |
| Load | Send graph artifacts into the configured graph backend. In local development, that backend is FalkorDB. |
| No-load mode | `cih-engine analyze --no-load`. It writes artifacts to disk but skips the graph database load step. Useful for tests and smoke checks. |
| Re-index | Run analysis again after source code or scope changes. Current bulk loading is additive and idempotent, not a full graph replacement. |

## Files And Artifacts

| Term | Definition |
| --- | --- |
| `.cih/` | Generated metadata directory inside the target repo being indexed. |
| `.cih/repo-map.json` | Output of `scan`. Stores repo modules, Java counts, Spring signals, JAR catalog entries, and decompiled dirs. |
| `.cih/scope.json` | Persisted effective scope after CLI flags or `cih.scope.toml` are resolved. Makes future runs reproducible. |
| `cih.scope.toml` | Optional user-authored scope config. It can select modules/includes/excludes and decompiled behavior. |
| `.cih/parsed/<version>/parsed-files.jsonl` | Persisted parse IR. Phase 4 consumes this to resolve references. |
| `.cih/artifacts/<version>/nodes.jsonl` | JSON Lines file containing graph nodes emitted by analysis. |
| `.cih/artifacts/<version>/edges.jsonl` | JSON Lines file containing graph edges emitted by analysis. |
| GraphArtifacts | The handle for `nodes.jsonl`, `edges.jsonl`, and their version. Backends read this format for loading. |
| Version | A content hash used to identify one analysis output. Current versioning includes nodes, edges, and parsed IR. |
| JSONL | JSON Lines. Each line is one JSON object. Used for graph artifacts and parsed files. |
| Generated artifact | A file CIH creates from source input, such as repo maps, scope files, parsed IR, nodes, and edges. |

## Rust Crates

| Term | Definition |
| --- | --- |
| `cih-core` | Shared domain types: node ids, edge kinds, repo map structs, parse IR structs, and graph artifact helpers. |
| `cih-engine` | CLI orchestration crate. Implements `scan` and `analyze`. |
| `cih-lang` | Language support crate. Currently provides Java tree-sitter parser/query helpers. |
| `cih-parse` | Turns selected Java files into structure graph nodes/edges plus unresolved parse IR. |
| `cih-resolve` | Builds resolution indexes and emits Phase 4 reference-resolution edges such as `CALLS`, `ACCESSES`, `IMPORTS`, `EXTENDS`, and `IMPLEMENTS`. |
| `cih-jar` | Extracts signature-only API nodes from JAR files. |
| `cih-graph-store` | Defines the graph-store traits used by the server and engine. |
| `cih-falkor` | FalkorDB implementation of the graph-store and bulk-load behavior. |
| `cih-server` | MCP server that exposes graph queries like `context`, `impact`, and `call_chain`. |

## Repository Scan Terms

| Term | Definition |
| --- | --- |
| RepoMap | The scan result for a repository. Includes root path, build system, modules, JARs, Java count, LOC, and decompiled dirs. |
| ModuleInfo | Metadata for one module/build unit: name, path, build file, Java file count, LOC, packages, Spring signals, and sibling dependencies. |
| BuildSystem | Detected build system enum: Maven, Gradle, or None. |
| Maven | Java build system using `pom.xml`. |
| Gradle | Java build system using files like `settings.gradle`, `settings.gradle.kts`, `build.gradle`, or `build.gradle.kts`. |
| Module | A build unit or fallback folder that CIH can present as a scope choice. |
| Sibling dependency | A module-to-module dependency inside the same repo, inferred from Maven or Gradle metadata. |
| Package declaration | Java `package com.example;`. Used for FQCN construction and module package summaries. |
| LOC | Lines of code. In scan, CIH estimates LOC by newline count without parsing. |
| SpringSignal | Cheap file-level counts for Spring annotations such as controllers, services, repositories, entities, configs, and route mappings. |
| Hardcoded ignores | Built-in ignore names/extensions used by scan, alongside `.gitignore` and `.cihignore`. |
| `.cihignore` | Optional ignore file specific to CIH scanning. |
| `.workspace-dependencies/` | Directory commonly used for decompiled or external dependency sources. CIH detects it and excludes it by default unless requested. |
| Deferred module | A module CIH recommends not indexing first, usually because it is generated, decompiled, vendored, or third-party-looking. |

## Java Parsing Terms

| Term | Definition |
| --- | --- |
| Parse | Read selected Java source files with tree-sitter and extract declarations, references, imports, type bindings, and route data. |
| tree-sitter | Incremental parsing library used to produce Java syntax trees. |
| tree-sitter-java | Java grammar used by tree-sitter. |
| AST | Abstract syntax tree. Tree-sitter's parsed structure of source code. |
| Query | A tree-sitter pattern file that finds syntax nodes of interest. In this repo, `query.scm` captures declarations, imports, references, and bindings. |
| Capture | A named match from a tree-sitter query, such as `@declaration.name`, `@reference.receiver`, or `@type-binding.type`. |
| Scope query | The Java tree-sitter query used by `cih-lang` to identify scopes, declarations, imports, references, and type bindings. |
| IR | Intermediate representation. CIH stores parsed source facts in structs before final graph resolution. |
| ParsedFile | IR for one Java source file: file path, package, definitions, imports, reference sites, and type bindings. |
| SymbolDef | IR entry for a declared symbol: class, interface, method, constructor, or field. |
| RawImport | IR entry for an import statement before CIH resolves it to a graph node. |
| ReferenceSite | IR entry for a usage site, such as a method call, constructor call, field read/write, type reference, extends, or implements. |
| TypeBinding | IR entry linking a variable/field/parameter name to a raw type or inferred source. Used to resolve receivers. |
| BindingKind | TypeBinding category: Param, Local, Field, CallResult, Alias, Pattern, or Return. |
| Param binding | A binding from a method or constructor parameter, such as `OwnerService service`. |
| Local binding | A binding from a local variable with an explicit or inferred type. |
| Field binding | A binding from a class field declaration. |
| CallResult binding | A `var` inference binding where CIH follows a method return type, such as `var owner = service.findOwner()`. |
| Alias binding | A `var` inference binding where one variable aliases another, such as `var x = service`. |
| Pattern binding | A binding from Java pattern matching, such as `if (x instanceof Owner owner)`. |
| Return binding | A method return-type binding. |
| Raw type | A type name as written in source, often unresolved, such as `OwnerService`, `List<Owner>`, or `com.example.Owner`. |
| FQCN | Fully qualified class name. Example: `com.example.OwnerService`. |
| Nested class FQCN | FQCN for an inner class. CIH uses dot notation such as `com.example.Outer.Inner`. |
| Arity | Number of arguments or parameters. Method ids currently use name plus arity, such as `findOwner/1`. |
| Receiver | The object or type before a member access. In `service.findOwner()`, `service` is the receiver. |
| Free call | A method call without an explicit receiver, such as `helper()`. |
| Constructor call | A `new Type(...)` expression. Resolved to a constructor node. |
| Field read | A field access used as a value, such as `owner.name`. |
| Field write | A field access assigned to, such as `owner.name = value`. |
| Heritage reference | An unresolved `extends` or `implements` relationship captured during parse. |

## Graph Model

| Term | Definition |
| --- | --- |
| Graph | A set of nodes and edges representing code structure and relationships. |
| Node | A code entity such as a file, folder, class, method, field, constructor, route, or process. |
| Edge | A relationship between two nodes, such as `CALLS`, `CONTAINS`, or `IMPLEMENTS`. |
| NodeId | Stable string id for a node, such as `Class:com.example.OwnerService` or `Method:com.example.OwnerService#findOwner/1`. |
| NodeKind | Type of node: File, Folder, Class, Interface, Enum, Record, Annotation, Method, Constructor, Field, Route, and others. |
| EdgeKind | Type of relationship. Serialized to Cypher labels like `CALLS`, `HAS_METHOD`, and `EXTENDS`. |
| Qualified name | Human-readable full symbol name stored on many nodes. Usually similar to the id without the node-kind prefix. |
| Range | Source location of a node/reference: start line/column and end line/column. |
| Props | JSON metadata attached to nodes, such as Spring stereotype, route path, or JAR flags. |
| Confidence | A score on an edge indicating how strong the resolver evidence was. Exact receiver type matches use higher confidence than best-effort fallbacks. |
| Reason | Short string explaining why an edge was emitted, such as `receiver-bound`, `heritage`, or `import`. |
| Idempotent | Safe to run repeatedly with the same input. Re-loading the same graph artifacts should not duplicate nodes/edges. |
| MERGE | Cypher operation used by FalkorDB loader to create a node/edge if missing or match it if already present. |

## Edge Labels

| Term | Definition |
| --- | --- |
| `CONTAINS` | File/folder/type containment relationship. |
| `HAS_METHOD` | Type to method/constructor relationship. |
| `HAS_FIELD` | Type to field relationship. |
| `CALLS` | A method/constructor calls another method or constructor. |
| `ACCESSES` | A method reads or writes a field. |
| `USES` | A code entity references a type or symbol. |
| `IMPORTS` | A file imports a resolved type. |
| `EXTENDS` | A class or interface extends another type. |
| `IMPLEMENTS` | A class implements an interface. |
| `METHOD_OVERRIDES` | A method overrides a superclass method. Planned in Phase 4.3. |
| `METHOD_IMPLEMENTS` | A class method implements an interface method. Planned in Phase 4.3. |
| `HANDLES_ROUTE` | A method handles a Spring route endpoint. |
| `MEMBER_OF` | Generic membership edge type available in core, not the main Java structure edge today. |
| `STEP_IN_PROCESS` | Process/workflow relationship type reserved for process graph modeling. |

## Resolution Terms

| Term | Definition |
| --- | --- |
| ResolveIndex | Cross-file index built from ParsedFile IR. It maps types, methods, fields, imports, bindings, and heritage relationships for resolver passes. |
| Def index | Lookup table from FQCN/member keys to SymbolDef entries. |
| Type registry | Lookup table from simple type names to possible FQCNs. |
| Import table | Per-file imports used to resolve raw type names. |
| Scope binding map | Lookup table from callable scope to receiver name/type bindings. |
| Lookup cascade | Ordered fallback strategy for finding a graph id: exact owner/name/arity first, then broader matches. |
| Receiver-bound call | A call with a receiver, such as `service.findOwner()`. CIH resolves the receiver type first, then finds the method on that type. |
| Free-call fallback | Resolver pass for bare calls like `helper()` that searches the enclosing class and inherited members. |
| Remaining refs pass | Resolver pass that drains unhandled references such as field reads/writes, constructors, and type refs. |
| Handled set | Per-reference marker so one reference site does not emit duplicate edges in later passes. |
| Drop unresolved | If a target cannot be resolved, CIH does not emit a fake edge. It increments a skipped/unresolved counter. |
| Unresolved external FQCN | A qualified type outside the parsed scope. These names are useful for demand-driven JAR API extraction. |
| Out-of-scope boundary | A call or type reference that points outside the selected scope. It may remain unresolved until JAR/API or decompiled data is available. |
| Static type receiver | A receiver that names a class instead of an object instance, such as `OwnerService.create()`. |
| `this` receiver | Receiver meaning the current class instance. |
| `super` receiver | Receiver meaning the superclass part of the current instance. |
| Compound receiver | Receiver expression with chains or calls, such as `factory.service().findOwner()`. CIH handles common cases conservatively. |
| Same-package resolution | If a raw type is not imported, CIH checks whether it exists in the same Java package. |
| Wildcard import | Import like `com.example.*`. CIH can use it when a matching type is known. |
| `java.lang.*` | Default Java imports. Mentioned in plans as a raw-type resolution fallback. |

## MRO And Inheritance

| Term | Definition |
| --- | --- |
| MRO | Method resolution order. The order CIH searches a class, superclass, and interfaces to find inherited methods. |
| C3 linearization | Algorithm for computing a consistent MRO across superclass and interface chains. Planned for Phase 4.3. |
| Heritage adjacency | Graph/index structure mapping a type to its superclass/interfaces and reverse implementor relationships. |
| Supertype | A superclass or implemented/extended interface. |
| Implementor | A class that implements an interface, or a subtype connected to a parent type. |
| Override | A method with the same name/arity as a superclass method. |
| Interface implementation | A class method satisfying an interface method. |
| DI-aware resolution | Dependency-injection-aware binding from interfaces to concrete Spring beans. The docs mark this as a later phase, not current Phase 4. |

## Spring Terms

| Term | Definition |
| --- | --- |
| Spring | Java framework commonly used for web apps and services. CIH detects some Spring annotations. |
| Stereotype | Spring role tag on a class, such as controller, service, repository, component, config, or entity. |
| Controller | Spring class that handles web requests, often annotated with `@Controller` or `@RestController`. |
| Service | Spring business-logic class, often annotated with `@Service`. |
| Repository | Spring persistence/data-access class, often annotated with `@Repository`. |
| Component | Generic Spring-managed class, often annotated with `@Component`. |
| Configuration | Spring config class, often annotated with `@Configuration`. |
| Entity | Persistence model class, often annotated with `@Entity`. |
| Route | HTTP endpoint represented as a graph node. Example id: `Route:GET /owners/{id}`. |
| Mapping annotation | Spring annotation that defines a route, such as `@GetMapping`, `@PostMapping`, or `@RequestMapping`. |
| Class route prefix | Path prefix from a class-level `@RequestMapping`. |
| Method route path | Path from a method-level mapping annotation. Combined with class prefix to form a route path. |
| `HANDLES_ROUTE` | Edge from a handler method to its route node. |

## JAR And Dependency Terms

| Term | Definition |
| --- | --- |
| JAR | Java archive file. It is a ZIP containing compiled `.class` files and metadata. |
| `.class` file | JVM bytecode file compiled from Java/Kotlin/etc. |
| JarInfo | RepoMap entry describing a discovered JAR path, group id, artifact, own/third-party flag, and class count. |
| Group id | Maven/Gradle dependency group, such as `org.springframework`. |
| Artifact id | Maven/Gradle dependency artifact, such as `spring-web`. |
| Own lib | A dependency belonging to your organization, usually detected by configured group id prefixes. |
| Third-party lib | External dependency such as Spring, Apache, or Guava. Usually not indexed deeply by default. |
| Source-less lib | Dependency available as bytecode but not source code in the repo. |
| API surface | Signature-only graph nodes extracted from a JAR: classes, methods, constructors, and fields, without method bodies. |
| Signature-only | Only public-ish declarations and descriptors are modeled. CIH does not parse bytecode bodies for calls. |
| Descriptor | JVM method/field type encoding, used to derive parameter count and return types. |
| `cafebabe` | Rust crate used by `cih-jar` to parse `.class` files. |
| Demand-driven extraction | Extract only JAR classes referenced by unresolved external FQCNs, instead of indexing whole JARs. |
| Full decompile | Convert bytecode back into source-like Java, then parse it. More expensive and noisier than API surface extraction. |
| Fernflower | Java decompiler mentioned in the docs for full decompile mode. |
| Synthetic member | Compiler-generated class/method/field. Usually skipped to reduce graph noise. |

## Graph Backend And Server Terms

| Term | Definition |
| --- | --- |
| GraphStore | Trait abstracting graph database operations such as context, impact, call chain, and bulk loading. |
| BulkLoader | Trait/path for loading GraphArtifacts into a backend. |
| FalkorDB | Local openCypher-compatible graph database used for development. It speaks the Redis protocol. |
| FalkorStore | FalkorDB adapter implementing GraphStore behavior. |
| Neptune | AWS graph database mentioned as a later production backend target. |
| Postgres fallback | Planned lower-cost backend option mentioned in server config comments. |
| openCypher | Query language style used by graph backends like FalkorDB and Neptune. |
| Cypher label | Relationship/node label string used in graph queries, such as `CALLS`. |
| `_CihMeta` | Metadata node used by Falkor loader to record the current graph version. |
| Context query | MCP/server query that returns a symbol with nearby callers, callees, and related nodes. |
| Impact query | MCP/server query that walks `CALLS` edges to find upstream/downstream callers or callees. |
| Call chain | Path of `CALLS` edges between two methods. |
| Upstream | Direction toward callers of a node. |
| Downstream | Direction toward callees/dependencies of a node. |
| Streamable HTTP | MCP transport used by `cih-server` at `/mcp`. |
| `rmcp` | Rust crate used to implement the MCP server. |
| `axum` | Rust web framework used by the MCP server. |

## Common Examples

| Example | Meaning |
| --- | --- |
| `Class:com.example.OwnerService` | NodeId for a Java class. |
| `Method:com.example.OwnerService#findOwner/1` | NodeId for method `findOwner` on `OwnerService` with one parameter. |
| `Constructor:com.example.Owner#<init>/0` | NodeId for a zero-argument constructor. |
| `Field:com.example.OwnerController#service` | NodeId for field `service` on `OwnerController`. |
| `Route:GET /owners/{id}` | NodeId for a Spring HTTP route. |
| `service.findOwner(id)` | Receiver-bound call. Receiver `service` must be typed before the target method can be found. |
| `helper()` | Free call. CIH searches the current class/inheritance chain. |
| `class A extends B` | Heritage reference that should resolve to an `EXTENDS` edge. |
| `class A implements I` | Heritage reference that should resolve to an `IMPLEMENTS` edge. |
| `import com.example.Owner` | RawImport that can become an `IMPORTS` edge from file to type. |
