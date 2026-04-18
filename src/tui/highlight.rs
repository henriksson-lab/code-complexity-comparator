//! Tokenization for rendering. Parses the function's source with tree-sitter,
//! walks the leaf nodes within the function's byte range, and emits tokens
//! with positions relative to the first line of the function.
//!
//! Two rendering modes live on top of these tokens; both consume the same
//! `Vec<Token>`:
//!
//! 1. `Language` — classic per-kind coloring (keyword, string, comment, ...).
//! 2. `IdentityShared` — every identifier gets a per-name color so the
//!    matching variable/method on the other side shows up in the same color.

use tree_sitter::{Node, Parser, TreeCursor};

use crate::core::{FunctionAnalysis, Language};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightMode {
    Language,
    IdentityShared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Type,
    Identifier,
    String,
    Number,
    Comment,
    Operator,
    Punctuation,
    Other,
}

#[derive(Clone, Debug)]
pub struct Token {
    /// 0-based line within the function source (as split by `\n`).
    pub line: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub text: String,
    pub kind: TokenKind,
}

pub fn tokenize_function(lang: Language, source: &str, fa: &FunctionAnalysis) -> Vec<Token> {
    let Some(ts_lang) = ts_language(lang) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let fn_start_row = fa.location.line_start.saturating_sub(1);
    let fn_start_col = fa.location.col_start;
    let byte_start = fa.location.byte_start as usize;
    let byte_end = (fa.location.byte_end as usize).min(source.len());

    let mut tokens = Vec::new();
    let mut cursor = tree.walk();
    visit(
        &mut cursor,
        source.as_bytes(),
        lang,
        byte_start,
        byte_end,
        fn_start_row,
        fn_start_col,
        &mut tokens,
    );
    tokens
}

