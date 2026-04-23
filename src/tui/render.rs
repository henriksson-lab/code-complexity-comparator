//! Ratatui drawing: split-pane source, a stats row per side, title, footer.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::compare::matching::normalize_name;
use crate::core::Metrics;

use super::highlight::{HighlightMode, Token, TokenKind};
use super::pairs::{LocatedFn, Pair};
use super::App;

const PALETTE: &[Color] = &[
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
];

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // title
            Constraint::Min(3),    // code panes
            Constraint::Length(6), // stats
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_title(f, root[0], app);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(root[1]);
    let stats = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(root[2]);

    let pair = app.pairs.get(app.idx);
    let (left, right) = match pair {
        Some(p) => (p.rust.as_ref(), p.other.as_ref()),
        None => (None, None),
    };

    // Mode-2 coloring is shared across both panes: build the color map once
    // from the union of identifier names across both sides so the same name
    // on rust and other resolves to the same color.
    let id_colors = match app.mode {
        HighlightMode::Language => HashMap::new(),
        HighlightMode::IdentityShared => build_identity_colors(left, right),
    };

    draw_pane(f, panes[0], "rust", left, pair, app, &id_colors, PaneSide::Rust);
    draw_pane(f, panes[1], lang_label(pair), right, pair, app, &id_colors, PaneSide::Other);

    draw_stats(f, stats[0], left.and_then(|x| x.metrics.as_ref()));
    draw_stats(f, stats[1], right.and_then(|x| x.metrics.as_ref()));

    draw_footer(f, root[3], app);
}

enum PaneSide {
    Rust,
    Other,
}

fn lang_label(pair: Option<&Pair>) -> &'static str {
    match pair.and_then(|p| p.other.as_ref()).map(|f| f.language) {
        Some(l) => l.as_str(),
        None => "other",
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let title = if let Some(p) = app.pairs.get(app.idx) {
        let rl = p
            .rust
            .as_ref()
            .map(|f| {
                format!(
                    "{} @ {}:{}",
                    qualified_name(&f.enclosing_type, &f.name),
                    f.file.display(),
                    f.line_start
                )
            })
            .unwrap_or_else(|| format!("{} [missing]", p.rust_target));
        let ol = p
            .other
            .as_ref()
            .map(|f| {
                format!(
                    "{} @ {}:{}",
                    qualified_name(&f.enclosing_type, &f.name),
                    f.file.display(),
                    f.line_start
                )
            })
            .unwrap_or_else(|| format!("{} [missing]", p.other_target));
        format!("pair {}/{}   rust: {}    other: {}", app.idx + 1, app.pairs.len(), rl, ol)
    } else {
        "no pairs".to_string()
    };
    let p = Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(p, area);
}

/// Render `Class::method` when the function lives inside a class/impl, else
/// the bare name. Mirrors how both Rust (`Cluster::new`) and Python
/// (`Cluster.__init__`, displayed here as `Cluster::__init__` for uniformity)
/// users typically refer to methods.
fn qualified_name(enclosing: &Option<String>, name: &str) -> String {
    match enclosing {
        Some(cls) => format!("{cls}::{name}"),
        None => name.to_string(),
    }
}

