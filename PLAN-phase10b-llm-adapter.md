# PLAN — Phase 10b: Adapter-Based LLM Wiki Enrichment

## Status: DRAFT

## Context

Current state (`wiki_cmd.rs`):
- Provider selected by `base_url.contains("anthropic.com")` — fragile, implicit.
- Evidence pack: community name, up to 5 routes, stereotypes, callers, callees — minimal.
- Prompt output parsed by JSON bracket extraction; bails on failure, leaving community unenriched silently.
- `max_tokens` hardcoded to 400.
- API key: `CIH_LLM_API_KEY → OPENAI_API_KEY → ANTHROPIC_API_KEY`.
- No BRD/external evidence.

This phase makes the provider explicit, the evidence pack richer, and adds a generic `http-json` adapter for local/custom models.

---

## Decisions (all ambiguous points resolved)

| Topic | Decision |
|---|---|
| Provider detection | Explicit `--llm-provider` flag; remove `base_url.contains("anthropic.com")` |
| Local/custom model support | Adapters receive `Option<&str>` API keys; `http-json` supports no-auth local models plus optional `{{api_key}}` and `{{env:VAR}}` substitution |
| `http-json` template format | JSON-value body template with safe placeholder substitution; dotted response path e.g. `response.0.text` |
| Structured output fallback | After retries exhausted: log warn, record failure in manifest, write no LLM content for that community (current empty-map behavior, made explicit) |
| Source snippets portability | Read source files only when repo is present at wiki time; skip snippet evidence silently if files are absent; reject absolute or escaping paths |
| `--llm-api-key-env VAR` | Replaces the fallback chain when provided; missing key fails only for adapters/configs that require a key |
| `--wiki-language` | Ship `en` and `vi` only; drop `auto` from this phase |
| `.docx` support | Deferred; ship `.md` and `.txt` only in this phase |
| BRD matching | Keyword match on route paths, class names, feature name; require ≥ 2 distinct term hits before including a chunk; cap at 2 chunks per community |
| BRD path segment extraction | Split route path on `/`; filter empty, `api`, `v\d+`, and `{...}` segments. `/api/v1/orders/{id}/cancel` → `["orders", "cancel"]` |
| Citation markers in output | Model-inserted `[R1]`, `[T1]` etc. pass through verbatim into rendered pages — intentional inline evidence references, do not strip |
| `LlmAdapter` crate home | `cih-engine` only — it is an I/O concern; `cih-wiki` stays pure data |
| `max_tokens` | Configurable via `--llm-max-tokens` (default 600, up from 400) |
| `--llm-base-url` scope | Meaningful for `openai-compatible` (default `https://api.openai.com/v1`) and `anthropic` (default `https://api.anthropic.com/v1`) only; ignored for `http-json` — URL comes from config file. Emit `tracing::warn!` when `--llm-base-url` is set alongside `--llm-provider http-json` |
| `language` in `LlmRequest` | Not a field of `LlmRequest`; baked into `LlmRequest.system` by the prompt builder before the struct is constructed |
| `failed_community_ids` content | Community names (from `graph.community_name(id)`), not raw `Community:N` IDs |
| Manifest migration | `WikiStats.llm_model: Option<String>` is removed; replaced by `llm: Option<WikiLlmInfo>`. Old manifests still deserialize (both optional). Manifest unit tests that assert `llm_model` must be updated |

---

## Files to Create / Modify

| File | Change |
|---|---|
| `crates/cih-engine/src/llm/mod.rs` | New — `LlmAdapter` trait + `LlmRequest` / `LlmResponse` types |
| `crates/cih-engine/src/llm/openai.rs` | New — OpenAI-compatible adapter (extracted from `wiki_cmd.rs`) |
| `crates/cih-engine/src/llm/anthropic.rs` | New — Anthropic adapter (extracted from `wiki_cmd.rs`) |
| `crates/cih-engine/src/llm/http_json.rs` | New — generic adapter with config file |
| `crates/cih-engine/src/llm/evidence.rs` | New — evidence pack builder |
| `crates/cih-engine/src/wiki_cmd.rs` | Modify — wire new flags, replace inline HTTP calls with adapters |
| `crates/cih-engine/src/main.rs` | Modify — add new CLI flags |
| `crates/cih-wiki/src/lib.rs` | Modify — pass LLM metadata into `WikiInput` |
| `crates/cih-wiki/src/manifest.rs` | Modify — add top-level `llm` metadata object |

