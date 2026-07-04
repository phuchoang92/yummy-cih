//! Write per-site unresolved-reference diagnostics alongside the graph artifacts.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::UnresolvedRef;

/// Write `unresolved-refs.jsonl` and `unresolved-refs.md` into `dir`.
/// Creates `dir` if it does not exist.
pub fn write_unresolved_reports(refs: &[UnresolvedRef], dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    write_jsonl(refs, dir)?;
    write_markdown(refs, dir)?;
    Ok(())
}

fn write_jsonl(refs: &[UnresolvedRef], dir: &Path) -> io::Result<()> {
    let path = dir.join("unresolved-refs.jsonl");
    let file = fs::File::create(&path)?;
    let mut w = BufWriter::new(file);
    for r in refs {
        serde_json::to_writer(&mut w, r).map_err(io::Error::other)?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

fn write_markdown(refs: &[UnresolvedRef], dir: &Path) -> io::Result<()> {
    let path = dir.join("unresolved-refs.md");
    let file = fs::File::create(&path)?;
    let mut w = BufWriter::new(file);

    let total = refs.len();
    let ext_count = refs.iter().filter(|r| r.external_fqcn.is_some()).count();

    writeln!(w, "# Unresolved References")?;
    writeln!(w)?;
    writeln!(
        w,
        "**Total:** {total}  |  **External types missing:** {ext_count}"
    )?;
    writeln!(w)?;

    // By reason
    let mut by_reason: HashMap<&str, usize> = HashMap::new();
    for r in refs {
        *by_reason.entry(r.reason.as_str()).or_insert(0) += 1;
    }
    let mut reason_counts: Vec<(&str, usize)> = by_reason.into_iter().collect();
    reason_counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    writeln!(w, "## By reason")?;
    writeln!(w)?;
    writeln!(w, "| reason | count |")?;
    writeln!(w, "|---|---|")?;
    for (reason, count) in &reason_counts {
        writeln!(w, "| {reason} | {count} |")?;
    }
    writeln!(w)?;

    // Top files
    let mut by_file: HashMap<&str, usize> = HashMap::new();
    for r in refs {
        *by_file.entry(r.file.as_str()).or_insert(0) += 1;
    }
    let mut file_counts: Vec<(&str, usize)> = by_file.into_iter().collect();
    file_counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let top_files: Vec<_> = file_counts.into_iter().take(20).collect();

    writeln!(w, "## Top files by unresolved count")?;
    writeln!(w)?;
    writeln!(w, "| file | count |")?;
    writeln!(w, "|---|---|")?;
    for (file, count) in &top_files {
        writeln!(w, "| {file} | {count} |")?;
    }
    writeln!(w)?;

    // External FQCNs
    let mut ext_fqcns: Vec<&str> = refs
        .iter()
        .filter_map(|r| r.external_fqcn.as_deref())
        .collect();
    ext_fqcns.sort_unstable();
    ext_fqcns.dedup();

    if !ext_fqcns.is_empty() {
        writeln!(w, "## External FQCNs still missing")?;
        writeln!(w)?;
        for fqcn in ext_fqcns {
            writeln!(w, "- {fqcn}")?;
        }
    }

    Ok(())
}
