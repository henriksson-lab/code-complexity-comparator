use anyhow::Result;
use crate::core::{Language, Report};
use std::path::Path;

pub trait LanguageAnalyzer: Send + Sync {
    fn language(&self) -> Language;
    fn extensions(&self) -> &[&'static str];
    fn analyze_source(&self, src: &str, path: &Path) -> Result<Report>;

    fn analyze_file(&self, path: &Path) -> Result<Report> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("read {}: {}", path.display(), e))?;
        let src = decode_source_lossy_preserve_offsets(&bytes);
        self.analyze_source(&src, path)
    }
}

fn decode_source_lossy_preserve_offsets(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(src) => src.to_owned(),
        Err(_) => {
            let mut out = String::with_capacity(bytes.len());
            let mut rest = bytes;
            while !rest.is_empty() {
                match std::str::from_utf8(rest) {
                    Ok(valid) => {
                        out.push_str(valid);
                        break;
                    }
                    Err(err) => {
                        let valid_up_to = err.valid_up_to();
                        if valid_up_to > 0 {
                            out.push_str(std::str::from_utf8(&rest[..valid_up_to]).unwrap());
                        }
                        let invalid_len = err.error_len().unwrap_or(rest.len() - valid_up_to);
                        for _ in 0..invalid_len {
                            out.push('?');
                        }
                        rest = &rest[valid_up_to + invalid_len..];
                    }
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decode_source_lossy_preserve_offsets;

    #[test]
    fn lossy_decode_preserves_byte_offsets_for_invalid_source() {
        let bytes = b"abc\xff\xfed\xc3\xa9f";
        let decoded = decode_source_lossy_preserve_offsets(bytes);
        assert_eq!(decoded, "abc??déf");
        assert_eq!(decoded.as_bytes().len(), bytes.len());
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
