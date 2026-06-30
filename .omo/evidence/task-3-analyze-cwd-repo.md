# Task 3: Analyze CWD Repo — Evidence

## Change

**File**: `crates/cih-engine/src/tui.rs` (line ~196)

Changed the `analyze` command's `repo` positional field from:
```rust
Field::text("", "repo", "Absolute path to the Java/Spring repository root.", "/path/to/java-project", true),
```
to:
```rust
Field::text("", "repo", "Repository root. Leave blank to use the current directory.", "/path/to/java-project", false),
```

This makes the repo field **optional** — when blank, the `to_shell_fragment()` method returns `None` (no `<repo>` placeholder), which means `assembled_command()` produces `cih-engine analyze` without a positional arg. The CLI then resolves the current directory at dispatch time.

No other commands (`scan`, `discover`, `embed`, `wiki`) were changed — their repo fields remain `required: true`.

## Tests Added

`#[cfg(test)] mod tests` at end of `tui.rs`:

| Test | Assertion |
|------|-----------|
| `analyze_empty_repo_omits_positional_arg` | Empty repo field → `assembled_command()` returns `"cih-engine analyze"` (no `<repo>`) |
| `analyze_filled_repo_includes_explicit_path` | Filled repo field → `assembled_command()` returns string starting with `"cih-engine analyze /home/user/my-project"` |

## Test Results

All 21 lib tests pass, including:
- `tui::tests::analyze_empty_repo_omits_positional_arg ... ok`
- `tui::tests::analyze_filled_repo_includes_explicit_path ... ok`

All 24 binary tests pass (same tests compiled into `src/main.rs`).

Full test suite: **107 tests pass, 0 fail** across all test targets.
