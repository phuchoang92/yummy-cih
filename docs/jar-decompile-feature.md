# JAR Decompile-First Feature

## Context

Banking codebases often ship internal libraries (MFA core, auth commons) as closed-source JARs.
CIH currently extracts only bytecode signatures from JARs — no method bodies, no internal call
graph. This means calls from app code into the JAR are partially resolved (class/method nodes
exist) but the JAR's own logic (how it calls MFA strategy, how it chains face-auth + OTP) is
invisible to graph queries, wiki enrichment, and taint analysis.

**Goal**: Before the analyze phase, decompile user-configured first-party JARs to `.java` source
and inject them as regular source files — giving CIH full call graph visibility into the library.
Users configure which JAR directories to scan and which filename prefixes to select (to avoid
decompiling third-party deps).

---

## Config Format — `cih.decompile.toml`

Follows the existing `cih.*.toml` naming pattern (same as `cih.taint.toml`, `cih.scope.toml`).
Place at repo root.

```toml
# cih.decompile.toml

tool     = "vineflower"              # "vineflower" (recommended) | "cfr" | "jadx"
tool_jar = "/opt/vineflower.jar"     # path to vineflower.jar or cfr.jar
# tool_bin = "/usr/local/bin/jadx"   # for "jadx" only
cache_dir = ".cih/decompiled" # where decompiled .java files live

[[sources]]
dir    = "target/lib"
prefix = "mfa-"               # decompiles mfa-core-2.1.jar, mfa-auth-1.0.jar
                              # skips  commons-lang3.jar, spring-core.jar

[[sources]]
dir    = "~/.m2/repository/com/bank"
prefix = "bank-"
```

**Prefix rule**: matches the JAR **filename** (not path).
- `prefix = "mfa-"` → selects `mfa-core-2.1.jar`, skips `commons-lang3-3.12.jar`
- Multiple JARs matching the prefix in one directory are all decompiled in parallel.

---

## Execution Order

Decompilation **must** run before the analyzer. The resolver needs every class in its
symbol table before it can emit cross-reference edges. Partial symbol tables produce
dropped or wrong edges.

```
cih analyze
  │
  ├─ Step 0 [NEW]: Decompile pre-step
  │     for each [[sources]] entry:
  │       walk dir/, keep files whose name starts_with(prefix)
  │     parallel decompile all matched JARs:
  │       cache_key = sha256(jar bytes)
  │       if .cih/decompiled/<cache_key>/ exists → cache hit, skip
  │       else → run CFR/jadx → .cih/decompiled/<cache_key>/
  │     inject all cache dirs as additional source directories
  │
  ├─ Step 1: parse Java files (decompiled .java treated as normal source)
  ├─ Step 2: resolve + build graph
  └─ Step 3: JAR bytecode extraction (remaining unresolved refs only)
```

**Cache**: On the second run, the decompile step is a hash check (~ms). Cost is only
paid on first run or when the JAR bytes change.

**Multiple JARs** are decompiled concurrently via `rayon::par_iter` — each JAR writes
to its own isolated cache directory, so there is no write contention.

---

## Data Structures

