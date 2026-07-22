#!/usr/bin/env python3
"""Enforce the crate layer rules documented in the root Cargo.toml.

Each crate may depend only on crates in its own layer or layers ABOVE it
(lower layer number = closer to the foundation). Run from the repo root:

    python3 scripts/check_layering.py

Exits non-zero naming every violating dependency edge.
"""

import re
import sys
from pathlib import Path

# Layer map — keep in sync with the diagram at the top of the root Cargo.toml.
LAYERS = {
    # Foundation
    "cih-core": 0,
    # Language
    "cih-lang": 1,
    "cih-parse": 1,
    "cih-jar": 1,
    # Analysis
    "cih-resolve": 2,
    "cih-community": 2,
    "cih-search": 2,
    "cih-embed": 2,
    "cih-taint": 2,
    "cih-patterns": 2,
    # Storage
    "cih-graph-store": 3,
    "cih-falkor": 3,
    "cih-ladybug": 3,
    "cih-store-factory": 3,
    # Product
    "cih-engine": 4,
    "cih-wiki": 4,
    "cih-grouping": 4,
    "cih-server": 4,
}

DEP_RE = re.compile(r'^\s*"?(cih-[a-z-]+)"?\s*(?:=|\.workspace)')


def crate_deps(manifest: Path) -> list[str]:
    deps: list[str] = []
    in_deps = False
    for line in manifest.read_text().splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            # [dependencies], [dev-dependencies], [build-dependencies] all count.
            in_deps = "dependencies" in stripped
            continue
        if in_deps:
            m = DEP_RE.match(line)
            if m:
                deps.append(m.group(1))
    return deps


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    violations: list[str] = []
    seen: set[str] = set()

    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        crate = manifest.parent.name
        seen.add(crate)
        if crate not in LAYERS:
            violations.append(
                f"{crate}: not in scripts/check_layering.py LAYERS map — add it"
            )
            continue
        for dep in crate_deps(manifest):
            if dep == crate:
                continue
            if dep not in LAYERS:
                violations.append(
                    f"{crate} -> {dep}: dep not in LAYERS map — add it"
                )
            elif LAYERS[dep] > LAYERS[crate]:
                violations.append(
                    f"{crate} (layer {LAYERS[crate]}) -> {dep} (layer {LAYERS[dep]}): "
                    "depends on a layer below it"
                )

    for crate in sorted(set(LAYERS) - seen):
        violations.append(f"{crate}: in LAYERS map but crates/{crate}/ not found")

    if violations:
        print("crate layering violations:")
        for v in violations:
            print(f"  - {v}")
        return 1
    print(f"crate layering OK ({len(seen)} crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
