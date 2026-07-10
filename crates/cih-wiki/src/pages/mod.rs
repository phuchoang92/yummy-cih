pub mod api_flow;
pub mod ba;
pub mod community;
pub mod dev;
pub mod feature_ba;
pub mod feature_index;
pub mod feature_po;
pub mod po;
pub mod shared;
pub mod system_index;

/// Provenance metadata attached to each generated wiki page.
pub struct WikiPageMeta<'a> {
    /// One of `"graph-only"`, `"llm-summary"`, or `"llm-full"`.
    pub enrichment_tier: &'a str,
    pub graph_version: &'a str,
}

/// Emit YAML front matter with provenance fields for a Docusaurus MDX page.
///
/// `generated_at` is intentionally omitted from page content so that re-running
/// the wiki with an unchanged graph produces byte-identical pages (write-if-different
/// determinism). The timestamp lives in `wiki_meta.json` instead.
pub fn provenance_front_matter(title: &str, sidebar_position: u32, meta: &WikiPageMeta<'_>) -> String {
    format!(
        "---\ntitle: {title}\nsidebar_position: {sidebar_position}\ncih_enrichment: {tier}\ncih_graph_version: {ver}\n---\n\n",
        tier = meta.enrichment_tier,
        ver = meta.graph_version,
    )
}

/// Escape a string for use as a Markdown table cell value.
/// Prevents `|` from breaking table column boundaries.
pub fn escape_table_cell(s: &str) -> String {
    s.replace('|', r"\|").replace('\n', " ")
}

/// Make a string safe for Docusaurus MDX output.
/// `{` starts a JSX expression and `<` starts a JSX tag; both must be escaped
/// to avoid build failures when LLM-generated prose contains them.
pub fn mdx_safe(s: &str) -> String {
    s.replace('{', r"\{").replace('<', "&lt;")
}