---

## Step 1 — `LlmAdapter` trait (`crates/cih-engine/src/llm/mod.rs`)

```rust
pub mod anthropic;
pub mod evidence;
pub mod http_json;
pub mod openai;

pub struct LlmRequest {
    pub system: String,
    pub user: String,
    pub model: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}

pub struct LlmResponse {
    pub text: String,
}

pub trait LlmAdapter: Send + Sync {
    fn call(&self, api_key: Option<&str>, req: &LlmRequest) -> anyhow::Result<LlmResponse>;
}

pub fn make_adapter(
    provider: &str,
    base_url: &str,
    provider_config: Option<&str>,
) -> anyhow::Result<Box<dyn LlmAdapter>> {
    match provider {
        "openai-compatible" => Ok(Box::new(openai::OpenAiAdapter::new(base_url))),
        "anthropic"         => Ok(Box::new(anthropic::AnthropicAdapter::new(base_url))),
        "http-json"         => {
            let config_path = provider_config.ok_or_else(||
                anyhow::anyhow!("--llm-provider http-json requires --llm-provider-config <path>")
            )?;
            Ok(Box::new(http_json::HttpJsonAdapter::load(config_path)?))
        }
        other => anyhow::bail!("unknown --llm-provider '{}'; expected openai-compatible | anthropic | http-json", other),
    }
}
```

Authentication rules:
- `openai-compatible` requires an API key and returns a clear error if missing.
- `anthropic` requires an API key and returns a clear error if missing.
- `http-json` does not require an API key unless the config uses `{{api_key}}` in a header or body template.

---

## Step 2 — `http-json` config format (`crates/cih-engine/src/llm/http_json.rs`)

Config file is JSON:

```json
{
  "url": "http://localhost:11434/api/generate",
  "headers": {
    "Content-Type": "application/json"
  },
  "body_template": {
    "model": "{{model}}",
    "prompt": "{{prompt}}",
    "stream": false
  },
  "response_path": "response"
}
```

Template substitution is applied inside JSON string values, not by string-splicing raw JSON. Supported variables:

| Variable | Contains |
|---|---|
| `{{system}}` | System instruction string (safety rules + language directive). If the config omits `{{system}}`, emit `tracing::warn!` — system instructions are silently dropped |
| `{{prompt}}` | User evidence turn only (module name + evidence block) |
| `{{model}}` | Value of `--llm-model` |
| `{{max_tokens}}` | Value of `--llm-max-tokens` as a **JSON number** (see below) |
| `{{api_key}}` | Resolved API key |
| `{{env:VAR}}` | Value of environment variable `VAR` |

If a string value is exactly `{{max_tokens}}`, substitute a JSON number, not a string. Other substitutions produce strings. This keeps `"max_tokens": "{{max_tokens}}"` valid for APIs that require a numeric value.

`response_path`: dot-separated path into the response JSON, e.g. `choices.0.message.content` or `response`.  
If the path doesn't resolve, return an error (do not silently return empty string).

```rust
pub struct HttpJsonConfig {
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body_template: serde_json::Value,
    pub response_path: String,
}
```

Header values may also use `{{api_key}}` or `{{env:VAR}}`, for example:

```json
{
  "headers": {
    "Authorization": "Bearer {{api_key}}",
    "Content-Type": "application/json"
  }
}
```

If `{{api_key}}` is present and no key is resolved, return a clear error. If `{{env:VAR}}` is present and `VAR` is unset, return a clear error. If no auth placeholders are present, allow the request with no key so local APIs work.

---

## Step 3 — Evidence pack (`crates/cih-engine/src/llm/evidence.rs`)

### Sources (in priority order)

1. **Routes** — all routes for community (no cap)
2. **Stereotypes** — class stereotypes from graph
3. **Callers / callees** — community names (all)
4. **DB tables** — from `community_db_tables` (read/write flags)
5. **Events** — published/subscribed topics
6. **Source snippets** — up to 3 snippets, 10 lines each, from the member files with the highest method count; skip silently if repo root is not provided or file absent; only read normalized repo-relative paths under the repo root
7. **BRD chunks** — from `--evidence` files; include a chunk only when ≥ 2 distinct terms from (route path segments, primary class name, feature name, community name) match; cap at 2 chunks per community

