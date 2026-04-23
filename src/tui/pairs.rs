//! Mapping-driven pair discovery. For each mapping entry we resolve the
//! Rust side and the other-language side independently; either may come back
//! `None` (file or function not found), which is kept in the list.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use std::collections::HashSet;

use crate::analyzer::LanguageAnalyzer;
use crate::compare::matching::{class_matches, path_suffix_matches, Mapping};
use crate::core::{FunctionAnalysis, Language, Metrics};
use crate::lang_c::CAnalyzer;
use crate::lang_fortran::FortranAnalyzer;
use crate::lang_java::JavaAnalyzer;
use crate::lang_perl::PerlAnalyzer;
use crate::lang_python::PythonAnalyzer;
use crate::lang_r::RAnalyzer;
use crate::lang_rust::RustAnalyzer;

use super::highlight::{tokenize_range, Token};
use super::Args;

pub struct Pair {
    pub rust: Option<LocatedFn>,
    pub other: Option<LocatedFn>,
    pub rust_target: String,
    pub other_target: String,
    pub rust_path_hint: Option<String>,
    pub other_path_hint: Option<String>,
    pub rust_note: Option<String>,
    pub other_note: Option<String>,
}

pub struct LocatedFn {
    pub name: String,
    pub enclosing_type: Option<String>,
    pub file: PathBuf,
    pub line_start: u32,
    pub line_end: u32,
    pub language: Language,
    pub lines: Vec<String>,
    pub tokens: Vec<Token>,
    pub metrics: Option<Metrics>,
}

