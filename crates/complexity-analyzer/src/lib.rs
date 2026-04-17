use anyhow::Result;
use complexity_core::{Language, Report};
use std::path::Path;

pub mod walker;
pub use walker::{analyze_function, collect_functions, find_kind_text, LanguageSpec, NodeClass};

pub trait LanguageAnalyzer: Send + Sync {
    fn language(&self) -> Language;
    fn extensions(&self) -> &[&'static str];
    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report>;

    fn analyze_file(&self, path: &Path) -> Result<Report> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {}", path.display(), e))?;
        self.analyze_source(&src, path)
    }
}

pub struct Registry {
    analyzers: Vec<Box<dyn LanguageAnalyzer>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { analyzers: Vec::new() }
    }

    pub fn register(&mut self, a: Box<dyn LanguageAnalyzer>) {
        self.analyzers.push(a);
    }

    pub fn for_language(&self, lang: Language) -> Option<&dyn LanguageAnalyzer> {
        self.analyzers
            .iter()
            .find(|a| a.language() == lang)
            .map(|b| b.as_ref())
    }

    pub fn for_extension(&self, ext: &str) -> Option<&dyn LanguageAnalyzer> {
        let e = ext.to_ascii_lowercase();
        self.analyzers
            .iter()
            .find(|a| a.extensions().iter().any(|x| x.eq_ignore_ascii_case(&e)))
            .map(|b| b.as_ref())
    }

    pub fn for_path(&self, path: &Path) -> Option<&dyn LanguageAnalyzer> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(|e| self.for_extension(e))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