### Size cap

Total evidence string ≤ 3 000 characters per community. Apply truncation in this order until the budget is met:

1. Drop trailing BRD chunks (2 → 1 → 0)
2. Drop source snippets (3 → 2 → 1 → 0)
3. Truncate callers list to first 10 entries, then callees list to first 10 entries
4. Truncate routes list to first 10 entries

Log a `tracing::debug!` message for each truncation applied, stating which source was truncated and the resulting character count.

### Source snippet safety and determinism

Only read files whose graph `file` field is a relative path that stays under the repo root after normalization. Reject absolute paths and paths containing escaping `..` segments. When choosing snippets, sort by descending method count, then path, then start line so output is deterministic.

### Evidence IDs

Each evidence item gets a short label (`R1`, `R2` for routes; `T1` for tables; `E1` for events; `S1`, `S2` for snippets; `B1`, `B2` for BRD chunks).  
The prompt instructs the model to cite only these IDs and avoid inventing behavior not in the evidence.

### BRD file loading

Supported formats: `.md`, `.txt`.  
Split into chunks of ≤ 400 characters at paragraph boundaries (double newline `\n\n`; fall back to single newline if a paragraph exceeds the limit).  
`.docx` deferred to next phase.

### BRD route path segment extraction

Split the route path on `/`, then discard: empty segments, the literal `api`, version segments matching `v\d+`, and path parameters matching `\{[^}]+\}`. The remaining tokens form the match term set.

Example: `/api/v1/orders/{id}/cancel` → `["orders", "cancel"]`

---

## Step 4 — Prompt and output format

### System prompt (new — currently no system prompt)

```
You are a code documentation assistant. Write only from the provided evidence.
Do not invent behavior, routes, tables, or class names not in the evidence.
Cite evidence IDs (R1, T1, S1, B1, ...) when they support a claim.
```

### User prompt (replaces `build_enrich_prompt`)

```
Module: "{name}"

Evidence:
[R1] GET /api/v1/orders
[R2] POST /api/v1/orders/{id}/cancel
[T1] ORDERS (read+write)
[S1] OrderService.java:45-54
    public OrderSummary findById(Long id) { ... }
[B1] BRD §3.2: "The order module handles cancellation within 24 hours."

Write exactly this JSON:
{
  "po": "<2-3 sentences, plain business language, cite evidence IDs>",
  "ba": "<2-3 sentences, workflows and contracts, cite evidence IDs>",
  "dev": "<2-3 sentences, technical structure, cite evidence IDs>"
}
Output only the JSON object.
```

### Citation markers in rendered output

Evidence ID citations (`[R1]`, `[T1]`, etc.) that the model embeds in `po`/`ba`/`dev` strings are passed through verbatim into the rendered Markdown pages. This is intentional — they serve as inline evidence references. Do not strip them.

### Fallback on parse failure

If `parse_llm_summary` fails after retries: return `Err(...)`, the caller logs `tracing::warn!` and records `graph.community_name(id).to_string()` (not the raw `Community:N` ID) in the `failed_community_ids` list. The community's pages are written with graph-only content (no LLM sections). Failure count goes into `manifest.json`.

---

## Step 5 — New CLI flags (`crates/cih-engine/src/main.rs`)

```
--llm-provider <openai-compatible|anthropic|http-json>   [default: openai-compatible]
--llm-provider-config <path>                              required when provider=http-json
--llm-api-key-env <VAR>                                   optional explicit key env var; required only when selected adapter/config needs a key
--evidence <path>                                          repeatable; .md or .txt BRD files
--llm-max-tokens <n>                                       [default: 600]
--wiki-language <en|vi>                                    [default: en]
```

Keep (unchanged):  
`--llm`, `--llm-enrich` (hidden alias), `--llm-model`, `--llm-timeout-secs`, `--llm-retries`, `--llm-concurrency`, `--llm-dry-run`

