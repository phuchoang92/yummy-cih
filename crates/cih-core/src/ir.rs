//! Per-file parse IR (Phase 3 output). The structural parts are emitted directly
//! as graph `Node`/`Edge`; `imports` + `reference_sites` are collected here
//! UNRESOLVED and consumed by Phase 4 (scope resolution) to emit
//! `CALLS`/`EXTENDS`/`ACCESSES`/… edges.

use crate::{NodeId, NodeKind, Range};
use serde::{Deserialize, Serialize};

/// Everything the parser extracts from one source file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFile {
    /// Repo-relative path of the source file.
    pub file: String,
    /// Language identifier: `"java"`, `"typescript"`, `"python"`, etc.
    #[serde(default)]
    pub language: String,
    /// Declared package/module (`None` = default/unknown).
    pub package: Option<String>,
    /// Type / method / constructor / field definitions declared in this file.
    pub defs: Vec<SymbolDef>,
    /// Raw (unresolved) import statements; resolved in Phase 4.
    pub imports: Vec<RawImport>,
    /// Unresolved usage sites (calls, field access, heritage); resolved in Phase 4.
    pub reference_sites: Vec<ReferenceSite>,
    /// Receiver-name → raw-type bindings (params, locals, fields, `var` inference,
    /// patterns, aliases) scoped to their enclosing callable. Phase 4 uses these,
    /// precedence-ordered, to resolve a receiver's type. Raw (unresolved) names.
    pub type_bindings: Vec<TypeBinding>,
    /// Inter-service communication sites discovered in this file. Phase 21 turns
    /// these into ExternalEndpoint/KafkaTopic nodes plus cross-service edges.
    #[serde(default)]
    pub contract_sites: Vec<ContractSite>,
    /// Static SQL constant fields (`private static final String QUERY_... = "..."`).
    /// Used by the DB-access emit pass to link execution sites to SQL text.
    #[serde(default)]
    pub sql_constants: Vec<SqlConstant>,
    /// Sites where a known DB execution API is called (DBUtil, JdbcTemplate, etc.).
    #[serde(default)]
    pub sql_execution_sites: Vec<SqlExecutionSite>,
    /// All `static final String` fields (superset of sql_constants); used by constant propagation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub string_constants: Vec<StringConstant>,
    /// Same-repo HTTP wrapper functions detected in this file (script
    /// languages; see [`HttpWrapperDef`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_wrappers: Vec<HttpWrapperDef>,
}

/// A `static final String` field (or script-language module constant) with its
/// folded literal value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StringConstant {
    /// Field name, e.g. `"BASE_URL"`.
    pub const_name: String,
    /// FQCN of the declaring class, or the module path for script-language
    /// module-level constants (`src/services/apiClient` / `src.app.client`).
    pub owner_fqcn: String,
    /// Folded literal value (adjacent string literals concatenated).
    pub value: String,
    /// True when concat included non-literals.
    pub dynamic: bool,
    /// True when the value is the literal DEFAULT of an env override
    /// (`x ?? '/api/v1'`, `os.environ.get(k, "/api/v1")`) — the effective
    /// runtime value may differ; consumers surface this as provenance.
    #[serde(default)]
    pub env_default: bool,
    pub range: Range,
}

/// A same-repo HTTP wrapper function: `apiFetch(endpoint, options?) =>
/// fetch(BASE + endpoint)`. Call sites to it become HTTP contract sites at
/// resolve time (URL = `prefix_parts` + the caller's arg-0 parts). v1 rules:
/// the pass-through param is the FINAL url piece, `prefix_parts` contain only
/// `Lit`/`ConstRef`, and the caller's options object sits at
/// `options_arg_index`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpWrapperDef {
    /// Function name callers use (`apiFetch`).
    pub name: String,
    /// Extensionless repo-relative module path (`src/services/apiClient`).
    pub module: String,
    /// URL parts BEFORE the pass-through param.
    pub prefix_parts: Vec<UrlPart>,
    /// Positional index of the options object at call sites (v1: always 1).
    pub options_arg_index: u32,
    /// Verb hard-coded by the wrapper itself (`requests.get` inside
    /// `api_get` → `Some("GET")`) — overrides the call site's placeholder
    /// method at join. `None` for TS options-object wrappers, whose verb
    /// comes from the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_method: Option<String>,
    pub range: Range,
}

/// A `private static final String` field whose initializer is (or folds to) a SQL string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlConstant {
    /// Field name, e.g. `"QUERY_GETCUSTOMOVERDRAFTTYPEBYCODE"`.
    pub const_name: String,
    /// FQCN of the declaring class.
    pub owner_fqcn: String,
    /// Folded SQL text (adjacent string literals concatenated).
    pub sql_text: String,
    /// True when any non-literal expression appeared in the initializer concat chain.
    pub dynamic: bool,
    pub range: Range,
}

