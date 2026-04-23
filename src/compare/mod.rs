use anyhow::{anyhow, Result};
use crate::core::{Constant, FunctionAnalysis, Language, Metrics, Report};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub mod matching;
pub mod deviation;
pub mod constants_diff;
pub mod sort;
pub mod structs;
pub mod upstream;
pub mod call_graph;

pub use matching::{match_reports, Mapping, MatchResult, MatchStrategy, Pair};
pub use deviation::{deviation_rows, DeviationRow, Weights};
pub use constants_diff::{constants_diff, ConstantsDiff, FunctionConstantsDiff};
pub use sort::{sort_report, SortKey};
pub use structs::{
    category_histogram, match_structs, struct_deviation_rows, struct_metric_vector,
    struct_missing, StructDeviationRow, StructMatchResult, StructMatchStrategy, StructMissingReport,
    StructPair,
};
pub use upstream::{
    analyze_upstream, FunctionRef, FunctionSelector, UpstreamAnalysis, UpstreamPairRow,
    UpstreamWarning,
};
pub use call_graph::{
    analyze_call_graph_diff, CallGraphDiffAnalysis, CallGraphDiffSummary, CallGraphPairDiffRow,
    GraphEdgeRef,
};

pub fn load_report(path: &Path) -> Result<Report> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read {}: {}", path.display(), e))?;
    let r: Report = serde_json::from_str(&s)
        .map_err(|e| anyhow!("parse {}: {}", path.display(), e))?;
    Ok(r)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingReport {
    pub missing_in_rust: Vec<String>,
    pub extra_in_rust: Vec<String>,
    pub partial: Vec<PartialMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialMatch {
    pub rust_name: String,
    pub other_name: String,
    pub reason: String,
}

fn function_key(f: &FunctionAnalysis) -> (String, String, u32) {
    (
        f.name.clone(),
        f.location.file.to_string_lossy().into_owned(),
        f.location.line_start,
    )
}

fn is_constructor_name(name: &str) -> bool {
    let Some((class, func)) = name.rsplit_once("::") else {
        return false;
    };
    !class.is_empty() && class == func
}

fn is_destructor_name(name: &str) -> bool {
    let Some((class, func)) = name.rsplit_once("::") else {
        return false;
    };
    func == format!("~{}", class)
}

fn is_thin_bin_main_wrapper(pair: &Pair<'_>) -> bool {
    pair.other.name == "main"
        && pair.rust.name == "main"
        && pair.rust.metrics.loc_code <= 5
        && pair
            .rust
            .location
            .file
            .to_string_lossy()
            .contains("src/bin/")
}

fn should_exempt_partial(pair: &Pair<'_>) -> bool {
    pair.strategy == MatchStrategy::Mapping
        && ((pair.rust.metrics.loc_code <= 1
            && (is_constructor_name(&pair.other.name) || is_destructor_name(&pair.other.name)))
            || is_thin_bin_main_wrapper(pair))
}

pub fn missing(
    rust: &Report,
    other: &Report,
    m: &MatchResult,
    stub_loc_ratio: f64,
) -> MissingReport {
    let matched_rust: HashSet<(String, String, u32)> =
        m.pairs.iter().map(|p| function_key(p.rust)).collect();
    let matched_other: HashSet<(String, String, u32)> =
        m.pairs.iter().map(|p| function_key(p.other)).collect();

    let mut missing_in_rust = Vec::new();
    for f in &other.functions {
        if !matched_other.contains(&function_key(f)) {
            missing_in_rust.push(f.name.clone());
        }
    }
    let mut extra_in_rust = Vec::new();
    for f in &rust.functions {
        if !matched_rust.contains(&function_key(f)) {
            extra_in_rust.push(f.name.clone());
        }
    }

    let mut partial = Vec::new();
    for p in &m.pairs {
        if should_exempt_partial(p) {
            continue;
        }
        let r_loc = p.rust.metrics.loc_code.max(1) as f64;
        let o_loc = p.other.metrics.loc_code.max(1) as f64;
        let ratio = r_loc / o_loc;
        if ratio < stub_loc_ratio {
            partial.push(PartialMatch {
                rust_name: p.rust.name.clone(),
                other_name: p.other.name.clone(),
                reason: format!(
                    "rust LOC {} is {:.0}% of other LOC {}",
                    p.rust.metrics.loc_code,
                    100.0 * ratio,
                    p.other.metrics.loc_code
                ),
            });
        }
    }
    MissingReport { missing_in_rust, extra_in_rust, partial }
}

pub fn summary_line(r: &Report) -> String {
    format!(
        "{}: {:?} {} functions",
        r.source_file.display(),
        r.language,
        r.functions.len()
    )
}

#[allow(dead_code)]
fn languages_compatible(a: Language, b: Language) -> bool {
    matches!(
        (a, b),
        (Language::Rust, _) | (_, Language::Rust) | (Language::C, Language::Cpp) | (Language::Cpp, Language::C)
    )
}

pub fn metric_vector(m: &Metrics) -> Vec<(&'static str, f64)> {
    vec![
        ("loc_code", m.loc_code as f64),
        ("loc_comments", m.loc_comments as f64),
        ("branches", m.branches as f64),
        ("loops", m.loops as f64),
        ("max_loop_nesting", m.max_loop_nesting as f64),
        ("max_if_nesting", m.max_if_nesting as f64),
        ("max_combined_nesting", m.max_combined_nesting as f64),
        ("calls_unique", m.calls_unique as f64),
        ("calls_total", m.calls_total as f64),
        ("cyclomatic", m.cyclomatic as f64),
        ("cognitive", m.cognitive as f64),
        ("halstead_volume", m.halstead.volume),
        ("halstead_difficulty", m.halstead.difficulty),
        ("early_returns", m.early_returns as f64),
        ("goto_count", m.goto_count as f64),
        ("inputs", m.inputs as f64),
        ("outputs", m.outputs as f64),
    ]
}

pub fn constants_histogram(fa: &FunctionAnalysis) -> HashMap<&'static str, u32> {
    let mut h = HashMap::new();
    for c in &fa.constants {
        *h.entry(c.kind_name()).or_insert(0) += 1;
    }
    h
}

