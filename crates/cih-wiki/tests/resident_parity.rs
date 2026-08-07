//! Parity + timing for on-demand rendering (P3.8), run manually against a repo
//! that has graph artifacts + a batch graph-mode wiki on disk:
//!
//!   CIH_TEST_REPO=/abs/repo CIH_TEST_WIKI=/abs/repo/.cih/wiki \
//!     cargo test -p cih-wiki --test resident_parity -- --ignored --nocapture
//!
//! Asserts `OwnedWiki::render_slug(slug)` byte-equals the batch file for every
//! manifest page, and prints load + per-page render timing.

use std::path::Path;
use std::time::Instant;

use cih_wiki::OwnedWiki;
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    pages: Vec<PageMeta>,
}
#[derive(Deserialize)]
struct PageMeta {
    slug: String,
    path: String,
}

#[test]
#[ignore = "needs CIH_TEST_REPO + CIH_TEST_WIKI on disk"]
fn render_slug_matches_batch_graph_mode() {
    let repo = std::env::var("CIH_TEST_REPO").expect("set CIH_TEST_REPO");
    let wiki = std::env::var("CIH_TEST_WIKI").expect("set CIH_TEST_WIKI");
    let repo = Path::new(&repo);
    let wiki = Path::new(&wiki);
    let repo_name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    let t0 = Instant::now();
    let owned = OwnedWiki::load_package_mode(repo, repo_name).expect("load");
    let load_ms = t0.elapsed().as_millis();
    println!(
        "load_package_mode: {load_ms} ms (graph_version={})",
        owned.graph_version()
    );

    // Timing-only mode (no batch wiki needed): load + slug enumeration + a few
    // renders. Use to gauge per-request cost at scale (e.g. Fineract).
    if std::env::var("CIH_SINGLE_ONLY").is_ok() {
        let t = Instant::now();
        let slugs = owned.slugs();
        println!(
            "slugs(): {} pages in {} ms (builds ctx+index once)",
            slugs.len(),
            t.elapsed().as_millis()
        );
        for slug in slugs.iter().take(5) {
            let t = Instant::now();
            let _ = owned.render_slug(slug);
            println!("  render_slug({slug}): {} ms", t.elapsed().as_millis());
        }
        return;
    }

    let manifest: Manifest =
        serde_json::from_str(&std::fs::read_to_string(wiki.join("manifest.json")).unwrap())
            .unwrap();
    println!("manifest pages: {}", manifest.pages.len());

    // Dump mode: full batch vs live for one slug, then stop.
    if let Ok(slug) = std::env::var("CIH_DUMP_SLUG") {
        let page = manifest
            .pages
            .iter()
            .find(|p| p.slug == slug)
            .expect("slug in manifest");
        let disk = std::fs::read_to_string(wiki.join(&page.path)).unwrap_or_default();
        let live = owned
            .render_slug(&slug)
            .map(|r| r.content)
            .unwrap_or_else(|| "<NONE>".into());
        println!(
            "===== BATCH ({}) =====\n{disk}\n===== LIVE =====\n{live}\n===== END =====",
            page.path
        );
        return;
    }

    // Time a single render (cold) to gauge per-request cost.
    if let Some(first) = manifest.pages.first() {
        let t = Instant::now();
        let _ = owned.render_slug(&first.slug);
        println!(
            "single render_slug (resident lookup): {} ms",
            t.elapsed().as_millis()
        );
    }

    let mut mismatches = 0usize;
    let mut missing = 0usize;
    let mut checked = 0usize;
    let t1 = Instant::now();
    for page in &manifest.pages {
        let Some(rendered) = owned.render_slug(&page.slug) else {
            missing += 1;
            if missing <= 10 {
                println!("MISSING slug: {}", page.slug);
            }
            continue;
        };
        let disk = std::fs::read_to_string(wiki.join(&page.path)).unwrap_or_default();
        checked += 1;
        if rendered.content != disk {
            mismatches += 1;
            if mismatches <= 5 {
                println!("--- MISMATCH slug={} path={} ---", page.slug, page.path);
                print_first_diff(&disk, &rendered.content);
            }
        }
    }
    let dur = t1.elapsed();
    println!(
        "checked={checked} mismatches={mismatches} missing={missing} total={} ms",
        dur.as_millis()
    );
    assert_eq!(mismatches, 0, "{mismatches} pages differ from batch output");
    assert_eq!(missing, 0, "{missing} manifest slugs did not resolve");
}

fn print_first_diff(a: &str, b: &str) {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            println!("  line {i}:\n    batch: {la:?}\n    live : {lb:?}");
            return;
        }
    }
    println!(
        "  (prefix equal; lengths batch={} live={})",
        a.len(),
        b.len()
    );
}
