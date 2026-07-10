use std::path::Path;

use anyhow::Result;

/// Statistics from a [`PageSink::flush`] call.
pub struct FlushStats {
    /// Files written (new or content changed).
    pub written: usize,
    /// Files skipped (content identical to what was on disk).
    pub unchanged: usize,
}

/// Accumulates rendered page content and writes everything to disk at once
/// using write-if-different semantics: a file is only touched when its content
/// has actually changed. This preserves file mtimes for Docusaurus incremental
/// builds, rsync-based deploys, and clean `git diff` output on the wiki directory.
pub struct PageSink {
    entries: Vec<(String, String)>,
}

impl PageSink {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Queue a page for writing. `rel_path` is relative to the wiki `out_dir`.
    pub fn push(&mut self, rel_path: impl Into<String>, content: impl Into<String>) {
        self.entries.push((rel_path.into(), content.into()));
    }

    /// Number of pages queued so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Flush all queued pages to `out_dir`.
    ///
    /// Each page is compared byte-for-byte against the existing file. If the
    /// content is identical the file is left untouched (mtime preserved). Parent
    /// directories are created as needed.
    pub fn flush(self, out_dir: &Path) -> Result<FlushStats> {
        let mut written = 0usize;
        let mut unchanged = 0usize;
        for (rel_path, content) in self.entries {
            let path = out_dir.join(&rel_path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let needs_write = match std::fs::read_to_string(&path) {
                Ok(existing) => existing != content,
                Err(_) => true,
            };
            if needs_write {
                std::fs::write(&path, &content)?;
                written += 1;
            } else {
                unchanged += 1;
            }
        }
        Ok(FlushStats { written, unchanged })
    }
}

impl Default for PageSink {
    fn default() -> Self {
        Self::new()
    }
}
