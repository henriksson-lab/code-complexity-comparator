use anyhow::{anyhow, Result};
use crate::core::{FunctionAnalysis, Report};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStrategy {
    Mapping,
    FfiAttribute,
    ExactName,
    Normalized,
    Fingerprint,
}

#[derive(Debug, Clone)]
pub struct Pair<'a> {
    pub rust: &'a FunctionAnalysis,
    pub other: &'a FunctionAnalysis,
    pub strategy: MatchStrategy,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Mapping {
    #[serde(default)]
    pub entries: Vec<MappingEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MappingEntry {
    pub rust: String,
    pub other: String,
    /// Optional path constraint for the Rust function: a path *suffix* (in
    /// path-component units) of `Location.file`. When set, only Rust functions
    /// whose source file ends with these components are considered. Lets the
    /// user disambiguate same-named functions across modules — e.g.
    ///   rust = "decode", rust_path = "format/messages/datatype.rs"
    /// matches only the `decode` defined in
    ///   .../src/format/messages/datatype.rs
    /// and not other `decode` functions elsewhere in the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_path: Option<String>,
    /// Same as `rust_path`, for the other-language side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_path: Option<String>,
    /// Optional exact `line_start` for the Rust function. Fragile (shifts
    /// with edits), so prefer `rust_class` when you can. Still useful for
    /// free functions that aren't inside any class/impl, or to pin by line
    /// during bisection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_line: Option<u32>,
    /// Same as `rust_line`, for the other-language side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_line: Option<u32>,
    /// Optional enclosing type (impl-target in Rust, class in Python/Java/
    /// C++). Preferred over line pinning because it survives adding, moving,
    /// or reordering functions within a file. Matches `FunctionAnalysis.
    /// enclosing_type` exactly — e.g. `rust_class = "Cluster"` selects the
    /// method defined in `impl Cluster { ... }` or
    /// `impl SomeTrait for Cluster { ... }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_class: Option<String>,
    /// Same as `rust_class`, for the other-language side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_class: Option<String>,
}

impl Mapping {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("read mapping {}: {}", path.display(), e))?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "toml" {
            toml::from_str(&s).map_err(|e| anyhow!("parse mapping: {}", e))
        } else {
            serde_json::from_str(&s).map_err(|e| anyhow!("parse mapping: {}", e))
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatchResult<'a> {
    pub pairs: Vec<Pair<'a>>,
}

pub fn match_reports<'a>(
    rust: &'a Report,
    other: &'a Report,
    mapping: Option<&Mapping>,
) -> MatchResult<'a> {
    let mut pairs = Vec::new();
    let mut used_rust: HashSet<usize> = HashSet::new();
    let mut used_other: HashSet<usize> = HashSet::new();

    // 1. Explicit mapping.
    //
    // Each entry pins one Rust function to one other-language function by
    // name, optionally constrained by a path suffix on either side so the
    // same name in different modules can be disambiguated. When several
    // candidates remain after the optional path filter, the first unused
    // one wins — which means callers should pin both sides whenever a name
    // is overloaded across files.
    if let Some(m) = mapping {
        for e in &m.entries {
            let ri = rust.functions.iter().enumerate().find(|(i, f)| {
                !used_rust.contains(i)
                    && f.name == e.rust
                    && path_suffix_matches(&f.location.file, e.rust_path.as_deref())
                    && e.rust_line.is_none_or(|ln| f.location.line_start == ln)
                    && class_matches(f.enclosing_type.as_deref(), e.rust_class.as_deref())
            });
            let oi = other.functions.iter().enumerate().find(|(i, f)| {
                !used_other.contains(i)
                    && f.name == e.other
                    && path_suffix_matches(&f.location.file, e.other_path.as_deref())
                    && e.other_line.is_none_or(|ln| f.location.line_start == ln)
                    && class_matches(f.enclosing_type.as_deref(), e.other_class.as_deref())
            });
            if let (Some((ri, _)), Some((oi, _))) = (ri, oi) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: &rust.functions[ri],
                    other: &other.functions[oi],
                    strategy: MatchStrategy::Mapping,
                });
            }
        }
    }

    // 2. FFI attribute: rust.original_name matches other.name.
    for (ri, rf) in rust.functions.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        if let Some(orig) = &rf.original_name {
            if let Some((oi, _)) = other
                .functions
                .iter()
                .enumerate()
                .find(|(oi, f)| !used_other.contains(oi) && &f.name == orig)
            {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: &other.functions[oi],
                    strategy: MatchStrategy::FfiAttribute,
                });
            }
        }
    }

    // 3. Exact name match.
    let mut by_name_other: HashMap<&str, usize> = HashMap::new();
    for (oi, f) in other.functions.iter().enumerate() {
        by_name_other.insert(f.name.as_str(), oi);
    }
    for (ri, rf) in rust.functions.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        if let Some(&oi) = by_name_other.get(rf.name.as_str()) {
            if !used_other.contains(&oi) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: &other.functions[oi],
                    strategy: MatchStrategy::ExactName,
                });
            }
        }
    }

    // 4. Normalized name match.
    let mut by_norm_other: HashMap<String, Vec<usize>> = HashMap::new();
    for (oi, f) in other.functions.iter().enumerate() {
        if used_other.contains(&oi) {
            continue;
        }
        by_norm_other
            .entry(normalize_name(&f.name))
            .or_default()
            .push(oi);
    }
    for (ri, rf) in rust.functions.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        let norm = normalize_name(&rf.name);
        if let Some(cands) = by_norm_other.get(&norm) {
            if let Some(&oi) = cands.iter().find(|oi| !used_other.contains(oi)) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: &other.functions[oi],
                    strategy: MatchStrategy::Normalized,
                });
            }
        }
    }

    // 5. Fingerprint fallback: only when names share a substantial token.
    //   Short names ("w", "k", "map") are common as builder methods or
    //   accessors and produce too many spurious matches, so require the
    //   shared token to be >= 4 chars and the fingerprints to match exactly.
    let r_unmatched: Vec<usize> = (0..rust.functions.len())
        .filter(|i| !used_rust.contains(i))
        .collect();
    let o_unmatched: Vec<usize> = (0..other.functions.len())
        .filter(|i| !used_other.contains(i))
        .collect();
    for &ri in &r_unmatched {
        let rf = &rust.functions[ri];
        let fp_r = fingerprint(rf);
        let rtoks = tokenize_name(&rf.name);
        for &oi in &o_unmatched {
            if used_other.contains(&oi) {
                continue;
            }
            let of = &other.functions[oi];
            if fp_r != fingerprint(of) {
                continue;
            }
            let otoks = tokenize_name(&of.name);
            if shared_substantial_token(&rtoks, &otoks) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(Pair {
                    rust: rf,
                    other: of,
                    strategy: MatchStrategy::Fingerprint,
                });
                break;
            }
        }
    }

    MatchResult { pairs }
}

