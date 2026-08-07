//! Conformance matrix: the Java ways of declaring a callable (method /
//! constructor), and whether we extract each as a `Method`/`Constructor` node.
//!
//! Mirrors `typescript_idioms.rs` — see its header for *why* an explicit
//! Supported/KnownGap table beats implicit support discoverable only by reading
//! the walker. This closes the same blind spot for Java: the extraction gaps
//! (below) are now visible in code rather than found the hard way on a real repo.
//!
//! # The ratchet (both directions)
//!
//! A `Supported` row that stops emitting is a REGRESSION and fails the test. A
//! `KnownGap` row that starts emitting is GOOD NEWS — the fix is to promote it to
//! `Supported` so the table keeps telling the truth. Pair with
//! `cih-engine/tests/corpus_coverage.rs`: this matrix pins idioms we thought of;
//! the corpus catches the ones we didn't.

use cih_core::NodeKind;
use cih_lang::java::JavaProvider;
use cih_lang::LanguageProvider;

#[derive(Clone, Copy, PartialEq)]
enum Support {
    /// Becomes a callable node today. Regressing this fails the test.
    Supported,
    /// Not extracted today. Implementing it fails this test — move the row up.
    KnownGap,
}
use Support::*;

struct Idiom {
    what: &'static str,
    src: &'static str,
    /// Callable name we expect to find (constructors are named `<init>`).
    name: &'static str,
    support: Support,
}

const MATRIX: &[Idiom] = &[
    Idiom {
        what: "instance method",
        src: "class Svc { public String getUser(long id) { return \"\"; } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "static method",
        src: "class Svc { static String getUser() { return \"\"; } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "constructor (named `<init>`)",
        src: "class Svc { Svc(long id) {} }",
        name: "<init>",
        support: Supported,
    },
    Idiom {
        what: "interface abstract method",
        src: "interface Repo { User getUser(long id); }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "interface default method",
        src: "interface Repo { default User getUser() { return null; } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "abstract method",
        src: "abstract class Base { abstract User getUser(); }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "generic method (`<T> T getUser(T)`)",
        src: "class Svc { <T> T getUser(T id) { return id; } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "enum-body method",
        src: "enum E { A; String getUser() { return \"\"; } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "inner (non-static) class method",
        src: "class Outer { class Inner { void getUser() {} } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "static nested class method",
        src: "class Outer { static class Inner { void getUser() {} } }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "anonymous class method",
        src: "class C { Runnable r = new Runnable() { public void getUser() {} }; }",
        name: "getUser",
        support: Supported,
    },
    // ── Known gaps ────────────────────────────────────────────────────────────
    // Real Java callables we do not extract. Each is a candidate for the next
    // round; listed here so the omission is visible and reviewable.
    Idiom {
        what: "record component accessor (implicit `name()`)",
        src: "record User(String getUser) {}",
        name: "getUser",
        support: KnownGap,
    },
    Idiom {
        what: "annotation element (`@interface { String value(); }`)",
        src: "@interface Ann { String getUser(); }",
        name: "getUser",
        support: KnownGap,
    },
];

fn emits_callable(src: &str, name: &str) -> bool {
    let unit = JavaProvider::new()
        .parse_file("Sample.java", src)
        .expect("fixture must parse");
    unit.nodes.iter().any(|n| {
        matches!(
            n.kind,
            NodeKind::Method | NodeKind::Constructor | NodeKind::Function
        ) && n.name == name
    })
}

#[test]
fn callable_declaration_idiom_matrix() {
    let mut broken = Vec::new();
    let mut newly_supported = Vec::new();

    for idiom in MATRIX {
        let emitted = emits_callable(idiom.src, idiom.name);
        match (idiom.support, emitted) {
            (Supported, false) => broken.push(idiom.what),
            (KnownGap, true) => newly_supported.push(idiom.what),
            _ => {}
        }
    }

    assert!(
        broken.is_empty(),
        "REGRESSION — these Java callable idioms used to produce a node and no longer do. \
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

/// Things that merely *hold* or *are* a value must not be mistaken for a callable
/// declaration — otherwise the extractor invents nodes and inflates the graph.
#[test]
fn non_callables_are_not_extracted() {
    for (what, src, name) in [
        ("field", "class C { int getUser = 1; }", "getUser"),
        (
            "local variable",
            "class C { void m() { int getUser = 1; } }",
            "getUser",
        ),
        (
            "lambda assigned to a field",
            "class C { Runnable getUser = () -> {}; }",
            "getUser",
        ),
    ] {
        assert!(
            !emits_callable(src, name),
            "{what}: `{src}` must not produce a callable named {name}"
        );
    }
}