pub fn dedup_map<T: Clone>(v: &[T], key: impl Fn(&T) -> String) -> BTreeMap<String, u32> {
    let mut m = BTreeMap::new();
    for item in v {
        *m.entry(key(item)).or_insert(0u32) += 1;
    }
    m
}

pub fn constants_by_kind<'a>(fa: &'a FunctionAnalysis) -> BTreeMap<&'static str, Vec<&'a Constant>> {
    let mut m: BTreeMap<&'static str, Vec<&Constant>> = BTreeMap::new();
    for c in &fa.constants {
        m.entry(c.kind_name()).or_default().push(c);
    }
    m
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::matching::{match_reports, Mapping, MappingEntry};
    use crate::core::{Halstead, Location, Signature, TypeRef};
    use std::path::PathBuf;

    fn fa(name: &str, file: &str, line_start: u32) -> FunctionAnalysis {
        FunctionAnalysis {
            name: name.into(),
            original_name: None,
            mangled: None,
            enclosing_type: None,
            location: Location {
                file: PathBuf::from(file),
                line_start,
                line_end: line_start,
                col_start: 0,
                col_end: 0,
                byte_start: 0,
                byte_end: 0,
            },
            signature: Signature::default(),
            metrics: Metrics {
                loc_code: 10,
                halstead: Halstead::default(),
                ..Metrics::default()
            },
            constants: Vec::new(),
            calls: Vec::new(),
            types_used: vec![TypeRef::new("void")],
            attributes: BTreeMap::new(),
        }
    }

    fn rep(language: Language, functions: Vec<FunctionAnalysis>) -> Report {
        Report {
            schema_version: crate::core::SCHEMA_VERSION,
            language,
            source_file: PathBuf::from("/tmp/test"),
            source_hash: "0".into(),
            functions,
            structs: Vec::new(),
        }
    }

    #[test]
    fn missing_respects_mapping_for_duplicate_names() {
        let rust = rep(
            Language::Rust,
            vec![
                fa("helper", "/abs/src/a.rs", 10),
                fa("helper", "/abs/src/b.rs", 20),
            ],
        );
        let other = rep(
            Language::C,
            vec![
                fa("target_c", "/abs/orig/a.c", 100),
                fa("other_c", "/abs/orig/b.c", 200),
            ],
        );
        let mapping = Mapping {
            entries: vec![MappingEntry {
                rust: "helper".into(),
                rust_path: Some("src/b.rs".into()),
                other: "other_c".into(),
                other_path: Some("orig/b.c".into()),
                ..Default::default()
            }],
        };

        let matched = match_reports(&rust, &other, Some(&mapping));
        assert_eq!(matched.pairs.len(), 1);
        assert_eq!(matched.pairs[0].rust.location.line_start, 20);
        assert_eq!(matched.pairs[0].other.location.line_start, 200);

        let rep = missing(&rust, &other, &matched, 0.2);
        assert_eq!(rep.missing_in_rust, vec!["target_c".to_string()]);
        assert_eq!(rep.extra_in_rust, vec!["helper".to_string()]);
    }

    #[test]
    fn missing_exempts_mapped_destructor_wrappers_from_partial() {
        let mut rust_f = fa("destroy", "/abs/src/compact_hash.rs", 10);
        rust_f.metrics.loc_code = 1;
        let rust = rep(Language::Rust, vec![rust_f]);

        let mut other_f = fa(
            "CompactHashTable::~CompactHashTable",
            "/abs/orig/compact_hash.cc",
            48,
        );
        other_f.metrics.loc_code = 6;
        let other = rep(Language::Cpp, vec![other_f]);
        let mapping = Mapping {
            entries: vec![MappingEntry {
                rust: "destroy".into(),
                rust_path: Some("src/compact_hash.rs".into()),
                other: "CompactHashTable::~CompactHashTable".into(),
                other_path: Some("orig/compact_hash.cc".into()),
                ..Default::default()
            }],
        };

        let matched = match_reports(&rust, &other, Some(&mapping));
        assert_eq!(matched.pairs.len(), 1);

        let rep = missing(&rust, &other, &matched, 0.2);
        assert!(rep.partial.is_empty());
    }


    #[test]
    fn missing_exempts_mapped_bin_main_wrappers_from_partial() {
        let mut rust_f = fa("main", "/abs/project/src/bin/classify.rs", 3);
        rust_f.metrics.loc_code = 5;
        let rust = rep(Language::Rust, vec![rust_f]);

        let mut other_f = fa("main", "/abs/orig/classify.cc", 416);
        other_f.metrics.loc_code = 62;
        let other = rep(Language::Cpp, vec![other_f]);
        let mapping = Mapping {
            entries: vec![MappingEntry {
                rust: "main".into(),
                rust_path: Some("src/bin/classify.rs".into()),
                other: "main".into(),
                other_path: Some("classify.cc".into()),
                ..Default::default()
            }],
        };

        let matched = match_reports(&rust, &other, Some(&mapping));
        assert_eq!(matched.pairs.len(), 1);

        let rep = missing(&rust, &other, &matched, 0.2);
        assert!(rep.partial.is_empty());
    }
}
