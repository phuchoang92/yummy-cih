//! Conformance matrix: Go callable-declaration idioms and whether we extract each
//! as a `Function`/`Method` node. See `typescript_idioms.rs` for *why* an explicit
//! Supported/KnownGap ratchet beats implicit support.
//!
//! The ratchet cuts both ways: a `Supported` row that stops emitting is a
//! REGRESSION; a `KnownGap` row that starts emitting is GOOD NEWS to promote.

use cih_core::NodeKind;
use cih_lang::go::GoProvider;
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
    name: &'static str,
    support: Support,
}

const MATRIX: &[Idiom] = &[
    Idiom {
        what: "function",
        src: "package main\nfunc getUser(id int) string { return \"\" }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "value-receiver method",
        src: "package main\nfunc (s Svc) getUser() string { return \"\" }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "pointer-receiver method",
        src: "package main\nfunc (s *Svc) getUser() string { return \"\" }",
        name: "getUser",
        support: Supported,
    },
    Idiom {
        what: "generic function (`func f[T any](T) T`)",
        src: "package main\nfunc getUser[T any](x T) T { return x }",
        name: "getUser",
        support: Supported,
    },
    // ── Known gaps ────────────────────────────────────────────────────────────
    Idiom {
        what: "interface method signature",
        src: "package main\ntype Repo interface { getUser() User }",
        name: "getUser",
        support: KnownGap,
    },
    Idiom {
        what: "func literal assigned to a package var",
        src: "package main\nvar getUser = func() {}",
        name: "getUser",
        support: KnownGap,
    },
];

fn emits_callable(src: &str, name: &str) -> bool {
    let unit = GoProvider::new()
        .parse_file("main.go", src)
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
        "REGRESSION — these Go callable idioms used to be extracted and no longer are:\n  - {}",
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
        ("package var", "package main\nvar getUser = 1", "getUser"),
        (
            "struct field",
            "package main\ntype S struct { getUser int }",
            "getUser",
        ),
    ] {
        assert!(
            !emits_callable(src, name),
            "{what}: `{src}` must not produce a callable named {name}"
        );
    }
}
