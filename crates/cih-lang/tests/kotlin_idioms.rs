//! Conformance matrix: Kotlin callable-declaration idioms and whether we extract
//! each as a `Function`/`Method`/`Constructor` node. See `typescript_idioms.rs`
//! for *why* an explicit Supported/KnownGap ratchet beats implicit support.
//!
//! The ratchet cuts both ways: a `Supported` row that stops emitting is a
//! REGRESSION; a `KnownGap` row that starts emitting is GOOD NEWS to promote.

use cih_core::NodeKind;
use cih_lang::kotlin::KotlinProvider;
use cih_lang::LanguageProvider;

#[derive(Clone, Copy, PartialEq)]
enum Support {
    Supported,
    KnownGap,
}
use Support::*;

struct Idiom {
    what: &'static str,
    src: &'static str,
    /// Callable name we expect (secondary constructors are named `<init>`).
    name: &'static str,
    support: Support,
}

const MATRIX: &[Idiom] = &[
    Idiom {
        what: "top-level function",
        src: "fun getUser(id: Long): String = \"\"",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "member function",
        src: "class Svc { fun getUser(id: Long): String = \"\" }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "extension function",
        src: "fun String.getUser(): String = this",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "interface method",
        src: "interface Repo { fun getUser(): User }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "abstract method",
        src: "abstract class Base { abstract fun getUser(): User }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "object-declaration method",
        src: "object Svc { fun getUser() = \"\" }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "suspend function",
        src: "suspend fun getUser(): String = \"\"",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "secondary constructor (`<init>`)",
        src: "class Svc { constructor(id: Long) {} }",
        name: "<init>",
        support: Supported,
    },
    // ── Known gaps ────────────────────────────────────────────────────────────
    Idiom {
        what: "companion-object function",
        src: "class Svc { companion object { fun getUser() = \"\" } }",
        name: "getUser",
        support: KnownGap,
    },
    Idiom {
        what: "local (nested) function",
        src: "fun outer() { fun getUser() = \"\" }",
        name: "getUser",
        support: KnownGap,
    },
];

fn emits_callable(src: &str, name: &str) -> bool {
    let unit = KotlinProvider::new()
        .parse_file("Sample.kt", src)
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
        match (idiom.support, emits_callable(idiom.src, idiom.name)) {
            (Supported, false) => broken.push(idiom.what),
            (KnownGap, true) => newly_supported.push(idiom.what),
            _ => {}
        }
    }

    assert!(
        broken.is_empty(),
        "REGRESSION — these Kotlin callable idioms used to be extracted and no longer are:\n  - {}",
        broken.join("\n  - ")
    );
    assert!(
        newly_supported.is_empty(),
        "GOOD NEWS — these idioms are now extracted; move them from `KnownGap` to `Supported`:\n  - {}",
        newly_supported.join("\n  - ")
    );
}

#[test]
fn non_callables_are_not_extracted() {
    for (what, src, name) in [
        ("top-level val", "val getUser = 1", "getUser"),
        (
            "class property",
            "class C { val getUser: Int = 1 }",
            "getUser",
        ),
    ] {
        assert!(
            !emits_callable(src, name),
            "{what}: `{src}` must not produce a callable named {name}"
        );
    }
}