`--llm-base-url` — kept, but scope is now explicit:
- `openai-compatible`: used; default `https://api.openai.com/v1`
- `anthropic`: used; default `https://api.anthropic.com/v1`
- `http-json`: **ignored** (URL comes from config file); emit `tracing::warn!` if set alongside `http-json`

`--llm-debug-evidence` — updated behaviour: prints the full assembled evidence pack per community (all items with evidence IDs, total character count, any truncation applied) then exits without calling the LLM. Useful for debugging BRD matching and snippet selection.

Remove: the implicit Anthropic detection inside `enrich_one_community`.

### `EnrichConfig` struct

To avoid a 12-argument function signature, introduce:

```rust
pub struct EnrichConfig<'a> {
    pub adapter:        &'a dyn LlmAdapter,
    pub api_key:        Option<&'a str>,
    pub model:          &'a str,
    pub max_tokens:     u32,
    pub timeout_secs:   u64,
    pub retries:        u32,
    pub dry_run:        bool,
    pub repo_root:      Option<&'a std::path::Path>,
    pub evidence_files: &'a [std::path::PathBuf],
    pub language:       &'a str,
}

fn enrich_one_community(
    community: &Node,
    graph: &WikiGraph,
    cfg: &EnrichConfig<'_>,
) -> Result<CommunityLlmSummary>
```

`language` is consumed by the prompt builder to construct `LlmRequest.system`; it is not a field on `LlmRequest`.

### Known limitation — retry sleep blocks rayon slot

`std::thread::sleep` inside a rayon thread blocks that slot for the duration of the backoff. With `--llm-concurrency 8` and 2 retries, a burst of rate-limit failures can idle most of the pool for up to 1.5 s per failing community. Acceptable for current batch sizes; revisit if concurrency is increased significantly.

### API key resolution (new)

```rust
let api_key: Option<String> = if let Some(var) = llm_api_key_env {
    Some(std::env::var(&var).with_context(|| {
        format!("--llm-api-key-env: env var '{}' is unset", var)
    })?)
} else {
    std::env::var("CIH_LLM_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .ok()
};
```

Adapters decide whether `api_key` is required. This allows no-auth local providers through `http-json`.

---

## Step 6 — Manifest extension (`crates/cih-wiki/src/manifest.rs`)

Do not add LLM provider fields to `WikiStats`; that would duplicate the current top-level `llm_model` shape and would serialize noisy null/default fields in graph-only mode.

Remove `WikiStats.llm_model: Option<String>` and replace it with an optional top-level metadata object:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub llm: Option<WikiLlmInfo>,

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WikiLlmInfo {
    pub provider: String,               // "openai-compatible" | "anthropic" | "http-json"
    pub model: String,
    pub language: String,
    pub evidence_file_count: usize,
    pub enriched_community_count: usize,
    pub failed_community_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_community_ids: Vec<String>,  // community names, not Community:N IDs
}
```

Extend `WikiInput` to carry:

```rust
pub llm_info: Option<WikiLlmInfo>,
```

`cih-engine` owns all provider execution and failure tracking, then passes `WikiLlmInfo` into `cih-wiki` only for manifest rendering.

**Migration from Phase 10a manifest format:** old manifests that have `llm_model` at the `WikiStats` level still deserialize correctly because both the old field and the new `llm` field are `Option` with `#[serde(default)]`. No version bump needed. Update the manifest unit tests in `manifest.rs` that assert `llm_model` is present — replace with assertions on the new `llm` object.

Note: never write API keys, request headers, or config file contents into the manifest. For `http-json`, record the provider name only (`"http-json"`), not the config file path or contents.

---

## Step 7 — `--wiki-language`

Pass `language: String` through the LLM request path and into `WikiLlmInfo`. Prepend to system prompt:
- `en`: no change (current behavior)
- `vi`: system prompt adds `"Write all documentation in Vietnamese."`

No translation of graph facts (route paths, class names, table names stay as-is).

---

## Test Plan

### Unit tests — `cih-engine`

