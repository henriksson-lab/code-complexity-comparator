//! Mapping-driven pair discovery. For each mapping entry we resolve the
//! Rust side and the other-language side independently; either may come back
//! `None` (file or function not found), which is kept in the list.

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::analyzer::LanguageAnalyzer;
use crate::compare::matching::{path_suffix_matches, Mapping};
use crate::core::{FunctionAnalysis, Language, Metrics};
use crate::lang_c::CAnalyzer;
use crate::lang_fortran::FortranAnalyzer;
use crate::lang_java::JavaAnalyzer;
use crate::lang_perl::PerlAnalyzer;
use crate::lang_python::PythonAnalyzer;
use crate::lang_r::RAnalyzer;
use crate::lang_rust::RustAnalyzer;

use super::highlight::{tokenize_function, Token};
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

    let mut pairs = Vec::new();
    for entry in &mapping.entries {
        let (rust, rnote) = resolve_side(
            &rust_index,
            &mut file_cache,
            Language::Rust,
            &entry.rust,
            entry.rust_path.as_deref(),
        );
        let (other, onote) = resolve_side(
            &other_index,
            &mut file_cache,
            args.other_lang,
            &entry.other,
            entry.other_path.as_deref(),
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

fn resolve_side(
    index: &FileIndex,
    cache: &mut HashMap<PathBuf, FileAnalysis>,
    lang: Language,
    fn_name: &str,
    path_hint: Option<&str>,
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
        if let Some(fa) = analysis.functions.iter().find(|f| f.name == fn_name) {
            let tokens = tokenize_function(lang, &analysis.source, fa);
            let lines = slice_lines(&analysis.source, fa);
            return (
                Some(LocatedFn {
                    name: fa.name.clone(),
                    file: file.clone(),
                    line_start: fa.location.line_start,
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
        Some(format!(
            "'{}' not found in {} file(s)",
            fn_name,
            candidates.len()
        ))
    };
    (None, note)
}

fn slice_lines(source: &str, fa: &FunctionAnalysis) -> Vec<String> {
    let start = fa.location.byte_start as usize;
    let end = (fa.location.byte_end as usize).min(source.len());
    if start >= end {
        return Vec::new();
    }
    source[start..end]
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect()
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
