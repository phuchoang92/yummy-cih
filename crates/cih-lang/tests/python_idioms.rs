//! Conformance matrix: Python callable-declaration idioms and whether we extract
//! each as a callable node (Python `def`s surface as `Function` nodes, including
//! methods). See `typescript_idioms.rs` for *why* an explicit Supported/KnownGap
//! ratchet beats implicit support.
//!
//! The ratchet cuts both ways: a `Supported` row that stops emitting is a
//! REGRESSION; a `KnownGap` row that starts emitting is GOOD NEWS to promote.

use cih_core::NodeKind;
use cih_lang::python::PythonProvider;
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
        what: "module-level def",
        src: "def get_user(id):\n    return id\n",
        name: "get_user",
        support: Supported,
    },
    Idiom {
        what: "instance method",
        src: "class Svc:\n    def get_user(self):\n        return 1\n",
        name: "get_user",
        support: Supported,
    },
    Idiom {
        what: "@staticmethod",
        src: "class Svc:\n    @staticmethod\n    def get_user():\n        return 1\n",
        name: "get_user",
        support: Supported,
    },
    Idiom {
        what: "@classmethod",
        src: "class Svc:\n    @classmethod\n    def get_user(cls):\n        return 1\n",
        name: "get_user",
        support: Supported,
    },
    Idiom {
        what: "async def",
        src: "async def get_user():\n    return 1\n",
        name: "get_user",
        support: Supported,
    },
    Idiom {
        what: "nested def",
        src: "def outer():\n    def get_user():\n        return 1\n",
        name: "get_user",
        support: Supported,
    },
    Idiom {
        what: "@property",
        src: "class Svc:\n    @property\n    def get_user(self):\n        return 1\n",
        name: "get_user",
        support: Supported,
    },
    // ── Known gaps ────────────────────────────────────────────────────────────
    Idiom {
        what: "lambda bound to a name",
        src: "get_user = lambda x: x\n",
        name: "get_user",
        support: KnownGap,
    },
];

fn emits_callable(src: &str, name: &str) -> bool {
    let unit = PythonProvider::new()
        .parse_file("sample.py", src)
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
        "REGRESSION — these Python callable idioms used to be extracted and no longer are:\n  - {}",
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
        ("module variable", "get_user = 1\n", "get_user"),
        (
            "class attribute",
            "class C:\n    get_user = 1\n",
            "get_user",
        ),
    ] {
        assert!(
            !emits_callable(src, name),
            "{what}: `{src}` must not produce a callable named {name}"
        );
    }
}
