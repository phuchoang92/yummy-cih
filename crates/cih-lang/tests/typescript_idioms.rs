//! Conformance matrix: every JS/TS way of declaring a function, and whether we
//! extract it.
//!
//! # Why a matrix
//!
//! The CommonJS blind spot survived a green suite because nothing anywhere
//! enumerated *which* function-declaration idioms the parser understood. Support
//! was implicit — discoverable only by reading the walker — so "module-scope arrow
//! const" could be unsupported for as long as nobody happened to write that fixture.
//!
//! This table makes support explicit and reviewable. Each row is an idiom a JS
//! developer would call "declaring a function"; the row says whether it becomes a
//! `Function` node.
//!
//! # The ratchet
//!
//! `KnownGap` rows assert the idiom is **still not** extracted. That is deliberate:
//! when someone implements one, this test **fails**, and the fix is to move the row
//! to `Supported`. A gap can therefore never be quietly closed *or* quietly
//! regress — and the list of what we don't handle is always current, in code,
//! rather than living in someone's head.
//!
//! Pair this with `cih-engine/tests/corpus_coverage.rs`: the matrix pins idioms we
//! thought of; the corpus catches the ones we didn't.

use cih_core::NodeKind;
use cih_lang::typescript::TypescriptProvider;
use cih_lang::LanguageProvider;

#[derive(Clone, Copy, PartialEq)]
enum Support {
    /// Becomes a `Function` node today. Regressing this fails the test.
    Supported,
    /// Not extracted today. Implementing it fails this test — move the row up.
    KnownGap,
}
use Support::*;

struct Idiom {
    what: &'static str,
    rel: &'static str,
    src: &'static str,
    /// The function name we expect to find (or, for a gap, expect to be missing).
    name: &'static str,
    support: Support,
}

const MATRIX: &[Idiom] = &[
    Idiom {
        what: "function declaration",
        rel: "src/m.js",
        src: "function getUser(id) { return id; }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "class method",
        rel: "src/m.ts",
        src: "class Svc { getUser(id) { return id; } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "module-scope arrow const",
        rel: "src/m.js",
        src: "const getUser = async (id) => id;",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "function-expression const",
        rel: "src/m.js",
        src: "const getUser = function (id) { return id; };",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "higher-order wrapped const (catchAsync/memo/forwardRef)",
        rel: "src/m.js",
        src: "const getUser = catchAsync(async (req, res) => { return 1; });",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "exports.foo = arrow",
        rel: "src/m.js",
        src: "exports.getUser = async (req, res) => 1;",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "module.exports.foo = function",
        rel: "src/m.js",
        src: "module.exports.getUser = function (req) { return 1; };",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "exported arrow const (ESM)",
        rel: "src/m.ts",
        src: "export const getUser = (id: string): string => id;",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        // Reached via the `method_definition` walk arm — tree-sitter models an
        // object-literal method the same as a class method.
        what: "object-literal method (`{ getUser() {} }`)",
        rel: "src/m.js",
        src: "module.exports = { getUser(id) { return id; } };",
        name: "getUser",
        support: Supported,
    },
    // ── Known gaps ────────────────────────────────────────────────────────────
    // Real idioms we do not extract. Each costs call-graph coverage; each is a
    // candidate for the next round. Listed here so they are visible rather than
    // discovered the hard way on someone's repo.
    Idiom {
        what: "object-literal arrow property (`{ getUser: () => {} }`)",
        rel: "src/m.js",
        src: "module.exports = { getUser: async (id) => id };",
        name: "getUser",
        support: KnownGap,
    },
    Idiom {
        what: "class field arrow (`class X { getUser = () => {} }`)",
        rel: "src/m.ts",
        src: "class Svc { getUser = (id) => id; }",
        name: "getUser",
        support: KnownGap,
    },
    Idiom {
        what: "generator function declaration (`function* getUser() {}`)",
        rel: "src/m.js",
        src: "function* getUser() { yield 1; }",
        name: "getUser",
        support: KnownGap,
    },
];

fn emits_function(rel: &str, src: &str, name: &str) -> bool {
    let unit = TypescriptProvider::new()
        .parse_file(rel, src)
        .expect("fixture must parse");
    unit.parsed_file
        .defs
        .iter()
        .any(|d| matches!(d.kind, NodeKind::Function | NodeKind::Method) && d.name == name)
}

#[test]
fn function_declaration_idiom_matrix() {
    let mut broken = Vec::new();
    let mut newly_supported = Vec::new();

    for idiom in MATRIX {
        let emitted = emits_function(idiom.rel, idiom.src, idiom.name);
        match (idiom.support, emitted) {
            (Supported, false) => broken.push(idiom.what),
            (KnownGap, true) => newly_supported.push(idiom.what),
            _ => {}
        }
    }

    assert!(
        broken.is_empty(),
        "REGRESSION — these idioms used to produce a Function node and no longer do. \
         Real code uses them; dropping one silently guts the call graph:\n  - {}",
        broken.join("\n  - ")
    );
    assert!(
        newly_supported.is_empty(),
        "GOOD NEWS — these idioms are now extracted. Move them from `KnownGap` to \
         `Supported` so the matrix keeps telling the truth:\n  - {}",
        newly_supported.join("\n  - ")
    );
}

/// Things that merely *contain* or *return* a function must not be mistaken for a
/// function definition — the wrapper rule has to stay narrow or it invents nodes.
#[test]
fn non_definitions_are_not_extracted() {
    for (what, src, name) in [
        (
            "useMemo value",
            "const v = useMemo(() => compute(), [dep]);",
            "v",
        ),
        (
            "useCallback value",
            "const cb = useCallback(() => {}, [dep]);",
            "cb",
        ),
        ("require import", "const mod = require('./m');", "mod"),
        ("plain call result", "const app = express();", "app"),
        ("object literal", "const cfg = { a: 1 };", "cfg"),
    ] {
        assert!(
            !emits_function("src/m.js", src, name),
            "{what}: `{src}` must not produce a Function node named {name}"
        );
    }
}
