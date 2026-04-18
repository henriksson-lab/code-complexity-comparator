//! Terminal UI for qualitatively comparing a Rust function against its
//! original-language counterpart, pair by pair. Input is a mapping file; we
//! locate each referenced file by basename under the provided roots, parse
//! it with the existing analyzer, and pull out the function's source slice.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::PathBuf;

use crate::core::Language;

pub mod highlight;
pub mod pairs;
pub mod render;

pub use highlight::HighlightMode;
pub use pairs::{LocatedFn, Pair};

pub struct Args {
    pub mapping: PathBuf,
    pub rust_root: PathBuf,
    pub other_root: PathBuf,
    pub other_lang: Language,
}

pub struct App {
    pub pairs: Vec<Pair>,
    pub idx: usize,
    pub scroll: u16,
    pub mode: HighlightMode,
    pub status: String,
}

impl App {
    pub fn new(pairs: Vec<Pair>) -> Self {
        Self {
            pairs,
            idx: 0,
            scroll: 0,
            mode: HighlightMode::Language,
            status: String::new(),
        }
    }

    pub fn next(&mut self) {
        if self.idx + 1 < self.pairs.len() {
            self.idx += 1;
            self.scroll = 0;
        }
    }

    pub fn prev(&mut self) {
        if self.idx > 0 {
            self.idx -= 1;
            self.scroll = 0;
        }
    }

    pub fn scroll_down(&mut self, n: u16) {
        let max = self.max_scroll();
        self.scroll = self.scroll.saturating_add(n).min(max);
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    fn max_scroll(&self) -> u16 {
        let Some(p) = self.pairs.get(self.idx) else { return 0 };
        let rl = p.rust.as_ref().map(|f| f.line_count()).unwrap_or(0);
        let ol = p.other.as_ref().map(|f| f.line_count()).unwrap_or(0);
        rl.max(ol).saturating_sub(1) as u16
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            HighlightMode::Language => HighlightMode::IdentityShared,
            HighlightMode::IdentityShared => HighlightMode::Language,
        };
    }
}

/// Walk `root` and pick the language with the most source files, excluding
/// Rust (since we're always comparing against Rust) and `Unknown`. Returns
/// `None` when the directory holds no recognized source.
pub fn detect_other_language(root: &std::path::Path) -> Option<Language> {
    use std::collections::HashMap;
    let mut counts: HashMap<Language, usize> = HashMap::new();
    let _ = scan_counts(root, &mut counts);
    counts
        .into_iter()
        .filter(|(l, _)| !matches!(l, Language::Rust | Language::Unknown))
        .max_by_key(|(_, n)| *n)
        .map(|(l, _)| l)
}

fn scan_counts(root: &std::path::Path, counts: &mut std::collections::HashMap<Language, usize>) -> std::io::Result<()> {
    if root.is_file() {
        if let Some(ext) = root.extension().and_then(|e| e.to_str()) {
            *counts.entry(Language::from_ext(ext)).or_insert(0) += 1;
        }
        return Ok(());
    }
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n.starts_with('.') || n == "target" || n == "node_modules" {
            continue;
        }
        let p = entry.path();
        if p.is_dir() {
            let _ = scan_counts(&p, counts);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            *counts.entry(Language::from_ext(ext)).or_insert(0) += 1;
        }
    }
    Ok(())
}

pub fn run(args: Args) -> Result<()> {
    let pairs = pairs::load(&args)?;
    if pairs.is_empty() {
        anyhow::bail!("no pairs loaded from mapping");
    }
    let mut app = App::new(pairs);
    app.status = format!("{} pairs", app.pairs.len());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let res = event_loop(&mut term, &mut app);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn event_loop<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        term.draw(|f| render::draw(f, app))?;
        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => break,
                KeyCode::Char('c') | KeyCode::Char('C') => app.toggle_mode(),
                KeyCode::Up => app.prev(),
                KeyCode::Down => app.next(),
                KeyCode::Left => app.scroll_up(1),
                KeyCode::Right => app.scroll_down(1),
                KeyCode::PageUp => app.scroll_up(10),
                KeyCode::PageDown => app.scroll_down(10),
                KeyCode::Home => app.scroll = 0,
                KeyCode::End => app.scroll = app.max_scroll(),
                _ => {}
            }
        }
    }
    Ok(())
}
