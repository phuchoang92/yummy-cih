//! Interactive TUI command builder.
//! Launched by `cih-engine ui` (no subcommand shows usage, not the TUI).
//!
//! Navigation:
//!   CmdList mode  — ↑/↓ pick command, Enter/→ move to fields
//!   FieldNav mode — ↑/↓ move between fields, Enter/Space edit/toggle, r run, Esc back
//!   TextEdit mode — type value, Enter/Esc confirm
//!   Confirm mode  — Enter/y run, Esc/n go back
//!
//! The assembled command is shown live at the bottom. On confirm the TUI exits
//! and returns the argument list; main.rs spawns the current binary with those args.

use std::io;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

// ── Field model ───────────────────────────────────────────────────────────────

#[derive(Clone)]
enum FieldVal {
    Bool(bool),
    Text(String),
    /// Index into Field::options.
    Select(usize),
}

struct Field {
    /// CLI flag, e.g. "--all". Empty for positional args.
    flag: &'static str,
    /// Label shown in the form (padded to column width).
    label: &'static str,
    /// One-line description shown below the active field.
    desc: &'static str,
    /// Ghost text shown when a Text field is empty.
    placeholder: &'static str,
    val: FieldVal,
    required: bool,
    /// Non-empty only for Select fields.
    options: &'static [&'static str],
}

impl Field {
    fn bool(flag: &'static str, label: &'static str, desc: &'static str) -> Self {
        Field {
            flag, label, desc, placeholder: "",
            val: FieldVal::Bool(false), required: false, options: &[],
        }
    }

    fn text(
        flag: &'static str,
        label: &'static str,
        desc: &'static str,
        placeholder: &'static str,
        required: bool,
    ) -> Self {
        Field {
            flag, label, desc, placeholder,
            val: FieldVal::Text(String::new()), required, options: &[],
        }
    }

    fn select(
        flag: &'static str,
        label: &'static str,
        desc: &'static str,
        options: &'static [&'static str],
        default: usize,
    ) -> Self {
        Field {
            flag, label, desc, placeholder: "",
            val: FieldVal::Select(default), required: false, options,
        }
    }

    /// The display string for the value column.
    fn display_value(&self) -> String {
        match &self.val {
            FieldVal::Bool(b) => if *b { "[x]".into() } else { "[ ]".into() },
            FieldVal::Text(s) => {
                if s.is_empty() { self.placeholder.into() } else { s.clone() }
            }
            FieldVal::Select(i) => {
                self.options.get(*i).copied().unwrap_or("").to_string()
            }
        }
    }

    /// Whether this field has a non-default value set by the user.
    fn has_value(&self) -> bool {
        match &self.val {
            FieldVal::Bool(b) => *b,
            FieldVal::Text(s) => !s.is_empty(),
            FieldVal::Select(_) => true, // selects always contribute
        }
    }

    /// Build the shell fragment for the assembled command string.
    fn to_shell_fragment(&self) -> Option<String> {
        match &self.val {
            FieldVal::Bool(true) => Some(self.flag.to_string()),
            FieldVal::Bool(false) => None,
            FieldVal::Text(s) if s.is_empty() => {
                if self.required {
                    Some(format!("<{}>", self.label.trim()))
                } else {
                    None
                }
            }
            FieldVal::Text(s) => {
                let q = shell_quote(s);
                if self.flag.is_empty() {
                    Some(q)
                } else {
                    Some(format!("{} {}", self.flag, q))
                }
            }
            FieldVal::Select(i) => {
                let v = self.options.get(*i).copied().unwrap_or("");
                if self.flag.is_empty() {
                    Some(v.to_string())
                } else {
                    Some(format!("{} {}", self.flag, v))
                }
            }
        }
    }
}

