# Add `grep_files` MCP Tool

## Context

`read_file` reads a known file by path+line range. The MCP server has no tool for searching
arbitrary text across files — comments, `TODO`s, annotation values, string literals, inline
SQL, etc. `search_code` only covers graph-indexed symbols; free-form text in comments is
never stored. This adds a `grep_files` tool: gitignore-aware, regex-capable file search over
the live repo filesystem, so agents can find anything not captured by the parser.

## Files to Change

### 1. Root `Cargo.toml` + `crates/cih-server/Cargo.toml`

`ignore` and `globset` are already workspace deps used by `cih-engine`. `regex` is not yet.

**Root `Cargo.toml`** (`[workspace.dependencies]`):
```toml
regex = "1"
```

**`crates/cih-server/Cargo.toml`**:
```toml
globset.workspace = true
ignore.workspace = true
regex.workspace = true
```

### 2. `crates/cih-server/src/args.rs`

Add `GrepFilesArgs` following the same `Deserialize + JsonSchema` pattern as `ReadFileArgs`:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepFilesArgs {
    /// Regex pattern to search for (e.g. "TODO|FIXME", "^import ", "@Deprecated").
    pub pattern: String,
    /// Glob to filter files (e.g. "**/*.java", "src/**/*.rs").
    /// Leave empty to search all non-ignored files.
    #[serde(default)]
    pub glob: String,
    /// Repo name or absolute path from the registry. Leave empty for the server's active repo.
    #[serde(default)]
    pub repo: String,
    /// Max matches to return (default 200, capped at 1000; pass 0 for default).
    #[serde(default)]
    pub limit: usize,
}
```

### 3. `crates/cih-server/src/files.rs`

Split like `read_file`/`read_sliced`: a thin async wrapper that resolves the repo and
validates args, plus a pure, unit-testable core.

```rust
pub async fn grep_files(graph_key: &str, args: GrepFilesArgs) -> Result<CallToolResult, McpError>
fn grep_dir(root: &Path, regex: &Regex, glob: Option<&GlobSet>, limit: usize) -> GrepOutcome
```

**Wrapper (async, cheap):**

1. `find_repo_path(repo, graph_key)` — same as `read_file`
2. Compile `args.pattern` into `regex::Regex`; return `invalid_params` if invalid
3. If `args.glob` is non-empty, compile to a `globset::GlobSet` for O(1) per-file matching
4. `let limit = if args.limit == 0 { 200 } else { args.limit }.min(1000);`
5. Run `grep_dir` inside `tokio::task::spawn_blocking` — a repo-wide walk is sync I/O
   and must not block the tokio worker (same pattern as `taint.rs:98`, `browser.rs:154`)

**`grep_dir` (sync core):**

1. Walk with `ignore::WalkBuilder`, full engine settings (`cih-engine/src/scan/walk.rs:18-35`):
   `.hidden(false).git_ignore(true).git_exclude(true).git_global(true).add_custom_ignore_filename(".cihignore")`.
   The `ignore` crate only honors `.gitignore` inside a git repo, and sources copied into
   Docker volumes have no `.git` — so also add an inline `filter_entry` skip list for the
   usual build/vendor dirs: `target`, `node_modules`, `build`, `dist`, `.git`.
   (`cih-server` has no `cih-engine` dep, so `ignore_rules::should_ignore_dir` can't be
   reused — this small list is the copy.)
2. Per file:
   - Skip if `entry.path_is_symlink()` — the walker doesn't follow symlinks, so this is
     the only escape hatch out of the root; skipping is cheaper than canonicalizing
     every file (no per-file containment check needed)
   - If glob set: skip if repo-relative path doesn't match
   - Skip files larger than ~2 MB (stat before read — keeps stray artifacts out of memory)
   - Read as bytes; skip if contains `\0` (binary heuristic)
   - Decode with `String::from_utf8_lossy` (invalid bytes become U+FFFD)
   - Scan lines with `regex.find(line)` — collect `{ file: String, line: u32, text: String }`,
     truncating `text` to 500 chars (one minified single-line file must not flood the
     agent's context)
   - Break outer loop once `matches.len() >= limit`

**Response** via `json_result` (`matches_returned` is the returned count, not a repo
total — when `truncated` is true it always equals `limit`):

```json
{
  "pattern": "TODO",
  "glob": "**/*.java",
  "matches_returned": 50,
  "truncated": true,
  "matches": [
    { "file": "src/main/java/Foo.java", "line": 12, "text": "  // TODO fix this" }
  ]
}
```

### 4. `crates/cih-server/src/app.rs`

**a)** Add `GrepFilesArgs` to the existing args import at the top.

**b)** Add `#[tool]` method inside the `#[tool_router]` block, right after `read_file`:

```rust
#[tool(
    description = "Search for a regex pattern across source files in the repo. \
        Use this to find comments, TODOs, annotations, string literals, or any \
        text not captured by the graph index. Prefix the pattern with (?i) for \
        case-insensitive search. `glob` filters by file path \
        (e.g. \"**/*.java\", \"src/**/*.rs\"). Returns up to `limit` matches (default 200)."
)]
async fn grep_files(
    &self,
    Parameters(args): Parameters<GrepFilesArgs>,
) -> Result<CallToolResult, McpError> {
    files::grep_files(&self.graph_key, args).await
}
```

**c)** Add `grep_files` to the tool list in `get_info()` instructions string.

## What to Reuse (don't reinvent)

| Reuse | Location |
|-------|----------|
| `find_repo_path()` | `crates/cih-server/src/symbol.rs:66` |
| `json_result()` | `crates/cih-server/src/utils.rs` |
| `WalkBuilder` settings | `crates/cih-engine/src/scan/walk.rs:18-35` (copy settings; the crate isn't a dep) |
| `spawn_blocking` pattern | `crates/cih-server/src/taint.rs:98`, `browser.rs:154` |
| Wrapper/core test split | `read_file`/`read_sliced` in `crates/cih-server/src/files.rs` |

## Verification

**Unit tests** (in `files.rs`, mirroring the `read_sliced` tests — `grep_dir` over a temp
dir, no registry needed):

- match found with correct file/line/text
- glob filter excludes non-matching files
- limit truncation sets `truncated: true` and returns exactly `limit` matches
- binary file (contains `\0`) is skipped
- long matched line is truncated to the text cap
- invalid regex returns `invalid_params` (wrapper-level test)

```bash
cargo build -p cih-server
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Manual smoke-test (server running, MCP client connected):
# Find all TODOs in Java files
grep_files(pattern="TODO", glob="**/*.java", limit=50)

# Find @Deprecated usages anywhere
grep_files(pattern="@Deprecated")

# Invalid regex must return a clear invalid_params error
grep_files(pattern="[unclosed")
```