impl LocatedFn {
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

pub fn load(args: &Args) -> Result<Vec<Pair>> {
    let mapping = Mapping::load(&args.mapping)?;
    let rust_index = FileIndex::build(&args.rust_root, &["rs"])?;
    let other_exts = extensions_for(args.other_lang);
    let other_index = FileIndex::build(&args.other_root, &other_exts)?;

    let mut file_cache: HashMap<PathBuf, FileAnalysis> = HashMap::new();
    // Track which specific functions have already been picked by earlier
    // entries so that repeated mapping entries (e.g. two `new`s in the same
    // file disambiguated by class) each grab a different candidate.
    // Keyed by (resolved file path, byte_start) — byte_start is the most
    // stable per-function identity the analyzer emits.
    let mut used_rust: HashSet<(PathBuf, u32)> = HashSet::new();
    let mut used_other: HashSet<(PathBuf, u32)> = HashSet::new();

    let mut pairs = Vec::new();
    for entry in &mapping.entries {
        let (rust, rnote) = resolve_side(
            &rust_index,
            &mut file_cache,
            &mut used_rust,
            Language::Rust,
            &entry.rust,
            entry.rust_path.as_deref(),
            entry.rust_class.as_deref(),
            entry.rust_line,
        );
        let (other, onote) = resolve_side(
            &other_index,
            &mut file_cache,
            &mut used_other,
            args.other_lang,
            &entry.other,
            entry.other_path.as_deref(),
            entry.other_class.as_deref(),
            entry.other_line,
        );
        pairs.push(Pair {
            rust,
            other,
            rust_target: entry.rust.clone(),
            other_target: entry.other.clone(),
            rust_path_hint: entry.rust_path.clone(),
            other_path_hint: entry.other_path.clone(),
            rust_note: rnote,
            other_note: onote,
        });
    }
    Ok(pairs)
}

#[allow(clippy::too_many_arguments)]
fn resolve_side(
    index: &FileIndex,
    cache: &mut HashMap<PathBuf, FileAnalysis>,
    used: &mut HashSet<(PathBuf, u32)>,
    lang: Language,
    fn_name: &str,
    path_hint: Option<&str>,
    class_hint: Option<&str>,
    line_hint: Option<u32>,
) -> (Option<LocatedFn>, Option<String>) {
    let candidates: Vec<PathBuf> = match path_hint {
        Some(hint) => {
            let leaf = Path::new(hint)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(hint);
            match index.by_basename.get(leaf) {
                Some(paths) => paths
                    .iter()
                    .filter(|p| path_suffix_matches(p, Some(hint)))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            }
        }
        None => index.all.clone(),
    };

    for file in &candidates {
        if !cache.contains_key(file.as_path()) {
            let a = analyze_file(lang, file).unwrap_or_else(|e| FileAnalysis {
                source: String::new(),
                functions: Vec::new(),
                error: Some(e.to_string()),
            });
            cache.insert(file.clone(), a);
        }
        let analysis = cache.get(file.as_path()).expect("inserted above");
        if analysis.error.is_some() {
            continue;
        }
        let hit = analysis.functions.iter().find(|f| {
            f.name == fn_name
                && class_matches(f.enclosing_type.as_deref(), class_hint)
                && line_hint.is_none_or(|ln| f.location.line_start == ln)
                && !used.contains(&(file.clone(), f.location.byte_start))
        });
        if let Some(fa) = hit {
            used.insert((file.clone(), fa.location.byte_start));
            // Expand the displayed slice backward to include leading comment
            // blocks so Rust `///` docs show alongside the function, matching
            // the way Python docstrings (which live inside the body) already
            // render. Attributes (`#[...]`) are pulled in on the Rust side
            // too since readers treat them as part of the signature.
            let expanded_start =
                expand_to_leading_comments(&analysis.source, fa.location.byte_start as usize, lang);
            // Snap to the beginning of the line so the first displayed row
            // is always column-0 aligned. Without this, methods whose
            // `byte_start` sits mid-indent (e.g. `    fn foo`) slice into
            // the middle of their own leading whitespace, and token
            // columns on row 0 end up shifted relative to every subsequent
            // row — which is the off-by-indent coloring you were seeing.
            let display_start = line_start_offset(&analysis.source, expanded_start);
            let line_start = line_for_byte(&analysis.source, display_start);
            let tokens = tokenize_range(
                lang,
                &analysis.source,
                display_start,
                fa.location.byte_end as usize,
                line_start.saturating_sub(1),
                0,
            );
            let lines = slice_range_as_lines(
                &analysis.source,
                display_start,
                fa.location.byte_end as usize,
            );
            return (
                Some(LocatedFn {
                    name: fa.name.clone(),
                    enclosing_type: fa.enclosing_type.clone(),
                    file: file.clone(),
                    line_start,
                    line_end: fa.location.line_end,
                    language: lang,
                    lines,
                    tokens,
                    metrics: Some(fa.metrics.clone()),
                }),
                None,
            );
        }
    }

    let note = if candidates.is_empty() {
        Some(format!(
            "no file matched hint {:?}",
            path_hint.unwrap_or(fn_name)
        ))
    } else {
        let mut parts = vec![format!("'{}'", fn_name)];
        if let Some(c) = class_hint {
            parts.push(format!("class={c}"));
        }
        if let Some(ln) = line_hint {
            parts.push(format!("line={ln}"));
        }
        Some(format!(
            "{} not found in {} file(s)",
            parts.join(" "),
            candidates.len()
        ))
    };
    (None, note)
}

fn slice_range_as_lines(source: &str, start: usize, end: usize) -> Vec<String> {
    let end = end.min(source.len());
    if start >= end {
        return Vec::new();
    }
    source[start..end]
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect()
}

/// Line number (1-based) for the given byte offset.
fn line_for_byte(source: &str, byte: usize) -> u32 {
    let byte = byte.min(source.len());
    // Count the newlines before this offset.
    let count = source[..byte].bytes().filter(|b| *b == b'\n').count();
    (count + 1) as u32
}

/// Byte offset of the first character on the line containing `byte`.
fn line_start_offset(source: &str, byte: usize) -> usize {
    let byte = byte.min(source.len());
    source[..byte].rfind('\n').map(|n| n + 1).unwrap_or(0)
}

/// Walk backward from the function's byte_start past contiguous leading
/// comment (and, for Rust, attribute) lines, stopping at the first blank or
/// non-comment line. Returns the new byte offset at which to begin slicing.
///
/// Handles both line comments and multi-line block comments. The language
/// determines which markers count: `#` for Python/R/Perl, `!` for Fortran,
/// `//` + `/* ... */` + `#[...]` for Rust, and similar for C / C++ / Java.
fn expand_to_leading_comments(source: &str, byte_start: usize, lang: Language) -> usize {
    if byte_start == 0 || byte_start > source.len() {
        return byte_start;
    }
    // Collect (line_start_offset, line_content_without_newline) for every
    // line ending at or before byte_start. Only the lines *above* the fn's
    // first line are considered; the fn itself starts at a line boundary
    // (its byte_start comes from the node's start position).
    let head = &source[..byte_start];
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in head.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    // `line_starts` ends with the offset of the line that contains byte_start
    // (when byte_start sits at column 0 the final entry *is* byte_start).
    // Walk backward over the preceding lines.
    let mut new_start = byte_start;
    let mut in_block = false;
    let mut i = line_starts.len();
    while i > 1 {
        i -= 1;
        let line_start = line_starts[i - 1];
        let line_end_excl = line_starts[i].saturating_sub(1); // drop the newline
        let line_end_excl = line_end_excl.min(head.len());
        if line_start > line_end_excl {
            break;
        }
        let line = &source[line_start..line_end_excl];
        let trimmed = line.trim_start();
        let all_trim = line.trim();

        if in_block {
            // Still inside a /* ... */ that closed on a later line.
            new_start = line_start;
            if all_trim.contains("/*") {
                in_block = false;
            }
            continue;
        }
        if all_trim.is_empty() {
            // Blank line separates the doc block from anything further up.
            break;
        }
        // A line ending with "*/" is the close of a block comment. If the
        // same line also opens one with "/*" it's self-contained; otherwise
        // we enter multi-line mode and continue scanning upward.
        if all_trim.ends_with("*/") {
            new_start = line_start;
            if !all_trim.contains("/*") {
                in_block = true;
            }
            continue;
        }

        let include = match lang {
            Language::Rust => {
                trimmed.starts_with("///")
                    || trimmed.starts_with("//!")
                    || trimmed.starts_with("//")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*') // continuation inside /* */
                    || trimmed.starts_with("#[")
                    || trimmed.starts_with("#![")
            }
            Language::C | Language::Cpp | Language::Java => {
                trimmed.starts_with("//")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with('*')
                    || trimmed.starts_with('@') // javadoc @param continuations
            }
            Language::Python | Language::R | Language::Perl => trimmed.starts_with('#'),
            Language::Fortran => trimmed.starts_with('!') || trimmed.starts_with('C'),
            Language::Unknown => false,
        };
        if !include {
            break;
        }
        new_start = line_start;
    }
    new_start
}

struct FileAnalysis {
    source: String,
    functions: Vec<FunctionAnalysis>,
    error: Option<String>,
}

fn analyze_file(lang: Language, path: &Path) -> Result<FileAnalysis> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read {}: {}", path.display(), e))?;
    let analyzer: Box<dyn LanguageAnalyzer> = match lang {
        Language::C => Box::new(CAnalyzer::c()),
        Language::Cpp => Box::new(CAnalyzer::cpp()),
        Language::Rust => Box::new(RustAnalyzer::new()),
        Language::Java => Box::new(JavaAnalyzer::new()),
        Language::Python => Box::new(PythonAnalyzer::new()),
        Language::R => Box::new(RAnalyzer::new()),
        Language::Perl => Box::new(PerlAnalyzer::new()),
        Language::Fortran => Box::new(FortranAnalyzer::new()),
        Language::Unknown => return Err(anyhow!("unknown language")),
    };
    let report = analyzer.analyze_source(&src, path)?;
    Ok(FileAnalysis {
        source: src,
        functions: report.functions,
        error: None,
    })
}