fn shell_quote(s: &str) -> String {
    if s.contains(' ') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

// ── Command definitions ───────────────────────────────────────────────────────

struct Cmd {
    name: &'static str,
    desc: &'static str,
    fields: Vec<Field>,
}

fn make_commands() -> Vec<Cmd> {
    vec![
        Cmd {
            name: "scan",
            desc: "Fast discovery pass — writes .cih/repo-map.json, no graph load.",
            fields: vec![
                Field::text("", "repo", "Absolute path to the Java/Spring repository root.", "/path/to/java-project", true),
                Field::bool("--json", "--json", "Print machine-readable JSON instead of the human summary."),
            ],
        },
        Cmd {
            name: "analyze",
            desc: "Parse Java source, build call graph, load into FalkorDB.",
            fields: vec![
                Field::text("", "repo", "Absolute path to the Java/Spring repository root.", "/path/to/java-project", true),
                Field::bool("--all", "--all", "Index every eligible Java file. Most common choice for first runs."),
                Field::text("--module", "--module", "Comma-separated module names instead of --all, e.g. payment,order,auth.", "payment,order", false),
                Field::text("--include", "--include", "Glob to add to scope (repo-relative), e.g. src/main/java/**.", "src/main/java/**", false),
                Field::text("--exclude", "--exclude", "Glob to remove from scope, e.g. **/generated/**.", "**/generated/**", false),
                Field::bool("--include-decompiled", "--include-decompiled", "Include .workspace-dependencies/ (decompiled JARs). Slow — use on demand."),
                Field::text("--falkor-url", "--falkor-url", "FalkorDB URL. Leave empty to use $FALKOR_URL or redis://127.0.0.1:6380.", "redis://127.0.0.1:6380", false),
                Field::text("--graph-key", "--graph-key", "Graph name inside FalkorDB. Leave empty for default 'cih'.", "cih", false),
                Field::bool("--no-load", "--no-load", "Write JSONL artifacts only — skip loading into FalkorDB (dry run)."),
                Field::bool("--no-cache", "--no-cache", "Force re-parse of every file; ignore the incremental file cache."),
                Field::bool("--skip-xml-integration", "--skip-xml-integration", "Skip Spring XML DI extraction. Faster for large repos that don't use XML config."),
            ],
        },
        Cmd {
            name: "discover",
            desc: "Community detection + process traces from prior analyze artifacts.",
            fields: vec![
                Field::text("", "repo", "Repository root — must have .cih/artifacts/ from a prior analyze run.", "/path/to/java-project", true),
                Field::text("--falkor-url", "--falkor-url", "FalkorDB URL. Leave empty to use $FALKOR_URL or default.", "redis://127.0.0.1:6380", false),
                Field::text("--graph-key", "--graph-key", "Graph name inside FalkorDB.", "cih", false),
                Field::bool("--no-load", "--no-load", "Write community artifacts only — skip FalkorDB load."),
                Field::text("--resolution", "--resolution", "Louvain resolution γ. Higher = more, smaller communities. Default: 1.0.", "1.0", false),
                Field::text("--min-community-size", "--min-community-size", "Drop communities smaller than N members. Default: 2.", "2", false),
                Field::text("--max-trace-depth", "--max-trace-depth", "BFS depth per process trace. Default: 10.", "10", false),
            ],
        },
        Cmd {
            name: "embed",
            desc: "Vectorise graph nodes into pgvector (enables semantic search in the query tool).",
            fields: vec![
                Field::text("", "repo", "Repository root — must have .cih/artifacts/ from a prior analyze run.", "/path/to/java-project", true),
                Field::text("--pg-url", "--pg-url", "PostgreSQL URL. Leave empty to use $CIH_PG_URL.", "postgres://cih:pass@localhost:5433/cih", false),
                Field::select("--model", "--model", "Embedding model. all-minilm-l6-v2 is smaller/faster; bge-small-en-v1.5 is slightly more accurate.", &["all-minilm-l6-v2", "bge-small-en-v1.5"], 0),
            ],
        },
        Cmd {
            name: "wiki",
            desc: "Generate role-based Markdown wiki pages from graph + community artifacts.",
            fields: vec![
                Field::text("", "repo", "Repository root — needs both analyze and discover to have run.", "/path/to/java-project", true),
                Field::text("--out", "--out", "Output directory. Default: <repo>/.cih/wiki.", "", false),
                Field::select("--wiki-mode", "--wiki-mode", "graph = no LLM (fast), llm-summary = AI summaries, llm-full = fully AI-generated pages.", &["graph", "llm-summary", "llm-full"], 0),
                Field::bool("--llm", "--llm", "Enable LLM enrichment. Set a provider API key env var before running."),
                Field::select("--llm-provider", "--llm-provider", "LLM provider. Set the matching env var: DEEPSEEK_API_KEY / GEMINI_API_KEY / ANTHROPIC_API_KEY / OPENAI_API_KEY.", &["deepseek", "gemini", "anthropic", "openai-compatible"], 0),
                Field::text("--llm-model", "--llm-model", "Model name, e.g. deepseek-chat, gemini-2.5-flash, claude-haiku-4-5-20251001, gpt-4o-mini.", "", false),
                Field::text("--wiki-language", "--wiki-language", "Language for generated text, e.g. en, vi, ja, fr.", "en", false),
                Field::bool("--incremental", "--incremental", "Skip communities whose evidence hasn't changed since last wiki run."),
                Field::bool("--html", "--html", "Also write a standalone index.html viewer alongside the Markdown files."),
            ],
        },
    ]
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    /// Left panel focused — ↑/↓ picks a command.
    CmdList,
    /// Right panel focused — ↑/↓ navigates fields.
    FieldNav,
    /// Keyboard input goes into the active Text field.
    TextEdit,
    /// Assembled command shown, waiting for run confirmation.
    Confirm,
}

struct App {
    mode: Mode,
    cmds: Vec<Cmd>,
    cmd_list: ListState,
    field_idx: usize,
}

impl App {
    fn new() -> Self {
        let mut cmd_list = ListState::default();
        cmd_list.select(Some(0));
        App {
            mode: Mode::CmdList,
            cmds: make_commands(),
            cmd_list,
            field_idx: 0,
        }
    }

    fn cmd_idx(&self) -> usize {
        self.cmd_list.selected().unwrap_or(0)
    }

    fn cmd(&self) -> &Cmd {
        &self.cmds[self.cmd_idx()]
    }

    fn cmd_mut(&mut self) -> &mut Cmd {
        let i = self.cmd_idx();
        &mut self.cmds[i]
    }

    fn field_count(&self) -> usize {
        self.cmd().fields.len()
    }

    fn assembled_command(&self) -> String {
        let cmd = self.cmd();
        let mut parts = vec!["cih-engine".to_string(), cmd.name.to_string()];

        // Positional args first, then flags.
        for pass in [true, false] {
            for f in &cmd.fields {
                if (f.flag.is_empty()) == pass {
                    if let Some(frag) = f.to_shell_fragment() {
                        parts.push(frag);
                    }
                }
            }
        }
        parts.join(" ")
    }

    /// Args to pass when spawning the binary (everything after "cih-engine").
    fn run_args(&self) -> Vec<String> {
        let cmd = self.cmd();
        let mut args = vec![cmd.name.to_string()];

        // Positional
        for f in &cmd.fields {
            if f.flag.is_empty() {
                if let FieldVal::Text(s) = &f.val {
                    if !s.is_empty() {
                        args.push(s.clone());
                    }
                }
            }
        }
        // Flags
        for f in &cmd.fields {
            if f.flag.is_empty() {
                continue;
            }
            match &f.val {
                FieldVal::Bool(true) => args.push(f.flag.to_string()),
                FieldVal::Text(s) if !s.is_empty() => {
                    args.push(f.flag.to_string());
                    args.push(s.clone());
                }
                FieldVal::Select(i) => {
                    let v = f.options.get(*i).copied().unwrap_or("");
                    args.push(f.flag.to_string());
                    args.push(v.to_string());
                }
                _ => {}
            }
        }
        args
    }
}

// ── Event handling ────────────────────────────────────────────────────────────

fn handle_cmd_list(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.cmd_idx();
            if i > 0 {
                app.cmd_list.select(Some(i - 1));
                app.field_idx = 0;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.cmd_idx();
            if i + 1 < app.cmds.len() {
                app.cmd_list.select(Some(i + 1));
                app.field_idx = 0;
            }
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Tab => {
            app.mode = Mode::FieldNav;
            app.field_idx = 0;
        }
        _ => {}
    }
}

fn handle_field_nav(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
            if app.field_idx > 0 {
                app.field_idx -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            if app.field_idx + 1 < app.field_count() {
                app.field_idx += 1;
            }
        }
        KeyCode::Esc | KeyCode::Left => {
            app.mode = Mode::CmdList;
        }
        KeyCode::Char('r') => {
            app.mode = Mode::Confirm;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let fi = app.field_idx;
            let val = app.cmd().fields[fi].val.clone();
            match val {
                FieldVal::Bool(b) => {
                    app.cmd_mut().fields[fi].val = FieldVal::Bool(!b);
                }
                FieldVal::Text(_) => {
                    app.mode = Mode::TextEdit;
                }
                FieldVal::Select(i) => {
                    let len = app.cmd().fields[fi].options.len();
                    app.cmd_mut().fields[fi].val = FieldVal::Select((i + 1) % len);
                }
            }
        }
        KeyCode::Right => {
            let fi = app.field_idx;
            let val = app.cmd().fields[fi].val.clone();
            match val {
                FieldVal::Select(i) => {
                    let len = app.cmd().fields[fi].options.len();
                    app.cmd_mut().fields[fi].val = FieldVal::Select((i + 1) % len);
                }
                FieldVal::Text(_) => app.mode = Mode::TextEdit,
                _ => {}
            }
        }
        _ => {}
    }
}

