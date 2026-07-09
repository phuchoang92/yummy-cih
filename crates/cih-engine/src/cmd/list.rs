//! `cih-engine list` — print the registry table.

use anyhow::Result;
use cih_core::Registry;

pub fn run(json: bool) -> Result<()> {
    let reg = Registry::load();
    if json {
        println!("{}", serde_json::to_string_pretty(&reg)?);
    } else if reg.entries.is_empty() {
        println!("No repositories indexed yet. Run `cih-engine analyze <repo>` first.");
    } else {
        println!(
            "{:<24} {:<12} {:>8} {:>8} {:>6}  path",
            "name", "indexed_at", "nodes", "edges", "files"
        );
        println!("{}", "-".repeat(90));
        for e in &reg.entries {
            let date = e.indexed_at.get(..10).unwrap_or(&e.indexed_at);
            println!(
                "{:<24} {:<12} {:>8} {:>8} {:>6}  {}",
                e.name, date, e.stats.nodes, e.stats.edges, e.stats.files, e.path
            );
        }
    }
    Ok(())
}