/// A site where a DB execution API (DBUtil, JdbcTemplate, PreparedStatement) is called.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlExecutionSite {
    /// Simple method name: `"executeQuery"`, `"prepareStatement"`, `"query"`, etc.
    pub api_name: String,
    /// Field-name argument referencing a SQL constant in the same class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub const_ref: Option<String>,
    /// Inline SQL literal passed directly as an argument (not via a named constant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_sql: Option<String>,
    /// Graph id of the enclosing callable — the `EXECUTES_QUERY` edge source.
    pub in_callable: NodeId,
    pub range: Range,
}

/// A source location that participates in an inter-service contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSite {
    pub kind: ContractKind,
    /// URL template for HTTP calls, e.g. `/api/orders/{id}`.
    #[serde(default)]
    pub url_template: Option<String>,
    /// Kafka/Spring topic name or Spring event class simple name.
    #[serde(default)]
    pub topic: Option<String>,
    /// HTTP method for HTTP calls.
    #[serde(default)]
    pub http_method: Option<String>,
    /// Messaging framework for event contracts (`EventPublish` / `EventListen`), so the
    /// contract carries its own Kafka-vs-Spring identity instead of consumers guessing.
    #[serde(default)]
    pub messaging_framework: Option<MessagingFramework>,
    /// Structured pieces of a URL (or topic) built from non-literal parts —
    /// constants and concatenation — for the resolve phase to fold. `None` for
    /// fully-literal URLs (`url_template` carries those unchanged).
    #[serde(default)]
    pub url_parts: Option<Vec<UrlPart>>,
    /// Set when this site is a call to a (potential) same-repo HTTP wrapper
    /// function rather than fetch/axios directly — the callee identifier.
    /// PROVISIONAL: the resolve phase joins it against detected
    /// [`HttpWrapperDef`]s and silently drops sites with no match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_wrapper: Option<String>,
    /// Graph id of the enclosing callable that makes/listens to this contract.
    pub in_callable: NodeId,
    pub range: Range,
}

/// One piece of a URL argument that isn't a plain string literal.
/// Produced by the parsers, folded by `cih-resolve` via the constant index:
/// resolved `ConstRef`s inline their value; unresolved refs and `Dynamic`
/// parts wildcard their whole path segment to `{*}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlPart {
    /// Literal fragment, as written.
    Lit(String),
    /// Reference to a named constant (`BASE`, `Constants.BASE`).
    ConstRef(String),
    /// Statically unresolvable expression (call, arithmetic, `${expr}`).
    Dynamic,
}

/// Messaging framework behind an event contract, determined by the parser
/// (`@KafkaListener` / `KafkaTemplate` → Kafka; `@EventListener` /
/// `ApplicationEventPublisher` → Spring).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagingFramework {
    Kafka,
    Spring,
    /// socket.io realtime events (`socket.emit` / `socket.on`).
    SocketIo,
    /// Bull / BullMQ job queues (`queue.add` / `new Worker`).
    Bull,
    /// RabbitMQ via amqplib (`channel.sendToQueue` / `channel.consume`).
    Rabbitmq,
    /// NestJS microservices / WebSocket gateways (`@MessagePattern` / `@EventPattern`
    /// / `@SubscribeMessage`, `client.emit`).
    NestMicroservice,
}

/// Type of contract site discovered by the parser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractKind {
    /// HTTP call via RestTemplate / WebClient.
    HttpCall,
    /// Declarative HTTP client proxy (e.g. Feign/OpenFeign interface).
    HttpClientProxy,
    /// KafkaTemplate.send() / ApplicationEventPublisher.publishEvent().
    EventPublish,
    /// @KafkaListener / @EventListener method.
    EventListen,
    /// Language-specific contract kind emitted by a parser (e.g. `"grpc_stub"`, `"graphql_query"`).
    /// Not resolved to edges automatically; consumers check the inner key.
    Custom(String),
}

