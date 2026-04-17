use anyhow::{anyhow, Result};
use complexity_compare::{match_reports, metric_vector, Mapping};
use complexity_core::{FunctionAnalysis, Metrics, Report};
use nalgebra::{DMatrix, DVector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub mod heuristics;

pub use heuristics::{apply_heuristics, HeuristicRule};

pub const TARGET_METRICS: &[&str] = &[
    "loc_code",
    "branches",
    "loops",
    "max_combined_nesting",
    "calls_unique",
    "calls_total",
    "cyclomatic",
    "cognitive",
    "halstead_volume",
    "halstead_difficulty",
    "early_returns",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearFit {
    pub coefs: Vec<f64>,
    pub intercept: f64,
    pub feature_order: Vec<String>,
    pub rmse: f64,
    pub residual_std: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub per_metric: BTreeMap<String, LinearFit>,
    pub heuristics: Vec<HeuristicRule>,
    pub feature_order: Vec<String>,
}

impl Model {
    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s).map_err(|e| anyhow!("write model {}: {}", path.display(), e))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("read model {}: {}", path.display(), e))?;
        Ok(serde_json::from_str(&s)?)
    }
}

pub fn feature_vector(f: &FunctionAnalysis) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = metric_vector(&f.metrics)
        .into_iter()
        .map(|(k, x)| (k.to_string(), x))
        .collect();
    v.push(("arity".to_string(), f.signature.inputs.len() as f64));
    v.push(("outputs_count".to_string(), f.signature.outputs.len() as f64));
    v.push((
        "constants".to_string(),
        f.constants.len() as f64,
    ));
    v.push((
        "types_used".to_string(),
        f.types_used.len() as f64,
    ));
    v.push((
        "loc_log".to_string(),
        (f.metrics.loc_code as f64 + 1.0).ln(),
    ));
    v
}

pub fn target_value(m: &Metrics, name: &str) -> f64 {
    match name {
        "loc_code" => m.loc_code as f64,
        "branches" => m.branches as f64,
        "loops" => m.loops as f64,
        "max_combined_nesting" => m.max_combined_nesting as f64,
        "calls_unique" => m.calls_unique as f64,
        "calls_total" => m.calls_total as f64,
        "cyclomatic" => m.cyclomatic as f64,
        "cognitive" => m.cognitive as f64,
        "halstead_volume" => m.halstead.volume,
        "halstead_difficulty" => m.halstead.difficulty,
        "early_returns" => m.early_returns as f64,
        _ => 0.0,
    }
}

pub struct TrainingExample<'a> {
    pub other: &'a FunctionAnalysis,
    pub rust: &'a FunctionAnalysis,
}

pub fn train(pairs: &[(Report, Report)]) -> Result<Model> {
    // Collect matched pairs across all report pairs (rust, other).
    let mut examples: Vec<TrainingExample> = Vec::new();
    for (rust, other) in pairs {
        let m = match_reports(rust, other, None);
        for p in &m.pairs {
            examples.push(TrainingExample {
                other: p.other,
                rust: p.rust,
            });
        }
    }
    if examples.is_empty() {
        return Err(anyhow!("no matched training pairs"));
    }

    // Build feature matrix from "other" side; targets are rust-side metrics.
    let feat_order: Vec<String> = feature_vector(examples[0].other)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let n = examples.len();
    let m = feat_order.len() + 1; // +1 intercept

    let mut x = DMatrix::<f64>::zeros(n, m);
    for (i, ex) in examples.iter().enumerate() {
        let fv = feature_vector(ex.other);
        for (j, (_k, v)) in fv.iter().enumerate() {
            x[(i, j)] = *v;
        }
        x[(i, m - 1)] = 1.0; // intercept
    }

    let mut per_metric: BTreeMap<String, LinearFit> = BTreeMap::new();
    for target in TARGET_METRICS {
        let mut y = DVector::<f64>::zeros(n);
        for (i, ex) in examples.iter().enumerate() {
            y[i] = target_value(&ex.rust.metrics, target);
        }
        let fit = fit_ols(&x, &y, &feat_order)?;
        per_metric.insert(target.to_string(), fit);
    }

    Ok(Model {
        per_metric,
        heuristics: heuristics::default_rules(),
        feature_order: feat_order,
    })
}

