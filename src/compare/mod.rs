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

pub use matching::{match_reports, Mapping, MatchResult, MatchStrategy, Pair};
pub use deviation::{deviation_rows, DeviationRow, Weights};
pub use constants_diff::{constants_diff, ConstantsDiff, FunctionConstantsDiff};
pub use sort::{sort_report, SortKey};
pub use structs::{
    category_histogram, match_structs, struct_deviation_rows, struct_metric_vector,
    struct_missing, StructDeviationRow, StructMatchResult, StructMatchStrategy, StructMissingReport,
    StructPair,
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

pub fn missing(
    rust: &Report,
    other: &Report,
    m: &MatchResult,
    stub_loc_ratio: f64,
) -> MissingReport {
    let matched_rust: HashSet<&str> = m.pairs.iter().map(|p| p.rust.name.as_str()).collect();
    let matched_other: HashSet<&str> = m.pairs.iter().map(|p| p.other.name.as_str()).collect();

    let mut missing_in_rust = Vec::new();
    for f in &other.functions {
        if !matched_other.contains(f.name.as_str()) {
            missing_in_rust.push(f.name.clone());
        }
    }
    let mut extra_in_rust = Vec::new();
    for f in &rust.functions {
        if !matched_rust.contains(f.name.as_str()) {
            extra_in_rust.push(f.name.clone());
        }
    }

    let mut partial = Vec::new();
    for p in &m.pairs {
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
