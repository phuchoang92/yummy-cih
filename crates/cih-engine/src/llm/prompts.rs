//! All LLM prompt string constants used across wiki enrichment and grouping.
//!
//! Static prompts are `pub const`. Language-aware prompts that append a
//! localisation clause are thin wrapper functions built on top of those consts.

// ── Community enrichment ──────────────────────────────────────────────────────

pub const COMMUNITY_SYSTEM_PROMPT: &str =
    "You are a code documentation assistant. Write only from the provided evidence. \
     Do not invent behavior not in the evidence.";

pub const COMMUNITY_FULL_JSON_TEMPLATE: &str = r#"Write exactly ten JSON fields (2–4 sentences each, cite evidence IDs):
{
  "po_summary": "<business purpose and value>",
  "po_capabilities": "<key business capabilities exposed>",
  "po_workflows": "<end-to-end user-facing workflows>",
  "po_open_questions": "<gaps or assumptions needing clarification>",
  "ba_process_overview": "<high-level process flow>",
  "ba_contracts": "<API and event contracts with other modules>",
  "ba_business_rules": "<validations, rules, and invariants>",
  "dev_responsibility": "<what this module owns in the system>",
  "dev_key_classes": "<central classes and their roles>",
  "dev_entry_points": "<primary entry points: routes, listeners, scheduled tasks>"
}
Only output the JSON object. Do not add commentary."#;

// ── Feature enrichment ────────────────────────────────────────────────────────

pub const FEATURE_SYSTEM_PROMPT: &str =
    "You are a software architect writing business documentation from code evidence.\n\
     Write only from the provided evidence. Cite evidence IDs exactly as shown, like [C1-R1],[C1-P1],[C2-B1].\n\
     Do not invent behavior not in the evidence.";

pub const FEATURE_JSON_TEMPLATE: &str = r#"Respond ONLY with a JSON object:
{
  "po_overview": "<3-5 sentences of plain-language business overview>",
  "po_capabilities": "<bullet list of business capabilities, one per line starting with - >",
  "ba_process_overview": "<3-5 sentences describing business processes and flows>",
  "ba_business_rules": "<key business rules or invariants, one per line starting with - >"
}"#;

// ── HTTP flow enrichment ──────────────────────────────────────────────────────

pub const HTTP_FLOW_SYSTEM_PROMPT: &str =
    "You are a code documentation assistant. Describe this HTTP request flow \
     based solely on the provided call chain. Do not invent behavior not shown. \
     Each step description must start with an action verb and must not repeat \
     the class name, method name, or arity notation (e.g. /2()).";

pub const HTTP_FLOW_JSON_TEMPLATE: &str = r#"Respond ONLY with this JSON object (no extra commentary):
{
  "narrative": "<2-3 sentences describing this request flow for a business analyst>",
  "business_impact": "<1-2 sentences describing the business value for a product owner>",
  "step_descriptions": [<one quoted sentence per step, {step_count} total>]
}"#;

// ── Process flow enrichment ───────────────────────────────────────────────────

pub const PROCESS_FLOW_SYSTEM_PROMPT: &str =
    "You are a code documentation assistant. Describe this business process \
     based solely on the provided evidence. Do not invent behavior not shown.";

pub const PROCESS_FLOW_JSON_TEMPLATE: &str = r#"Respond ONLY with this JSON object (no extra commentary):
{
  "narrative": "<2-3 sentences describing this flow for a business analyst>",
  "business_impact": "<1-2 sentences describing the business value for a product owner>",
  "step_descriptions": [<one quoted sentence per step, {step_count} total>]
}"#;

// ── Class enrichment ──────────────────────────────────────────────────────────

pub const CLASS_SYSTEM_PROMPT: &str =
    "You are a code documentation assistant. Describe Java class methods in one sentence \
     each for a business analyst. Return JSON only. Do not invent behavior. \
     Start each method description with an action verb. \
     Do not mention the class name, method name, or arity (e.g. /2()) in the description.";

pub const CLASS_ENRICH_JSON_TEMPLATE: &str =
    "Return exactly this JSON:\n\
     {\n\
       \"summary\": \"one paragraph: what this class does in the system\",\n\
       \"methods\": {\n\
         \"methodName\": \"Validates the request payload and delegates to the write service.\"\n\
       }\n\
     }\n\
     Each method value must start with a verb and must not repeat the class or method name.\n\
     Output only the JSON object.";

// ── LLM grouping ─────────────────────────────────────────────────────────────

pub const COMMUNITY_ASSIGN_SYSTEM_PROMPT: &str =
    "You are a software architect assigning code communities to product modules.\n\
     Assign each community to the BEST matching module slug from the established list.\n\
     If a community truly does not fit any established module, assign it to the closest one.\n\
     Respond ONLY with a valid JSON object — no prose, no markdown fences.";

pub const MODULE_OUTLINE_SYSTEM_PROMPT: &str =
    "You are a software architect designing product modules for a backend service.\n\
     Your job is to propose a MODULE OUTLINE — the list of distinct business-capability modules \
     that make sense for this codebase. Do NOT assign communities yet.\n\
     \n\
     Rules:\n\
     1. Group by BUSINESS DOMAIN (bounded context), not technical layer.\n\
     2. Use route prefixes as the primary signal for domain boundaries.\n\
     3. Merge weak auto-detected hints like 'repo', 'service', 'dto', 'entity', 'util' into \
        the domain modules that own them.\n\
     4. Target approximately {estimated_modules} modules.\n\
     5. Name modules after the business capability (e.g. orders, payments), not the layer.\n\
     6. Respond ONLY with a valid JSON object — no prose, no markdown fences.";

pub const GROUPING_SYSTEM_PROMPT_TEMPLATE: &str =
    "You are a software architect grouping code communities into product modules.\n\
     \n\
     Rules:\n\
     1. Group by BUSINESS DOMAIN (bounded context), not technical layer.\n\
     2. \"prefixes\" are the PRIMARY signal — communities sharing a route prefix almost always \
        belong to the same module.\n\
     3. \"controllers\" and \"tables\" reveal data ownership — group communities that share them.\n\
     4. \"hint\" is the current auto-grouping — use it as a starting point; fix it where it \
        lumps unrelated things or uses technical names (repo, service, dto, util).\n\
     5. Target approximately {estimated_modules} modules. Don't create a module for one tiny \
        community unless it is truly standalone.\n\
     6. Name modules after the business capability (orders, payments, customers), not the layer.\n\
     7. Every community_id must appear in exactly one module.\n\
     8. Respond ONLY with a valid JSON object — no prose, no markdown fences.";

// ── Language-aware helpers ────────────────────────────────────────────────────

/// Append a localisation directive to a base prompt if the language is not English.
pub fn with_language(base: &str, language: &str) -> String {
    if language == "en" {
        base.to_string()
    } else {
        format!("{base} Write all documentation in language: {language}.")
    }
}

pub fn community_system(language: &str) -> String {
    with_language(COMMUNITY_SYSTEM_PROMPT, language)
}

pub fn http_flow_system(language: &str) -> String {
    with_language(HTTP_FLOW_SYSTEM_PROMPT, language)
}

pub fn process_flow_system(language: &str) -> String {
    with_language(PROCESS_FLOW_SYSTEM_PROMPT, language)
}

pub fn class_system(language: &str) -> String {
    with_language(CLASS_SYSTEM_PROMPT, language)
}
