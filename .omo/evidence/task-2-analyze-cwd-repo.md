# Task 2: Parser/runtime tests for `cih-engine analyze` CLI behavior

## Test Results

### Tests Added to `crates/cih-engine/src/main.rs`

Three unit tests appended as `#[cfg(test)] mod tests { ... }` after all production code:

| Test | Input | Expected | Result |
|------|-------|----------|--------|
| `test_analyze_explicit_repo` | `["cih-engine", "analyze", "/tmp/repo", "--all"]` | `repo == Some(PathBuf::from("/tmp/repo"))` | ✅ PASS |
| `test_analyze_omitted_repo` | `["cih-engine", "analyze", "--all"]` | `repo == None` | ✅ PASS |
| `test_analyze_no_repo_and_no_scope` | `["cih-engine", "analyze"]` | `repo == None` (parse succeeds, scope gate is runtime) | ✅ PASS |

### Evidence: Cargo Test Output

```
$ cargo test -p cih-engine -- analyze_explicit analyze_omitted analyze_no_repo -- --nocapture
    Finished `test` profile [unoptimized + debuginfo] target(s) in 10.64s
     Running unittests src/main.rs (target/debug/deps/cih_engine-48c8cacdc0d807fb)

running 3 tests
test tests::test_analyze_omitted_repo ... ok
test tests::test_analyze_explicit_repo ... ok
test tests::test_analyze_no_repo_and_no_scope ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 19 filtered out; finished in 0.00s
```

### Evidence: Cargo Check

```
$ cargo check -p cih-engine
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.87s
```

### Test Source

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    /// Parsing `analyze /tmp/repo --all` should set repo to Some("/tmp/repo").
    #[test]
    fn test_analyze_explicit_repo() {
        let result = Cli::try_parse_from(["cih-engine", "analyze", "/tmp/repo", "--all"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        let cli = result.unwrap();
        match cli.command {
            Command::Analyze { repo, .. } => {
                assert_eq!(repo, Some(PathBuf::from("/tmp/repo")));
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }

    /// Parsing `analyze --all` (no repo) should keep repo as None (cwd fallback at runtime).
    #[test]
    fn test_analyze_omitted_repo() {
        let result = Cli::try_parse_from(["cih-engine", "analyze", "--all"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        let cli = result.unwrap();
        match cli.command {
            Command::Analyze { repo, .. } => {
                assert_eq!(repo, None, "repo should be None when omitted, got {repo:?}");
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }

    /// Parsing `analyze` (no repo, no --all) should succeed — scope gate is a runtime check.
    #[test]
    fn test_analyze_no_repo_and_no_scope() {
        let result = Cli::try_parse_from(["cih-engine", "analyze"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        let cli = result.unwrap();
        match cli.command {
            Command::Analyze { repo, .. } => {
                assert_eq!(repo, None, "repo should be None when omitted, got {repo:?}");
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }
}
```

## Verification

- ✅ `cargo test -p cih-engine -- analyze_explicit analyze_omitted analyze_no_repo -- --nocapture` — **all 3 pass**
- ✅ `cargo check -p cih-engine` — **passes**
- ✅ No production code modified
- ✅ No `Cargo.toml` dependencies added
- ✅ Tests use only `clap::Parser::try_parse_from` — no filesystem or FalkorDB required