fn draw_pane(
    f: &mut Frame,
    area: Rect,
    side_label: &str,
    located: Option<&LocatedFn>,
    pair: Option<&Pair>,
    app: &App,
    id_colors: &HashMap<String, Color>,
    side: PaneSide,
) {
    let title = match located {
        Some(lf) => format!(
            "{} — {} ({}:{}-{})",
            side_label,
            qualified_name(&lf.enclosing_type, &lf.name),
            file_tail(&lf.file.display().to_string()),
            lf.line_start,
            lf.line_end
        ),
        None => {
            let note = pair.and_then(|p| match side {
                PaneSide::Rust => p.rust_note.as_deref(),
                PaneSide::Other => p.other_note.as_deref(),
            });
            match note {
                Some(n) => format!("{} — <missing: {}>", side_label, n),
                None => format!("{} — <missing>", side_label),
            }
        }
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(lf) = located else {
        return;
    };
    let lines = build_lines(lf, app.mode, id_colors);
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
    f.render_widget(para, inner);
}

fn file_tail(path: &str) -> String {
    // Keep the last two components so titles stay readable in narrow panes.
    let parts: Vec<&str> = path.rsplit('/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        path.to_string()
    }
}

fn draw_stats(f: &mut Frame, area: Rect, metrics: Option<&Metrics>) {
    let text = match metrics {
        Some(m) => vec![
            Line::from(format!(
                "cyclomatic {:>4}   cognitive {:>4}   nesting {:>3}",
                m.cyclomatic, m.cognitive, m.max_combined_nesting
            )),
            Line::from(format!(
                "loc code   {:>4}   comments  {:>4}   asm     {:>3}",
                m.loc_code, m.loc_comments, m.loc_asm
            )),
            Line::from(format!(
                "calls {:>3}/{:<3}  branches {:>4}  loops {:>3}  returns {:>3}",
                m.calls_unique, m.calls_total, m.branches, m.loops, m.early_returns
            )),
            Line::from(format!(
                "halstead V={:.1}  D={:.2}  n1={} n2={} N1={} N2={}",
                m.halstead.volume,
                m.halstead.difficulty,
                m.halstead.n1,
                m.halstead.n2,
                m.halstead.big_n1,
                m.halstead.big_n2,
            )),
        ],
        None => vec![Line::from(Span::styled(
            "<no metrics>",
            Style::default().fg(Color::DarkGray),
        ))],
    };
    let para = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("stats"));
    f.render_widget(para, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let mode = match app.mode {
        HighlightMode::Language => "Language",
        HighlightMode::IdentityShared => "Identity-Shared",
    };
    let s = format!(
        "mode: {}   ←→ pair   ↑↓ scroll   PgUp/PgDn ±10   C toggle mode   Q quit",
        mode
    );
    let p = Paragraph::new(s).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}

fn build_lines(
    lf: &LocatedFn,
    mode: HighlightMode,
    id_colors: &HashMap<String, Color>,
) -> Vec<Line<'static>> {
    // Group tokens by line for O(total) assembly.
    let mut by_line: Vec<Vec<&Token>> = (0..lf.lines.len()).map(|_| Vec::new()).collect();
    for t in &lf.tokens {
        let idx = t.line as usize;
        if idx < by_line.len() {
            by_line[idx].push(t);
        }
    }
    for row in by_line.iter_mut() {
        row.sort_by_key(|t| t.col_start);
    }

    let mut out = Vec::with_capacity(lf.lines.len());
    for (i, line_text) in lf.lines.iter().enumerate() {
        out.push(render_line(line_text, &by_line[i], mode, id_colors));
    }
    out
}

fn render_line(
    line: &str,
    tokens: &[&Token],
    mode: HighlightMode,
    id_colors: &HashMap<String, Color>,
) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    for t in tokens {
        let start = (t.col_start as usize).min(chars.len());
        let end = (t.col_end as usize).min(chars.len());
        if start < cursor {
            // Overlapping tokens (can happen if tree-sitter emits something we
            // didn't expect). Skip the overlap.
            continue;
        }
        if start > cursor {
            let gap: String = chars[cursor..start].iter().collect();
            spans.push(Span::raw(gap));
        }
        let text: String = chars[start..end].iter().collect();
        spans.push(Span::styled(text, style_for(t, mode, id_colors)));
        cursor = end;
    }
    if cursor < chars.len() {
        let tail: String = chars[cursor..].iter().collect();
        spans.push(Span::raw(tail));
    }
    Line::from(spans)
}

fn style_for(t: &Token, mode: HighlightMode, id_colors: &HashMap<String, Color>) -> Style {
    match mode {
        HighlightMode::Language => language_style(t.kind),
        HighlightMode::IdentityShared => match t.kind {
            TokenKind::Identifier | TokenKind::Type => {
                let key = normalize_name(&t.text);
                if let Some(c) = id_colors.get(&key) {
                    Style::default().fg(*c).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                }
            }
            TokenKind::Comment => Style::default().fg(Color::DarkGray),
            _ => Style::default().fg(Color::Gray),
        },
    }
}

fn language_style(kind: TokenKind) -> Style {
    match kind {
        TokenKind::Keyword => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        TokenKind::Type => Style::default().fg(Color::Cyan),
        TokenKind::Identifier => Style::default().fg(Color::White),
        TokenKind::String => Style::default().fg(Color::Green),
        TokenKind::Number => Style::default().fg(Color::LightMagenta),
        TokenKind::Comment => Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        TokenKind::Operator => Style::default().fg(Color::LightBlue),
        TokenKind::Punctuation => Style::default().fg(Color::Gray),
        TokenKind::Other => Style::default(),
    }
}

fn build_identity_colors(
    rust: Option<&LocatedFn>,
    other: Option<&LocatedFn>,
) -> HashMap<String, Color> {
    let mut colors: HashMap<String, Color> = HashMap::new();
    let mut next = 0usize;
    for lf in [rust, other].into_iter().flatten() {
        for t in &lf.tokens {
            if !matches!(t.kind, TokenKind::Identifier | TokenKind::Type) {
                continue;
            }
            let key = normalize_name(&t.text);
            if key.is_empty() {
                continue;
            }
            if !colors.contains_key(&key) {
                colors.insert(key, PALETTE[next % PALETTE.len()]);
                next += 1;
            }
        }
    }
    colors
}
