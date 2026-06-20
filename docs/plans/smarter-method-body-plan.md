# Smarter Method Body Display — Original-Line Cap + God-Function Fallback

## Context

The current implementation shows AST-stripped bodies in `<details>` blocks on dev wiki pages, with an 80-line cap on the **stripped** result. Reviewing real code exposed two compounding problems:

**Problem 1 — Wrong metric for the cap.**
A 200-line method that strips to 79 lines still shows a wall of code. The original source line count is the right signal for "too complex to summarise." The `createOrderFromCart` service method (87 lines) stripped to ~75 lines — currently hidden because the stripped result exceeded 80. The cap should fire on the original, not the result.

**Problem 2 — God functions get nothing.**
A real banking codebase (`verifyOverdraft`, ~400 lines, 3 top-level flows) has methods where stripping removes maybe 20% of the noise — the remaining 320 lines are legitimate business logic. No line-based stripping approach touches this. With an 80-line original cap, these methods are simply hidden.

But a developer reading a wiki page about the overdraft module doesn't need the code — they need the **orchestration map**: which services does this function coordinate, in what flow? That information already exists in the graph as `calls_out` edges. This is the god-function fallback.

**Resolution:**
- ≤ 80 original lines → show stripped body (existing approach, wrong metric fixed)
- > 80 original lines → show call-summary block derived from `graph.calls_out` (new)

---

## What Changes

### 1. `crates/cih-wiki/src/bodies.rs`

**Add `BodyEntry` struct** — returned instead of bare `String`:

```rust
pub struct BodyEntry {
    pub stripped: String,
    /// Raw source line count before stripping (end_line - start_line + 1).
    pub original_lines: usize,
}
```

**Change `source_bodies()` return type** from `HashMap<String, String>` to `HashMap<String, BodyEntry>`.

Inside the loop, after `let raw = lines[from..to].join("\n")`:
```rust
let original_lines = to - from;
let stripped = match file_ext(&node.file) { "java" => strip_java_body(&raw), _ => raw };
if !stripped.trim().is_empty() {
    bodies.insert(node.id.as_str().to_string(), BodyEntry { stripped, original_lines });
}
```

No change to strip rules — conservative stripping (logs, null-guards, trivial getters) is correct.

---

### 2. `crates/cih-wiki/src/lib.rs`

- Add `pub use bodies::BodyEntry;`
- Change `WikiInput.bodies` field type:
  ```rust
  pub bodies: HashMap<String, BodyEntry>,
  ```
- `HashMap::new()` in test helpers works for any value type — no test changes needed.

`crates/cih-engine/src/wiki_cmd.rs` — **no change** (type is inferred).

---

### 3. `crates/cih-wiki/src/pages/dev.rs`

**Two rendering paths** — decided by `body.original_lines` vs the threshold.

#### Path A — Short method (original ≤ 80 lines): stripped body block

```rust
use crate::bodies::BodyEntry;

// In the collapsible body loop:
let Some(body) = bodies.get(method.id.as_str()) else { continue; };

if body.original_lines <= 80 {
    // Path A: show stripped body with reduction header
    let stripped_lines = body.stripped.trim().lines().count();
    let comment_prefix = if lang == "python" { "#" } else { "//" };
    let header = format!(
        "{} stripped · {} of {} lines shown\n",
        comment_prefix, stripped_lines, body.original_lines
    );
    let code_content = format!("{}{}", header, body.stripped.trim());
    md.push_str(&format!("<details>\n<summary><code>{}</code>{}</summary>\n\n", sig, location));
    if lang.is_empty() {
        md.push_str(&format!("```\n{}\n```\n\n", code_content));
    } else {
        md.push_str(&format!("```{}\n{}\n```\n\n", lang, code_content));
    }
    md.push_str("</details>\n\n");
}
```

#### Path B — God function (original > 80 lines): call-summary block

Derived entirely from `graph.calls_out` — no source file read needed.

