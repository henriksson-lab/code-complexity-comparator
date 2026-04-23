//! Struct comparison: matching (same strategies as functions minus FFI
//! attributes, since structs don't carry `#[no_mangle]`) and a deviation
//! score built from per-type-category field counts.

use crate::compare::matching::{normalize_name, Mapping};
use crate::core::{Report, StructAnalysis, StructMetrics, TypeCategory};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StructMatchStrategy {
    Mapping,
    ExactName,
    Normalized,
}

#[derive(Debug, Clone)]
pub struct StructPair<'a> {
    pub rust: &'a StructAnalysis,
    pub other: &'a StructAnalysis,
    pub strategy: StructMatchStrategy,
}

#[derive(Debug, Clone)]
pub struct StructMatchResult<'a> {
    pub pairs: Vec<StructPair<'a>>,
}

/// Match structs across two reports. Mapping entries are reused from the
/// existing function `Mapping` — the `rust` / `other` fields on each entry
/// are checked against struct names when no function matched. This keeps
/// users from having to maintain a second mapping file for structs that
/// also happen to share a name with a function (rare but possible).
pub fn match_structs<'a>(
    rust: &'a Report,
    other: &'a Report,
    mapping: Option<&Mapping>,
) -> StructMatchResult<'a> {
    let mut pairs = Vec::new();
    let mut used_rust: HashSet<usize> = HashSet::new();
    let mut used_other: HashSet<usize> = HashSet::new();

    if let Some(m) = mapping {
        for e in &m.entries {
            let ri = rust
                .structs
                .iter()
                .enumerate()
                .find(|(i, s)| !used_rust.contains(i) && s.name == e.rust)
                .map(|(i, _)| i);
            let oi = other
                .structs
                .iter()
                .enumerate()
                .find(|(i, s)| !used_other.contains(i) && s.name == e.other)
                .map(|(i, _)| i);
            if let (Some(ri), Some(oi)) = (ri, oi) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(StructPair {
                    rust: &rust.structs[ri],
                    other: &other.structs[oi],
                    strategy: StructMatchStrategy::Mapping,
                });
            }
        }
    }

    let mut by_name_other: HashMap<&str, usize> = HashMap::new();
    for (oi, s) in other.structs.iter().enumerate() {
        by_name_other.insert(s.name.as_str(), oi);
    }
    for (ri, rs) in rust.structs.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        if let Some(&oi) = by_name_other.get(rs.name.as_str()) {
            if !used_other.contains(&oi) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(StructPair {
                    rust: rs,
                    other: &other.structs[oi],
                    strategy: StructMatchStrategy::ExactName,
                });
            }
        }
    }

    let mut by_norm_other: HashMap<String, Vec<usize>> = HashMap::new();
    for (oi, s) in other.structs.iter().enumerate() {
        if used_other.contains(&oi) {
            continue;
        }
        by_norm_other
            .entry(normalize_name(&s.name))
            .or_default()
            .push(oi);
    }
    for (ri, rs) in rust.structs.iter().enumerate() {
        if used_rust.contains(&ri) {
            continue;
        }
        let norm = normalize_name(&rs.name);
        if let Some(cands) = by_norm_other.get(&norm) {
            if let Some(&oi) = cands.iter().find(|oi| !used_other.contains(oi)) {
                used_rust.insert(ri);
                used_other.insert(oi);
                pairs.push(StructPair {
                    rust: rs,
                    other: &other.structs[oi],
                    strategy: StructMatchStrategy::Normalized,
                });
            }
        }
    }

    StructMatchResult { pairs }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructDeviationRow {
    pub rust_name: String,
    pub other_name: String,
    pub total: f64,
    /// Per-category (category, rust_count, other_count, contribution) —
    /// ordered by contribution descending so the biggest gap bubbles to the
    /// top in table output.
    pub per_category: Vec<(String, f64, f64, f64)>,
}

