use crate::{matching::MatchResult, metric_vector};
use complexity_core::Report;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Weights {
    pub per_metric: HashMap<String, f64>,
}

impl Default for Weights {
    fn default() -> Self {
        let mut m = HashMap::new();
        for (k, v) in [
            ("cyclomatic", 2.0),
            ("cognitive", 2.0),
            ("max_combined_nesting", 2.0),
            ("calls_total", 1.0),
            ("calls_unique", 1.0),
            ("loc_code", 1.0),
            ("branches", 1.5),
            ("loops", 1.0),
            ("halstead_volume", 1.0),
            ("halstead_difficulty", 1.0),
            ("inputs", 0.5),
            ("outputs", 0.5),
            ("goto_count", 1.0),
            ("early_returns", 0.5),
        ] {
            m.insert(k.to_string(), v);
        }
        Self { per_metric: m }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviationRow {
    pub rust_name: String,
    pub other_name: String,
    pub total: f64,
    pub per_metric: Vec<(String, f64, f64, f64)>, // (metric, rust_val, other_val, contribution)
}

pub fn deviation_rows(
    rust: &Report,
    other: &Report,
    m: &MatchResult,
    w: &Weights,
) -> Vec<DeviationRow> {
    // Build 95th percentile scale per metric from the union of both reports.
    let mut samples: HashMap<&'static str, Vec<f64>> = HashMap::new();
    for f in rust.functions.iter().chain(other.functions.iter()) {
        for (k, v) in metric_vector(&f.metrics) {
            samples.entry(k).or_default().push(v);
        }
    }
    let mut scale: HashMap<&'static str, f64> = HashMap::new();
    for (k, mut v) in samples {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95 = percentile(&v, 0.95);
        scale.insert(k, p95.max(1.0));
    }

    let mut rows = Vec::new();
    for p in &m.pairs {
        let rv = metric_vector(&p.rust.metrics);
        let ov = metric_vector(&p.other.metrics);
        let mut total = 0.0;
        let mut per = Vec::new();
        for ((k, rv), (_, ov)) in rv.iter().zip(ov.iter()) {
            let w = *w.per_metric.get(*k).unwrap_or(&1.0);
            let s = *scale.get(k).unwrap_or(&1.0);
            let d = (rv - ov).abs() / s;
            let contrib = w * d;
            total += contrib;
            per.push((k.to_string(), *rv, *ov, contrib));
        }
        per.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        rows.push(DeviationRow {
            rust_name: p.rust.name.clone(),
            other_name: p.other.name.clone(),
            total,
            per_metric: per,
        });
    }
    rows.sort_by(|a, b| b.total.partial_cmp(&a.total).unwrap_or(std::cmp::Ordering::Equal));
    rows
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