/// A declared symbol — a type, method, constructor, or field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDef {
    /// Graph node id, built via the locked id scheme
    /// (`cih_core::{type_id, method_id, constructor_id, field_id}`).
    pub id: NodeId,
    pub kind: NodeKind,
    /// The FQCN this id is built from: the type's **own** FQCN for a type, or the
    /// **enclosing type's** FQCN for a method/constructor/field member.
    pub fqcn: String,
    /// Simple (unqualified) name.
    pub name: String,
    /// Enclosing type's node id for members; `None` for top-level types.
    pub owner: Option<NodeId>,
    pub range: Range,
    /// Source modifiers (`public`, `static`, `abstract`, …).
    pub modifiers: Vec<String>,
    /// Parameter types for methods/constructors, ordered, raw (simple/unresolved)
    /// names — empty for non-callables. Phase 4 uses these for overload narrowing.
    #[serde(default)]
    pub param_types: Vec<String>,
    /// Return type for methods, raw name (`None` for `void`/non-methods).
    #[serde(default)]
    pub return_type: Option<String>,
    /// Declared type for fields, raw name (`None` for non-fields).
    #[serde(default)]
    pub declared_type: Option<String>,
    /// Framework role for type-kind defs emitted by language parsers.
    /// Java examples: `"service"`, `"repository"`, `"controller"`, `"entity"`.
    /// `None` for non-types and unannotated classes.
    #[serde(default, rename = "stereotype")]
    pub framework_role: Option<String>,
    /// Optional complexity analysis (Gap 1). None = language provider did not compute this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<ComplexityRecord>,
    /// Optional MinHash body fingerprint (Gap 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_fingerprint: Option<BodyFingerprint>,
    /// Language-specific metadata not part of the universal IR.
    /// Parsers may write arbitrary JSON here; consumers must check the `language` field
    /// of the enclosing `ParsedFile` before reading. `None` for parsers that don't use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang_meta: Option<serde_json::Value>,
}

/// A raw import statement, pre-resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawImport {
    /// Imported path as written, e.g. `java.util.List` or `com.acme.util.*`.
    pub raw: String,
    /// `import static …`.
    pub is_static: bool,
    /// Wildcard import (`…*`).
    pub is_wildcard: bool,
    /// Local binding alias: python `import a.b as c` / TS
    /// `import * as c from './m'` → `Some("c")`. Named/default/from-import
    /// aliases are not captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub range: Range,
}

/// One call-site record stored per CALLS edge (multiple calls to same target → list).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSiteRecord {
    pub range: Range,
    /// Resolved (constant-propagated) arg texts, <= 120 chars each.
    pub args: Vec<String>,
}

/// Optional complexity analysis. None = language provider did not compute this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplexityRecord {
    /// e.g., "java"
    pub provider: String,
    pub cyclomatic: u16,
    pub cognitive: u16,
    pub loop_depth: u8,
    /// Set during transitive loop depth propagation pass.
    pub is_recursive: bool,
    /// Control-flow statement counts used to build the class-level StructuralProfile.
    /// All default to 0 so old serialised records remain valid.
    #[serde(default)]
    pub if_count: u16,
    #[serde(default)]
    pub for_count: u16,
    #[serde(default)]
    pub while_count: u16,
    #[serde(default)]
    pub switch_count: u16,
    #[serde(default)]
    pub try_count: u16,
    #[serde(default)]
    pub return_count: u16,
    #[serde(default)]
    pub throw_count: u16,
}

/// 25-float structural fingerprint of a class node.
///
/// Features (fixed order — index = feature):
///  0  method_count        1  field_count          2  constructor_count
///  3  avg_cyclomatic      4  max_cyclomatic        5  avg_cognitive
///  6  max_cognitive       7  avg_loop_depth        8  max_loop_depth
///  9  if_count (sum)     10  for_count (sum)      11  while_count (sum)
/// 12  switch_count (sum) 13  try_count (sum)      14  return_count (sum)
/// 15  throw_count (sum)  16  annotation_count     17  has_framework_stereotype
/// 18  is_interface       19  is_abstract          20  is_enum
/// 21  implements_count   22  extends_count        23  is_test
/// 24  loc_normalized (LOC/1000, clamped 1.0)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructuralProfile {
    pub features: [f32; 25],
}

impl StructuralProfile {
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        let dot: f32 = self
            .features
            .iter()
            .zip(other.features.iter())
            .map(|(a, b)| a * b)
            .sum();
        let na: f32 = self.features.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = other.features.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            0.0
        } else {
            (dot / (na * nb)).clamp(-1.0, 1.0)
        }
    }

    pub fn to_json_array(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.features
                .iter()
                .map(|&f| {
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(f as f64)
                            .unwrap_or(serde_json::Number::from(0)),
                    )
                })
                .collect(),
        )
    }

    pub fn from_json_array(v: &serde_json::Value) -> Option<Self> {
        let arr = v.as_array()?;
        if arr.len() != 25 {
            return None;
        }
        let mut features = [0f32; 25];
        for (i, val) in arr.iter().enumerate() {
            features[i] = val.as_f64()? as f32;
        }
        Some(Self { features })
    }
}

