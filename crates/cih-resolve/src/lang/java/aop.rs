//! Spring AOP resolution: parse the pointcut expressions on `@Aspect` advice
//! methods and emit `ADVISES` edges (advice method → advised method).
//!
//! Runs in `JavaResolver::post_process` over the assembled graph — the parser
//! already retains every annotation (name + string attrs) on node props, so no
//! source re-parsing is needed. Supported designators: `execution`, `within`,
//! `@within`, `@annotation`, `bean`, same-class named `@Pointcut` refs, and
//! `&&`/`||`/`!` combinators. Anything else (`args`, `this`, `target`,
//! cross-class refs, …) is fail-soft: an unsupported conjunct under `&&` is
//! ignored and the edge marked `approximate`; unsupported anywhere else skips
//! the advice. Targets are restricted to methods of Spring-stereotyped bean
//! classes — Spring proxies only intercept beans, and an unrestricted
//! `within(..*)` would flood the graph.

use std::collections::{HashMap, HashSet};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};

/// Per-advice safety valve: an overly broad pointcut (`within(com..*)` on a
/// monorepo) must not explode the edge set.
const MAX_MATCHES_PER_ADVICE: usize = 2_000;

const ADVICE_ANNOTATIONS: &[(&str, &str)] = &[
    ("Around", "around"),
    ("Before", "before"),
    ("After", "after"),
    ("AfterReturning", "after_returning"),
    ("AfterThrowing", "after_throwing"),
];

/// Spring stereotype annotations that make a class a proxyable bean.
const BEAN_ANNOTATIONS: &[&str] = &[
    "Component",
    "Service",
    "Repository",
    "Controller",
    "RestController",
    "Configuration",
];

// ---------------------------------------------------------------------------
// Pointcut expression AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Pointcut {
    /// `execution([modifiers] ret [type.]name(params))`. `path` is the
    /// combined declaring-type + name pattern; when the type portion carried a
    /// `+`, `path` holds only the type pattern and the name pattern lives in
    /// `name_after_plus`.
    Execution {
        ret: String,
        path: String,
        name_after_plus: Option<String>,
        params: ParamsPat,
    },
    /// `within(type-pattern)`; `plus` includes subtypes.
    Within { pattern: String, plus: bool },
    /// `@within(com.acme.Loggable)` — declaring class carries the annotation.
    AnnotationOnType(String),
    /// `@annotation(com.acme.Loggable)` — the method carries the annotation.
    AnnotationOnMethod(String),
    /// `bean(name-pattern)`.
    Bean(String),
    /// Reference to a named `@Pointcut` method in the same aspect class.
    NamedRef(String),
    And(Box<Pointcut>, Box<Pointcut>),
    Or(Box<Pointcut>, Box<Pointcut>),
    Not(Box<Pointcut>),
    /// Designator we cannot evaluate statically (`args`, `this`, `target`,
    /// cross-class named ref, malformed input, …).
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
enum ParamEntry {
    /// `..` — any number of parameters (including none).
    Ellipsis,
    /// `*` — exactly one parameter of any type.
    Any,
    /// A concrete type pattern, matched by base simple name.
    Type(String),
}

type ParamsPat = Vec<ParamEntry>;

// ---------------------------------------------------------------------------
// Parser (recursive descent; precedence: ! > && > ||)
// ---------------------------------------------------------------------------

struct Parser<'a> {
    chars: Vec<char>,
    src: &'a str,
    pos: usize,
}

fn parse_pointcut(expr: &str) -> Pointcut {
    let mut p = Parser {
        chars: expr.chars().collect(),
        src: expr,
        pos: 0,
    };
    let pc = p.parse_or();
    p.skip_ws();
    if p.pos < p.chars.len() {
        // Trailing garbage — don't half-match a partially parsed expression.
        return Pointcut::Unsupported(expr.to_string());
    }
    pc
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, tok: &str) -> bool {
        self.skip_ws();
        let t: Vec<char> = tok.chars().collect();
        if self.chars[self.pos..].starts_with(&t[..]) {
            self.pos += t.len();
            true
        } else {
            false
        }
    }

    fn parse_or(&mut self) -> Pointcut {
        let mut left = self.parse_and();
        while self.eat("||") {
            let right = self.parse_and();
            left = Pointcut::Or(Box::new(left), Box::new(right));
        }
        left
    }

    fn parse_and(&mut self) -> Pointcut {
        let mut left = self.parse_unary();
        while self.eat("&&") {
            let right = self.parse_unary();
            left = Pointcut::And(Box::new(left), Box::new(right));
        }
        left
    }

    fn parse_unary(&mut self) -> Pointcut {
        if self.eat("!") {
            return Pointcut::Not(Box::new(self.parse_unary()));
        }
        if self.eat("(") {
            let inner = self.parse_or();
            if !self.eat(")") {
                return Pointcut::Unsupported(self.src.to_string());
            }
            return inner;
        }
        self.parse_designator()
    }

    fn parse_designator(&mut self) -> Pointcut {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            if c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '@') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        if name.is_empty() || !self.eat("(") {
            self.pos = self.chars.len(); // poison: stop parsing
            return Pointcut::Unsupported(self.src.to_string());
        }
        // Scan to the matching close paren (execution bodies contain nested parens).
        let body_start = self.pos;
        let mut depth = 1usize;
        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        if depth != 0 {
            return Pointcut::Unsupported(self.src.to_string());
        }
        let body: String = self.chars[body_start..self.pos].iter().collect();
        self.pos += 1; // consume ')'
        designator(&name, body.trim())
    }
}