pub fn normalize_name(name: &str) -> String {
    // Lowercase, split on camelCase and snake_case, drop underscores and common
    // suffixes/prefixes like _impl, _inner, p_, _c, _rs.
    let mut chars = String::with_capacity(name.len());
    let mut prev_is_lower = false;
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && prev_is_lower {
            chars.push('_');
        }
        chars.push(ch.to_ascii_lowercase());
        prev_is_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    let s = chars.replace("::", "_");
    let parts: Vec<&str> = s
        .split('_')
        .filter(|p| !p.is_empty())
        .filter(|p| !matches!(*p, "impl" | "inner" | "helper" | "rs" | "c" | "cpp"))
        .collect();
    parts.join("_")
}

fn tokenize_name(name: &str) -> Vec<String> {
    normalize_name(name).split('_').map(|s| s.to_string()).collect()
}

fn shared_substantial_token(a: &[String], b: &[String]) -> bool {
    for ta in a {
        if ta.len() < 4 {
            continue;
        }
        for tb in b {
            if ta == tb {
                return true;
            }
        }
    }
    false
}

fn fingerprint(f: &FunctionAnalysis) -> (u32, u32, u32) {
    let loc_bucket = bucket_log(f.metrics.loc_code);
    let arity = f.signature.inputs.len() as u32;
    let outputs = f.signature.outputs.len() as u32;
    (arity, outputs, loc_bucket)
}

fn bucket_log(n: u32) -> u32 {
    if n == 0 {
        0
    } else {
        (n as f64).log2() as u32
    }
}