```rust
else {
    // Path B: god function — show orchestration map from graph
    let empty_calls: Vec<String> = Vec::new();
    let call_ids = graph.calls_out.get(method.id.as_str()).unwrap_or(&empty_calls);
    let call_names: Vec<String> = call_ids.iter()
        .filter_map(|cid| graph.nodes_by_id.get(cid))
        .map(|n| {
            // "ClassName.methodName" format
            n.qualified_name.as_deref()
                .and_then(|qn| qn.split('#').next())
                .and_then(|fqcn| fqcn.rsplit('.').next())
                .map(|cls| format!("{}.{}", cls, n.name))
                .unwrap_or_else(|| n.name.clone())
        })
        .collect();

    if !call_names.is_empty() {
        md.push_str(&format!(
            "<details>\n<summary><code>{}</code>{} ⚠ large method</summary>\n\n",
            sig, location
        ));
        let comment_prefix = if lang == "python" { "#" } else { "//" };
        let calls_line = call_names.join(", ");
        md.push_str(&format!(
            "```\n{} god function · {} lines\n{} calls: {}\n```\n\n",
            comment_prefix, body.original_lines, comment_prefix, calls_line
        ));
        md.push_str("</details>\n\n");
    }
}
```

**Example output for `verifyOverdraft` (~400 lines):**

```
// god function · 400 lines
// calls: WizardCacheUtil.getAction, OverdraftBusinessValidation.checkOverdraftHour,
//        BackendBankingServices.getSavingAccountsFromBackend, BankingAccountsBO.getBankingAccounts,
//        OverdraftAdapterImpl.getOpenedOverdraft, validateOverdraft, getOverdraftFee, setVerified
```

**Example output for `createOrderFromCart` (87 lines):**

```
// god function · 87 lines
// calls: UserRepository.findByPhone, CartRepository.findByUserId,
//        ProductService.mapToProductSummaryResponse, InventoryService.checkAndReserve,
//        OrderRepository.save, CartRepository.save, ApplicationEventPublisher.publishEvent
```

---

## File Touch List

| File | Change |
|---|---|
| `crates/cih-wiki/src/bodies.rs` | Add `BodyEntry` struct; update `source_bodies()` return type |
| `crates/cih-wiki/src/lib.rs` | `pub use bodies::BodyEntry`; update `WikiInput.bodies` field type |
| `crates/cih-wiki/src/pages/dev.rs` | Two rendering paths: stripped body (≤80) vs call-summary (>80) |

**No other files need changing.**
- `wiki_cmd.rs`: `source_bodies()` return type is inferred — no touch needed
- `render_dev_community` already receives `&graph` as first argument (`lib.rs:408`) — `graph.calls_out` and `graph.nodes_by_id` are available to Path B without any new parameter

---

## Threshold

80 lines as the short/god boundary:
- Under 80 lines: method fits on one screen; stripped version is readable at a glance
- Over 80 lines: method spans multiple screens; the orchestration map is more useful than code

Configurable later via `--body-threshold N` on the wiki command.

---

## Verification

```bash
# 1. Build
cargo build -p cih-engine

# 2. Unit tests
cargo test -p cih-wiki

# 3. Regenerate wiki
FALKOR_URL=redis://127.0.0.1:6380 ./target/debug/cih-engine wiki \
  /Users/phuc/BigMoves/dienmaychiben/212ecom-be

# 4. Confirm stripped header on a short method (≤80 original lines)
grep "stripped ·" \
  /Users/phuc/BigMoves/dienmaychiben/212ecom-be/.cih/wiki/pages/payments/dev/payment-controller.md

# 5. Confirm god-function call-summary on createOrderFromCart (87 lines original)
grep "god function" \
  /Users/phuc/BigMoves/dienmaychiben/212ecom-be/.cih/wiki/pages/orders/dev/order-service.md

# 6. Confirm call targets appear
grep "calls:" \
  /Users/phuc/BigMoves/dienmaychiben/212ecom-be/.cih/wiki/pages/orders/dev/order-service.md

# 7. Rebuild docs-viewer
cd docs-viewer && \
CIH_WIKI_PATH=/Users/phuc/BigMoves/dienmaychiben/212ecom-be/.cih/wiki/pages \
  node scripts/gen-index.js && \
  CIH_WIKI_PATH=/Users/phuc/BigMoves/dienmaychiben/212ecom-be/.cih/wiki/pages \
  npx docusaurus build && npx docusaurus serve --port 3001
```