fn extensions_for(lang: Language) -> Vec<&'static str> {
    match lang {
        Language::C => vec!["c", "h"],
        Language::Cpp => vec!["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        Language::Rust => vec!["rs"],
        Language::Java => vec!["java"],
        Language::Python => vec!["py"],
        Language::R => vec!["r", "R"],
        Language::Perl => vec!["pl", "pm", "t"],
        Language::Fortran => vec!["f", "f90", "f95", "f03", "f08", "for", "ftn"],
        Language::Unknown => vec![],
    }
}

struct FileIndex {
    all: Vec<PathBuf>,
    by_basename: HashMap<String, Vec<PathBuf>>,
}

impl FileIndex {
    fn build(root: &Path, exts: &[&str]) -> Result<Self> {
        let mut all = Vec::new();
        walk(root, exts, &mut all)?;
        let mut by_basename: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for p in &all {
            if let Some(base) = p.file_name().and_then(|s| s.to_str()) {
                by_basename.entry(base.to_string()).or_default().push(p.clone());
            }
        }
        Ok(Self { all, by_basename })
    }
}

fn walk(root: &Path, exts: &[&str], out: &mut Vec<PathBuf>) -> Result<()> {
    if root.is_file() {
        if ext_matches(root, exts) {
            out.push(root.to_path_buf());
        }
        return Ok(());
    }
    if !root.is_dir() {
        return Err(anyhow!("not a directory: {}", root.display()));
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let p = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if p.is_dir() {
            walk(&p, exts, out)?;
        } else if ext_matches(&p, exts) {
            out.push(p);
        }
    }
    Ok(())
}