/// Compute struct deviations using per-type-category field counts plus
/// total `field_count` as features. Scaling is by 95th percentile across
/// both reports' structs so a struct with two ints isn't dominated by a
/// giant 200-field one.
pub fn struct_deviation_rows(
    rust: &Report,
    other: &Report,
    m: &StructMatchResult,
) -> Vec<StructDeviationRow> {
    let mut samples: HashMap<&'static str, Vec<f64>> = HashMap::new();
    for s in rust.structs.iter().chain(other.structs.iter()) {
        for (k, v) in struct_metric_vector(&s.metrics) {
            samples.entry(k).or_default().push(v);
        }
    }
    let mut scale: HashMap<&'static str, f64> = HashMap::new();
    for (k, mut v) in samples {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        scale.insert(k, percentile(&v, 0.95).max(1.0));
    }

    let mut rows = Vec::new();
    for p in &m.pairs {
        let rv = struct_metric_vector(&p.rust.metrics);
        let ov = struct_metric_vector(&p.other.metrics);
        let mut total = 0.0;
        let mut per = Vec::new();
        for ((k, rv), (_, ov)) in rv.iter().zip(ov.iter()) {
            let s = *scale.get(k).unwrap_or(&1.0);
            let w = category_weight(k);
            let d = (rv - ov).abs() / s;
            let contrib = w * d;
            total += contrib;
            per.push((k.to_string(), *rv, *ov, contrib));
        }
        per.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        rows.push(StructDeviationRow {
            rust_name: p.rust.name.clone(),
            other_name: p.other.name.clone(),
            total,
            per_category: per,
        });
    }
    rows.sort_by(|a, b| b.total.partial_cmp(&a.total).unwrap_or(std::cmp::Ordering::Equal));
    rows
}

/// Feature vector for a struct. The primary features are per-category
/// field counts (the user's asked-for comparison feature); `field_count`
/// is added so that "same categories but wildly different sizes" still
/// shows up as a deviation.
pub fn struct_metric_vector(m: &StructMetrics) -> Vec<(&'static str, f64)> {
    vec![
        ("field_count", m.field_count as f64),
        ("int", m.int_count as f64),
        ("float", m.float_count as f64),
        ("bool", m.bool_count as f64),
        ("char", m.char_count as f64),
        ("string", m.string_count as f64),
        ("pointer", m.pointer_count as f64),
        ("array", m.array_count as f64),
        ("collection", m.collection_count as f64),
        ("other", m.other_count as f64),
    ]
}

/// Weights for the deviation total. `other` carries a lower weight because
/// user-defined types (which land in `Other`) naturally diverge in name
/// across a port even when they mean the same thing; the primitive
/// categories are the trustworthy comparison signal.
fn category_weight(k: &str) -> f64 {
    match k {
        "field_count" => 1.0,
        "other" => 0.5,
        _ => 1.0,
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructMissingReport {
    pub missing_in_rust: Vec<String>,
    pub extra_in_rust: Vec<String>,
}

pub fn struct_missing(
    rust: &Report,
    other: &Report,
    m: &StructMatchResult,
) -> StructMissingReport {
    let matched_rust: HashSet<&str> = m.pairs.iter().map(|p| p.rust.name.as_str()).collect();
    let matched_other: HashSet<&str> = m.pairs.iter().map(|p| p.other.name.as_str()).collect();
    let missing_in_rust = other
        .structs
        .iter()
        .filter(|s| !matched_other.contains(s.name.as_str()))
        .map(|s| s.name.clone())
        .collect();
    let extra_in_rust = rust
        .structs
        .iter()
        .filter(|s| !matched_rust.contains(s.name.as_str()))
        .map(|s| s.name.clone())
        .collect();
    StructMissingReport {
        missing_in_rust,
        extra_in_rust,
    }
}

/// Build a compact histogram keyed by the `TypeCategory` string name.
/// Useful when you just want the count-per-type for one struct without
/// the full metrics struct.
pub fn category_histogram(s: &StructAnalysis) -> HashMap<&'static str, u32> {
    let mut h = HashMap::new();
    for f in &s.fields {
        *h.entry(f.category.as_str()).or_insert(0) += 1;
    }
    // Seed zero-counts so consumers get a stable set of keys regardless of
    // which categories the struct uses.
    for c in TypeCategory::all() {
        h.entry(c.as_str()).or_insert(0);
    }
    h
}
