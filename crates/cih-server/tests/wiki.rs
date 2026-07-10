use std::fs;
use std::path::Path;

use cih_server::wiki::{load_wiki_index, make_snippet, strip_front_matter, WikiFacets};

fn write_page(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A minimal generated wiki mirroring the manifest.json shape cih-wiki writes.
/// Note the (historical) field semantics: `role` is the feature/module
/// grouping and `kind` is the page type — persona pages carry their persona
/// AS the kind (`po`, `ba`, `dev`).
fn fixture_wiki(dir: &Path) {
    write_page(
        dir,
        "po/loan-repayment.md",
        "---\nenrichment: graph\ngraph_version: v1\n---\n# Loan repayment\n\nLoan repayment schedules and interest accrual for borrowers.\n",
    );
    write_page(
        dir,
        "ba/loan-repayment.md",
        "---\nenrichment: graph\n---\n# Loan repayment (BA)\n\nRepayment schedule rules, penalties, and grace periods.\n",
    );
    write_page(
        dir,
        "dev/invoice-service.md",
        "# InvoiceService\n\nInvoice generation for monthly billing runs.\n",
    );
    fs::write(
        dir.join("manifest.json"),
        serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-07-10T00:00:00Z",
            "repo_name": "fixture",
            "graph_version": "v1",
            "community_version": "v1",
            "stats": {},
            "roles": ["po", "ba", "dev"],
            "nav": {},
            "pages": [
                {"slug": "po/loan-repayment", "role": "loan", "title": "Loan repayment",
                 "kind": "po", "path": "po/loan-repayment.md", "community_id": "c1"},
                {"slug": "ba/loan-repayment", "role": "loan", "title": "Loan repayment",
                 "kind": "ba", "path": "ba/loan-repayment.md", "community_id": "c1"},
                {"slug": "dev/invoice-service", "role": "billing", "title": "InvoiceService",
                 "kind": "dev", "path": "dev/invoice-service.md", "community_id": "c2"},
                {"slug": "missing", "role": "billing", "title": "Ghost page",
                 "kind": "dev", "path": "dev/missing.md"},
                {"slug": "escape", "role": "billing", "title": "Escape",
                 "kind": "dev", "path": "../outside.md"}
            ]
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn wiki_index_ranks_and_facets() {
    let tmp = tempfile::tempdir().unwrap();
    fixture_wiki(tmp.path());
    let index = load_wiki_index(tmp.path()).unwrap();

    assert_eq!(index.page_count(), 5);
    assert_eq!(index.graph_version, "v1");
    assert_eq!(index.repo_name, "fixture");

    // Body text ranks the repayment pages above the invoice page.
    let hits = index.search("repayment schedule", &WikiFacets::default(), 10);
    assert!(hits.len() >= 2);
    assert!(hits[0].slug.contains("loan-repayment"));
    assert!(hits.iter().all(|h| h.slug != "dev/invoice-service"));

    // Kind facet narrows to a single persona (persona pages carry their
    // persona as the kind).
    let hits = index.search(
        "repayment schedule",
        &WikiFacets {
            kind: Some("ba"),
            ..Default::default()
        },
        10,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "ba/loan-repayment");
    // Snippet comes from the body, never the front matter.
    assert!(hits[0].snippet.contains("Repayment schedule rules"));
    assert!(!hits[0].snippet.contains("enrichment"));

    // Feature facet matches community_id.
    let hits = index.search(
        "loan repayment invoice",
        &WikiFacets {
            feature: Some("c2"),
            ..Default::default()
        },
        10,
    );
    assert!(hits.iter().all(|h| h.community_id.as_deref() == Some("c2")));

    // Role facet = feature/module grouping.
    let hits = index.search(
        "invoice billing",
        &WikiFacets {
            role: Some("billing"),
            ..Default::default()
        },
        10,
    );
    assert_eq!(hits[0].slug, "dev/invoice-service");
    assert!(hits.iter().all(|h| h.role == "billing"));

    // Limit is honored.
    let hits = index.search("repayment schedule", &WikiFacets::default(), 1);
    assert_eq!(hits.len(), 1);
}

#[test]
fn missing_and_escaping_pages_still_index_on_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    fixture_wiki(tmp.path());
    // A file outside the wiki dir must never be read, even if the manifest
    // points at it.
    fs::write(tmp.path().parent().unwrap().join("outside.md"), "secret").unwrap();
    let index = load_wiki_index(tmp.path()).unwrap();

    // Metadata-only pages are findable by title...
    let hits = index.search("ghost page", &WikiFacets::default(), 10);
    assert_eq!(hits[0].slug, "missing");
    assert_eq!(hits[0].snippet, "");

    // ...and traversal paths never leak file content.
    let hits = index.search("secret escape", &WikiFacets::default(), 10);
    assert!(hits.iter().all(|h| !h.snippet.contains("secret")));
}

#[test]
fn page_by_slug_and_raw_content() {
    let tmp = tempfile::tempdir().unwrap();
    fixture_wiki(tmp.path());
    let index = load_wiki_index(tmp.path()).unwrap();

    // Slug lookup: hit and miss.
    let page = index.page_by_slug("po/loan-repayment").expect("slug exists");
    assert_eq!(page.kind, "po");
    assert!(index.page_by_slug("nope/nothing").is_none());

    // Raw page content keeps the front matter (provenance).
    let raw = index.page_raw(page).expect("file readable");
    assert!(raw.starts_with("---\n"));
    assert!(raw.contains("enrichment: graph"));
    assert!(raw.contains("# Loan repayment"));

    // Traversal paths from the manifest never read outside the wiki dir.
    let escape = index.page_by_slug("escape").expect("in manifest");
    assert!(index.page_raw(escape).is_none());
}

/// Smoke test against a real generated wiki. Run manually with:
/// `CIH_WIKI_SMOKE_DIR=<repo>/.cih/wiki cargo test -p cih-server --test wiki -- --ignored --nocapture`
#[test]
#[ignore]
fn smoke_real_wiki() {
    let dir = std::env::var("CIH_WIKI_SMOKE_DIR").expect("set CIH_WIKI_SMOKE_DIR");
    let start = std::time::Instant::now();
    let index = load_wiki_index(Path::new(&dir)).unwrap();
    let loaded = start.elapsed();

    let start = std::time::Instant::now();
    let hits = index.search("loan repayment schedule", &WikiFacets::default(), 10);
    let searched = start.elapsed();

    println!(
        "pages={} load={loaded:?} search={searched:?}",
        index.page_count()
    );
    for hit in &hits {
        println!("{:>8.3}  [{}/{}] {} — {}", hit.score, hit.role, hit.kind, hit.slug, hit.snippet);
    }
    assert!(!hits.is_empty());
}

#[test]
fn front_matter_and_snippets() {
    assert_eq!(strip_front_matter("---\na: 1\n---\nbody\n"), "body\n");
    assert_eq!(strip_front_matter("no front matter"), "no front matter");
    // An unterminated front matter block is left as-is.
    assert_eq!(strip_front_matter("---\na: 1\n"), "---\na: 1\n");

    let body = "# Heading\n\n| a | b |\n\nFirst prose line.\nThe repayment line.\n";
    assert_eq!(make_snippet(body, "repayment", 240), "The repayment line.");
    // Fenced code (e.g. mermaid diagrams) never becomes a snippet.
    let fenced = "```mermaid\nNode_repayment[\"repayment\"]\n```\n\nProse about repayment.\n";
    assert_eq!(make_snippet(fenced, "repayment", 240), "Prose about repayment.");
    // No token match falls back to the first prose line.
    assert_eq!(make_snippet(body, "zzz", 240), "First prose line.");
    // Truncation is char-based (multibyte-safe) and marked with an ellipsis.
    assert_eq!(make_snippet("éééé words", "words", 5), "éééé …");
}