fn visit(
    cursor: &mut TreeCursor,
    src: &[u8],
    lang: Language,
    byte_start: usize,
    byte_end: usize,
    fn_start_row: u32,
    fn_start_col: u32,
    out: &mut Vec<Token>,
) {
    let node = cursor.node();
    let s = node.start_byte();
    let e = node.end_byte();
    if e <= byte_start || s >= byte_end {
        return;
    }
    if node.child_count() == 0 {
        emit_leaf(node, src, lang, fn_start_row, fn_start_col, out);
        return;
    }
    if cursor.goto_first_child() {
        loop {
            visit(cursor, src, lang, byte_start, byte_end, fn_start_row, fn_start_col, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn emit_leaf(
    node: Node,
    src: &[u8],
    lang: Language,
    fn_start_row: u32,
    fn_start_col: u32,
    out: &mut Vec<Token>,
) {
    let s = node.start_byte();
    let e = node.end_byte();
    if s >= e || e > src.len() {
        return;
    }
    let text = match std::str::from_utf8(&src[s..e]) {
        Ok(t) => t,
        Err(_) => return,
    };
    let parent_kind = node.parent().map(|p| p.kind().to_string());
    let kind = classify(lang, node.kind(), parent_kind.as_deref(), text);

    let sp = node.start_position();
    let row = (sp.row as u32).saturating_sub(fn_start_row);
    let col0 = if sp.row as u32 == fn_start_row {
        (sp.column as u32).saturating_sub(fn_start_col)
    } else {
        sp.column as u32
    };

    // Split multi-line tokens at newlines so every emitted Token lives on a
    // single visual line. Columns accumulate per line; multi-line tokens
    // (block comments, raw strings) restart at column 0 on subsequent lines.
    let mut cur_line = row;
    let mut cur_col = col0;
    let mut buf = String::new();
    for ch in text.chars() {
        if ch == '\n' {
            if !buf.is_empty() {
                let w = display_width(&buf);
                out.push(Token {
                    line: cur_line,
                    col_start: cur_col,
                    col_end: cur_col + w,
                    text: std::mem::take(&mut buf),
                    kind,
                });
            }
            cur_line += 1;
            cur_col = 0;
        } else if ch == '\r' {
            // ignore
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        let w = display_width(&buf);
        out.push(Token {
            line: cur_line,
            col_start: cur_col,
            col_end: cur_col + w,
            text: buf,
            kind,
        });
    }
}

fn display_width(s: &str) -> u32 {
    // Tab-agnostic column count; enough for laying out spans on a line.
    s.chars().count() as u32
}

fn classify(_lang: Language, node_kind: &str, parent_kind: Option<&str>, text: &str) -> TokenKind {
    // Comments.
    if node_kind.contains("comment") {
        return TokenKind::Comment;
    }
    // Strings and character literals.
    if node_kind.contains("string") || node_kind.contains("char_literal") {
        return TokenKind::String;
    }
    // Numeric literals.
    if node_kind.contains("integer")
        || node_kind.contains("number_literal")
        || node_kind.contains("float")
        || node_kind == "number"
    {
        return TokenKind::Number;
    }
    // Type identifiers.
    if node_kind.contains("type_identifier") || node_kind == "primitive_type" {
        return TokenKind::Type;
    }
    // Identifiers — covers `identifier`, `field_identifier`, `scoped_identifier`,
    // `property_identifier`, `word`, etc.
    if node_kind.contains("identifier") || node_kind == "word" || node_kind == "name" {
        return TokenKind::Identifier;
    }
    // Keywords: tree-sitter represents most keywords as the literal string
    // (e.g. `"if"`, `"return"`, `"fn"`). Heuristic: an unnamed leaf whose
    // text is all lowercase letters is a keyword.
    let t = text.trim();
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_lowercase() || c == '_') && t.len() <= 24 {
        if matches_keyword(node_kind, parent_kind, t) {
            return TokenKind::Keyword;
        }
    }
    // Operators and punctuation — symbolic tokens.
    if !t.is_empty() && t.chars().all(|c| !c.is_alphanumeric() && !c.is_whitespace()) {
        // Heuristic split: brackets and separators are punctuation; the rest
        // (arithmetic, comparison, assignment) are operators.
        if matches!(t, "(" | ")" | "[" | "]" | "{" | "}" | "," | ";" | ":" | ".") {
            return TokenKind::Punctuation;
        }
        return TokenKind::Operator;
    }
    TokenKind::Other
}

fn matches_keyword(node_kind: &str, _parent_kind: Option<&str>, text: &str) -> bool {
    // Tree-sitter gives keywords as either anonymous nodes whose `kind()` is
    // the literal (e.g. `"if"` — same as text) or a named node like
    // `"primitive_type"`. If node kind equals text, it's the keyword form.
    if node_kind == text {
        return true;
    }
    // A small universal list covering our supported languages.
    matches!(
        text,
        "if" | "else"
            | "elif"
            | "while"
            | "for"
            | "do"
            | "loop"
            | "return"
            | "break"
            | "continue"
            | "switch"
            | "case"
            | "default"
            | "match"
            | "fn"
            | "def"
            | "function"
            | "let"
            | "const"
            | "var"
            | "mut"
            | "pub"
            | "use"
            | "mod"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "where"
            | "class"
            | "interface"
            | "extends"
            | "implements"
            | "package"
            | "import"
            | "from"
            | "as"
            | "in"
            | "is"
            | "not"
            | "and"
            | "or"
            | "true"
            | "false"
            | "null"
            | "none"
            | "nil"
            | "self"
            | "super"
            | "this"
            | "new"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "throws"
            | "yield"
            | "lambda"
            | "async"
            | "await"
            | "unsafe"
            | "extern"
            | "static"
            | "inline"
            | "volatile"
            | "typedef"
            | "sizeof"
            | "goto"
            | "cycle"
            | "exit"
            | "stop"
            | "subroutine"
            | "program"
            | "end"
            | "then"
            | "my"
            | "our"
            | "sub"
            | "foreach"
            | "last"
            | "next"
            | "redo"
            | "unless"
            | "until"
    )
}

fn ts_language(lang: Language) -> Option<tree_sitter::Language> {
    match lang {
        Language::C => Some(tree_sitter_c::LANGUAGE.into()),
        Language::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::R => Some(tree_sitter_r::LANGUAGE.into()),
        Language::Perl => Some(tree_sitter_perl_next::LANGUAGE.into()),
        Language::Fortran => Some(tree_sitter_fortran::LANGUAGE.into()),
        Language::Unknown => None,
    }
}