fn fit_ols(x: &DMatrix<f64>, y: &DVector<f64>, feat_order: &[String]) -> Result<LinearFit> {
    // β = (X^T X + λI)^{-1} X^T y  with small ridge for stability
    let xt = x.transpose();
    let mut xtx = &xt * x;
    let lambda = 1e-6;
    for i in 0..xtx.ncols() {
        xtx[(i, i)] += lambda;
    }
    let inv = xtx
        .try_inverse()
        .ok_or_else(|| anyhow!("singular feature matrix"))?;
    let beta = &inv * (&xt * y);
    let pred = x * &beta;
    let resid = y - &pred;
    let n = y.len() as f64;
    let rmse = (resid.dot(&resid) / n).sqrt();
    let mean_resid = resid.sum() / n;
    let var = resid
        .iter()
        .map(|r| (r - mean_resid).powi(2))
        .sum::<f64>()
        / n;
    let residual_std = var.sqrt().max(1e-6);

    let m = beta.len();
    let coefs = beta.rows(0, m - 1).iter().copied().collect::<Vec<_>>();
    let intercept = beta[m - 1];

    Ok(LinearFit {
        coefs,
        intercept,
        feature_order: feat_order.to_vec(),
        rmse,
        residual_std,
    })
}

impl LinearFit {
    pub fn predict(&self, features: &[(String, f64)]) -> f64 {
        // Align by feature_order; missing features => 0.
        let mut x_by_name: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
        for (k, v) in features {
            x_by_name.insert(k.as_str(), *v);
        }
        let mut y = self.intercept;
        for (i, name) in self.feature_order.iter().enumerate() {
            let v = *x_by_name.get(name.as_str()).unwrap_or(&0.0);
            y += self.coefs[i] * v;
        }
        y
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictedMetrics {
    pub predicted: BTreeMap<String, f64>,
    pub residual_std: BTreeMap<String, f64>,
}

pub fn predict_for(model: &Model, other: &FunctionAnalysis) -> PredictedMetrics {
    let feats = feature_vector(other);
    let mut predicted = BTreeMap::new();
    let mut residual_std = BTreeMap::new();
    for (name, fit) in &model.per_metric {
        let p = fit.predict(&feats);
        predicted.insert(name.clone(), p);
        residual_std.insert(name.clone(), fit.residual_std);
    }
    apply_heuristics(&model.heuristics, other, &mut predicted);
    PredictedMetrics {
        predicted,
        residual_std,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionReport {
    pub language_from: String,
    pub model_source: String,
    pub functions: Vec<FunctionPrediction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionPrediction {
    pub other_name: String,
    pub predicted: BTreeMap<String, f64>,
    pub residual_std: BTreeMap<String, f64>,
    /// If `--against` was provided, diff of actual - predicted in residual-std units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z_scores: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_rust_name: Option<String>,
}

pub fn predict_report(
    model: &Model,
    other: &Report,
    against: Option<(&Report, Option<&Mapping>)>,
) -> PredictionReport {
    let matched = against.map(|(rust, map)| match_reports(rust, other, map));
    let mut functions = Vec::new();
    for f in &other.functions {
        let pm = predict_for(model, f);
        let (actual, z, matched_rust) = if let Some(m) = &matched {
            let pair = m.pairs.iter().find(|p| p.other.name == f.name);
            if let Some(p) = pair {
                let mut actual = BTreeMap::new();
                let mut zs = BTreeMap::new();
                for name in TARGET_METRICS {
                    let a = target_value(&p.rust.metrics, name);
                    actual.insert(name.to_string(), a);
                    let pred = *pm.predicted.get(*name).unwrap_or(&0.0);
                    let rstd = *pm.residual_std.get(*name).unwrap_or(&1.0);
                    zs.insert(name.to_string(), (a - pred) / rstd.max(1e-6));
                }
                (Some(actual), Some(zs), Some(p.rust.name.clone()))
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

        functions.push(FunctionPrediction {
            other_name: f.name.clone(),
            predicted: pm.predicted,
            residual_std: pm.residual_std,
            actual,
            z_scores: z,
            matched_rust_name: matched_rust,
        });
    }
    PredictionReport {
        language_from: format!("{:?}", other.language),
        model_source: String::new(),
        functions,
    }
}