fn ext_matches(p: &Path, exts: &[&str]) -> bool {
    match p.extension().and_then(|e| e.to_str()) {
        Some(e) => exts.iter().any(|x| x.eq_ignore_ascii_case(e)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn byte_offset_of(src: &str, needle: &str) -> usize {
        src.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in source"))
    }

    #[test]
    fn rust_line_doc_comments_are_pulled_in() {
        let src = "\
use foo;

/// First line.
/// Second line.
pub fn bar() {}
";
        let fn_start = byte_offset_of(src, "pub fn");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Rust);
        assert_eq!(
            &src[expanded..fn_start],
            "/// First line.\n/// Second line.\n"
        );
    }

    #[test]
    fn rust_attributes_between_docs_and_fn_are_preserved() {
        let src = "\
struct X;

/// Doc.
#[inline]
pub fn bar() {}
";
        let fn_start = byte_offset_of(src, "pub fn");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Rust);
        assert_eq!(&src[expanded..fn_start], "/// Doc.\n#[inline]\n");
    }

    #[test]
    fn rust_block_doc_comment_is_pulled_in() {
        let src = "\
mod m;

/**
 * Block doc.
 * Continues here.
 */
pub fn bar() {}
";
        let fn_start = byte_offset_of(src, "pub fn");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Rust);
        let slice = &src[expanded..fn_start];
        assert!(slice.starts_with("/**"), "got {slice:?}");
        assert!(slice.ends_with("*/\n"), "got {slice:?}");
    }

    #[test]
    fn blank_line_stops_expansion() {
        let src = "\
/// Unrelated comment.

/// Attached doc.
pub fn bar() {}
";
        let fn_start = byte_offset_of(src, "pub fn");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Rust);
        // Only the doc directly above the fn is included; the one above the
        // blank line is treated as separate and skipped.
        assert_eq!(&src[expanded..fn_start], "/// Attached doc.\n");
    }

    #[test]
    fn python_hash_comments_above_def() {
        let src = "\
x = 1

# This helper does X.
# See issue 42.
def bar():
    pass
";
        let fn_start = byte_offset_of(src, "def bar");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Python);
        assert_eq!(
            &src[expanded..fn_start],
            "# This helper does X.\n# See issue 42.\n"
        );
    }

    #[test]
    fn non_comment_line_stops_expansion() {
        let src = "\
let x = 1;
/// Doc line.
pub fn bar() {}
";
        let fn_start = byte_offset_of(src, "pub fn");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Rust);
        // Stops at the `let x = 1;` line.
        assert_eq!(&src[expanded..fn_start], "/// Doc line.\n");
    }

    #[test]
    fn file_starting_with_doc_comment_does_not_panic() {
        // Edge case: the function starts at byte 0 offset relative to comments,
        // i.e. the file has no content above the doc.
        let src = "\
/// Top doc.
pub fn bar() {}
";
        let fn_start = byte_offset_of(src, "pub fn");
        let expanded = expand_to_leading_comments(src, fn_start, Language::Rust);
        assert_eq!(&src[expanded..fn_start], "/// Top doc.\n");
    }

    #[test]
    fn line_for_byte_is_one_indexed() {
        let src = "a\nb\nc\n";
        assert_eq!(line_for_byte(src, 0), 1);
        assert_eq!(line_for_byte(src, 2), 2);
        assert_eq!(line_for_byte(src, 4), 3);
    }

    #[test]
    fn line_start_offset_snaps_to_column_zero() {
        // byte inside an indented first line of a method: we should snap
        // back to the `    ` at the start, so column numbering on the
        // displayed first line matches every subsequent line.
        let src = "impl Foo {\n    pub fn bar() {}\n}\n";
        let inside_bar = byte_offset_of(src, "pub fn bar");
        assert_eq!(line_start_offset(src, inside_bar), 11); // right after the first '\n'
        assert!(src[line_start_offset(src, inside_bar)..].starts_with("    pub fn bar"));
    }

    #[test]
    fn line_start_offset_handles_first_line() {
        let src = "pub fn bar() {}\n";
        assert_eq!(line_start_offset(src, 5), 0);
    }
}
