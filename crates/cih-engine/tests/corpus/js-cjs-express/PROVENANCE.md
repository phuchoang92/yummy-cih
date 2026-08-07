# Corpus: js-cjs-express

A vendored snapshot of real, third-party CommonJS source used as an
**extraction-coverage corpus**. It is test data — not part of the build, never
compiled, and never shipped in a binary.

| | |
|---|---|
| Upstream | https://github.com/hagopj13/node-express-boilerplate |
| Commit | `179ae84efec61b14206d0305d941daed6c6d07f9` |
| Vendored | `src/` only (38 `.js` files) |
| License | MIT — see [LICENSE](./LICENSE) (© 2019 Hagop Jamkojian) |

## Why this exists

Hand-written fixtures only ever contain the idioms their author already had in
mind — which is exactly how a parser blind spot survives a green test suite. This
corpus is **real code we did not write**, so it exercises idioms nobody enumerated.

It was added after a CommonJS resolver fix passed every unit test and then produced
**zero** improvement on this repo (10 resolved edges → 10, 1837 unresolved → 1837).
The fix was correct; the tests were self-confirming. This corpus is the ruler that
would have caught it on day one.

## What it pins

`crates/cih-engine/tests/corpus_coverage.rs` runs the real `scan → analyze` chain
over this tree and asserts:

- **callable coverage** — emitted `Function`/`Method` nodes ÷ syntactic callables in
  the AST. Catches idioms the parser silently skips (the failure mode above).
- **resolved-call ratio** — resolved edges ÷ (resolved + unresolved refs).
- **baseline counts** within tolerance, to catch regressions.

## Idioms it happens to cover

Chosen because its shape matches the repos CIH targets (Express + `require`,
`controllers/` → `services/`), and because it is dominated by idioms our
hand-written fixtures missed:

- `const f = async () => {}` module-scope arrow consts (**49 of them; zero `function`
  declarations**)
- `const h = catchAsync(async (req, res) => {})` — higher-order wrappers
- `const { userService } = require('../services')` — destructure used as a *receiver*
- `module.exports.userService = require('./user.service')` — barrel re-exports
- `require('../services')` — directory imports resolving to `index.js`

## Updating

Re-vendoring means re-recording baselines. Keep the snapshot pinned to the commit
above; a moving target defeats regression detection.
