use cih_wiki::html::write_html_viewer;
use cih_wiki::manifest::{NavEntry, PageEntry, WikiManifest, WikiStats};
use std::collections::BTreeMap;

#[test]
fn html_viewer_escapes_script_end_tags() {
    // pid is unique per test process and this prefix is used by only this test,
    // so no wall-clock timestamp (which can collide) is needed; clear any stale
    // leftover from a prior run first.
    let dir = std::env::temp_dir().join(format!("cih-wiki-html-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("pages")).unwrap();
    std::fs::write(dir.join("pages/index.md"), "# Hi\n</script>").unwrap();
    let manifest = WikiManifest {
        schema_version: 1,
        generated_at: "now".to_string(),
        repo_name: "repo".to_string(),
        graph_version: "g".to_string(),
        community_version: "c".to_string(),
        stats: WikiStats::default(),
        roles: vec![],
        nav: BTreeMap::<String, Vec<NavEntry>>::new(),
        pages: vec![PageEntry {
            slug: "index".to_string(),
            role: "system".to_string(),
            title: "Index".to_string(),
            kind: "index".to_string(),
            path: "pages/index.md".to_string(),
            json_path: None,
            community_id: None,
        }],
        llm: None,
        generation: None,
        module_tree_path: None,
        wiki_meta_path: None,
        warnings: vec![],
    };
    write_html_viewer(&dir, &manifest).unwrap();
    let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
    assert!(!html.contains("</script>\""));
    assert!(!html.contains("https://cdn"));
    let _ = std::fs::remove_dir_all(dir);
}
