//! Terminal progress display — inspired by codegraph's shimmer-worker design.
//!
//! `PhaseProgress` wraps `indicatif::ProgressBar` which internally runs a
//! dedicated render thread (same role as codegraph's shimmer-worker.ts) so the
//! animation keeps ticking even while the main thread is blocked in I/O.
//!
//! On non-TTY output (piped, CI) `indicatif` automatically falls back to
//! no-draw mode; set `CIH_ASCII=1` to force plain-text output on any terminal.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use console::Term;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

// ── Glyph sets (mirrors codegraph's glyphs.ts) ───────────────────────────────

struct Glyphs {
    spinner: &'static [&'static str],
    bar_filled: &'static str,
    bar_empty: &'static str,
    rail: &'static str,
    phase_done: &'static str,
    dash: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    spinner: &["·", "✢", "✳", "✶", "✻", "✽"],
    bar_filled: "█",
    bar_empty: "░",
    rail: "│",
    phase_done: "◆",
    dash: "—",
};

const ASCII: Glyphs = Glyphs {
    spinner: &[".", "*", "+", "x", "o", "O"],
    bar_filled: "#",
    bar_empty: "-",
    rail: "|",
    phase_done: "*",
    dash: "-",
};

fn glyphs() -> &'static Glyphs {
    if std::env::var("CIH_ASCII").as_deref() == Ok("1") {
        return &ASCII;
    }
    // Term::stdout() detects TTY and Unicode capability
    if Term::stdout().features().colors_supported() {
        &UNICODE
    } else {
        &ASCII
    }
}

// ── PhaseProgress ─────────────────────────────────────────────────────────────

/// Phase-aware progress display.
///
/// Each phase is started with `start_phase()`, updated via `tick()`, and
/// closed with `finish_phase()`. The completed summary line is printed with a
/// `◆` glyph before the next phase begins (matching codegraph's `finishPhase`).
pub struct PhaseProgress {
    pub(crate) bar: ProgressBar,
    phase_name: String,
    /// Running count of ok / failed for the finish-phase summary.
    ok: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
}

