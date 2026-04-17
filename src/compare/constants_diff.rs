use crate::compare::matching::MatchResult;
use crate::core::{Constant, FunctionAnalysis};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstantsDiff {
    pub per_function: Vec<FunctionConstantsDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionConstantsDiff {
    pub rust_name: String,
    pub other_name: String,
    pub only_in_rust: Vec<ConstantSummary>,
    pub only_in_other: Vec<ConstantSummary>,
    pub in_both: Vec<ConstantSummary>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstantSummary {
    pub kind: String,
    pub display: String,
}

pub fn constants_diff(m: &MatchResult) -> ConstantsDiff {
    let mut per_function = Vec::new();
    for p in &m.pairs {
        let d = diff_pair(p.rust, p.other);
        per_function.push(d);
    }
    per_function.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    ConstantsDiff { per_function }
}

fn diff_pair(rust: &FunctionAnalysis, other: &FunctionAnalysis) -> FunctionConstantsDiff {
    // Match by kind + equivalent value, multiset-style.
    let mut rust_remaining: Vec<&Constant> = rust.constants.iter().collect();
    let mut only_in_other = Vec::new();
    let mut in_both = Vec::new();

    for c in &other.constants {
        let mut matched = None;
        for (i, rc) in rust_remaining.iter().enumerate() {
            if rc.kind_name() == c.kind_name() && rc.equivalent_to(c) {
                matched = Some(i);
                break;
            }
        }
        if let Some(i) = matched {
            let rc = rust_remaining.remove(i);
            in_both.push(ConstantSummary {
                kind: rc.kind_name().to_string(),
                display: rc.display(),
            });
        } else {
            only_in_other.push(ConstantSummary {
                kind: c.kind_name().to_string(),
                display: c.display(),
            });
        }
    }

    let only_in_rust: Vec<ConstantSummary> = rust_remaining
        .iter()
        .map(|c| ConstantSummary {
            kind: c.kind_name().to_string(),
            display: c.display(),
        })
        .collect();

    // Heavier penalty for kind-mismatched constants in same function.
    let kind_hist_rust = kind_hist(&rust.constants);
    let kind_hist_other = kind_hist(&other.constants);
    let mut kind_penalty = 0.0;
    let kinds = ["int", "float", "string", "char", "bool"];
    for k in kinds {
        let r = *kind_hist_rust.get(k).unwrap_or(&0) as f64;
        let o = *kind_hist_other.get(k).unwrap_or(&0) as f64;
        kind_penalty += (r - o).abs();
    }

    let score =
        only_in_rust.len() as f64 + only_in_other.len() as f64 + 0.5 * kind_penalty;

    FunctionConstantsDiff {
        rust_name: rust.name.clone(),
        other_name: other.name.clone(),
        only_in_rust,
        only_in_other,
        in_both,
        score,
    }
}

fn kind_hist(cs: &[Constant]) -> HashMap<&'static str, u32> {
    let mut h = HashMap::new();
    for c in cs {
        *h.entry(c.kind_name()).or_insert(0u32) += 1;
    }
    h
}
