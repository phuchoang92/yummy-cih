# Code Quality Review: analyze cwd repo

Verdict: **REJECT**

Reviewed diff:

```bash
GIT_MASTER=1 git diff -- crates/cih-engine/src/main.rs crates/cih-engine/src/tui.rs
```

Read requested context:
- `crates/cih-engine/src/main.rs`: lines 60-79, 521-570, 1000-1066
- `crates/cih-engine/src/tui.rs`: lines 185-219, 730-796

Verification:
- `cargo check -p cih-engine` succeeds.
- Existing workspace warnings remain in unrelated files/modules. No compiler warning in the touched `main.rs` / `tui.rs` hunks was observed.

## Blocking issue

1. **Production panic in cwd fallback** — `crates/cih-engine/src/main.rs:537-542`

   ```rust
   let repo = repo.unwrap_or_else(|| {
       std::env::current_dir()
           .with_context(|| {
               "failed to determine current working directory — pass an explicit repo path or run from a valid directory"
           })
           .unwrap()
   });
   ```

   The `.unwrap()` is not justified in production code. `main()` already returns `anyhow::Result<()>`, and `current_dir()` can fail when the process cwd is deleted or inaccessible. This should propagate with `?` instead of panicking, e.g. by matching `repo` and using `std::env::current_dir().with_context(...)?` in the `None` branch.

   `anyhow::Context` is therefore appropriate **only if** the error is propagated. In the current code it is syntactically used, but the error handling path still panics instead of returning a contextual `anyhow` error.

## Non-blocking issue

2. **TUI filled-repo test is too weak** — `crates/cih-engine/src/tui.rs:789-792`

   ```rust
   assert!(assembled.starts_with("cih-engine analyze /home/user/my-project"), ...);
   ```

   The empty-repo test properly checks the exact assembled command (`cih-engine analyze`) and ensures `<repo>` is omitted. The filled-repo test should likewise assert the exact assembled command. `starts_with` would still pass if extra unintended flags/arguments were appended, so it does not fully protect assembled command output.

   Consider also testing `run_args()` for the actual command arguments passed to process spawning; `assembled_command()` is the preview string, not the execution vector.

## Reviewed as acceptable

- CLI parser tests in `main.rs` are meaningful: they cover explicit repo, omitted repo with `--all`, and omitted repo without scope flags. These are directly relevant to the positional argument change and are not just tautological smoke tests.
- Test-only `unwrap()` / `panic!` usage is acceptable in the new test modules.
- Scope is focused: making `analyze` repo optional, updating TUI field requiredness/help text, and adding targeted parser/TUI tests. No obvious unrelated refactor or scope creep.

---

## Re-review: unwrap/match and assert_eq fixes (2026-06-30)

Verdict: **APPROVE**

Evidence read:
- `crates/cih-engine/src/main.rs:536-542` now uses an idiomatic `match repo { Some(r) => r, None => std::env::current_dir().with_context(...)? }`; the production `.unwrap()` in cwd fallback is gone and the contextual error is propagated with `?`.
- `crates/cih-engine/src/tui.rs:789-794` now uses `assert_eq!(assembled, "cih-engine analyze /home/user/my-project", ...)`; the weaker `starts_with` assertion is gone.

Required verification:

```bash
cargo test -p cih-engine 2>&1 | grep -E "(test result|FAILED)"
```

Result: all reported test suites were `test result: ok`; no `FAILED` line appeared.

Warnings: a warning-focused pass still reports the existing workspace warnings already noted above (e.g. unused imports/variables and dead code across `cih-lang`, `cih-community`, `cih-resolve`, `cih-taint`, and `cih-engine`). No warning is attributable to the two reviewed fix hunks.