fn designator(name: &str, body: &str) -> Pointcut {
    match name {
        "execution" => parse_execution(body),
        "within" => {
            let (pattern, plus) = match body.strip_suffix('+') {
                Some(p) => (p.trim_end().to_string(), true),
                None => (body.to_string(), false),
            };
            Pointcut::Within { pattern, plus }
        }
        "@within" => Pointcut::AnnotationOnType(body.to_string()),
        "@annotation" => Pointcut::AnnotationOnMethod(body.to_string()),
        "bean" => Pointcut::Bean(body.to_string()),
        // Runtime-type / argument designators are statically undecidable here.
        "args" | "this" | "target" | "@args" | "@target" => {
            Pointcut::Unsupported(format!("{name}({body})"))
        }
        // A bare identifier with call parens is a named-pointcut reference;
        // a dotted one lives in another class, which we don't resolve.
        _ if name.contains('.') || name.starts_with('@') => {
            Pointcut::Unsupported(format!("{name}({body})"))
        }
        _ => Pointcut::NamedRef(name.to_string()),
    }
}

fn parse_execution(body: &str) -> Pointcut {
    let Some(open) = body.find('(') else {
        return Pointcut::Unsupported(format!("execution({body})"));
    };
    let Some(close) = body.rfind(')') else {
        return Pointcut::Unsupported(format!("execution({body})"));
    };
    let head = body[..open].trim();
    let params = parse_params(&body[open + 1..close]);

    // head = [modifiers] ret-type-pattern [declaring-type.]name-pattern
    let tokens: Vec<&str> = head.split_whitespace().collect();
    let (ret, path) = match tokens.len() {
        0 => return Pointcut::Unsupported(format!("execution({body})")),
        1 => ("*", tokens[0]),
        n => (tokens[n - 2], tokens[n - 1]),
    };

    // `+` terminates the declaring-type pattern: `com.acme.Service+.save`.
    let (path, name_after_plus) = match path.find('+') {
        Some(i) => {
            let name = path[i + 1..].trim_start_matches('.').to_string();
            (
                path[..i].to_string(),
                Some(if name.is_empty() { "*".into() } else { name }),
            )
        }
        None => (path.to_string(), None),
    };

    Pointcut::Execution {
        ret: ret.to_string(),
        path,
        name_after_plus,
        params,
    }
}