/// Returns true if the function's `enclosing_type` matches `want`. When
/// `want` is None, the constraint is satisfied vacuously. When `want` is
/// `Some("")` we require the function have no enclosing type (free fn).
/// Comparison is exact-string: `Cluster` matches a function recorded as
/// `Cluster` but not `ClusterRefiner` or `Option<Cluster>`.
pub(crate) fn class_matches(enclosing: Option<&str>, want: Option<&str>) -> bool {
    match want {
        None => true,
        Some("") => enclosing.is_none(),
        Some(w) => enclosing == Some(w),
    }
}

/// Returns true if `file` ends with the path components of `suffix`. When
/// `suffix` is None, the constraint is satisfied vacuously. Matching is on
/// path components, so `format/messages/datatype.rs` matches
/// `/abs/.../src/format/messages/datatype.rs` but not
/// `format/messages_datatype.rs` and not partial component prefixes
/// (`atype.rs` does NOT match `datatype.rs`).
pub(crate) fn path_suffix_matches(file: &Path, suffix: Option<&str>) -> bool {
    let Some(suffix) = suffix else {
        return true;
    };
    let want: Vec<&std::ffi::OsStr> = Path::new(suffix)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if want.is_empty() {
        return true;
    }
    let have: Vec<&std::ffi::OsStr> = file
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if have.len() < want.len() {
        return false;
    }
    have[have.len() - want.len()..] == want[..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Halstead, Language, Location, Metrics, Report, Signature};
    use std::path::PathBuf;

    fn fa(name: &str, file: &str) -> FunctionAnalysis {
        fa_at(name, file, 1)
    }

    fn fa_at(name: &str, file: &str, line_start: u32) -> FunctionAnalysis {
        fa_full(name, file, line_start, None)
    }

    fn fa_in_class(
        name: &str,
        file: &str,
        line_start: u32,
        class: &str,
    ) -> FunctionAnalysis {
        fa_full(name, file, line_start, Some(class.to_string()))
    }

    fn fa_full(
        name: &str,
        file: &str,
        line_start: u32,
        enclosing_type: Option<String>,
    ) -> FunctionAnalysis {
        FunctionAnalysis {
            name: name.into(),
            original_name: None,
            mangled: None,
            enclosing_type,
            location: Location {
                file: PathBuf::from(file),
                line_start,
                line_end: line_start + 1,
                col_start: 0,
                col_end: 0,
                byte_start: 0,
                byte_end: 0,
            },
            signature: Signature::default(),
            metrics: Metrics {
                halstead: Halstead::default(),
                ..Default::default()
            },
            constants: vec![],
            calls: vec![],
            types_used: vec![],
            attributes: Default::default(),
        }
    }

    fn rep(lang: Language, fns: Vec<FunctionAnalysis>) -> Report {
        Report {
            schema_version: crate::core::SCHEMA_VERSION,
            language: lang,
            source_file: PathBuf::from("/tmp/x"),
            source_hash: "0".into(),
            functions: fns,
            structs: Vec::new(),
        }
    }

    #[test]
    fn path_suffix_matches_basics() {
        let p = Path::new("/a/b/c/format/messages/datatype.rs");
        assert!(path_suffix_matches(p, None));
        assert!(path_suffix_matches(p, Some("datatype.rs")));
        assert!(path_suffix_matches(p, Some("messages/datatype.rs")));
        assert!(path_suffix_matches(p, Some("format/messages/datatype.rs")));
        assert!(path_suffix_matches(p, Some("/format/messages/datatype.rs")));
        assert!(!path_suffix_matches(p, Some("atype.rs")));
        assert!(!path_suffix_matches(p, Some("link.rs")));
        assert!(!path_suffix_matches(
            p,
            Some("wrong/messages/datatype.rs")
        ));
    }

    #[test]
    fn mapping_disambiguates_same_named_rust_functions() {
        // Two `decode` Rust functions in different files; mapping pins one of
        // them to a specific C target by path. The other should remain
        // available for a second mapping entry or for fingerprint matching.
        let r = rep(
            Language::Rust,
            vec![
                fa("decode", "/abs/src/format/messages/datatype.rs"),
                fa("decode", "/abs/src/format/messages/link.rs"),
            ],
        );
        let o = rep(
            Language::C,
            vec![
                fa("H5O__dtype_decode", "/abs/hdf5/src/H5Odtype.c"),
                fa("H5O__link_decode", "/abs/hdf5/src/H5Olink.c"),
            ],
        );
        let m = Mapping {
            entries: vec![
                MappingEntry {
                    rust: "decode".into(),
                    rust_path: Some("messages/datatype.rs".into()),
                    other: "H5O__dtype_decode".into(),
                    ..Default::default()
                },
                MappingEntry {
                    rust: "decode".into(),
                    rust_path: Some("messages/link.rs".into()),
                    other: "H5O__link_decode".into(),
                    ..Default::default()
                },
            ],
        };
        let res = match_reports(&r, &o, Some(&m));
        assert_eq!(res.pairs.len(), 2);
        for p in &res.pairs {
            assert_eq!(p.strategy, MatchStrategy::Mapping);
            let rfile = p.rust.location.file.to_string_lossy().to_string();
            let ofile = p.other.location.file.to_string_lossy().to_string();
            match rfile.as_str() {
                f if f.ends_with("datatype.rs") => assert!(ofile.ends_with("H5Odtype.c")),
                f if f.ends_with("link.rs") => assert!(ofile.ends_with("H5Olink.c")),
                f => panic!("unexpected rust file in pair: {}", f),
            }
        }
    }

    #[test]
    fn mapping_without_path_falls_back_to_first_unused() {
        // Bare-name mapping (no rust_path / other_path) keeps prior behavior:
        // the first unused candidate on each side is paired by Mapping. The
        // remaining duplicates fall through to later strategies.
        let r = rep(
            Language::Rust,
            vec![
                fa("decode", "/abs/src/a.rs"),
                fa("decode", "/abs/src/b.rs"),
            ],
        );
        let o = rep(
            Language::C,
            vec![
                fa("c_decode", "/abs/hdf5/src/x.c"),
                fa("c_decode", "/abs/hdf5/src/y.c"),
            ],
        );
        let m = Mapping {
            entries: vec![MappingEntry {
                rust: "decode".into(),
                other: "c_decode".into(),
                ..Default::default()
            }],
        };
        let res = match_reports(&r, &o, Some(&m));
        let mapping_pairs: Vec<&Pair> = res
            .pairs
            .iter()
            .filter(|p| p.strategy == MatchStrategy::Mapping)
            .collect();
        assert_eq!(mapping_pairs.len(), 1, "exactly one Mapping pair expected");
        assert!(mapping_pairs[0].rust.location.file.ends_with("a.rs"));
        assert!(mapping_pairs[0].other.location.file.ends_with("x.c"));
    }

    #[test]
    fn mapping_disambiguates_by_line_within_single_file() {
        // Three `new` methods in one Rust file — one impl block per class.
        // Path suffix can't distinguish them; only line pinning can.
        let r = rep(
            Language::Rust,
            vec![
                fa_at("new", "/abs/src/model.rs", 21),   // Strand::new
                fa_at("new", "/abs/src/model.rs", 153),  // Protein::new
                fa_at("new", "/abs/src/model.rs", 292),  // Cluster::new
            ],
        );
        let o = rep(
            Language::Python,
            vec![
                fa_at("__init__", "/abs/gecco/model.py", 56),   // Strand
                fa_at("__init__", "/abs/gecco/model.py", 412),  // Cluster
            ],
        );
        let m = Mapping {
            entries: vec![
                MappingEntry {
                    rust: "new".into(),
                    rust_path: Some("src/model.rs".into()),
                    rust_line: Some(21),
                    other: "__init__".into(),
                    other_path: Some("gecco/model.py".into()),
                    other_line: Some(56),
                    ..Default::default()
                },
                MappingEntry {
                    rust: "new".into(),
                    rust_path: Some("src/model.rs".into()),
                    rust_line: Some(292),
                    other: "__init__".into(),
                    other_path: Some("gecco/model.py".into()),
                    other_line: Some(412),
                    ..Default::default()
                },
            ],
        };
        let res = match_reports(&r, &o, Some(&m));
        let mapped: Vec<&Pair> = res
            .pairs
            .iter()
            .filter(|p| p.strategy == MatchStrategy::Mapping)
            .collect();
        assert_eq!(mapped.len(), 2, "both entries should resolve");
        // Strand pair: Rust line 21 <-> Python line 56.
        assert!(mapped.iter().any(|p| p.rust.location.line_start == 21
            && p.other.location.line_start == 56));
        // Cluster pair: Rust line 292 <-> Python line 412. Protein::new at
        // line 153 must be skipped over despite sitting between the two
        // mapped Rust functions in source order.
        assert!(mapped.iter().any(|p| p.rust.location.line_start == 292
            && p.other.location.line_start == 412));
    }

    #[test]
    fn mapping_disambiguates_by_enclosing_class() {
        // Three `new` methods in one Rust file, each in a different impl
        // block. Class pinning picks the right one regardless of source
        // order — and is stable under code movement (unlike line pinning).
        let r = rep(
            Language::Rust,
            vec![
                fa_in_class("new", "/abs/src/model.rs", 21, "Strand"),
                fa_in_class("new", "/abs/src/model.rs", 153, "Protein"),
                fa_in_class("new", "/abs/src/model.rs", 292, "Cluster"),
            ],
        );
        let o = rep(
            Language::Python,
            vec![
                fa_in_class("__init__", "/abs/gecco/model.py", 56, "Strand"),
                fa_in_class("__init__", "/abs/gecco/model.py", 412, "Cluster"),
            ],
        );
        let m = Mapping {
            entries: vec![
                MappingEntry {
                    rust: "new".into(),
                    rust_class: Some("Strand".into()),
                    other: "__init__".into(),
                    other_class: Some("Strand".into()),
                    ..Default::default()
                },
                MappingEntry {
                    rust: "new".into(),
                    rust_class: Some("Cluster".into()),
                    other: "__init__".into(),
                    other_class: Some("Cluster".into()),
                    ..Default::default()
                },
            ],
        };
        let res = match_reports(&r, &o, Some(&m));
        let mapped: Vec<&Pair> = res
            .pairs
            .iter()
            .filter(|p| p.strategy == MatchStrategy::Mapping)
            .collect();
        assert_eq!(mapped.len(), 2);
        for p in &mapped {
            assert_eq!(
                p.rust.enclosing_type, p.other.enclosing_type,
                "mapped pairs should share an enclosing class"
            );
        }
        // Protein::new must remain unmapped — no corresponding Python entry.
        assert!(
            !res.pairs
                .iter()
                .any(|p| p.rust.enclosing_type.as_deref() == Some("Protein")
                    && p.strategy == MatchStrategy::Mapping),
            "Protein::new has no Python counterpart; must not be mapped"
        );
    }

    #[test]
    fn class_matches_empty_string_targets_free_functions() {
        // `rust_class = ""` constrains to enclosing_type == None (a module-
        // level fn), not to any class literally named "".
        assert!(class_matches(None, Some("")));
        assert!(!class_matches(Some("Foo"), Some("")));
        assert!(class_matches(Some("Foo"), None));
        assert!(class_matches(Some("Foo"), Some("Foo")));
        assert!(!class_matches(Some("Foo"), Some("Bar")));
    }

    #[test]
    fn mapping_skips_when_line_filter_has_no_match() {
        // No function at the requested line_start -> no Mapping pair.
        let r = rep(Language::Rust, vec![fa_at("new", "/abs/src/model.rs", 21)]);
        let o = rep(
            Language::Python,
            vec![fa_at("__init__", "/abs/gecco/model.py", 56)],
        );
        let m = Mapping {
            entries: vec![MappingEntry {
                rust: "new".into(),
                rust_line: Some(999),
                other: "__init__".into(),
                ..Default::default()
            }],
        };
        let res = match_reports(&r, &o, Some(&m));
        assert!(
            !res.pairs.iter().any(|p| p.strategy == MatchStrategy::Mapping),
            "line filter eliminates the only candidate"
        );
    }

    #[test]
    fn mapping_skips_when_path_filter_eliminates_all_candidates() {
        // The path filter rules out the only Rust candidate, so no Mapping
        // pair is created. Later strategies (Normalized) may still pair the
        // functions, but with strategy != Mapping.
        let r = rep(Language::Rust, vec![fa("decode", "/abs/src/a.rs")]);
        let o = rep(Language::C, vec![fa("c_decode", "/abs/hdf5/src/x.c")]);
        let m = Mapping {
            entries: vec![MappingEntry {
                rust: "decode".into(),
                rust_path: Some("nope/wrong.rs".into()),
                other: "c_decode".into(),
                ..Default::default()
            }],
        };
        let res = match_reports(&r, &o, Some(&m));
        assert!(
            !res.pairs.iter().any(|p| p.strategy == MatchStrategy::Mapping),
            "no pair should be created via Mapping strategy"
        );
    }
}