impl PhaseProgress {
    pub fn new() -> Self {
        let bar = ProgressBar::new(0);
        // 50 ms tick — same as codegraph's ANIM_INTERVAL / 3 render loop
        bar.enable_steady_tick(Duration::from_millis(50));
        Self {
            bar,
            phase_name: String::new(),
            ok: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Begin a new phase. `total = None` → indeterminate spinner; `Some(n)` → progress bar.
    pub fn start_phase(&mut self, name: impl Into<String>, total: Option<u64>) {
        let name = name.into();
        self.phase_name = name.clone();
        self.ok.store(0, Ordering::Relaxed);
        self.failed.store(0, Ordering::Relaxed);

        let g = glyphs();
        let style = if let Some(n) = total {
            self.bar.set_length(n);
            self.bar.set_position(0);
            // "{spinner}" is replaced by indicatif with cycling frames from set_chars
            ProgressStyle::with_template(&format!(
                "{{dim}}{}{{reset}}  {{spinner}}  {{msg:.cyan}}  {{wide_bar:.cyan/dim}}  {{pos}}/{{len}}",
                g.rail
            ))
            .unwrap()
            .tick_strings(g.spinner)
            .progress_chars(&format!("{}{}{}",
                g.bar_filled, g.bar_filled, g.bar_empty))
        } else {
            // indeterminate — no bar, just spinner + message
            self.bar.set_length(0);
            self.bar.set_position(0);
            ProgressStyle::with_template(&format!(
                "{{dim}}{}{{reset}}  {{spinner}}  {{msg:.cyan}}...",
                g.rail
            ))
            .unwrap()
            .tick_strings(g.spinner)
        };

        self.bar.set_style(style);
        self.bar.set_message(Cow::Owned(name));
    }

    /// Update the current item label and advance the counter by one.
    pub fn tick(&self, label: impl Into<String>) {
        self.bar.set_message(Cow::Owned(label.into()));
        self.bar.inc(1);
    }

    /// Mark one item succeeded (counted in finish-phase summary).
    pub fn inc_ok(&self) {
        self.ok.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark one item failed (counted in finish-phase summary).
    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn tick_skipped(&self, label: impl Into<String>) {
        // cached/skipped counts as ok for summary purposes
        self.tick(label);
        self.inc_ok();
    }

    /// Close the current phase: clear the bar line and print the completion summary.
    pub fn finish_phase(&self) {
        let g = glyphs();
        let ok = self.ok.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);

        let detail = if ok > 0 || failed > 0 {
            if failed > 0 {
                format!("  {} {} ok, {} failed", g.dash, ok, failed)
            } else {
                format!("  {} {} done", g.dash, ok)
            }
        } else {
            String::new()
        };

        // Use println-style abandon so the finish line is permanent
        self.bar.println(format!(
            "\x1b[2m{}\x1b[0m  \x1b[32m{}\x1b[0m  {}{}",
            g.rail, g.phase_done, self.phase_name, detail,
        ));
        self.bar.finish_and_clear();
    }

    /// Completely hide the bar (e.g., non-TTY or `--json` mode).
    pub fn hide(&self) {
        self.bar.set_draw_target(ProgressDrawTarget::hidden());
    }

    /// Start an indeterminate spinner phase (shorthand for `start_phase(name, None)`).
    pub fn spin(&mut self, name: impl Into<String>) {
        self.start_phase(name, None);
    }

    /// Update the bar message without advancing the counter.
    /// Used by the decompile phase to show currently-active JAR names in-place.
    pub fn set_label(&self, msg: impl Into<String>) {
        self.bar.set_message(Cow::Owned(msg.into()));
    }

    /// Finish and print a freeform detail string instead of the ok/failed counters.
    /// Example: `ui.finish_with("5,292 nodes, 8,426 edges")`
    pub fn finish_with(&self, detail: impl Into<String>) {
        let g = glyphs();
        self.bar.println(format!(
            "\x1b[2m{}\x1b[0m  \x1b[32m{}\x1b[0m  {}  {} {}",
            g.rail,
            g.phase_done,
            self.phase_name,
            g.dash,
            detail.into(),
        ));
        self.bar.finish_and_clear();
    }
}

impl Default for PhaseProgress {
    fn default() -> Self {
        Self::new()
    }
}

// ── Styled summary output (static helpers) ───────────────────────────────────

const LABEL_WIDTH: usize = 14;

/// Print a "command complete" header:
///   `  ✓  Analyze  ·  212ecom-be  ·  v3bbb2159`
pub fn print_header(command: &str, repo_name: &str, version: Option<&str>) {
    let g = glyphs();
    let ok_glyph = if g.rail == "│" { "✓" } else { "[OK]" };
    let sep = if g.rail == "│" { "·" } else { "|" };

    let mut parts = vec![
        format!("\x1b[1m\x1b[32m{ok_glyph}\x1b[0m"),
        format!("\x1b[1m{command}\x1b[0m"),
    ];
    if !repo_name.is_empty() {
        parts.push(format!("\x1b[2m{sep}\x1b[0m \x1b[36m{repo_name}\x1b[0m"));
    }
    if let Some(v) = version {
        parts.push(format!("\x1b[2m{sep}\x1b[0m \x1b[2mv{v}\x1b[0m"));
    }
    eprintln!();
    eprintln!("  {}", parts.join("  "));
    eprintln!();
}

/// Print a single key/value summary row:
///   `     Files       843 parsed`
pub fn print_row(label: &str, value: &str) {
    eprintln!(
        "     \x1b[2m{:<width$}\x1b[0m  {}",
        label,
        value,
        width = LABEL_WIDTH,
    );
}

/// Format a count with thousands separators, e.g. `1234567` → `"1,234,567"`.
pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}