fn parse_params(raw: &str) -> ParamsPat {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    // Generic type arguments would need depth-aware splitting; they are rare
    // in pointcuts and base-simple-name comparison ignores them anyway.
    raw.split(',')
        .filter_map(|entry| match entry.trim() {
            "" => None,
            ".." => Some(ParamEntry::Ellipsis),
            "*" => Some(ParamEntry::Any),
            t => Some(ParamEntry::Type(t.to_string())),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Wildcard matching (`*` = any run without '.', `..` = any run that starts
// and ends with '.', minimum a single '.')
// ---------------------------------------------------------------------------

fn wild_match(pattern: &str, candidate: &str) -> bool {
    // AspectJ: a bare `*` type pattern matches any type.
    if pattern == "*" {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = candidate.chars().collect();
    wild_go(&p, &t)
}

fn wild_go(p: &[char], t: &[char]) -> bool {
    let Some(&first) = p.first() else {
        return t.is_empty();
    };
    if first == '.' && p.get(1) == Some(&'.') {
        if t.first() != Some(&'.') {
            return false;
        }
        for k in 1..=t.len() {
            if t[k - 1] == '.' && wild_go(&p[2..], &t[k..]) {
                return true;
            }
        }
        return false;
    }
    match first {
        '*' => {
            let mut k = 0;
            loop {
                if wild_go(&p[1..], &t[k..]) {
                    return true;
                }
                if k < t.len() && t[k] != '.' {
                    k += 1;
                } else {
                    return false;
                }
            }
        }
        c => t.first() == Some(&c) && wild_go(&p[1..], &t[1..]),
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tri {
    True { approximate: bool },
    False,
    Unknown,
}

struct MethodCtx<'a> {
    owner_fqcn: &'a str,
    /// Owner FQCN plus all transitive supertypes (for `+` subtype patterns).
    owner_lineage: &'a [String],
    method_name: &'a str,
    arity: usize,
    /// Declared parameter type texts (`paramTypes` prop), when retained.
    param_types: Option<Vec<String>>,
    /// Declared return type text (`returnType` prop); `None` = void/unknown.
    return_type: Option<&'a str>,
    method_annotations: &'a [String],
    class_annotations: &'a [String],
    bean_name: &'a str,
}

fn eval(pc: &Pointcut, m: &MethodCtx<'_>) -> Tri {
    match pc {
        Pointcut::Execution {
            ret,
            path,
            name_after_plus,
            params,
        } => eval_execution(ret, path, name_after_plus.as_deref(), params, m),
        Pointcut::Within { pattern, plus } => {
            let hit = if *plus {
                m.owner_lineage.iter().any(|f| wild_match(pattern, f))
            } else {
                wild_match(pattern, m.owner_fqcn)
            };
            tri(hit, false)
        }
        Pointcut::AnnotationOnType(ann) => tri(has_annotation(m.class_annotations, ann), false),
        Pointcut::AnnotationOnMethod(ann) => tri(has_annotation(m.method_annotations, ann), false),
        Pointcut::Bean(pat) => tri(wild_match(pat, m.bean_name), false),
        // Named refs are inlined before evaluation; a survivor is unresolved.
        Pointcut::NamedRef(_) | Pointcut::Unsupported(_) => Tri::Unknown,
        Pointcut::And(a, b) => match (eval(a, m), eval(b, m)) {
            (Tri::False, _) | (_, Tri::False) => Tri::False,
            (Tri::Unknown, Tri::Unknown) => Tri::Unknown,
            // One side undecidable: keep the decidable side, flag the edge.
            (Tri::Unknown, Tri::True { .. }) | (Tri::True { .. }, Tri::Unknown) => {
                Tri::True { approximate: true }
            }
            (Tri::True { approximate: x }, Tri::True { approximate: y }) => {
                Tri::True { approximate: x || y }
            }
        },
        Pointcut::Or(a, b) => match (eval(a, m), eval(b, m)) {
            (t @ Tri::True { .. }, _) | (_, t @ Tri::True { .. }) => t,
            (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
            _ => Tri::False,
        },
        Pointcut::Not(inner) => match eval(inner, m) {
            Tri::True { .. } => Tri::False,
            Tri::False => Tri::True { approximate: false },
            Tri::Unknown => Tri::Unknown,
        },
    }
}

fn tri(hit: bool, approximate: bool) -> Tri {
    if hit {
        Tri::True { approximate }
    } else {
        Tri::False
    }
}

fn eval_execution(
    ret: &str,
    path: &str,
    name_after_plus: Option<&str>,
    params: &ParamsPat,
    m: &MethodCtx<'_>,
) -> Tri {
    let mut approximate = false;

    match &m.param_types {
        // Declared parameter types retained → full positional match.
        Some(types) if types.len() == m.arity => {
            if !params_match(params, types) {
                return Tri::False;
            }
        }
        // Fall back to arity; concrete type constraints become approximate.
        _ => {
            let required = params
                .iter()
                .filter(|e| !matches!(e, ParamEntry::Ellipsis))
                .count();
            let open_ended = params.iter().any(|e| matches!(e, ParamEntry::Ellipsis));
            let arity_ok = if open_ended {
                m.arity >= required
            } else {
                m.arity == required
            };
            if !arity_ok {
                return Tri::False;
            }
            approximate |= params.iter().any(|e| matches!(e, ParamEntry::Type(_)));
        }
    }

    let name_and_type_ok = match name_after_plus {
        // `Type+.name`: subtype-aware — any type in the lineage may match.
        Some(name_pat) => {
            wild_match(name_pat, m.method_name)
                && m.owner_lineage.iter().any(|f| wild_match(path, f))
        }
        None => {
            if let Some(name_pat) = path.strip_prefix("*.").filter(|r| !r.contains('.')) {
                // `*.name`: a bare `*` declaring type matches any type.
                wild_match(name_pat, m.method_name)
            } else if path.contains('.') {
                // Combined declaring-type + name pattern against "fqcn.name" —
                // sidesteps ambiguous splitting of patterns like `com.acme..*`.
                wild_match(path, &format!("{}.{}", m.owner_fqcn, m.method_name))
            } else {
                // Bare name pattern: any declaring type.
                wild_match(path, m.method_name)
            }
        }
    };
    if !name_and_type_ok {
        return Tri::False;
    }

    if ret != "*" {
        match m.return_type {
            Some(declared) => {
                if !type_text_matches(ret, declared) {
                    return Tri::False;
                }
            }
            // `returnType` is absent for void; a `void` pattern is decidable.
            None => {
                if ret != "void" {
                    return Tri::False;
                }
            }
        }
    }

    Tri::True { approximate }
}

/// Positional match of pointcut param entries against declared types, with
/// `..` as an elastic gap.
fn params_match(entries: &[ParamEntry], types: &[String]) -> bool {
    let Some(first) = entries.first() else {
        return types.is_empty();
    };
    match first {
        ParamEntry::Ellipsis => {
            (0..=types.len()).any(|k| params_match(&entries[1..], &types[k..]))
        }
        ParamEntry::Any => !types.is_empty() && params_match(&entries[1..], &types[1..]),
        ParamEntry::Type(pat) => {
            !types.is_empty()
                && type_text_matches(pat, &types[0])
                && params_match(&entries[1..], &types[1..])
        }
    }
}

/// Compare a pointcut type pattern against declared source type text by base
/// simple name (generics erased, arrays/varargs stripped, packages dropped —
/// declared texts are written against imports, so packages aren't comparable).
fn type_text_matches(pattern: &str, declared: &str) -> bool {
    wild_match(base_simple(pattern), base_simple(declared))
}

fn base_simple(ty: &str) -> &str {
    let base = ty
        .split('<')
        .next()
        .unwrap_or(ty)
        .trim_end_matches("...")
        .trim_end_matches("[]")
        .trim();
    base.rsplit('.').next().unwrap_or(base)
}

/// Annotation patterns compare by simple name — the retained metadata stores
/// simple names, so `@annotation(com.acme.Loggable)` matches on `Loggable`.
fn has_annotation(annotations: &[String], pattern: &str) -> bool {
    let simple = pattern.rsplit('.').next().unwrap_or(pattern);
    annotations.iter().any(|a| wild_match(simple, a))
}

// ---------------------------------------------------------------------------
// Graph pass
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct AopStats {
    pub aspects: usize,
    pub advice_methods: usize,
    pub edges: usize,
}

struct Advice<'a> {
    method_id: &'a NodeId,
    kind: &'static str,
    pointcut: Pointcut,
    expr: String,
    order: Option<i64>,
}

/// Emit `ADVISES` edges for every `@Aspect` advice whose pointcut we can
/// evaluate. Pure graph-in/graph-out; no-op on repos without aspects.
pub(crate) fn emit_advises_edges(nodes: &[Node], edges: &mut Vec<Edge>) -> AopStats {
    let mut stats = AopStats::default();

    // --- Class-level lookups -------------------------------------------------
    let mut class_anns: HashMap<&str, Vec<String>> = HashMap::new();
    let mut bean_names: HashMap<&str, String> = HashMap::new();
    let mut fqcn_by_node_id: HashMap<&NodeId, &str> = HashMap::new();
    let mut aspect_classes: HashMap<&str, Option<i64>> = HashMap::new();
    let mut test_classes: HashSet<&str> = HashSet::new();
    for n in nodes {
        if !matches!(
            n.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record
        ) {
            continue;
        }
        let Some(fqcn) = n.qualified_name.as_deref() else {
            continue;
        };
        fqcn_by_node_id.insert(&n.id, fqcn);
        let anns = annotation_names(n);
        if let Some(bean) = bean_name(n, fqcn) {
            bean_names.insert(fqcn, bean);
        }
        if anns.iter().any(|a| a == "Aspect") {
            aspect_classes.insert(fqcn, annotation_order(n));
        }
        if stereotype(n) == Some("test") {
            test_classes.insert(fqcn);
        }
        class_anns.insert(fqcn, anns);
    }
    stats.aspects = aspect_classes.len();
    if aspect_classes.is_empty() {
        return stats;
    }

    // Direct supertypes (src = subtype), for `+` patterns.
    let mut supers: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in edges.iter() {
        if !matches!(e.kind, EdgeKind::Extends | EdgeKind::Implements) {
            continue;
        }
        if let (Some(sub), Some(sup)) = (fqcn_by_node_id.get(&e.src), fqcn_by_node_id.get(&e.dst))
        {
            supers.entry(sub).or_default().push(sup);
        }
    }

    // --- Aspect members: named pointcuts and advice methods ------------------
    // (owner fqcn, method simple name) → parsed named @Pointcut expression.
    let mut named_pointcuts: HashMap<(&str, String), Pointcut> = HashMap::new();
    let mut advices: Vec<Advice<'_>> = Vec::new();
    for n in nodes {
        if n.kind != NodeKind::Method {
            continue;
        }
        let Some((owner, name, _arity)) = split_method_qn(n.qualified_name.as_deref()) else {
            continue;
        };
        let Some(&order) = aspect_classes.get(owner) else {
            continue;
        };
        for ann in annotation_snapshots(n) {
            let Some(ann_name) = ann.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let expr = || {
                ann.get("attrs")
                    .and_then(|a| a.get("value").or_else(|| a.get("pointcut")))
                    .and_then(|v| v.as_str())
            };
            if ann_name == "Pointcut" {
                if let Some(e) = expr() {
                    let pc = resolve_binding_args(&parse_pointcut(e), &prop_param_types(n));
                    named_pointcuts.insert((owner, name.to_string()), pc);
                }
            } else if let Some((_, kind)) =
                ADVICE_ANNOTATIONS.iter().find(|(a, _)| *a == ann_name)
            {
                if let Some(e) = expr() {
                    advices.push(Advice {
                        method_id: &n.id,
                        kind,
                        pointcut: resolve_binding_args(&parse_pointcut(e), &prop_param_types(n)),
                        expr: e.to_string(),
                        order,
                    });
                }
            }
        }
    }
    stats.advice_methods = advices.len();
    if advices.is_empty() {
        return stats;
    }
    for advice in &mut advices {
        let (owner, _, _) = split_method_qn(Some(id_qn(advice.method_id))).unwrap_or(("", "", 0));
        let mut seen = HashSet::new();
        advice.pointcut = inline_named_refs(&advice.pointcut, owner, &named_pointcuts, &mut seen);
    }

    // --- Candidate methods: members of Spring bean classes -------------------
    let mut lineage_cache: HashMap<&str, Vec<String>> = HashMap::new();
    let mut new_edges: Vec<Edge> = Vec::new();
    let mut match_counts: Vec<usize> = vec![0; advices.len()];
    for n in nodes {
        if n.kind != NodeKind::Method {
            continue;
        }
        let Some((owner, name, arity)) = split_method_qn(n.qualified_name.as_deref()) else {
            continue;
        };
        if name.starts_with('<') // constructors / static initializers
            || aspect_classes.contains_key(owner)
            || test_classes.contains(owner)
        {
            continue;
        }
        let Some(bean) = bean_names.get(owner) else {
            continue; // not a Spring bean — proxies never see it
        };
        let class_annotations = class_anns.get(owner).cloned().unwrap_or_default();
        let method_annotations: Vec<String> = annotation_snapshots(n)
            .iter()
            .filter_map(|a| a.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let lineage = lineage_cache
            .entry(owner)
            .or_insert_with(|| lineage_of(owner, &supers));
        let param_types = n
            .props
            .as_ref()
            .and_then(|p| p.get("paramTypes"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
        let return_type = n
            .props
            .as_ref()
            .and_then(|p| p.get("returnType"))
            .and_then(|v| v.as_str());
        let ctx = MethodCtx {
            owner_fqcn: owner,
            owner_lineage: lineage,
            method_name: name,
            arity,
            param_types,
            return_type,
            method_annotations: &method_annotations,
            class_annotations: &class_annotations,
            bean_name: bean,
        };
        for (i, advice) in advices.iter().enumerate() {
            if match_counts[i] >= MAX_MATCHES_PER_ADVICE {
                continue;
            }
            let Tri::True { approximate } = eval(&advice.pointcut, &ctx) else {
                continue;
            };
            match_counts[i] += 1;
            let mut props = serde_json::json!({
                "advice_kind": advice.kind,
                "pointcut": advice.expr,
            });
            if approximate {
                props["approximate"] = serde_json::Value::Bool(true);
            }
            if let Some(order) = advice.order {
                props["aspect_order"] = serde_json::Value::from(order);
            }
            new_edges.push(Edge {
                src: advice.method_id.clone(),
                dst: n.id.clone(),
                kind: EdgeKind::Advises,
                confidence: if approximate { 0.8 } else { 1.0 },
                reason: format!("aop-{}", advice.kind),
                props: Some(props),
            });
        }
    }

    for (i, &count) in match_counts.iter().enumerate() {
        if count >= MAX_MATCHES_PER_ADVICE {
            tracing::warn!(
                pointcut = %advices[i].expr,
                cap = MAX_MATCHES_PER_ADVICE,
                "AOP pointcut hit the per-advice match cap; edges truncated"
            );
        }
    }

    // Dedup (src, dst) — named-ref inlining can make two advices equivalent.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    new_edges.retain(|e| seen.insert((e.src.to_string(), e.dst.to_string())));
    stats.edges = new_edges.len();
    edges.extend(new_edges);
    stats
}

/// `paramTypes` prop of a method node (declared source type texts).
fn prop_param_types(n: &Node) -> Vec<String> {
    n.props
        .as_ref()
        .and_then(|p| p.get("paramTypes"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve AspectJ parameter bindings: `@annotation(withFlushMode)` names the
/// advice-method parameter `WithFlushMode withFlushMode`, not a type. A
/// lowercase dot-free argument is looked up among the declaring method's
/// parameter types by decapitalized simple name; unresolvable bindings make
/// the designator `Unsupported` (fail-soft).
fn resolve_binding_args(pc: &Pointcut, param_types: &[String]) -> Pointcut {
    let resolve = |arg: &str, rebuild: &dyn Fn(String) -> Pointcut| -> Pointcut {
        let is_binding = !arg.contains('.')
            && arg.chars().next().is_some_and(|c| c.is_lowercase());
        if !is_binding {
            return rebuild(arg.to_string());
        }
        let hit = param_types.iter().map(|t| base_simple(t)).find(|simple| {
            decapitalize(simple) == arg || simple.eq_ignore_ascii_case(arg)
        });
        match hit {
            Some(simple) => rebuild(simple.to_string()),
            None => Pointcut::Unsupported(format!("{arg} [unresolved binding]")),
        }
    };
    match pc {
        Pointcut::AnnotationOnMethod(arg) => resolve(arg, &Pointcut::AnnotationOnMethod),
        Pointcut::AnnotationOnType(arg) => resolve(arg, &Pointcut::AnnotationOnType),
        Pointcut::And(a, b) => Pointcut::And(
            Box::new(resolve_binding_args(a, param_types)),
            Box::new(resolve_binding_args(b, param_types)),
        ),
        Pointcut::Or(a, b) => Pointcut::Or(
            Box::new(resolve_binding_args(a, param_types)),
            Box::new(resolve_binding_args(b, param_types)),
        ),
        Pointcut::Not(inner) => {
            Pointcut::Not(Box::new(resolve_binding_args(inner, param_types)))
        }
        other => other.clone(),
    }
}

/// Replace same-class `NamedRef`s with their `@Pointcut` expressions (cycle-safe).
fn inline_named_refs(
    pc: &Pointcut,
    owner: &str,
    named: &HashMap<(&str, String), Pointcut>,
    seen: &mut HashSet<String>,
) -> Pointcut {
    match pc {
        Pointcut::NamedRef(name) => {
            if !seen.insert(name.clone()) {
                return Pointcut::Unsupported(format!("{name}() [cyclic]"));
            }
            match named.get(&(owner, name.clone())) {
                Some(inner) => inline_named_refs(inner, owner, named, seen),
                None => Pointcut::Unsupported(format!("{name}() [unresolved]")),
            }
        }
        Pointcut::And(a, b) => Pointcut::And(
            Box::new(inline_named_refs(a, owner, named, seen)),
            Box::new(inline_named_refs(b, owner, named, seen)),
        ),
        Pointcut::Or(a, b) => Pointcut::Or(
            Box::new(inline_named_refs(a, owner, named, seen)),
            Box::new(inline_named_refs(b, owner, named, seen)),
        ),
        Pointcut::Not(inner) => Pointcut::Not(Box::new(inline_named_refs(inner, owner, named, seen))),
        other => other.clone(),
    }
}

/// `com.acme.Foo#pay/2` → (`com.acme.Foo`, `pay`, 2).
fn split_method_qn(qn: Option<&str>) -> Option<(&str, &str, usize)> {
    let qn = qn?;
    let (owner, rest) = qn.split_once('#')?;
    let (name, arity) = match rest.rsplit_once('/') {
        Some((n, a)) => (n, a.parse().ok()?),
        None => (rest, 0),
    };
    Some((owner, name, arity))
}

/// Qualified name embedded in a `Method:` node id (fallback when we only hold the id).
fn id_qn(id: &NodeId) -> &str {
    let s = id.as_str();
    s.split_once(':').map(|(_, qn)| qn).unwrap_or(s)
}

fn annotation_snapshots(n: &Node) -> Vec<serde_json::Value> {
    n.props
        .as_ref()
        .and_then(|p| p.get("annotations"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

fn annotation_names(n: &Node) -> Vec<String> {
    annotation_snapshots(n)
        .iter()
        .filter_map(|a| a.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

fn stereotype(n: &Node) -> Option<&str> {
    n.props
        .as_ref()
        .and_then(|p| p.get("stereotype"))
        .and_then(|s| s.as_str())
}

/// `@Order(value)` when the retained snapshot holds a parseable number.
fn annotation_order(n: &Node) -> Option<i64> {
    annotation_snapshots(n)
        .iter()
        .find(|a| a.get("name").and_then(|v| v.as_str()) == Some("Order"))
        .and_then(|a| a.get("attrs")?.get("value")?.as_str()?.parse().ok())
}

/// Spring bean name: stereotype annotation `value` attr, else the Java-Beans
/// decapitalized simple class name. `None` when the class is not a bean.
fn bean_name(n: &Node, fqcn: &str) -> Option<String> {
    let stereo = annotation_snapshots(n).into_iter().find(|a| {
        a.get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|name| BEAN_ANNOTATIONS.contains(&name))
    })?;
    if let Some(explicit) = stereo
        .get("attrs")
        .and_then(|attrs| attrs.get("value"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
    {
        return Some(explicit.to_string());
    }
    let simple = fqcn.rsplit('.').next().unwrap_or(fqcn);
    Some(decapitalize(simple))
}

/// `java.beans.Introspector::decapitalize`: `FooBar` → `fooBar`, but `URL` → `URL`.
fn decapitalize(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() >= 2 && chars[0].is_uppercase() && chars[1].is_uppercase() {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len());
    for (i, c) in chars.into_iter().enumerate() {
        if i == 0 {
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// FQCN plus its transitive supertypes (`+` subtype matching walks this).
fn lineage_of(fqcn: &str, supers: &HashMap<&str, Vec<&str>>) -> Vec<String> {
    let mut out = vec![fqcn.to_string()];
    let mut queue = vec![fqcn];
    let mut visited: HashSet<&str> = HashSet::from([fqcn]);
    while let Some(cur) = queue.pop() {
        for &sup in supers.get(cur).into_iter().flatten() {
            if visited.insert(sup) {
                out.push(sup.to_string());
                queue.push(sup);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::Range;

    // --- wildcard matcher ---------------------------------------------------

    #[test]
    fn wildcards() {
        assert!(wild_match("com.acme..*", "com.acme.x.Foo"));
        assert!(wild_match("com.acme..*", "com.acme.Foo"));
        assert!(!wild_match("com.acme.*", "com.acme.x.Foo"));
        assert!(wild_match("com.acme.*", "com.acme.Foo"));
        assert!(wild_match("com..Foo", "com.a.b.Foo"));
        assert!(wild_match("com..Foo", "com.Foo"));
        assert!(!wild_match("com..Foo", "com.a.FooBar"));
        assert!(wild_match("*Service", "OrderService"));
        assert!(wild_match("save*", "saveAll"));
        assert!(wild_match("*", "anything.at.all"));
    }

    // --- parser -------------------------------------------------------------

    #[test]
    fn parses_execution() {
        let pc = parse_pointcut("execution(* com.acme.service.*.*(..))");
        let Pointcut::Execution { ret, path, name_after_plus, params } = pc else {
            panic!("expected Execution, got {pc:?}");
        };
        assert_eq!(ret, "*");
        assert_eq!(path, "com.acme.service.*.*");
        assert_eq!(name_after_plus, None);
        assert_eq!(params, vec![ParamEntry::Ellipsis]);
    }

    #[test]
    fn parses_execution_with_modifiers_ret_and_typed_params() {
        let pc = parse_pointcut("execution(public java.util.List find*(String, ..))");
        let Pointcut::Execution { ret, path, params, .. } = pc else {
            panic!("expected Execution, got {pc:?}");
        };
        assert_eq!(ret, "java.util.List");
        assert_eq!(path, "find*");
        assert_eq!(
            params,
            vec![ParamEntry::Type("String".into()), ParamEntry::Ellipsis]
        );
    }

    #[test]
    fn parses_subtype_plus() {
        let pc = parse_pointcut("execution(* com.acme.PaymentService+.*(..))");
        let Pointcut::Execution { path, name_after_plus, .. } = pc else {
            panic!("expected Execution, got {pc:?}");
        };
        assert_eq!(path, "com.acme.PaymentService");
        assert_eq!(name_after_plus.as_deref(), Some("*"));
    }

    #[test]
    fn parses_combinators_with_precedence() {
        let pc = parse_pointcut("within(com.a..*) || @annotation(Tx) && !bean(fooService)");
        // && binds tighter than ||.
        let Pointcut::Or(l, r) = pc else { panic!("expected Or, got {pc:?}") };
        assert_eq!(*l, Pointcut::Within { pattern: "com.a..*".into(), plus: false });
        let Pointcut::And(al, ar) = *r else { panic!("expected And") };
        assert_eq!(*al, Pointcut::AnnotationOnMethod("Tx".into()));
        assert_eq!(*ar, Pointcut::Not(Box::new(Pointcut::Bean("fooService".into()))));
    }

    #[test]
    fn runtime_designators_are_unsupported() {
        assert!(matches!(
            parse_pointcut("args(java.lang.String)"),
            Pointcut::Unsupported(_)
        ));
        assert!(matches!(
            parse_pointcut("com.acme.Pointcuts.repoOps()"),
            Pointcut::Unsupported(_)
        ));
        assert!(matches!(parse_pointcut("loggableOps()"), Pointcut::NamedRef(_)));
        assert!(matches!(parse_pointcut("execution(*"), Pointcut::Unsupported(_)));
    }

    // --- graph pass ---------------------------------------------------------

    fn class(fqcn: &str, anns: serde_json::Value) -> Node {
        node(NodeKind::Class, &format!("Class:{fqcn}"), fqcn, anns)
    }

    fn interface(fqcn: &str) -> Node {
        node(
            NodeKind::Interface,
            &format!("Interface:{fqcn}"),
            fqcn,
            serde_json::json!([]),
        )
    }

    fn method(qn: &str, props: serde_json::Value) -> Node {
        Node {
            id: NodeId::new(format!("Method:{qn}")),
            kind: NodeKind::Method,
            name: qn.split('#').nth(1).unwrap_or(qn).split('/').next().unwrap().into(),
            qualified_name: Some(qn.to_string()),
            file: "F.java".into(),
            range: Range::default(),
            props: Some(props),
        }
    }

    fn node(kind: NodeKind, id: &str, fqcn: &str, anns: serde_json::Value) -> Node {
        Node {
            id: NodeId::new(id),
            kind,
            name: fqcn.rsplit('.').next().unwrap_or(fqcn).into(),
            qualified_name: Some(fqcn.to_string()),
            file: "F.java".into(),
            range: Range::default(),
            props: Some(serde_json::json!({ "annotations": anns })),
        }
    }

    fn ann(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "attrs": {} })
    }

    fn ann_value(name: &str, value: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "attrs": { "value": value } })
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge {
            src: NodeId::new(src),
            dst: NodeId::new(dst),
            kind,
            confidence: 1.0,
            reason: "test".into(),
            props: None,
        }
    }

    /// Aspect + service + controller + non-bean helper + subtype interface.
    fn fixture() -> (Vec<Node>, Vec<Edge>) {
        let aspect_fqcn = "com.acme.aspect.LoggingAspect";
        let nodes = vec![
            class(aspect_fqcn, serde_json::json!([ann("Aspect"), ann("Component")])),
            method(
                &format!("{aspect_fqcn}#log/1"),
                serde_json::json!({ "annotations": [
                    ann_value("Around", "execution(* com.acme.service.*.*(..))")
                ]}),
            ),
            method(
                &format!("{aspect_fqcn}#audit/1"),
                serde_json::json!({ "annotations": [
                    serde_json::json!({ "name": "AfterReturning",
                        "attrs": { "pointcut": "loggableOps()" } })
                ]}),
            ),
            method(
                &format!("{aspect_fqcn}#loggableOps/0"),
                serde_json::json!({ "annotations": [
                    ann_value("Pointcut", "@annotation(Loggable)")
                ]}),
            ),
            method(
                &format!("{aspect_fqcn}#sub/1"),
                serde_json::json!({ "annotations": [
                    ann_value("Around", "execution(* com.acme.service.PaymentService+.*(..))")
                ]}),
            ),
            interface("com.acme.service.PaymentService"),
            class("com.acme.service.OrderService", serde_json::json!([ann("Service")])),
            method(
                "com.acme.service.OrderService#pay/2",
                serde_json::json!({
                    "annotations": [ann("Loggable")],
                    "paramTypes": ["String", "int"],
                    "returnType": "PaymentResult",
                }),
            ),
            method("com.acme.service.OrderService#refund/1", serde_json::json!({})),
            method("com.acme.service.OrderService#<init>/0", serde_json::json!({})),
            class("com.acme.web.OrderController", serde_json::json!([ann("RestController")])),
            method(
                "com.acme.web.OrderController#create/1",
                serde_json::json!({ "annotations": [ann("Loggable")] }),
            ),
            // Not a Spring bean — never advised, even by broad pointcuts.
            class("com.acme.util.Helper", serde_json::json!([])),
            method("com.acme.util.Helper#util/0", serde_json::json!({})),
        ];
        let edges = vec![edge(
            "Class:com.acme.service.OrderService",
            "Interface:com.acme.service.PaymentService",
            EdgeKind::Implements,
        )];
        (nodes, edges)
    }

    fn advises(edges: &[Edge]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Advises)
            .map(|e| (e.src.to_string(), e.dst.to_string()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn emits_exact_advises_edge_set() {
        let (nodes, mut edges) = fixture();
        let stats = emit_advises_edges(&nodes, &mut edges);
        assert_eq!(stats.aspects, 1);
        assert_eq!(stats.advice_methods, 3);
        let got = advises(&edges);
        let a = "Method:com.acme.aspect.LoggingAspect";
        assert_eq!(
            got,
            vec![
                // audit: @annotation(Loggable) — service and controller methods.
                (format!("{a}#audit/1"), "Method:com.acme.service.OrderService#pay/2".into()),
                (format!("{a}#audit/1"), "Method:com.acme.web.OrderController#create/1".into()),
                // log: execution over com.acme.service.* — pay + refund, no <init>.
                (format!("{a}#log/1"), "Method:com.acme.service.OrderService#pay/2".into()),
                (format!("{a}#log/1"), "Method:com.acme.service.OrderService#refund/1".into()),
                // sub: PaymentService+ subtype pattern reaches the implementor.
                (format!("{a}#sub/1"), "Method:com.acme.service.OrderService#pay/2".into()),
                (format!("{a}#sub/1"), "Method:com.acme.service.OrderService#refund/1".into()),
            ],
            "unexpected ADVISES edge set"
        );
        assert_eq!(stats.edges, 6);
        // Exact (non-approximate) matches carry full confidence + advice_kind.
        let log_edge = edges
            .iter()
            .find(|e| e.kind == EdgeKind::Advises && e.src.as_str().ends_with("#log/1"))
            .unwrap();
        assert_eq!(log_edge.confidence, 1.0);
        assert_eq!(log_edge.reason, "aop-around");
        let props = log_edge.props.as_ref().unwrap();
        assert_eq!(props["advice_kind"], "around");
        assert!(props.get("approximate").is_none());
    }

    #[test]
    fn no_aspects_is_a_noop() {
        let (nodes, _) = fixture();
        let nodes: Vec<Node> = nodes
            .into_iter()
            .filter(|n| !n.id.as_str().contains("LoggingAspect"))
            .collect();
        let mut edges = Vec::new();
        let stats = emit_advises_edges(&nodes, &mut edges);
        assert_eq!(stats.aspects, 0);
        assert!(edges.is_empty());
    }

    #[test]
    fn typed_params_and_return_are_checked_not_approximated() {
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        // Typed pointcut that positionally matches pay(String, int).
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#typed/1",
            serde_json::json!({ "annotations": [
                ann_value("Before", "execution(PaymentResult com.acme.service.*.pay(String, ..))")
            ]}),
        ));
        emit_advises_edges(&nodes, &mut edges);
        let typed: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Advises && e.src.as_str().ends_with("#typed/1"))
            .collect();
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].dst.as_str(), "Method:com.acme.service.OrderService#pay/2");
        // Declared types were available, so the match is exact.
        assert!(typed[0].props.as_ref().unwrap().get("approximate").is_none());
        assert_eq!(typed[0].confidence, 1.0);
    }

    #[test]
    fn unsupported_conjunct_degrades_to_approximate() {
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#mixed/1",
            serde_json::json!({ "annotations": [
                ann_value("Around", "execution(* com.acme.service.*.pay(..)) && args(String, ..)")
            ]}),
        ));
        emit_advises_edges(&nodes, &mut edges);
        let mixed: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Advises && e.src.as_str().ends_with("#mixed/1"))
            .collect();
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed[0].props.as_ref().unwrap()["approximate"], true);
        assert!(mixed[0].confidence < 1.0);
    }

    #[test]
    fn unsupported_top_level_skips_the_advice() {
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#rt/1",
            serde_json::json!({ "annotations": [
                ann_value("Around", "this(com.acme.service.PaymentService)")
            ]}),
        ));
        // `false || unknown` is unknown — a decidable-true disjunct would
        // legitimately match, but here the outcome rests on the runtime check.
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#orRt/1",
            serde_json::json!({ "annotations": [
                ann_value("Around", "within(com.acme.nowhere.*) || target(com.acme.Foo)")
            ]}),
        ));
        emit_advises_edges(&nodes, &mut edges);
        assert!(
            !edges.iter().any(|e| e.kind == EdgeKind::Advises
                && (e.src.as_str().ends_with("#rt/1") || e.src.as_str().ends_with("#orRt/1"))),
            "undecidable pointcuts must not emit edges"
        );
    }

    #[test]
    fn negated_unsupported_conjunct_degrades_to_approximate() {
        // `!args(...)` is undecidable, but as an `&&` conjunct it is ignored
        // and the surviving `within` match is flagged approximate.
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#neg/1",
            serde_json::json!({ "annotations": [
                ann_value("Around", "within(com.acme.web.*) && !args(String)")
            ]}),
        ));
        emit_advises_edges(&nodes, &mut edges);
        let neg: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Advises && e.src.as_str().ends_with("#neg/1"))
            .collect();
        assert_eq!(neg.len(), 1);
        assert_eq!(neg[0].props.as_ref().unwrap()["approximate"], true);
    }

    #[test]
    fn parameter_binding_resolves_via_advice_param_types() {
        // Fineract idiom: `@within(withFlushMode) || @annotation(withFlushMode)`
        // binds the advice parameter `WithFlushMode withFlushMode`.
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#flush/2",
            serde_json::json!({
                "annotations": [
                    ann_value("Around", "@within(withFlushMode) || @annotation(withFlushMode)")
                ],
                "paramTypes": ["ProceedingJoinPoint", "WithFlushMode"],
            }),
        ));
        // Class-level @WithFlushMode → every method of the bean is advised.
        nodes.push(class(
            "com.acme.service.FlushingService",
            serde_json::json!([ann("Service"), ann("WithFlushMode")]),
        ));
        nodes.push(method(
            "com.acme.service.FlushingService#persist/1",
            serde_json::json!({}),
        ));
        emit_advises_edges(&nodes, &mut edges);
        let flush: Vec<_> = advises(&edges)
            .into_iter()
            .filter(|(s, _)| s.ends_with("#flush/2"))
            .map(|(_, d)| d)
            .collect();
        assert_eq!(
            flush,
            vec!["Method:com.acme.service.FlushingService#persist/1".to_string()],
            "@within binding must reach the class-annotated bean's methods"
        );
    }

    #[test]
    fn unresolvable_binding_skips_the_advice() {
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#ghost/1",
            serde_json::json!({
                "annotations": [ann_value("Around", "@annotation(nothingBoundHere)")],
                "paramTypes": ["ProceedingJoinPoint"],
            }),
        ));
        emit_advises_edges(&nodes, &mut edges);
        assert!(
            !advises(&edges).iter().any(|(s, _)| s.ends_with("#ghost/1")),
            "a binding that matches no parameter is undecidable"
        );
    }

    #[test]
    fn bean_and_within_designators() {
        let (nodes, mut edges) = fixture();
        let mut nodes = nodes;
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#byBean/1",
            serde_json::json!({ "annotations": [ann_value("Before", "bean(orderService)")] }),
        ));
        nodes.push(method(
            "com.acme.aspect.LoggingAspect#byWithin/1",
            serde_json::json!({ "annotations": [ann_value("Before", "within(com.acme.web.*)")] }),
        ));
        emit_advises_edges(&nodes, &mut edges);
        let by_bean = advises(&edges)
            .into_iter()
            .filter(|(s, _)| s.ends_with("#byBean/1"))
            .count();
        assert_eq!(by_bean, 2, "bean(orderService) hits pay + refund");
        let by_within: Vec<_> = advises(&edges)
            .into_iter()
            .filter(|(s, _)| s.ends_with("#byWithin/1"))
            .collect();
        assert_eq!(
            by_within,
            vec![(
                "Method:com.acme.aspect.LoggingAspect#byWithin/1".to_string(),
                "Method:com.acme.web.OrderController#create/1".to_string()
            )]
        );
    }
}