/// Optional MinHash fingerprint for near-clone detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyFingerprint {
    /// e.g., "java"
    pub provider: String,
    /// Leaf AST node count; size gate.
    pub leaf_token_count: u32,
    /// K=64 MinHash values.
    pub minhash: Vec<u32>,
}

/// A usage site (call / field access / heritage) before resolution. Phase 4 turns
/// each into a graph edge — or drops it if the target is out of scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSite {
    /// Referenced name (method / field / type simple name).
    pub name: String,
    /// Explicit receiver text for member calls (`service` in `service.save()`).
    pub receiver: Option<String>,
    pub kind: RefKind,
    /// Argument count for calls; `None` for non-call references.
    pub arity: Option<u16>,
    pub range: Range,
    /// Signature of the enclosing callable (`fqcn#name/arity`); kept for debugging
    /// and as a fallback. Prefer [`ReferenceSite::in_callable`] for the edge source.
    pub in_fqcn: String,
    /// Graph node id of the enclosing callable — the edge SOURCE Phase 4 attributes
    /// this reference to. A real [`NodeId`] (not the `in_fqcn` string), so resolved
    /// `CALLS`/`ACCESSES` edges never dangle.
    #[serde(default = "unknown_callable_id")]
    pub in_callable: NodeId,
    /// Raw argument texts captured at parse time (for Call sites only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arg_texts: Vec<String>,
}

fn unknown_callable_id() -> NodeId {
    NodeId::new("Method:<unknown>")
}

/// What a [`ReferenceSite`] represents → the graph-edge kind emitted after resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    Call,
    FieldRead,
    FieldWrite,
    Ctor,
    Extends,
    Implements,
    TypeRef,
}

/// A receiver-name → raw-type binding scoped to its enclosing callable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeBinding {
    /// Bound identifier — the receiver name (`service`, `u`, …).
    pub name: String,
    /// Raw (unresolved) type name as written (`OwnerService`, `List`); or, for
    /// `var` call-result inference, the invoked method name whose return type the
    /// resolver must follow.
    pub raw_type: String,
    /// How the binding was introduced — drives Phase 4 resolution precedence and
    /// whether `raw_type` is a type or a method/alias to chase.
    pub kind: BindingKind,
    /// Signature of the enclosing callable (`fqcn#name/arity`), or the type FQCN for
    /// a field binding — the lexical scope this binding lives in.
    pub in_fqcn: String,
    pub range: Range,
}

/// Per-file parse output: graph nodes/edges produced for this file, plus the
/// unresolved IR that the resolution phase consumes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParsedUnit {
    pub rel: String,
    pub nodes: Vec<crate::Node>,
    pub edges: Vec<crate::Edge>,
    pub parsed_file: ParsedFile,
    /// How many callables the AST actually contains (functions, arrows, methods —
    /// see `LanguageProvider::callable_kinds`). Compared against the `Function`/
    /// `Method` nodes we emitted, this is the extraction-coverage signal: a ratio
    /// well below 1 means the parser is silently skipping an idiom. `0` means the
    /// provider doesn't measure (opt-in), so callers must treat it as "unknown",
    /// not as "no callables".
    #[serde(default)]
    pub syntactic_callables: u32,
}

/// Origin of a [`TypeBinding`] — determines resolution precedence (nearest
/// param/local beats a field) and how `raw_type` is interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingKind {
    /// Method/constructor formal parameter (`void f(User u)`).
    Param,
    /// Local variable with an explicit type (`User u = …`).
    Local,
    /// Class field (`private User user;`).
    Field,
    /// `var x = svc.get();` — `raw_type` is the invoked method name to follow.
    CallResult,
    /// `var y = x;` — `raw_type` is another bound name to alias.
    Alias,
    /// Pattern binding (`if (o instanceof User u)`, `case User u ->`).
    Pattern,
    /// Method return-type binding.
    Return,
    /// `const x = require('./m')` — `raw_type` is the pre-resolved module path
    /// (the container FQCN of that module's top-level functions). The resolver
    /// returns it verbatim, so `x.method()` resolves against that module's members.
    ///
    /// Also used for a barrel re-export (`module.exports.svc = require('./svc')`),
    /// scoped to the barrel's module: that export *is* the target module.
    ModuleRef,
    /// `const { svc } = require('./m')` — `raw_type` is `<module>#<member>`. The
    /// resolver follows `member` through `<module>`'s exports (chasing a barrel
    /// re-export when there is one) to whatever module it denotes.
    ModuleMember,
}