fn handle_text_edit(app: &mut App, key: KeyCode) {
    let fi = app.field_idx;
    match key {
        KeyCode::Esc | KeyCode::Enter => {
            app.mode = Mode::FieldNav;
        }
        KeyCode::Backspace => {
            if let FieldVal::Text(ref mut s) = app.cmd_mut().fields[fi].val {
                s.pop();
            }
        }
        KeyCode::Char(c) => {
            if let FieldVal::Text(ref mut s) = app.cmd_mut().fields[fi].val {
                s.push(c);
            }
        }
        _ => {}
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Outer layout: title | content | cmd preview | help bar
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    // Title
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "  CIH",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " — Command Builder",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        outer[0],
    );

    // Content: left cmd list | right field form
    let content = Layout::horizontal([Constraint::Length(13), Constraint::Min(30)])
        .split(outer[1]);

    render_cmd_list(frame, app, content[0]);
    render_fields(frame, app, content[1]);

    // Assembled command preview
    let cmd_str = app.assembled_command();
    let (prefix, cmd_style) = if app.mode == Mode::Confirm {
        (
            "  Run? $ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("  $ ", Style::default().fg(Color::Green))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::DarkGray)),
            Span::styled(cmd_str, cmd_style),
        ])),
        outer[2],
    );

    // Help / confirm bar
    let help_line = match app.mode {
        Mode::Confirm => Line::from(vec![
            Span::styled(
                "  [Enter/y] Run",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled("[Esc/n] Edit more", Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled("[q] Quit", Style::default().fg(Color::DarkGray)),
        ]),
        Mode::TextEdit => Line::from(vec![
            Span::styled(
                "  [Enter/Esc] Done editing",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Mode::FieldNav => Line::from(vec![
            Span::styled("  [↑↓/jk]", Style::default().fg(Color::DarkGray)),
            Span::raw(" field   "),
            Span::styled("[Enter/Space]", Style::default().fg(Color::DarkGray)),
            Span::raw(" edit/toggle   "),
            Span::styled("[→]", Style::default().fg(Color::DarkGray)),
            Span::raw(" cycle   "),
            Span::styled("[r]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" run   "),
            Span::styled("[Esc]", Style::default().fg(Color::DarkGray)),
            Span::raw(" commands   "),
            Span::styled("[q]", Style::default().fg(Color::DarkGray)),
            Span::raw(" quit"),
        ]),
        Mode::CmdList => Line::from(vec![
            Span::styled("  [↑↓/jk]", Style::default().fg(Color::DarkGray)),
            Span::raw(" command   "),
            Span::styled("[Enter/→]", Style::default().fg(Color::DarkGray)),
            Span::raw(" options   "),
            Span::styled("[q]", Style::default().fg(Color::DarkGray)),
            Span::raw(" quit"),
        ]),
    };
    frame.render_widget(Paragraph::new(help_line), outer[3]);
}

fn render_cmd_list(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .cmds
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let is_sel = i == app.cmd_idx();
            let style = if is_sel && app.mode == Mode::CmdList {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_sel {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Span::styled(format!(" {}", cmd.name), style))
        })
        .collect();

    let border_style = if app.mode == Mode::CmdList {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(border_style),
        )
        .highlight_symbol("▶");

    let mut state = app.cmd_list.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_fields(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    // cmd description (1 line) | fields list | active field description (2 lines)
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(area);

    // Command description header
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", app.cmd().name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.cmd().desc, Style::default().fg(Color::DarkGray)),
        ])),
        chunks[0],
    );

    // Field rows
    let in_right = matches!(app.mode, Mode::FieldNav | Mode::TextEdit | Mode::Confirm);
    let items: Vec<ListItem> = app
        .cmd()
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let active = in_right && i == app.field_idx;
            let editing = active && app.mode == Mode::TextEdit;

            let label_style = if active {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if f.required || f.has_value() {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let val_str = f.display_value();
            let val_style = if editing {
                Style::default().fg(Color::Yellow)
            } else if active {
                Style::default().fg(Color::Green)
            } else {
                match &f.val {
                    FieldVal::Bool(true) => Style::default().fg(Color::Green),
                    FieldVal::Text(s) if !s.is_empty() => Style::default().fg(Color::White),
                    _ => Style::default().fg(Color::DarkGray),
                }
            };

            let cursor = if editing { "█" } else { "" };

            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:24}", f.label), label_style),
                Span::styled(format!("{}{}", val_str, cursor), val_style),
            ]))
        })
        .collect();

    frame.render_widget(List::new(items), chunks[1]);

    // Active field description
    let desc = if in_right && app.field_idx < app.cmd().fields.len() {
        app.cmd().fields[app.field_idx].desc
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", desc),
            Style::default().fg(Color::DarkGray),
        ))
        .wrap(Wrap { trim: true }),
        chunks[2],
    );
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the TUI. Returns `Some(args)` if the user confirmed a command to run,
/// or `None` if they quit without running.
pub fn run_tui() -> Result<Option<Vec<String>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let result = event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<Option<Vec<String>>> {
    loop {
        terminal.draw(|f| render(f, app))?;

        if let Event::Key(key) = event::read()? {
            // Ctrl-C always quits
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                return Ok(None);
            }

            // 'q' quits in non-edit modes
            if key.code == KeyCode::Char('q') && app.mode != Mode::TextEdit {
                return Ok(None);
            }

            match app.mode {
                Mode::CmdList => handle_cmd_list(app, key.code),
                Mode::FieldNav => handle_field_nav(app, key.code),
                Mode::TextEdit => handle_text_edit(app, key.code),
                Mode::Confirm => match key.code {
                    KeyCode::Enter | KeyCode::Char('y') => {
                        return Ok(Some(app.run_args()));
                    }
                    KeyCode::Esc | KeyCode::Char('n') => {
                        app.mode = Mode::FieldNav;
                    }
                    _ => {}
                },
            }
        }
    }
}