### `crates/cih-engine/src/decompile_config.rs` (new file)

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecompileConfig {
    pub tool: String,              // "cfr" | "jadx"
    pub tool_jar: Option<String>,  // path to cfr.jar
    pub tool_bin: Option<String>,  // path to jadx binary
    pub cache_dir: Option<String>, // default: ".cih/decompiled"
    pub sources: Vec<DecompileSource>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecompileSource {
    pub dir: String,    // directory to scan for JARs
    pub prefix: String, // JAR filename prefix filter
}

impl DecompileConfig {
    pub fn load_or_default(repo: &Path) -> Self  // reads cih.decompile.toml
    pub fn save(&self, repo: &Path) -> Result<()> // writes cih.decompile.toml
    pub fn resolved_cache_dir(&self, repo: &Path) -> PathBuf
    pub fn collect_jars(&self, repo: &Path) -> Vec<PathBuf> // walk + filter
}
```

### `crates/cih-engine/src/decompile.rs` (new file)

```rust
pub struct DecompileStats {
    pub jars_found: usize,
    pub jars_cached: usize,
    pub jars_decompiled: usize,
    pub jars_failed: usize,
    pub classes_written: usize,
}

/// Returns list of dirs containing decompiled .java files + stats.
pub fn run_decompile_precheck(
    repo: &Path,
    config: &DecompileConfig,
) -> Result<(Vec<PathBuf>, DecompileStats)>
```

**`run_one_jar`** (internal):
```rust
fn run_one_jar(jar: &Path, out_dir: &Path, config: &DecompileConfig) -> Result<usize> {
    match config.tool.as_str() {
        "cfr" => Command::new("java")
            .args(["-jar", tool_jar, jar, "--outputdir", out_dir])
            .status(),
        "jadx" => Command::new(tool_bin)
            .args(["-d", out_dir, jar])
            .status(),
    }
    // returns count of .java files written
}
```

Timeout: 60 seconds per JAR. Failure of one JAR logs a warning and continues.

---

## Config UI

### A) `cih config decompile` — interactive CLI command (primary)

Add to `Command` enum in `crates/cih-engine/src/main.rs`:

```rust
Config {
    #[command(subcommand)]
    command: ConfigCommand,
},

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Decompile {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}
```

New file `crates/cih-engine/src/config_cmd.rs` — uses `dialoguer` (already in deps):

```
$ cih config decompile

  Decompiler tool:   [cfr]  jadx
  Path to cfr.jar:   /opt/cfr.jar
  Cache directory:   .cih/decompiled

  Current sources:
    1. dir=target/lib   prefix=mfa-
    2. dir=target/lib   prefix=bank-auth-

  > Add source | Remove source | Done

  Saved to cih.decompile.toml ✓
```

### B) TUI integration (secondary — inside `cih ui`)

Add `"config"` entry to `make_commands()` in `crates/cih-engine/src/tui.rs`:

```rust
Cmd {
    name: "config",
    desc: "Edit CIH settings (decompile, taint, scope)",
    fields: vec![
        Field { flag: "--repo", label: "Repo", val: FieldVal::Text(".".into()), .. },
    ],
}
```

When confirmed, the TUI returns `["config", "decompile", "--repo", repo]` → main.rs
dispatches to `run_config_decompile()` which launches the interactive `dialoguer` editor.

---

## Pipeline Integration

### `crates/cih-engine/src/analyze/mod.rs`

Insert before `parse_scope()` — decompiled dirs must be on disk before the parser runs:

```rust
// ── Decompile pre-step ────────────────────────────────────────────────────
let decompile_cfg = DecompileConfig::load_or_default(&repo_root);
let extra_source_dirs = if !decompile_cfg.sources.is_empty() {
    ui.spin("Decompiling JARs");
    let (dirs, stats) = decompile::run_decompile_precheck(&repo_root, &decompile_cfg)?;
    ui.finish_with(format!(
        "{} decompiled, {} cached, {} failed",
        stats.jars_decompiled, stats.jars_cached, stats.jars_failed
    ));
    dirs
} else {
    vec![]
};
// ─────────────────────────────────────────────────────────────────────────

// Phase 2: parse — include decompiled dirs
let parse_output = parse_scope(
    &repo_root, &cih_dir,
    &scope_file.files,
    extra_source_dirs,  // ← new param
    cache,
)?;
```

Decompiled nodes get **no** `external: true` property — they are treated as ordinary
source-derived nodes, so the full call graph flows through them.

---

## Critical Files

| File | Action |
|---|---|
| `crates/cih-engine/src/decompile_config.rs` | **New** — `DecompileConfig`, `DecompileSource`, load/save/collect |
| `crates/cih-engine/src/decompile.rs` | **New** — `run_decompile_precheck`, `run_one_jar`, `DecompileStats` |
| `crates/cih-engine/src/config_cmd.rs` | **New** — `run_config_decompile` using `dialoguer` |
| `crates/cih-engine/src/main.rs` | Add `Config` + `ConfigCommand::Decompile` to `Command` enum |
| `crates/cih-engine/src/tui.rs` | Add `"config"` entry to `make_commands()` |
| `crates/cih-engine/src/analyze/mod.rs` | Insert decompile pre-step; pass extra dirs to `parse_scope` |
| `crates/cih-engine/src/lib.rs` | Declare new modules |

---

## Implementation Order

1. `decompile_config.rs` — config struct + load/save/collect  
2. `decompile.rs` — subprocess engine + parallel execution + caching  
3. `analyze/mod.rs` — wire pre-step into pipeline  
4. `config_cmd.rs` — interactive editor  
5. `main.rs` — `Config` subcommand dispatch  
6. `tui.rs` — add "config" to command list  

Steps 1–3 are independently testable before any UI is wired up.

---

## Verification

```bash
# Unit: config struct parsing + JAR collection
cargo test -p cih-engine -- decompile_config

# Unit: decompile engine with temp dir fixture
cargo test -p cih-engine -- decompile::tests

# Manual: interactive config editor
cargo run -p cih-engine -- config decompile --repo /path/to/banking-repo

# Manual: TUI flow
cargo run -p cih-engine -- ui
# → select "config" → confirm → interactive editor opens

# Full workspace
cargo test --workspace
```

**End-to-end on banking repo**:
1. Run `cih config decompile` → set `dir=target/lib`, `prefix=mfa-`
2. Run `cih analyze` → `.cih/decompiled/<hash>/` directories populated
3. Confirm `MfaStrategy` nodes appear **without** `external: true` in graph
4. Confirm `CALLS` edges from application code flow into JAR internal methods
5. Second run → cache hit, decompile step < 100ms
