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