- `make_adapter("openai-compatible", ...)` returns `OpenAiAdapter`; `"anthropic"` returns `AnthropicAdapter`
- `make_adapter("http-json", ..., None)` returns error "requires --llm-provider-config"
- `make_adapter("unknown", ...)` returns error with provider name in message
- `HttpJsonAdapter::load` with valid config → correct URL, headers, template, path
- `HttpJsonAdapter::load` with missing `response_path` → error
- `HttpJsonAdapter` response path extraction: `choices.0.message.content` on nested JSON
- `HttpJsonAdapter` response path misses → error (not empty string)
- `{{prompt}}` and `{{model}}` substitution in body template
- API key resolution: `--llm-api-key-env` set → uses that var, ignores others
- API key resolution: `--llm-api-key-env` set but var unset → clear error before calls
- API key resolution: no `--llm-api-key-env`, uses optional `CIH_LLM_API_KEY > OPENAI_API_KEY > ANTHROPIC_API_KEY`
- `http-json` without `{{api_key}}` works with no API key for local models
- `http-json` with `{{api_key}}` and no resolved key returns a clear error
- `http-json` with `{{env:VAR}}` and unset `VAR` returns a clear error
- `http-json` keeps `{{max_tokens}}` as a JSON number when the full value is that placeholder
- Evidence pack: routes included, all routes present (not capped at 5)
- Evidence pack: DB tables included when `community_db_tables` non-empty
- Evidence pack: source snippets skipped when repo root not provided
- Evidence pack: source snippet reads correct line range (±0 lines off)
- Evidence pack: absolute paths and `..` escaping paths are rejected
- Evidence pack: snippet selection is deterministic when method counts tie
- Evidence pack: total length ≤ 3 000 characters after BRD + snippet truncation
- BRD chunk matching: chunk with 2+ term hits included; chunk with 1 hit excluded
- BRD chunk matching: max 2 chunks per community
- `.txt` and `.md` BRD files loaded and split at paragraph boundaries
- `parse_llm_summary` succeeds on clean JSON
- `parse_llm_summary` succeeds on JSON embedded in prose (bracket extraction)
- `parse_llm_summary` succeeds when output contains citation markers like `[R1]`, `[T1]` in string values
- `parse_llm_summary` fails → error (not silent empty)
- Evidence pack: callers list with 15 entries is truncated to 10 before cap check
- Evidence pack: routes list with 20 entries is truncated to 10 only after BRD + snippets + callers/callees are exhausted
- BRD path segment extraction: `/api/v1/orders/{id}/cancel` → `["orders", "cancel"]`
- BRD path segment extraction: `/api/v2/payment` → `["payment"]`
- Manifest: graph-only run serializes no `llm` key (absent, not `null`)
- Manifest: LLM run has `llm.provider`, `llm.model`, `llm.language`, `llm.enriched_community_count`
- Manifest: `llm.failed_community_ids` contains community names, not `Community:N` strings
- Manifest: `llm.failed_community_count` correct after 2 failures
- Manifest: old `llm_model` field no longer serialized; existing `manifest.rs` tests updated
- Manifest: no API key or config file contents in serialized output
- `--llm-base-url` alongside `http-json` emits a `warn!` log and the flag is ignored

### Integration / command tests — `cih-engine`

- `wiki <repo>` (no `--llm`) works graph-only, manifest has no top-level `llm` field
- `wiki <repo> --llm --llm-dry-run` writes pages without any HTTP calls; dry-run text appears
- `wiki <repo> --llm --llm-provider http-json --llm-provider-config local.json` works with no API key when config has no `{{api_key}}`
- `wiki <repo> --llm --llm-provider http-json` without `--llm-provider-config` → clear error, exits non-zero
- `wiki <repo> --llm --llm-provider http-json --llm-provider-config bad.json` → clear parse error
- `wiki <repo> --llm --llm-provider anthropic --llm-base-url https://api.anthropic.com/v1` compiles and routes to Anthropic adapter (mock or dry-run)
- Per-community failure: one community fails all retries → wiki still written, failure recorded in manifest

### Run

```bash
cargo test -p cih-wiki
cargo test -p cih-engine
cargo test --workspace
```

---

## Out of scope (deferred)

- `.docx` BRD extraction
- `--wiki-language auto`
- PDF BRD extraction
- LLM-generated sidebar labels or category descriptions
- Streaming responses

---

## Migration note

Users of `--llm` with an Anthropic URL currently get Anthropic routing automatically.  
After this change they must add `--llm-provider anthropic`.  
The `--llm-enrich` hidden alias is preserved. No other breaking changes.
